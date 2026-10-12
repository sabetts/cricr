//! Events emitted by the client toward the UI, and commands sent from the UI.

use crate::message::{Message, Tag};

/// A server-time tag, per the IRCv3 `server-time` extension.
pub const TAG_TIME: &str = "time";
pub const TAG_ACCOUNT: &str = "account";
pub const TAG_MSGID: &str = "msgid";

/// Semantic events produced by the client.
#[derive(Debug, Clone)]
pub enum Event {
    Connecting {
        server: String,
    },
    Connected,
    TlsEstablished,
    Registered {
        nick: String,
    },
    Disconnected {
        reason: String,
    },
    ConnectionError {
        error: String,
    },

    Sent {
        line: String,
    },
    Received {
        line: String,
    },

    Privmsg {
        from: String,
        target: String,
        text: String,
        tags: Vec<Tag>,
    },
    Notice {
        from: String,
        target: String,
        text: String,
        tags: Vec<Tag>,
    },
    Action {
        from: String,
        target: String,
        text: String,
        tags: Vec<Tag>,
    },
    Ctcp {
        from: String,
        target: String,
        request: String,
        body: String,
    },

    Join {
        channel: String,
        nick: String,
    },
    Part {
        channel: String,
        nick: String,
        reason: String,
    },
    Kick {
        channel: String,
        nick: String,
        by: String,
        reason: String,
    },
    Quit {
        nick: String,
        reason: String,
    },
    Nick {
        old: String,
        new: String,
    },

    TopicChanged {
        channel: String,
        topic: String,
        by: String,
    },
    TopicReply {
        channel: String,
        topic: String,
        setter: String,
        time: String,
    },
    Mode {
        target: String,
        modes: String,
    },
    Names {
        channel: String,
        nicks: Vec<String>,
    },
    NamesEnd {
        channel: String,
    },
    Whois {
        nick: String,
        line: String,
    },

    Motd {
        text: String,
    },
    Isupport {
        tokens: Vec<String>,
    },
    ServerText {
        command: String,
        /// Channel the numeric refers to, if any (for routing).
        target: Option<String>,
        text: String,
    },

    Ping {
        token: String,
    },
    Pong {
        token: String,
    },

    CapAck {
        caps: Vec<String>,
    },
    CapNak {
        caps: Vec<String>,
    },
    CapList {
        caps: Vec<String>,
    },
    Authenticate,
    LoggedIn,
    StandardReply {
        level: String,
        text: String,
    },

    RawMessage {
        message: Message,
    },
}

/// Commands issued by the UI toward the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientCommand {
    Raw(String),
    Privmsg {
        target: String,
        text: String,
    },
    Notice {
        target: String,
        text: String,
    },
    Join {
        channel: String,
        keys: Option<String>,
    },
    Part {
        channel: String,
        reason: Option<String>,
    },
    Nick {
        nick: String,
    },
    Quit {
        reason: Option<String>,
    },
    Topic {
        channel: String,
        topic: Option<String>,
    },
    Mode {
        target: String,
        modes: String,
    },
    Kick {
        channel: String,
        nick: String,
        reason: Option<String>,
    },
    Invite {
        nick: String,
        channel: String,
    },
    Who {
        target: String,
    },
    Whois {
        nick: String,
    },
    Names {
        channel: String,
    },
    Away {
        message: Option<String>,
    },
    Disconnect,
}
