//! IRC message parsing and serialization.
//!
//! Implements the lenient parsing behavior of rcirc's `rcirc-process-regexp`
//! (see `contrib/rcirc.el`): message tags, an optional prefix, a command, and
//! up to 14 middle parameters followed by an optional trailing parameter.
//! Spaces between parameters may be repeated, and the leading `:` of the
//! trailing parameter may be omitted even if fewer than 15 parameters are
//! present.

use std::fmt;

/// A single IRCv3 message tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub key: String,
    pub value: Option<String>,
}

impl Tag {
    pub fn new(key: impl Into<String>, value: impl Into<Option<String>>) -> Self {
        Tag {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// A parsed IRC protocol message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    pub tags: Vec<Tag>,
    pub prefix: Option<String>,
    pub command: String,
    pub params: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Empty,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => f.write_str("empty IRC message"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Abstraction over a command that is either a numeric reply or a named command.
impl Message {
    /// Parse a single IRC protocol line. `\r`/`\n` terminators are ignored.
    pub fn parse(line: &str) -> Result<Message, ParseError> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() {
            return Err(ParseError::Empty);
        }
        let mut rest = line;
        let mut tags = Vec::new();
        let mut prefix = None;

        if let Some(after) = rest.strip_prefix('@') {
            let (tag_str, remainder) = split_token(after);
            tags = parse_tags(tag_str);
            rest = remainder.trim_start_matches(' ');
        }

        if let Some(after) = rest.strip_prefix(':') {
            let (prefix_str, remainder) = split_token(after);
            prefix = Some(prefix_str.to_string());
            rest = remainder.trim_start_matches(' ');
        }

        let (command, mut rest) = split_token(rest);
        if command.is_empty() {
            return Err(ParseError::Empty);
        }

        let mut params = Vec::new();
        rest = rest.trim_start_matches(' ');
        loop {
            if rest.is_empty() {
                break;
            }
            if let Some(trailing) = rest.strip_prefix(':') {
                params.push(trailing.to_string());
                break;
            }
            let (param, remainder) = split_token(rest);
            params.push(param.to_string());
            rest = remainder.trim_start_matches(' ');
        }

        Ok(Message {
            tags,
            prefix,
            command: command.to_string(),
            params,
        })
    }

    /// Serialize this message to a protocol line (without the trailing CRLF).
    pub fn to_line(&self) -> String {
        let mut out = String::new();
        if !self.tags.is_empty() {
            out.push('@');
            for (i, tag) in self.tags.iter().enumerate() {
                if i > 0 {
                    out.push(';');
                }
                out.push_str(&tag.key);
                if let Some(value) = &tag.value {
                    out.push('=');
                    out.push_str(&escape_tag_value(value));
                }
            }
            out.push(' ');
        }
        if let Some(prefix) = &self.prefix {
            out.push(':');
            out.push_str(prefix);
            out.push(' ');
        }
        out.push_str(&self.command);
        for (i, param) in self.params.iter().enumerate() {
            let last = i + 1 == self.params.len();
            let needs_colon =
                last && (param.is_empty() || param.contains(' ') || param.starts_with(':'));
            if needs_colon {
                out.push_str(" :");
                out.push_str(param);
            } else {
                out.push(' ');
                out.push_str(param);
            }
        }
        out
    }

    /// Look up a message tag value by key.
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|t| t.key == key)
            .and_then(|t| t.value.as_deref())
    }

    /// The nick portion of the prefix (`nick!user@host` -> `nick`).
    pub fn prefix_nick(&self) -> Option<&str> {
        self.prefix.as_deref().map(prefix_nick)
    }
}

/// Extract the nick from a `nick!user@host` style prefix.
pub fn prefix_nick(prefix: &str) -> &str {
    prefix.split(['!', '@']).next().unwrap_or(prefix)
}

/// Return the numeric reply code if the command is one (e.g. `"001"`).
pub fn numeric_code(command: &str) -> Option<&str> {
    if command.len() == 3 && command.chars().all(|c| c.is_ascii_digit()) {
        Some(command)
    } else {
        None
    }
}

fn split_token(s: &str) -> (&str, &str) {
    match s.find(' ') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

fn parse_tags(tag_str: &str) -> Vec<Tag> {
    tag_str
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| match s.find('=') {
            Some(i) => Tag {
                key: s[..i].to_string(),
                value: Some(unescape_tag_value(&s[i + 1..])),
            },
            None => Tag {
                key: s.to_string(),
                value: None,
            },
        })
        .collect()
}

fn unescape_tag_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn escape_tag_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ';' => out.push_str("\\:"),
            ' ' => out.push_str("\\s"),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_privmsg() {
        let m = Message::parse(":nick!user@host PRIVMSG #chan :hello world").unwrap();
        assert_eq!(m.prefix.as_deref(), Some("nick!user@host"));
        assert_eq!(m.command, "PRIVMSG");
        assert_eq!(m.params, vec!["#chan", "hello world"]);
        assert_eq!(m.prefix_nick(), Some("nick"));
    }

