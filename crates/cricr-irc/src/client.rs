//! Asynchronous IRC connection: TCP + optional TLS, IRCv3 negotiation, SASL,
//! registration, and command handling.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

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

/// A PEM-encoded client certificate and key, used for SASL EXTERNAL / certfp.
#[derive(Debug, Clone)]
pub struct ClientCert {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
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
    Send(String),
    Emit(Event),
    Close,
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

    for out in state.on_connected() {
        apply(out, &mut writer, evt_tx).await?;
    }

    let mut ping = tokio::time::interval(Duration::from_secs(KEEPALIVE_SECS));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            line = read_line(&mut reader) => {
                match line? {
                    Some(line) => {
                        let _ = evt_tx.send(Event::Received { line: line.clone() });
                        if let Ok(msg) = Message::parse(&line) {
                            for out in state.on_message(&msg) {
                                if let Out::Close = out { break; }
                                apply(out, &mut writer, evt_tx).await?;
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
                            apply(out, &mut writer, evt_tx).await?;
                        }
                        if closing {
                            let _ = evt_tx.send(Event::Disconnected { reason: "Quit".into() });
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }
            _ = ping.tick() => {
                let line = state.keepalive();
                apply(Out::Send(line), &mut writer, evt_tx).await?;
            }
        }
    }
}

async fn apply<W>(out: Out, writer: &mut W, evt_tx: &mpsc::UnboundedSender<Event>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match out {
        Out::Send(line) => {
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\r\n").await?;
            writer.flush().await?;
            let _ = evt_tx.send(Event::Sent { line });
        }
        Out::Emit(event) => {
            let _ = evt_tx.send(event);
        }
        Out::Close => {}
    }
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
}

impl State {
    fn new(config: ClientConfig) -> Self {
        let sasl_done = config.sasl.is_none();
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
                outs.extend(self.maybe_join());
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
        if !self.registered || !self.sasl_done || self.joined || self.config.channels.is_empty() {
            return Vec::new();
        }
        self.joined = true;
        self.config
            .channels
            .iter()
            .map(|c| Out::Send(format!("JOIN {c}")))
            .collect()
    }

    fn keepalive(&self) -> String {
        format!("PING {}", self.config.server)
    }

    fn on_command(&mut self, command: ClientCommand) -> Vec<Out> {
        use ClientCommand::*;
        match command {
            Raw(line) => vec![Out::Send(line)],
            Privmsg { target, text } => vec![Out::Send(format!("PRIVMSG {target} :{text}"))],
            Notice { target, text } => vec![Out::Send(format!("NOTICE {target} :{text}"))],
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
