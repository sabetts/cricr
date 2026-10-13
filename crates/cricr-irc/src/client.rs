//! Asynchronous IRC connection: TCP + optional TLS, IRCv3 negotiation, SASL,
//! registration, and command handling.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _};
use tokio::io::{
    self, AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{self, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::events::{ClientCommand, Event};
use crate::message::Message;
use crate::sasl::SaslCredentials;
use crate::{caps, handlers, sasl};

const KEEPALIVE_SECS: u64 = 60;
/// How long without *any* server traffic before the connection is treated as
/// dead (we probe with a keepalive PING every `KEEPALIVE_SECS`).
const LIVENESS_TIMEOUT_SECS: u64 = KEEPALIVE_SECS * 2;
/// How long to wait for NickServ to confirm before joining channels anyway.
const NICKSERV_TIMEOUT_SECS: u64 = 5;

/// Whether a NickServ NOTICE text indicates successful identification.
fn nickserv_success(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "now identified",
        "now logged in",
        "password accepted",
        "already identified",
        "already logged in",
        "successfully logged in",
        "successfully identified",
    ];
    MARKERS.iter().any(|m| t.contains(m))
}

/// A PEM-encoded client certificate and key, used for SASL EXTERNAL / certfp.
#[derive(Debug, Clone)]
pub struct ClientCert {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
}

/// NickServ identification sent after registration.
#[derive(Debug, Clone)]
pub struct NickServ {
    /// Account name for `IDENTIFY <account> <password>`; if `None`, only the
    /// password is sent.
    pub account: Option<String>,
    pub password: String,
    /// The service nick to message (default `NickServ`).
    pub service: String,
}

impl NickServ {
    fn identify_command(&self) -> String {
        match &self.account {
            Some(account) if !account.is_empty() => {
                format!(
                    "PRIVMSG {} :IDENTIFY {account} {}",
                    self.service, self.password
                )
            }
            _ => format!("PRIVMSG {} :IDENTIFY {}", self.service, self.password),
        }
    }
}

/// Everything needed to establish one connection.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub server: String,
    pub port: u16,
    pub tls: bool,
    pub nick: String,
    pub user_name: String,
    pub full_name: String,
    pub server_password: Option<String>,
    pub sasl: Option<SaslCredentials>,
    pub nickserv: Option<NickServ>,
    pub client_cert: Option<ClientCert>,
    pub channels: Vec<String>,
}

/// Handle used by the UI to send commands to a running connection.
#[derive(Debug, Clone)]
pub struct ClientHandle {
    tx: mpsc::UnboundedSender<ClientCommand>,
}

impl ClientHandle {
    pub fn send(&self, command: ClientCommand) {
        let _ = self.tx.send(command);
    }

    pub fn raw(&self, line: impl Into<String>) {
        self.send(ClientCommand::Raw(line.into()));
    }

    pub fn quit(&self, reason: impl Into<String>) {
        self.send(ClientCommand::Quit {
            reason: Some(reason.into()),
        });
    }
}

/// Spawn a connection task, returning a command handle and an event stream.
pub fn spawn(config: ClientConfig) -> (ClientHandle, mpsc::UnboundedReceiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(err) = run(config, cmd_rx, &evt_tx).await {
            let _ = evt_tx.send(Event::ConnectionError {
                error: format!("{err:#}"),
            });
        }
    });
    (ClientHandle { tx: cmd_tx }, evt_rx)
}

enum Out {
    /// Send immediately (control traffic: registration, PONG, keepalive).
    Send(String),
    /// Send subject to flood protection (user messages).
    Queue(String),
    Emit(Event),
    Close,
}

/// A simple token bucket limiting how fast user messages are sent.
struct FloodLimiter {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last: std::time::Instant,
}