    #[test]
    fn parses_no_prefix() {
        let m = Message::parse("PING :tok").unwrap();
        assert!(m.prefix.is_none());
        assert_eq!(m.command, "PING");
        assert_eq!(m.params, vec!["tok"]);
    }

    #[test]
    fn parses_tags() {
        let m = Message::parse(
            "@time=2023-01-01T00:00:00.000Z;msgid=abc;account=foo\\sbar :n!u@h PRIVMSG #c :hi",
        )
        .unwrap();
        assert_eq!(m.tag("time"), Some("2023-01-01T00:00:00.000Z"));
        assert_eq!(m.tag("msgid"), Some("abc"));
        assert_eq!(m.tag("account"), Some("foo bar"));
        assert_eq!(m.params, vec!["#c", "hi"]);
    }

    #[test]
    fn parses_tag_without_value() {
        let m = Message::parse("@+draft/flag :s PRIVMSG #c :x").unwrap();
        assert_eq!(m.tags[0].key, "+draft/flag");
        assert_eq!(m.tags[0].value, None);
    }

    #[test]
    fn tag_escapes_roundtrip() {
        let m = Message::parse("@k=a\\:b\\s\\r\\n\\\\c CMD").unwrap();
        assert_eq!(m.tag("k"), Some("a;b \r\n\\c"));
        let line = m.to_line();
        let again = Message::parse(&line).unwrap();
        assert_eq!(again.tag("k"), Some("a;b \r\n\\c"));
    }

    #[test]
    fn trailing_param_needs_colon() {
        let m = Message {
            tags: vec![],
            prefix: None,
            command: "PRIVMSG".into(),
            params: vec!["#chan".into(), "hello world".into()],
        };
        assert_eq!(m.to_line(), "PRIVMSG #chan :hello world");
    }

    #[test]
    fn empty_trailing_param_gets_colon() {
        let m = Message {
            tags: vec![],
            prefix: None,
            command: "TOPIC".into(),
            params: vec!["#chan".into(), "".into()],
        };
        assert_eq!(m.to_line(), "TOPIC #chan :");
    }

    #[test]
    fn command_only() {
        let m = Message::parse("AWAY").unwrap();
        assert_eq!(m.command, "AWAY");
        assert!(m.params.is_empty());
    }

    #[test]
    fn multiple_spaces_between_params() {
        let m = Message::parse("CMD  a   b :c d").unwrap();
        assert_eq!(m.params, vec!["a", "b", "c d"]);
    }

    #[test]
    fn numeric_code_detection() {
        assert_eq!(numeric_code("001"), Some("001"));
        assert_eq!(numeric_code("PRIVMSG"), None);
        assert_eq!(numeric_code("01a"), None);
    }
}
