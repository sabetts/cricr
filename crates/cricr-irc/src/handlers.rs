//! Translate parsed protocol messages into semantic [`Event`]s.
//!
//! This is the Rust analogue of the `rcirc-handler-*` family in
//! `contrib/rcirc.el`.

use crate::events::Event;
use crate::message::{prefix_nick, Message};

const CTCP: char = '\u{1}';

/// Turn a single protocol message into zero or more UI events.
pub fn handle(message: &Message) -> Vec<Event> {
    let cmd = message.command.as_str();
    let args = &message.params;
    let nick = message.prefix_nick().unwrap_or("").to_string();

    match cmd {
        "PRIVMSG" => vec![privmsg(message, false)],
        "NOTICE" => vec![privmsg(message, true)],
        "JOIN" => {
            let channel = args.first().cloned().unwrap_or_default();
            vec![Event::Join { channel, nick }]
        }
        "PART" => vec![Event::Part {
            channel: args.first().cloned().unwrap_or_default(),
            nick,
            reason: args.get(1).cloned().unwrap_or_default(),
        }],
        "KICK" => vec![Event::Kick {
            channel: args.first().cloned().unwrap_or_default(),
            nick: args.get(1).cloned().unwrap_or_default(),
            by: nick,
            reason: args.get(2).cloned().unwrap_or_default(),
        }],
        "QUIT" => vec![Event::Quit {
            nick,
            reason: args.first().cloned().unwrap_or_default(),
        }],
        "NICK" => vec![Event::Nick {
            old: nick,
            new: args.first().cloned().unwrap_or_default(),
        }],
        "TOPIC" if args.len() >= 2 => vec![Event::TopicChanged {
            channel: args[0].clone(),
            topic: args[1].clone(),
            by: nick,
        }],
        "MODE" => vec![Event::Mode {
            target: args.first().cloned().unwrap_or_default(),
            modes: join(&args[1.min(args.len())..]),
        }],
        "CAP" => vec![cap_event(args)],
        "AUTHENTICATE" => vec![Event::Authenticate],
        "900" | "903" => vec![Event::LoggedIn],
        "FAIL" | "WARN" | "NOTE" => vec![Event::StandardReply {
            level: cmd.to_string(),
            text: join(args),
        }],
        "904" | "905" | "906" | "907" => vec![Event::StandardReply {
            level: "FAIL".to_string(),
            text: format!("SASL authentication failed ({cmd}): {}", join(args)),
        }],
        "PING" => vec![Event::Ping {
            token: args.last().cloned().unwrap_or_default(),
        }],
        "PONG" => vec![Event::Pong {
            token: args.last().cloned().unwrap_or_default(),
        }],
        _ => vec![numeric_or_generic(message)],
    }
}

fn privmsg(message: &Message, notice: bool) -> Event {
    let from = message.prefix_nick().unwrap_or("").to_string();
    let target = message.params.first().cloned().unwrap_or_default();
    let text = message.params.get(1).cloned().unwrap_or_default();

    if let Some(body) = ctcp_body(&text) {
        let (request, rest) = split_first_word(body);
        if request.eq_ignore_ascii_case("ACTION") {
            return Event::Action {
                from,
                target,
                text: rest.to_string(),
                tags: message.tags.clone(),
            };
        }
        return Event::Ctcp {
            from,
            target,
            request: request.to_string(),
            body: rest.to_string(),
        };
    }

    if notice {
        Event::Notice {
            from,
            target,
            text,
            tags: message.tags.clone(),
        }
    } else {
        Event::Privmsg {
            from,
            target,
            text,
            tags: message.tags.clone(),
        }
    }
}

fn numeric_or_generic(message: &Message) -> Event {
    let args = &message.params;
    match message.command.as_str() {
        "005" => Event::Isupport {
            tokens: args[1.min(args.len())..].to_vec(),
        },
        "332" => Event::TopicReply {
            channel: args.get(1).cloned().unwrap_or_default(),
            topic: args.get(2).cloned().unwrap_or_default(),
            setter: String::new(),
            time: String::new(),
        },
        "353" => Event::Names {
            channel: args.get(2).cloned().unwrap_or_default(),
            nicks: args
                .get(3)
                .map(|s| s.split(' ').map(str::to_string).collect())
                .unwrap_or_default(),
        },
        "366" => Event::NamesEnd {
            channel: args.get(1).cloned().unwrap_or_default(),
        },
        "375" | "372" | "376" | "422" => Event::Motd {
            text: join(&args[1.min(args.len())..]),
        },
        "311" | "312" | "313" | "317" | "319" => Event::Whois {
            nick: args.get(1).cloned().unwrap_or_default(),
            line: join(&args[1.min(args.len())..]),
        },
        _ => {
            let code = message.command.as_str();
            let numeric = code.len() == 3 && code.chars().all(|c| c.is_ascii_digit());
            let text = if numeric {
                join(&args[1.min(args.len())..])
            } else {
                join(args)
            };
            // A numeric's second argument is often the channel it concerns.
            let target = args.get(1).filter(|a| is_channel(a)).cloned();
            Event::ServerText {
                command: code.to_string(),
                target,
                text,
            }
        }
    }
}

fn cap_event(args: &[String]) -> Event {
    let sub = args
        .get(1)
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_default();
    let caps = crate::caps::parse_cap_list(&args[2.min(args.len())..]);
    match sub.as_str() {
        "ACK" => Event::CapAck { caps },
        "NAK" => Event::CapNak { caps },
        _ => Event::CapList { caps },
    }
}

/// Strip the CTCP framing (`\x01...\x01`) from a message body.
fn ctcp_body(text: &str) -> Option<&str> {
    let t = text.strip_prefix(CTCP)?;
    Some(t.strip_suffix(CTCP).unwrap_or(t))
}

fn split_first_word(s: &str) -> (&str, &str) {
    match s.find(' ') {
        Some(i) => (&s[..i], s[i + 1..].trim_start()),
        None => (s, ""),
    }
}

fn join(args: &[String]) -> String {
    args.join(" ")
}

fn is_channel(s: &str) -> bool {
    matches!(
        s.chars().next(),
        Some('#') | Some('&') | Some('!') | Some('+')
    )
}

/// Exported for callers that need the nick from a prefix.
pub fn nick_of(message: &Message) -> String {
    message
        .prefix
        .as_deref()
        .map(|p| prefix_nick(p).to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(line: &str) -> Event {
        let m = Message::parse(line).unwrap();
        handle(&m).into_iter().next().unwrap()
    }

    #[test]
    fn privmsg_routes() {
        match ev(":a!u@h PRIVMSG #c :hello") {
            Event::Privmsg {
                from, target, text, ..
            } => {
                assert_eq!(from, "a");
                assert_eq!(target, "#c");
                assert_eq!(text, "hello");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn action_ctcp() {
        match ev(":a!u@h PRIVMSG #c :\u{1}ACTION waves\u{1}") {
            Event::Action { text, .. } => assert_eq!(text, "waves"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn names_reply() {
        match ev(":srv 353 me = #c :@a +b c") {
            Event::Names { channel, nicks } => {
                assert_eq!(channel, "#c");
                assert_eq!(nicks, vec!["@a", "+b", "c"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_is_server_text() {
        match ev(":srv 401 me othernick :No such nick") {
            Event::ServerText { command, text, .. } => {
                assert_eq!(command, "401");
                assert_eq!(text, "othernick No such nick");
            }
            other => panic!("{other:?}"),
        }
    }
}