impl FloodLimiter {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        FloodLimiter {
            tokens: capacity,
            capacity,
            refill_per_sec,
            last: std::time::Instant::now(),
        }
    }

    /// Replenish tokens based on elapsed time.
    fn refill(&mut self) {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
    }

    fn take(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Maximum length of an IRC protocol line (including CRLF).
const MAX_LINE: usize = 510;

/// Split `text` into chunks of at most `max` characters, preferring to break
/// on whitespace. Very long words are hard-split.
fn split_message(text: &str, max: usize) -> Vec<String> {
    if max == 0 || text.chars().count() <= max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let wlen = word.chars().count();
        if wlen > max {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut chars: Vec<char> = word.chars().collect();
            while chars.len() > max {
                out.push(chars.drain(..max).collect());
            }
            cur.extend(chars);
            continue;
        }
        let cur_len = cur.chars().count();
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur_len + 1 + wlen <= max {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Build the queued `PRIVMSG`/`NOTICE` lines for a user message, splitting it
/// to fit the protocol line limit. CTCP messages are not split (framing).
fn message_lines(command: &str, target: &str, text: &str) -> Vec<Out> {
    if text.starts_with('\u{1}') {
        return vec![Out::Queue(format!("{command} {target} :{text}"))];
    }
    let overhead = command.len() + 1 + target.len() + 2 + 2;
    let max = MAX_LINE.saturating_sub(overhead).clamp(16, 400);
    split_message(text, max)
        .into_iter()
        .map(|chunk| Out::Queue(format!("{command} {target} :{chunk}")))
        .collect()
}

enum Connection {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Connection::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Connection::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Connection::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::Plain(s) => Pin::new(s).poll_flush(cx),
            Connection::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Connection::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Connection::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

async fn run(
    config: ClientConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<ClientCommand>,
    evt_tx: &mpsc::UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let _ = evt_tx.send(Event::Connecting {
        server: config.server.clone(),
    });

    let tcp = TcpStream::connect((config.server.as_str(), config.port))
        .await
        .with_context(|| format!("connecting to {}:{}", config.server, config.port))?;

    let conn = if config.tls {
        let connector = tls_connector(&config)?;
        let server_name = ServerName::try_from(config.server.clone())
            .map_err(|_| anyhow!("invalid server name {:?}", config.server))?;
        let stream = connector
            .connect(server_name, tcp)
            .await
            .context("TLS handshake")?;
        let _ = evt_tx.send(Event::TlsEstablished);
        Connection::Tls(Box::new(stream))
    } else {
        Connection::Plain(tcp)
    };
    let _ = evt_tx.send(Event::Connected);

    let (read_half, write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;
    let mut state = State::new(config);
    // Time of the last line received; used to detect half-open connections.
    // Uses `tokio::time` so the timeout is testable under a paused clock.
    let mut last_rx = tokio::time::Instant::now();

    let mut pending: VecDeque<String> = VecDeque::new();
    let mut limiter = FloodLimiter::new(4.0, 0.7);

    for out in state.on_connected() {
        apply(out, &mut writer, evt_tx, &mut pending).await?;
    }

    let mut ping = tokio::time::interval(Duration::from_secs(KEEPALIVE_SECS));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // consume the immediate first tick

    // Drain the flood-protected queue frequently.
    let mut drain = tokio::time::interval(Duration::from_millis(100));
    drain.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    drain.tick().await;

    loop {
        tokio::select! {
            line = read_line(&mut reader) => {
                match line? {
                    Some(line) => {
                        last_rx = tokio::time::Instant::now();
                        let _ = evt_tx.send(Event::Received { line: line.clone() });
                        if let Ok(msg) = Message::parse(&line) {
                            for out in state.on_message(&msg) {
                                if let Out::Close = out { break; }
                                apply(out, &mut writer, evt_tx, &mut pending).await?;
                            }
                        }
                    }
                    None => {
                        let _ = evt_tx.send(Event::Disconnected { reason: "Connection closed".into() });
                        return Ok(());
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        let mut closing = false;
                        for out in state.on_command(cmd) {
                            if let Out::Close = out { closing = true; continue; }
                            apply(out, &mut writer, evt_tx, &mut pending).await?;
                        }
                        if closing {
                            let _ = evt_tx.send(Event::Disconnected { reason: "Quit".into() });
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
            _ = drain.tick() => {
                while !pending.is_empty() && limiter.take() {
                    let line = pending.pop_front().expect("checked non-empty");
                    write_line(&mut writer, &line, evt_tx).await?;
                }
                for out in state.on_tick() {
                    apply(out, &mut writer, evt_tx, &mut pending).await?;
                }
            }
            _ = ping.tick() => {
                // No traffic since well before the last keepalive probe: the
                // connection is half-open (e.g. a silent network drop). Bail so
                // the UI can reconnect.
                if last_rx.elapsed() > Duration::from_secs(LIVENESS_TIMEOUT_SECS) {
                    let _ = evt_tx.send(Event::ConnectionError {
                        error: "Ping timeout".into(),
                    });
                    return Ok(());
                }
                let line = state.keepalive();
                apply(Out::Send(line), &mut writer, evt_tx, &mut pending).await?;
            }
        }
    }
}

async fn apply<W>(
    out: Out,
    writer: &mut W,
    evt_tx: &mpsc::UnboundedSender<Event>,
    pending: &mut VecDeque<String>,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match out {
        Out::Send(line) => write_line(writer, &line, evt_tx).await?,
        Out::Queue(line) => pending.push_back(line),
        Out::Emit(event) => {
            let _ = evt_tx.send(event);
        }
        Out::Close => {}
    }
    Ok(())
}

async fn write_line<W>(
    writer: &mut W,
    line: &str,
    evt_tx: &mpsc::UnboundedSender<Event>,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\r\n").await?;
    writer.flush().await?;
    let _ = evt_tx.send(Event::Sent {
        line: line.to_string(),
    });
    Ok(())
}

async fn read_line<R>(reader: &mut R) -> io::Result<Option<String>>
where
    R: AsyncBufRead + Unpin,
{
    let mut buf = Vec::with_capacity(512);
    let n = reader.read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

fn tls_connector(config: &ClientConfig) -> anyhow::Result<TlsConnector> {
    let provider = rustls::crypto::ring::default_provider();
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .context("initializing TLS protocol versions")?;

    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let with_roots = builder.with_root_certificates(roots);

    let cfg = if let Some(cert) = &config.client_cert {
        let mut cert_reader = &cert.cert_pem[..];
        let certs = rustls_pemfile::certs(&mut cert_reader)
            .collect::<Result<Vec<_>, _>>()
            .context("parsing client certificate")?;
        if certs.is_empty() {
            return Err(anyhow!("client certificate contains no certificates"));
        }
        let mut key_reader = &cert.key_pem[..];
        let key = rustls_pemfile::private_key(&mut key_reader)
            .context("parsing client key")?
            .ok_or_else(|| anyhow!("client key file contains no private key"))?;
        with_roots
            .with_client_auth_cert(certs, key)
            .context("building TLS config with client certificate")?
    } else {
        with_roots.with_no_client_auth()
    };

    Ok(TlsConnector::from(Arc::new(cfg)))
}

struct State {
    config: ClientConfig,
    nick: String,
    registered: bool,
    joined: bool,
    cap_done: bool,
    sasl_done: bool,
    sasl_in_progress: bool,
    sasl_payload_sent: bool,
    nickserv_done: bool,
    nickserv_deadline: Option<Instant>,
}

impl State {
    fn new(config: ClientConfig) -> Self {
        let sasl_done = config.sasl.is_none();
        let nickserv_done = config.nickserv.is_none();
        let nick = config.nick.clone();
        State {
            config,
            nick,
            registered: false,
            joined: false,
            cap_done: false,
            sasl_done,
            sasl_in_progress: false,
            sasl_payload_sent: false,
            nickserv_done,
            nickserv_deadline: None,
        }
    }

    fn on_connected(&mut self) -> Vec<Out> {
        let mut outs = Vec::new();
        if let Some(password) = &self.config.server_password {
            outs.push(Out::Send(format!("PASS {password}")));
        }
        outs.push(Out::Send("CAP LS 302".into()));
        outs.push(Out::Send(format!("NICK {}", self.nick)));
        outs.push(Out::Send(format!(
            "USER {} 0 * :{}",
            self.config.user_name, self.config.full_name
        )));
        outs
    }

    fn on_message(&mut self, msg: &Message) -> Vec<Out> {
        let mut outs: Vec<Out> = handlers::handle(msg).into_iter().map(Out::Emit).collect();
        match msg.command.as_str() {
            "PING" => {
                let token = msg.params.last().cloned().unwrap_or_default();
                outs.push(Out::Send(format!("PONG :{token}")));
            }
            "CAP" => outs.extend(self.on_cap(msg)),
            "AUTHENTICATE" => outs.extend(self.on_authenticate(msg)),
            "001" => {
                self.registered = true;
                let nick = msg
                    .params
                    .first()
                    .cloned()
                    .unwrap_or_else(|| self.nick.clone());
                self.nick = nick.clone();
                outs.push(Out::Emit(Event::Registered { nick }));
                // Identify to NickServ before joining channels.
                if let Some(ns) = self.config.nickserv.clone() {
                    outs.push(Out::Send(ns.identify_command()));
                    self.nickserv_deadline =
                        Some(Instant::now() + Duration::from_secs(NICKSERV_TIMEOUT_SECS));
                }
                outs.extend(self.maybe_join());
            }
            "NOTICE" => {
                let success = match &self.config.nickserv {
                    Some(ns) => {
                        let from = msg.prefix_nick().unwrap_or("");
                        let text = msg.params.get(1).map(String::as_str).unwrap_or("");
                        !self.nickserv_done
                            && from.eq_ignore_ascii_case(&ns.service)
                            && nickserv_success(text)
                    }
                    None => false,
                };
                if success {
                    self.nickserv_done = true;
                    self.nickserv_deadline = None;
                    outs.extend(self.maybe_join());
                }
            }
            "900" | "903" => {
                self.sasl_done = true;
                self.sasl_in_progress = false;
                outs.extend(self.end_cap());
                outs.extend(self.maybe_join());
            }
            "904" | "905" | "906" | "907" => {
                self.sasl_done = true;
                self.sasl_in_progress = false;
                outs.extend(self.end_cap());
                outs.extend(self.maybe_join());
            }
            _ => {}
        }
        outs
    }

    fn on_cap(&mut self, msg: &Message) -> Vec<Out> {
        let sub = msg
            .params
            .get(1)
            .map(|s| s.to_ascii_uppercase())
            .unwrap_or_default();
        match sub.as_str() {
            "LS" => {
                let offered = caps::parse_cap_list(&msg.params[2.min(msg.params.len())..]);
                let requested = caps::select_requested(&offered);
                if requested.is_empty() {
                    return self.end_cap();
                }
                vec![Out::Send(format!("CAP REQ :{}", requested.join(" ")))]
            }
            "ACK" => {
                let acked = caps::parse_cap_list(&msg.params[2.min(msg.params.len())..]);
                let wants_sasl = self.config.sasl.is_some()
                    && acked.iter().any(|c| c == "sasl")
                    && !self.sasl_done
                    && !self.sasl_in_progress;
                if wants_sasl {
                    self.sasl_in_progress = true;
                    let mech = self
                        .config
                        .sasl
                        .as_ref()
                        .map(|c| c.mechanism.as_str())
                        .unwrap_or("PLAIN");
                    return vec![Out::Send(format!("AUTHENTICATE {mech}"))];
                }
                self.end_cap()
            }
            "NAK" => self.end_cap(),
            _ => Vec::new(),
        }
    }

    fn on_authenticate(&mut self, msg: &Message) -> Vec<Out> {
        let param = msg.params.first().cloned().unwrap_or_default();
        if param != "+" || self.sasl_payload_sent {
            return Vec::new();
        }
        let Some(creds) = self.config.sasl.as_ref() else {
            return Vec::new();
        };
        self.sasl_payload_sent = true;
        sasl::authenticate_lines(&creds.initial_response())
            .into_iter()
            .map(Out::Send)
            .collect()
    }

    fn end_cap(&mut self) -> Vec<Out> {
        if self.cap_done {
            return Vec::new();
        }
        self.cap_done = true;
        vec![Out::Send("CAP END".into())]
    }

    fn maybe_join(&mut self) -> Vec<Out> {
        if !self.registered
            || !self.sasl_done
            || !self.nickserv_done
            || self.joined
            || self.config.channels.is_empty()
        {
            return Vec::new();
        }
        self.joined = true;
        self.config
            .channels
            .iter()
            .map(|c| Out::Send(format!("JOIN {c}")))
            .collect()
    }

    /// Periodic check: give up waiting for NickServ and join anyway.
    fn on_tick(&mut self) -> Vec<Out> {
        if !self.nickserv_done {
            if let Some(deadline) = self.nickserv_deadline {
                if Instant::now() >= deadline {
                    self.nickserv_done = true;
                    self.nickserv_deadline = None;
                    return self.maybe_join();
                }
            }
        }
        Vec::new()
    }

    fn keepalive(&self) -> String {
        format!("PING {}", self.config.server)
    }

    fn on_command(&mut self, command: ClientCommand) -> Vec<Out> {
        use ClientCommand::*;
        match command {
            Raw(line) => vec![Out::Send(line)],
            Privmsg { target, text } => message_lines("PRIVMSG", &target, &text),
            Notice { target, text } => message_lines("NOTICE", &target, &text),
            Join { channel, keys } => match keys {
                Some(k) => vec![Out::Send(format!("JOIN {channel} {k}"))],
                None => vec![Out::Send(format!("JOIN {channel}"))],
            },
            Part { channel, reason } => match reason {
                Some(r) => vec![Out::Send(format!("PART {channel} :{r}"))],
                None => vec![Out::Send(format!("PART {channel}"))],
            },
            Nick { nick } => vec![Out::Send(format!("NICK {nick}"))],
            Quit { reason } => vec![
                Out::Send(format!(
                    "QUIT :{}",
                    reason.unwrap_or_else(|| "cricr".into())
                )),
                Out::Close,
            ],
            Topic { channel, topic } => match topic {
                Some(t) => vec![Out::Send(format!("TOPIC {channel} :{t}"))],
                None => vec![Out::Send(format!("TOPIC {channel}"))],
            },
            Mode { target, modes } => vec![Out::Send(format!("MODE {target} {modes}"))],
            Kick {
                channel,
                nick,
                reason,
            } => match reason {
                Some(r) => vec![Out::Send(format!("KICK {channel} {nick} :{r}"))],
                None => vec![Out::Send(format!("KICK {channel} {nick}"))],
            },
            Invite { nick, channel } => vec![Out::Send(format!("INVITE {nick} {channel}"))],
            Who { target } => vec![Out::Send(format!("WHO {target}"))],
            Whois { nick } => vec![Out::Send(format!("WHOIS {nick}"))],
            Names { channel } => vec![Out::Send(format!("NAMES {channel}"))],
            Away { message } => match message {
                Some(m) => vec![Out::Send(format!("AWAY :{m}"))],
                None => vec![Out::Send("AWAY".into())],
            },
            Disconnect => vec![Out::Send("QUIT :cricr".into()), Out::Close],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_message_keeps_short_text() {
        assert_eq!(split_message("hello world", 400), vec!["hello world"]);
    }

    #[test]
    fn split_message_hard_breaks_long_words() {
        let text = "a".repeat(50);
        let chunks = split_message(&text, 20);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 20));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_message_breaks_on_words() {
        let text = "word ".repeat(40);
        let chunks = split_message(text.trim(), 20);
        assert!(chunks.len() > 1);
        assert!(chunks
            .iter()
            .all(|c| !c.is_empty() && c.chars().count() <= 20));
    }

    #[test]
    fn message_lines_splits_and_fits() {
        let outs = message_lines("PRIVMSG", "#channel", &"x".repeat(2000));
        assert!(outs.len() > 1);
        for out in outs {
            match out {
                Out::Queue(line) => assert!(line.len() <= MAX_LINE),
                _ => panic!("expected queued line"),
            }
        }
    }

    #[test]
    fn message_lines_keeps_ctcp_whole() {
        let text = format!("\u{1}ACTION {}\u{1}", "z".repeat(2000));
        let outs = message_lines("PRIVMSG", "#c", &text);
        assert_eq!(outs.len(), 1);
    }

    #[test]
    fn flood_limiter_bursts_then_throttles() {
        let mut limiter = FloodLimiter::new(2.0, 0.7);
        assert!(limiter.take());
        assert!(limiter.take());
        assert!(!limiter.take());
    }
}
