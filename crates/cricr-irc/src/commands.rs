//! Parse user input into either a chat message, a server command, or a
//! client-local action. Mirrors rcirc's `rcirc-define-command` set.

use crate::events::ClientCommand;

/// A client-local action that does not (directly) hit the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalCommand {
    /// Open a query buffer for a nick (no message sent).
    Query(String),
    /// Close the current buffer.
    Close,
    /// Close the current buffer's connection.
    Disconnect,
    /// Clear the current buffer's scrollback.
    Clear,
    /// Switch to another buffer by name.
    Switch(String),
}

/// The result of interpreting a line of user input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedInput {
    /// Nothing to do (empty input).
    Nothing,
    /// Plain chat text; the caller supplies the current target.
    Message { text: String },
    /// A command destined for the server.
    Send(ClientCommand),
    /// A client-local action.
    Local(LocalCommand),
    /// An unrecognized `/command`.
    Unknown { command: String },
}

/// Wrap text in CTCP ACTION framing (`/me`).
pub fn ctcp_action(text: &str) -> String {
    let mut s = String::with_capacity(text.len() + 2);
    s.push('\u{1}');
    s.push_str("ACTION ");
    s.push_str(text);
    s.push('\u{1}');
    s
}

/// Interpret a line of user input.
pub fn parse(input: &str, current_target: Option<&str>) -> ParsedInput {
    let input = input.trim_end();
    if input.trim().is_empty() {
        return ParsedInput::Nothing;
    }
    let Some(rest) = input.strip_prefix('/') else {
        return ParsedInput::Message {
            text: input.to_string(),
        };
    };

    let (cmd, args) = match rest.find(' ') {
        Some(i) => (&rest[..i], rest[i + 1..].trim()),
        None => (rest, ""),
    };
    let (first, remainder) = split_first(args);
    let target = current_target.unwrap_or("");

    match cmd.to_ascii_lowercase().as_str() {
        "msg" if !first.is_empty() => ParsedInput::Send(ClientCommand::Privmsg {
            target: first.to_string(),
            text: remainder.to_string(),
        }),
        "query" => {
            if first.is_empty() {
                ParsedInput::Unknown {
                    command: cmd.to_string(),
                }
            } else {
                ParsedInput::Local(LocalCommand::Query(first.to_string()))
            }
        }
        "me" => ParsedInput::Send(ClientCommand::Privmsg {
            target: target.to_string(),
            text: ctcp_action(args),
        }),
        "notice" if !first.is_empty() => ParsedInput::Send(ClientCommand::Notice {
            target: first.to_string(),
            text: remainder.to_string(),
        }),
        "join" => ParsedInput::Send(ClientCommand::Join {
            channel: if first.is_empty() {
                target.to_string()
            } else {
                first.to_string()
            },
            keys: (!remainder.is_empty()).then(|| remainder.to_string()),
        }),
        "part" => ParsedInput::Send(ClientCommand::Part {
            channel: if first.is_empty() {
                target.to_string()
            } else {
                first.to_string()
            },
            reason: (!remainder.is_empty()).then(|| remainder.to_string()),
        }),
        "nick" if !first.is_empty() => ParsedInput::Send(ClientCommand::Nick {
            nick: first.to_string(),
        }),
        "quit" => ParsedInput::Send(ClientCommand::Quit {
            reason: (!args.is_empty()).then(|| args.to_string()),
        }),
        "topic" => {
            // `/topic #chan text` targets CHAN; otherwise the current channel.
            let (channel, text) = if is_channel(first) && !remainder.is_empty() {
                (first.to_string(), remainder.to_string())
            } else {
                (target.to_string(), args.to_string())
            };
            ParsedInput::Send(ClientCommand::Topic {
                channel,
                topic: (!text.is_empty()).then_some(text),
            })
        }
        "mode" if !first.is_empty() => ParsedInput::Send(ClientCommand::Mode {
            target: first.to_string(),
            modes: remainder.to_string(),
        }),
        "kick" if !first.is_empty() => ParsedInput::Send(ClientCommand::Kick {
            channel: target.to_string(),
            nick: first.to_string(),
            reason: (!remainder.is_empty()).then(|| remainder.to_string()),
        }),
        "invite" if !first.is_empty() => ParsedInput::Send(ClientCommand::Invite {
            nick: first.to_string(),
            channel: if remainder.is_empty() {
                target.to_string()
            } else {
                remainder.to_string()
            },
        }),
        "whois" if !first.is_empty() => ParsedInput::Send(ClientCommand::Whois {
            nick: first.to_string(),
        }),
        "who" => ParsedInput::Send(ClientCommand::Who {
            target: if first.is_empty() {
                target.to_string()
            } else {
                first.to_string()
            },
        }),
        "names" => ParsedInput::Send(ClientCommand::Names {
            channel: if first.is_empty() {
                target.to_string()
            } else {
                first.to_string()
            },
        }),
        "away" => ParsedInput::Send(ClientCommand::Away {
            message: (!args.is_empty()).then(|| args.to_string()),
        }),
        // NickServ / services: send the rest as a message to NickServ.
        "nickserv" | "ns" if !args.is_empty() => ParsedInput::Send(ClientCommand::Privmsg {
            target: "NickServ".to_string(),
            text: args.to_string(),
        }),
        "quote" | "raw" if !args.is_empty() => {
            ParsedInput::Send(ClientCommand::Raw(args.to_string()))
        }
        "close" => ParsedInput::Local(LocalCommand::Close),
        "clear" => ParsedInput::Local(LocalCommand::Clear),
        "disconnect" => ParsedInput::Local(LocalCommand::Disconnect),
        "buffer" if !first.is_empty() => {
            ParsedInput::Local(LocalCommand::Switch(first.to_string()))
        }
        other => ParsedInput::Unknown {
            command: other.to_string(),
        },
    }
}

