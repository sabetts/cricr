//! Plain-text scrollback cache: one file per channel/query, one logical
//! (unwrapped) line per entry, each prefixed with an absolute RFC3339
//! timestamp. On load the lines are re-rendered (colors) and re-wrapped at the
//! current fill column, so a changed width or theme applies to old history.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, TimeZone};

use crate::config::Config;

/// Marks the on-disk format; a parser bump can ignore older files.
const HEADER: &str = "# cricr history v1";

/// A parsed cache entry: absolute time (Unix epoch seconds) and the visible
/// body (everything after the timestamp column).
pub type Entry = (i64, String);

/// The cache directory, or `None` when caching is disabled.
pub fn dir(config: &Config) -> Option<PathBuf> {
    if !config.history.enabled {
        return None;
    }
    if let Some(dir) = &config.history.dir {
        return Some(dir.clone());
    }
    directories::ProjectDirs::from("", "", "cricr")
        .map(|p| p.cache_dir().join("history"))
        .or_else(|| Some(PathBuf::from(".cricr-history")))
}

/// A filesystem-safe, case-folded file/dir name for a server or target.
pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.to_lowercase().bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn path(dir: &Path, server: &str, target: &str) -> PathBuf {
    dir.join(sanitize(server))
        .join(format!("{}.txt", sanitize(target)))
}

fn rfc3339(time: i64) -> String {
    Local
        .timestamp_opt(time, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

/// Format one cache entry (timestamp, tab, body) onto `out`.
fn push_entry(out: &mut String, time: i64, body: &str) {
    out.push_str(&rfc3339(time));
    out.push('\t');
    for ch in body.chars() {
        // Keep one entry per line; collapse any stray newlines.
        out.push(if ch == '\n' || ch == '\r' { ' ' } else { ch });
    }
    out.push('\n');
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) => fs::create_dir_all(parent),
        None => Ok(()),
    }
}

/// Append `entries` to the cache for one target. The cache is append-only and
/// grows without bound; a fresh file (with a header) is created if none exists.
/// The caller only passes entries that are not already on disk.
pub fn append(dir: &Path, server: &str, target: &str, entries: &[Entry]) -> io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let path = path(dir, server, target);
    ensure_parent(&path)?;
    let mut out = String::new();
    if !path.exists() {
        out.push_str(HEADER);
        out.push('\n');
    }
    for (time, body) in entries {
        push_entry(&mut out, *time, body);
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?
        .write_all(out.as_bytes())
}

/// Read the last `max` cached entries for one target (empty if the file is
/// missing or unreadable). Only the tail of the file is read, so a cache that
/// has grown very large still loads quickly.
pub fn load(dir: &Path, server: &str, target: &str, max: usize) -> Vec<Entry> {
    let path = path(dir, server, target);
    let Ok(mut file) = fs::File::open(&path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return Vec::new();
    };

    // Read backwards in chunks until we have at least `max + 1` lines (the
    // extra line lets us drop a partial first line when we stop mid-file).
    const CHUNK: u64 = 64 * 1024;
    let wanted = max.saturating_add(1);
    let mut tail: Vec<u8> = Vec::new();
    let mut pos = len;
    let mut lines = 0usize;
    while pos > 0 && lines <= wanted {
        let size = CHUNK.min(pos);
        pos -= size;
        if file.seek(SeekFrom::Start(pos)).is_err() {
            return Vec::new();
        }
        let mut chunk = vec![0u8; size as usize];
        if file.read_exact(&mut chunk).is_err() {
            return Vec::new();
        }
        lines += chunk.iter().filter(|&&b| b == b'\n').count();
        chunk.extend_from_slice(&tail);
        tail = chunk;
    }
    // When we stopped before the start of the file, the first line is a
    // fragment; drop it.
    if pos > 0 {
        match tail.iter().position(|&b| b == b'\n') {
            Some(nl) => {
                tail.drain(0..=nl);
            }
            None => tail.clear(),
        }
    }

    let text = String::from_utf8_lossy(&tail);
    let mut entries = parse(&text);
    if entries.len() > max {
        entries.drain(0..entries.len() - max);
    }
    entries
}

fn parse(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line == HEADER {
            continue;
        }
        let Some((ts, body)) = line.split_once('\t') else {
            continue;
        };
        let Ok(dt) = DateTime::parse_from_rfc3339(ts) else {
            continue;
        };
        out.push((dt.timestamp(), body.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_encodes_unsafe_bytes() {
        assert_eq!(sanitize("#emacs"), "%23emacs");
        assert_eq!(sanitize("Bob"), "bob");
        assert_eq!(sanitize("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-roundtrip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let entries = vec![
            (1_700_000_000, "<nick> hello".to_string()),
            (1_700_000_003, "*** nick has joined #chan".to_string()),
        ];
        append(&dir, "irc.example.org", "#chan", &entries).unwrap();
        let back = load(&dir, "irc.example.org", "#chan", 10);
        assert_eq!(back, entries);
        // Missing target loads empty.
        assert!(load(&dir, "irc.example.org", "#other", 10).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_returns_only_the_tail() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-tail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let entries: Vec<Entry> = (0..100)
            .map(|i| (1_700_000_000 + i, format!("<nick> line {i}")))
            .collect();
        append(&dir, "s", "#c", &entries).unwrap();
        let tail = load(&dir, "s", "#c", 3);
        assert_eq!(tail, entries[97..].to_vec());
        // Asking for more than exists returns everything.
        assert_eq!(load(&dir, "s", "#c", 1000).len(), 100);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_tail_spans_multiple_chunks() {
        // Enough data to exceed one read chunk, exercising the partial-first-line
        // drop when reading backwards from the end.
        let dir = std::env::temp_dir().join(format!("cricr-hist-chunks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let entries: Vec<Entry> = (0..4000)
            .map(|i| (1_700_000_000 + i, format!("<nick> line number {i}")))
            .collect();
        append(&dir, "s", "#c", &entries).unwrap();
        let tail = load(&dir, "s", "#c", 5);
        assert_eq!(tail, entries[3995..].to_vec());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_adds_lines_without_duplicating() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-app-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let e0 = (1_700_000_000, "<nick> hello".to_string());
        let e1 = (1_700_000_003, "<nick> again".to_string());
        append(&dir, "irc.example.org", "#chan", std::slice::from_ref(&e0)).unwrap();
        append(&dir, "irc.example.org", "#chan", std::slice::from_ref(&e1)).unwrap();
        assert_eq!(load(&dir, "irc.example.org", "#chan", 10), vec![e0, e1]);
        // Appending nothing leaves the file untouched.
        append(&dir, "irc.example.org", "#chan", &[]).unwrap();
        assert_eq!(load(&dir, "irc.example.org", "#chan", 10).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_creates_missing_file_with_header() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-app2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let e = (1_700_000_000, "<nick> hi".to_string());
        append(&dir, "s", "#c", std::slice::from_ref(&e)).unwrap();
        let file = path(&dir, "s", "#c");
        assert!(fs::read_to_string(&file).unwrap().starts_with(HEADER));
        assert_eq!(load(&dir, "s", "#c", 10), vec![e]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_skips_bad_lines() {
        let text = format!(
            "{HEADER}\n2026-10-13T16:06:44-07:00\t<nick> hi\nnot a line\n\t no time\n2026-10-13T16:06:45-07:00\t*** bye\n"
        );
        let got = parse(&text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, "<nick> hi");
        assert_eq!(got[1].1, "*** bye");
    }
}
