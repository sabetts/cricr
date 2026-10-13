//! Plain-text scrollback cache: one file per channel/query, one logical
//! (unwrapped) line per entry, each prefixed with an absolute RFC3339
//! timestamp. On load the lines are re-rendered (colors) and re-wrapped at the
//! current fill column, so a changed width or theme applies to old history.

use std::fs;
use std::io;
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

/// Write `entries` for one target. Returns the number of lines written.
pub fn save(dir: &Path, server: &str, target: &str, entries: &[Entry]) -> io::Result<()> {
    let path = path(dir, server, target);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::with_capacity(entries.len() * 64 + HEADER.len() + 1);
    out.push_str(HEADER);
    out.push('\n');
    for (time, body) in entries {
        out.push_str(&rfc3339(*time));
        out.push('\t');
        for ch in body.chars() {
            // Keep one entry per line; collapse any stray newlines.
            out.push(if ch == '\n' || ch == '\r' { ' ' } else { ch });
        }
        out.push('\n');
    }
    fs::write(&path, out)
}

/// Read the cached entries for one target (empty if missing or unreadable).
pub fn load(dir: &Path, server: &str, target: &str) -> Vec<Entry> {
    let path = path(dir, server, target);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    parse(&text)
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
        let dir = std::env::temp_dir().join(format!("cricr-hist-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let entries = vec![
            (1_700_000_000, "<nick> hello".to_string()),
            (1_700_000_003, "*** nick has joined #chan".to_string()),
        ];
        save(&dir, "irc.example.org", "#chan", &entries).unwrap();
        let back = load(&dir, "irc.example.org", "#chan");
        assert_eq!(back, entries);
        // Missing target loads empty.
        assert!(load(&dir, "irc.example.org", "#other").is_empty());
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