fn is_channel(s: &str) -> bool {
    matches!(
        s.chars().next(),
        Some('#') | Some('&') | Some('!') | Some('+')
    )
}

fn split_first(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(' ') {
        Some(i) => (&s[..i], s[i + 1..].trim_start()),
        None => (s, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text() {
        assert_eq!(
            parse("hello there", Some("#c")),
            ParsedInput::Message {
                text: "hello there".into()
            }
        );
    }

    #[test]
    fn msg_command() {
        assert_eq!(
            parse("/msg bob hi you", Some("#c")),
            ParsedInput::Send(ClientCommand::Privmsg {
                target: "bob".into(),
                text: "hi you".into()
            })
        );
    }

    #[test]
    fn me_uses_current_target() {
        match parse("/me waves", Some("#c")) {
            ParsedInput::Send(ClientCommand::Privmsg { target, text }) => {
                assert_eq!(target, "#c");
                assert_eq!(text, "\u{1}ACTION waves\u{1}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn join_defaults_to_target() {
        assert_eq!(
            parse("/join", Some("#c")),
            ParsedInput::Send(ClientCommand::Join {
                channel: "#c".into(),
                keys: None
            })
        );
    }

    #[test]
    fn topic_command() {
        assert_eq!(
            parse("/topic hello world", Some("#c")),
            ParsedInput::Send(ClientCommand::Topic {
                channel: "#c".into(),
                topic: Some("hello world".into())
            })
        );
        assert_eq!(
            parse("/topic", Some("#c")),
            ParsedInput::Send(ClientCommand::Topic {
                channel: "#c".into(),
                topic: None
            })
        );
        assert_eq!(
            parse("/topic #other hi there", Some("#c")),
            ParsedInput::Send(ClientCommand::Topic {
                channel: "#other".into(),
                topic: Some("hi there".into())
            })
        );
    }

    #[test]
    fn nickserv_command() {
        assert_eq!(
            parse("/ns identify secret", None),
            ParsedInput::Send(ClientCommand::Privmsg {
                target: "NickServ".into(),
                text: "identify secret".into()
            })
        );
        assert_eq!(
            parse("/nickserv ghost me", None),
            ParsedInput::Send(ClientCommand::Privmsg {
                target: "NickServ".into(),
                text: "ghost me".into()
            })
        );
    }

    #[test]
    fn unknown_command() {
        assert_eq!(
            parse("/frobnicate x", None),
            ParsedInput::Unknown {
                command: "frobnicate".into()
            }
        );
    }
}
