//! Application state and the event loop.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::event::{Event as TermEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use cricr_irc::commands::{self, LocalCommand, ParsedInput};
use cricr_irc::message::Tag;
use cricr_irc::{ClientCommand, ClientHandle, Event};

use crate::buffer::{Buffer, Color, Line, Pos, Span, Style};
use crate::config::Config;
use crate::input::History;
use crate::keymap::{self, Action};
use crate::search::{Search, SearchDirection};

/// What kind of conversation a buffer represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Server,
    Channel,
    Query,
}

/// One connection's shared state.
pub struct ServerState {
    pub name: String,
    pub handle: ClientHandle,
    pub our_nick: String,
    pub connected: bool,
    pub buffer: usize,
}

/// One buffer (target).
pub struct Chan {
    pub server: usize,
    pub target: String,
    pub kind: Kind,
    pub buffer: Buffer,
    pub nicks: Vec<String>,
    pub unread: usize,
    pub topic: Option<String>,
}

const COMMANDS: &[&str] = &[
    "/msg",
    "/query",
    "/me",
    "/join",
    "/part",
    "/nick",
    "/quit",
    "/topic",
    "/mode",
    "/kick",
    "/invite",
    "/whois",
    "/who",
    "/names",
    "/away",
    "/quote",
    "/close",
    "/clear",
    "/disconnect",
    "/buffer",
];

/// Top-level application.
pub struct App {
    pub config: Config,
    pub servers: Vec<ServerState>,
    pub buffers: Vec<Chan>,
    pub current: usize,
    pub history: History,
    pub search: Search,
    pub message: Option<String>,
    pub kill_ring: String,
    pub sidebar: bool,
    pub scrollback: usize,
    pub should_quit: bool,
    ctrl_x: bool,
    quoted: bool,
    event_tx: mpsc::UnboundedSender<(usize, Event)>,
}

impl App {
    pub fn new(config: Config, event_tx: mpsc::UnboundedSender<(usize, Event)>) -> Self {
        let sidebar = config.ui.sidebar;
        let scrollback = config.ui.scrollback_lines;
        let mut app = App {
            config,
            servers: Vec::new(),
            buffers: Vec::new(),
            current: 0,
            history: History::default(),
            search: Search::default(),
            message: None,
            kill_ring: String::new(),
            sidebar,
            scrollback,
            should_quit: false,
            ctrl_x: false,
            quoted: false,
            event_tx,
        };
        app.connect_all();
        if app.buffers.is_empty() {
            app.buffers.push(Chan {
                server: 0,
                target: "*cricr*".to_string(),
                kind: Kind::Server,
                buffer: Buffer::new("> "),
                nicks: Vec::new(),
                unread: 0,
                topic: None,
            });
        }
        app
    }

    fn connect_all(&mut self) {
        let servers = self.config.servers.clone();
        for server in &servers {
            match self.config.client_config(server) {
                Ok(client_config) => {
                    let (handle, rx) = cricr_irc::spawn(client_config);
                    let index = self.servers.len();
                    let buf = self.new_buffer(index, server.name.clone(), Kind::Server);
                    self.servers.push(ServerState {
                        name: server.name.clone(),
                        handle,
                        our_nick: server
                            .nick
                            .clone()
                            .unwrap_or_else(|| self.config.defaults.nick.clone()),
                        connected: false,
                        buffer: buf,
                    });
                    let name = server.name.clone();
                    self.buffers[buf]
                        .buffer
                        .push_plain(format!("*** Connecting to {name}..."));
                    // Forward this connection's events into the shared channel.
                    let forward = self.event_tx.clone();
                    tokio::spawn(async move {
                        let mut rx = rx;
                        while let Some(event) = rx.recv().await {
                            let _ = forward.send((index, event));
                        }
                    });
                }
                Err(err) => {
                    let idx = self.new_buffer(0, "*cricr*".to_string(), Kind::Server);
                    self.buffers[idx]
                        .buffer
                        .push_plain(format!("*** Failed to configure {}: {err:#}", server.name));
                }
            }
        }
    }

    fn new_buffer(&mut self, server: usize, target: String, kind: Kind) -> usize {
        self.buffers.push(Chan {
            server,
            target,
            kind,
            buffer: Buffer::new("> "),
            nicks: Vec::new(),
            unread: 0,
            topic: None,
        });
        self.buffers.len() - 1
    }

    // ---- accessors ---------------------------------------------------------

    pub fn current(&self) -> &Chan {
        &self.buffers[self.current]
    }

    pub fn current_mut(&mut self) -> &mut Chan {
        let i = self.current;
        &mut self.buffers[i]
    }

    fn our_nick(&self, server: usize) -> String {
        self.servers[server].our_nick.clone()
    }

    fn find_buffer(&self, server: usize, target: &str, kind: Kind) -> Option<usize> {
        self.buffers
            .iter()
            .position(|b| b.server == server && b.target == target && b.kind == kind)
    }

    fn ensure_buffer(&mut self, server: usize, target: String, kind: Kind) -> usize {
        if let Some(i) = self.find_buffer(server, &target, kind) {
            return i;
        }
        self.new_buffer(server, target, kind)
    }

    fn push_line(&mut self, buf: usize, line: Line) {
        self.buffers[buf].buffer.push_line(line);
        self.trim_scrollback(buf);
    }

    fn trim_scrollback(&mut self, buf: usize) {
        let limit = self.scrollback.max(1);
        let b = &mut self.buffers[buf].buffer;
        if b.len() > limit {
            let excess = b.len() - limit;
            b.truncate_front(excess);
        }
    }

    fn bump_activity(&mut self, buf: usize) {
        let current = self.current;
        if buf != current {
            self.buffers[buf].unread += 1;
        }
    }

    fn switch_to(&mut self, idx: usize) {
        if idx < self.buffers.len() {
            self.current = idx;
            self.buffers[idx].unread = 0;
            self.buffers[idx].buffer.end_of_buffer();
        }
    }

    fn next_buffer(&mut self) {
        if self.buffers.is_empty() {
            return;
        }
        let n = self.buffers.len();
        self.switch_to((self.current + 1) % n);
    }

    fn next_unread(&mut self) {
        let n = self.buffers.len();
        for step in 1..=n {
            let i = (self.current + step) % n;
            if self.buffers[i].unread > 0 {
                self.switch_to(i);
                return;
            }
        }
        self.next_buffer();
    }

    // ---- input -------------------------------------------------------------

    fn submit_input(&mut self) {
        let input = self.current().buffer.input();
        if input.trim().is_empty() {
            return;
        }
        let (server, target, kind) = {
            let c = self.current();
            (c.server, c.target.clone(), c.kind)
        };
        let ctx_target = (kind != Kind::Server).then(|| target.clone());

        self.current_mut().buffer.set_input("");
        self.history.push(&input);

        match commands::parse(&input, ctx_target.as_deref()) {
            ParsedInput::Nothing => {}
            ParsedInput::Message { text } => {
                if kind == Kind::Server {
                    self.send(server, ClientCommand::Raw(text.clone()));
                    self.echo_plain(server, target, format!(">>> {text}"));
                } else {
                    if let Some(action) = action_body(&text) {
                        let nick = self.our_nick(server);
                        let line = self.action_line(server, &[], &nick, action);
                        self.push_current(line);
                    } else {
                        let nick = self.our_nick(server);
                        let line = self.chat_line(server, &[], &nick, &text);
                        self.push_current(line);
                    }
                    self.send(
                        server,
                        ClientCommand::Privmsg {
                            target: target.clone(),
                            text,
                        },
                    );
                }
            }
            ParsedInput::Send(cmd) => {
                if let ClientCommand::Privmsg { target: t, text } = &cmd {
                    if *t == target {
                        let nick = self.our_nick(server);
                        let line = if let Some(action) = action_body(text) {
                            self.action_line(server, &[], &nick, action)
                        } else {
                            self.chat_line(server, &[], &nick, text)
                        };
                        self.push_current(line);
                    }
                }
                let quitting = matches!(cmd, ClientCommand::Quit { .. });
                self.send(server, cmd);
                if quitting {
                    self.should_quit = true;
                }
            }
            ParsedInput::Local(local) => self.handle_local(server, local),
            ParsedInput::Unknown { command } => {
                self.message = Some(format!("Unknown command: /{command}"));
            }
        }
    }

    fn handle_local(&mut self, server: usize, local: LocalCommand) {
        match local {
            LocalCommand::Query(nick) => {
                let idx = self.ensure_buffer(server, nick, Kind::Query);
                self.switch_to(idx);
            }
            LocalCommand::Clear => {
                self.current_mut().buffer.clear();
            }
            LocalCommand::Disconnect => {
                self.servers[server].handle.send(ClientCommand::Disconnect);
            }
            LocalCommand::Close => {
                self.close_current();
            }
            LocalCommand::Switch(name) => {
                if let Some(i) = self.buffers.iter().position(|b| b.target == name) {
                    self.switch_to(i);
                }
            }
        }
    }

    fn close_current(&mut self) {
        if self.buffers.len() <= 1 || self.buffers[self.current].kind == Kind::Server {
            self.message = Some("Cannot close this buffer".to_string());
            return;
        }
        self.buffers.remove(self.current);
        if self.current >= self.buffers.len() {
            self.current = self.buffers.len() - 1;
        }
    }

    fn send(&mut self, server: usize, command: ClientCommand) {
        if let Some(state) = self.servers.get(server) {
            state.handle.send(command);
        }
    }

    fn push_current(&mut self, line: Line) {
        let i = self.current;
        self.push_line(i, line);
    }

    fn echo_plain(&mut self, server: usize, target: String, text: String) {
        let idx = self.ensure_buffer(server, target, Kind::Server);
        let line = self.system_line(&text);
        self.push_line(idx, line);
    }

    // ---- key handling ------------------------------------------------------

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.search.active {
            self.handle_search_key(key);
            return;
        }
        if self.quoted {
            self.quoted = false;
            self.message = None;
            if let KeyCode::Char(c) = key.code {
                self.current_mut().buffer.insert_char(c);
            } else if key.code == KeyCode::Tab {
                self.current_mut().buffer.insert_char('\t');
            } else if key.code == KeyCode::Enter {
                self.current_mut().buffer.insert_char('\n');
            }
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.ctrl_x {
            self.ctrl_x = false;
            self.message = None;
            if ctrl && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return;
            }
        } else if ctrl && key.code == KeyCode::Char('x') {
            self.ctrl_x = true;
            self.message = Some("C-x-".to_string());
            return;
        }
        self.message = None;
        let action = keymap::translate(key);
        self.perform(action);
    }

    fn perform(&mut self, action: Action) {
        use Action::*;
        match action {
            None => {}
            SelfInsert(c) => self.current_mut().buffer.insert_char(c),
            Send => self.submit_input(),
            ForwardChar => self.current_mut().buffer.forward_char(),
            BackwardChar => self.current_mut().buffer.backward_char(),
            NextLine => self.current_mut().buffer.next_line(),
            PrevLine => self.current_mut().buffer.prev_line(),
            BeginningOfLine => self.current_mut().buffer.beginning_of_line(),
            EndOfLine => self.current_mut().buffer.end_of_line(),
            ForwardWord => self.current_mut().buffer.forward_word(),
            BackwardWord => self.current_mut().buffer.backward_word(),
            BeginningOfBuffer => self.current_mut().buffer.beginning_of_buffer(),
            EndOfBuffer => self.current_mut().buffer.end_of_buffer(),
            PageUp => self.page_up(),
            PageDown => self.page_down(),
            DeleteBackward => self.current_mut().buffer.delete_backward(),
            DeleteForward => self.current_mut().buffer.delete_forward(),
            KillLine => {
                let killed = self.current_mut().buffer.kill_line();
                self.kill_ring = killed;
            }
            SetMark => self.current_mut().buffer.set_mark(),
            KillRegion => self.kill_region(),
            CopyRegion => {
                if let Some(text) = self.current().buffer.region_text() {
                    self.kill_ring = text;
                }
            }
            Yank => {
                let text = self.kill_ring.clone();
                self.current_mut().buffer.insert_str(&text);
            }
            HistoryPrev => self.history_prev(),
            HistoryNext => self.history_next(),
            Complete => self.complete(),
            SearchForward => self.start_search(SearchDirection::Forward),
            SearchBackward => self.start_search(SearchDirection::Backward),
            SwitchBuffer => self.next_unread(),
            QuotedInsert => {
                self.quoted = true;
                self.message = Some("C-q-".to_string());
            }
            Abort => {
                self.current_mut().buffer.clear_mark();
                self.message = Option::None;
            }
        }
    }

    fn page_up(&mut self) {
        self.current_mut().buffer.scroll_up(10);
    }

    fn page_down(&mut self) {
        self.current_mut().buffer.scroll_down(10);
    }

    fn kill_region(&mut self) {
        let buf = self.current_mut();
        if let Some(text) = buf.buffer.region_text() {
            self.kill_ring = text;
        }
    }

    fn history_prev(&mut self) {
        let current = self.current().buffer.input();
        if let Some(text) = self.history.prev(&current) {
            self.current_mut().buffer.set_input(&text);
        }
    }

    fn history_next(&mut self) {
        if let Some(text) = self.history.next() {
            self.current_mut().buffer.set_input(&text);
        }
    }

    // ---- search ------------------------------------------------------------

    fn start_search(&mut self, direction: SearchDirection) {
        let origin = self.current().buffer.point();
        self.search.start(direction, origin);
    }

    /// Run the current query. When `advance` is set, begin just past the
    /// current match (used for repeated `C-s` / `C-r`).
    fn search_run(&mut self, advance: bool) {
        let query = self.search.query.clone();
        if query.is_empty() {
            return;
        }
        let direction = self.search.direction;
        let point = self.current().buffer.point();
        let from = if advance {
            step_point(point, direction)
        } else if let Some(origin) = self.search.origin {
            if point == origin && self.search.failed {
                origin
            } else {
                point
            }
        } else {
            point
        };
        let forward = direction == SearchDirection::Forward;
        match self.current().buffer.search(&query, from, forward) {
            Some((pos, _)) => {
                self.current_mut().buffer.set_point(pos);
                self.search.failed = false;
            }
            None => self.search.failed = true,
        }
    }

    fn handle_search_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('g') if ctrl => self.cancel_search(),
            KeyCode::Char('s') if ctrl => {
                self.search.direction = SearchDirection::Forward;
                self.search_run(true);
            }
            KeyCode::Char('r') if ctrl => {
                self.search.direction = SearchDirection::Backward;
                self.search_run(true);
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.search.query.push(c);
                self.search_run(false);
            }
            KeyCode::Backspace => {
                self.search.query.pop();
                self.search_run(false);
            }
            KeyCode::Enter => self.search.accept(),
            KeyCode::Esc => self.cancel_search(),
            _ => {}
        }
    }

    fn cancel_search(&mut self) {
        if let Some(origin) = self.search.cancel() {
            self.current_mut().buffer.set_point(origin);
        }
    }

    // ---- completion --------------------------------------------------------

    fn complete(&mut self) {
        let buf = self.current();
        let input = buf.buffer.input();
        let prompt_len = buf.buffer.prompt_len();
        let cursor = buf.buffer.point().col.saturating_sub(prompt_len);
        let chars: Vec<char> = input.chars().collect();
        let cursor = cursor.min(chars.len());
        let mut start = cursor;
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let token: String = chars[start..cursor].iter().collect();
        let is_command = start == 0 && token.starts_with('/');

        let candidates: Vec<String> = if is_command {
            COMMANDS
                .iter()
                .filter(|c| c.starts_with(&token))
                .map(|s| s.to_string())
                .collect()
        } else {
            let lower = token.to_lowercase();
            buf.nicks
                .iter()
                .map(|n| strip_prefixes(n).to_string())
                .filter(|n| n.to_lowercase().starts_with(&lower))
                .collect()
        };
        if candidates.is_empty() {
            return;
        }
        let completion = if candidates.len() == 1 {
            let c = candidates[0].clone();
            if is_command {
                c
            } else {
                format!("{c}: ")
            }
        } else {
            common_prefix(&candidates)
        };

        let mut new: Vec<char> = chars[..start].to_vec();
        let added = completion.chars().count();
        new.extend(completion.chars());
        new.extend_from_slice(&chars[cursor..]);
        let text: String = new.into_iter().collect();
        let buf = self.current_mut();
        buf.buffer.set_input(&text);
        let pos = Pos::new(buf.buffer.point().row, prompt_len + start + added);
        buf.buffer.set_point(pos);
    }

    // ---- formatting --------------------------------------------------------

    fn nick_style(&self, server: usize, nick: &str) -> Style {
        if nick == self.our_nick(server) {
            Style::fg(Color::LightCyan).bold()
        } else {
            Style::fg(nick_color(nick)).bold()
        }
    }

    fn chat_line(&self, server: usize, tags: &[Tag], nick: &str, text: &str) -> Line {
        Line::from_spans(vec![
            Span::new(
                format!("[{}] ", timestamp(tags)),
                Style::fg(Color::DarkGray),
            ),
            Span::new(format!("<{nick}>"), self.nick_style(server, nick)),
            Span::new(format!(" {text}"), Style::default()),
        ])
    }

    fn action_line(&self, server: usize, tags: &[Tag], nick: &str, body: &str) -> Line {
        Line::from_spans(vec![
            Span::new(
                format!("[{}] ", timestamp(tags)),
                Style::fg(Color::DarkGray),
            ),
            Span::new("* ", Style::fg(Color::Magenta)),
            Span::new(nick.to_string(), self.nick_style(server, nick)),
            Span::new(format!(" {body}"), Style::default()),
        ])
    }

    fn system_line(&self, text: &str) -> Line {
        Line::from_spans(vec![
            Span::new(format!("[{}] ", now_hhmm()), Style::default().dim()),
            Span::new(text.to_string(), Style::fg(Color::DarkGray)),
        ])
    }

    fn event_line(&self, spans: Vec<Span>) -> Line {
        let mut all = vec![Span::new(
            format!("[{}] ", now_hhmm()),
            Style::default().dim(),
        )];
        all.extend(spans);
        Line::from_spans(all)
    }

    // ---- network events ----------------------------------------------------

    pub fn handle_event(&mut self, server: usize, event: Event) {
        match event {
            Event::Connecting { server: name } => {
                let idx = self.servers[server].buffer;
                let line = self.system_line(&format!("*** Connecting to {name}..."));
                self.push_line(idx, line);
            }
            Event::Connected => {
                let idx = self.servers[server].buffer;
                let line = self.system_line("*** Connected");
                self.push_line(idx, line);
                self.servers[server].connected = true;
            }
            Event::TlsEstablished => {
                let idx = self.servers[server].buffer;
                let line = self.system_line("*** TLS established");
                self.push_line(idx, line);
            }
            Event::Registered { nick } => {
                self.servers[server].our_nick = nick.clone();
                let idx = self.servers[server].buffer;
                let line = self.system_line(&format!("*** Registered as {nick}"));
                self.push_line(idx, line);
            }
            Event::Disconnected { reason } => {
                self.servers[server].connected = false;
                let idx = self.servers[server].buffer;
                let line = self.system_line(&format!("*** Disconnected: {reason}"));
                self.push_line(idx, line);
            }
            Event::ConnectionError { error } => {
                self.servers[server].connected = false;
                let idx = self.servers[server].buffer;
                let line = self.system_line(&format!("*** Error: {error}"));
                self.push_line(idx, line);
            }
            Event::Privmsg {
                from,
                target,
                text,
                tags,
            } => {
                let our = self.our_nick(server);
                let (target_name, kind) = if is_channel(&target) {
                    (target, Kind::Channel)
                } else if target == our {
                    (from.clone(), Kind::Query)
                } else {
                    (target, Kind::Query)
                };
                let idx = self.ensure_buffer(server, target_name, kind);
                let line = self.chat_line(server, &tags, &from, &text);
                self.push_line(idx, line);
                self.bump_activity(idx);
            }
            Event::Notice {
                from, target, text, ..
            } => {
                let our = self.our_nick(server);
                let (target_name, kind) = if is_channel(&target) {
                    (target, Kind::Channel)
                } else if target == our {
                    (from.clone(), Kind::Query)
                } else {
                    (target, Kind::Query)
                };
                let idx = self.ensure_buffer(server, target_name, kind);
                let line = self.event_line(vec![
                    Span::new("-".to_string(), Style::fg(Color::Yellow)),
                    Span::new(format!("{from}- "), Style::fg(Color::Yellow).bold()),
                    Span::new(text, Style::fg(Color::Yellow)),
                ]);
                self.push_line(idx, line);
                self.bump_activity(idx);
            }
            Event::Action {
                from,
                target,
                text,
                tags,
            } => {
                let idx = self.buffer_for_sender(server, &from, &target);
                let line = self.action_line(server, &tags, &from, &text);
                self.push_line(idx, line);
                self.bump_activity(idx);
            }
            Event::Ctcp {
                from,
                target,
                request,
                body,
            } => {
                let idx = self.buffer_for_sender(server, &from, &target);
                let line = self.event_line(vec![Span::new(
                    format!("CTCP {request} from {from}: {body}"),
                    Style::fg(Color::DarkGray),
                )]);
                self.push_line(idx, line);
            }
            Event::Join { channel, nick } => {
                let idx = self.ensure_buffer(server, channel.clone(), Kind::Channel);
                self.add_nick(idx, &nick);
                let style = self.nick_style(server, &nick);
                let line = self.event_line(vec![
                    Span::new("--> ".to_string(), Style::fg(Color::Green)),
                    Span::new(nick.clone(), style),
                    Span::new(format!(" joined {channel}"), Style::fg(Color::DarkGray)),
                ]);
                self.push_line(idx, line);
            }
            Event::Part {
                channel,
                nick,
                reason,
            } => {
                let idx = self.ensure_buffer(server, channel.clone(), Kind::Channel);
                self.remove_nick(idx, &nick);
                let suffix = if reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason})")
                };
                let style = self.nick_style(server, &nick);
                let line = self.event_line(vec![
                    Span::new("<-- ".to_string(), Style::fg(Color::Red)),
                    Span::new(nick.clone(), style),
                    Span::new(
                        format!(" left {channel}{suffix}"),
                        Style::fg(Color::DarkGray),
                    ),
                ]);
                self.push_line(idx, line);
            }
            Event::Kick {
                channel,
                nick,
                by,
                reason,
            } => {
                let idx = self.ensure_buffer(server, channel.clone(), Kind::Channel);
                self.remove_nick(idx, &nick);
                let suffix = if reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason})")
                };
                let line = self.event_line(vec![Span::new(
                    format!("{by} kicked {nick} from {channel}{suffix}"),
                    Style::fg(Color::Red),
                )]);
                self.push_line(idx, line);
            }
            Event::Quit { nick, reason } => {
                let suffix = if reason.is_empty() {
                    String::new()
                } else {
                    format!(" ({reason})")
                };
                for idx in 0..self.buffers.len() {
                    if self.buffers[idx].kind == Kind::Channel
                        && self.buffers[idx].server == server
                        && self.buffers[idx].nicks.iter().any(|n| n == &nick)
                    {
                        self.remove_nick(idx, &nick);
                        let line = self.event_line(vec![Span::new(
                            format!("<-- {nick} quit{suffix}"),
                            Style::fg(Color::Red),
                        )]);
                        self.push_line(idx, line);
                    }
                }
            }
            Event::Nick { old, new } => {
                if old == self.our_nick(server) {
                    self.servers[server].our_nick = new.clone();
                }
                for idx in 0..self.buffers.len() {
                    if self.buffers[idx].server == server {
                        if let Some(pos) = self.buffers[idx].nicks.iter().position(|n| n == &old) {
                            self.buffers[idx].nicks[pos] = new.clone();
                        }
                    }
                }
                let line = self.event_line(vec![Span::new(
                    format!("{old} is now known as {new}"),
                    Style::fg(Color::DarkGray),
                )]);
                let idx = self.servers[server].buffer;
                self.push_line(idx, line);
            }
            Event::TopicChanged { channel, topic, by } => {
                let idx = self.ensure_buffer(server, channel.clone(), Kind::Channel);
                self.buffers[idx].topic = Some(topic.clone());
                let line = self.event_line(vec![Span::new(
                    format!("{by} changed the topic of {channel} to: {topic}"),
                    Style::fg(Color::DarkGray),
                )]);
                self.push_line(idx, line);
            }
            Event::TopicReply { channel, topic, .. } => {
                let idx = self.ensure_buffer(server, channel.clone(), Kind::Channel);
                self.buffers[idx].topic = Some(topic.clone());
                let line = self.event_line(vec![Span::new(
                    format!("Topic for {channel}: {topic}"),
                    Style::fg(Color::DarkGray),
                )]);
                self.push_line(idx, line);
            }
            Event::Mode { target, modes } => {
                let idx = self.ensure_buffer(server, target.clone(), Kind::Channel);
                let line = self.event_line(vec![Span::new(
                    format!("mode {target}: {modes}"),
                    Style::fg(Color::DarkGray),
                )]);
                self.push_line(idx, line);
            }
            Event::Names { channel, nicks } => {
                let idx = self.ensure_buffer(server, channel, Kind::Channel);
                for nick in nicks {
                    self.add_nick(idx, &nick);
                }
            }
            Event::NamesEnd { .. } => {}
            Event::Whois { nick, line } => {
                let idx = self.servers[server].buffer;
                let line = self.event_line(vec![Span::new(
                    format!("{nick}: {line}"),
                    Style::fg(Color::DarkGray),
                )]);
                self.push_line(idx, line);
            }
            Event::Motd { text } | Event::ServerText { text, .. } => {
                let idx = self.servers[server].buffer;
                if !text.is_empty() {
                    let l = self.server_text_line(&text);
                    self.push_line(idx, l);
                }
            }
            Event::Isupport { tokens } => {
                let idx = self.servers[server].buffer;
                let line = self.event_line(vec![Span::new(
                    format!("*** Supported: {}", tokens.join(" ")),
                    Style::fg(Color::DarkGray),
                )]);
                self.push_line(idx, line);
            }
            Event::Ping { .. } | Event::Pong { .. } => {}
            Event::CapAck { caps } => {
                let idx = self.servers[server].buffer;
                let line = self.event_line(vec![Span::new(
                    format!("*** Capabilities enabled: {}", caps.join(" ")),
                    Style::fg(Color::DarkGray),
                )]);
                self.push_line(idx, line);
            }
            Event::CapNak { caps } => {
                let idx = self.servers[server].buffer;
                let line = self.event_line(vec![Span::new(
                    format!("*** Capabilities rejected: {}", caps.join(" ")),
                    Style::fg(Color::DarkGray),
                )]);
                self.push_line(idx, line);
            }
            Event::CapList { .. } | Event::Authenticate => {}
            Event::LoggedIn => {
                let idx = self.servers[server].buffer;
                let line = self.system_line("*** SASL authentication succeeded");
                self.push_line(idx, line);
            }
            Event::StandardReply { level, text } => {
                let idx = self.servers[server].buffer;
                let line = self.event_line(vec![Span::new(
                    format!("{level}: {text}"),
                    Style::fg(Color::Yellow),
                )]);
                self.push_line(idx, line);
            }
            Event::Sent { line } => {
                let noisy = line.starts_with("PRIVMSG")
                    || line.starts_with("NOTICE")
                    || line.starts_with("PASS")
                    || line.starts_with("AUTHENTICATE")
                    || line.starts_with("PING")
                    || line.starts_with("PONG");
                if !noisy {
                    let idx = self.servers[server].buffer;
                    let l = self.system_line(&format!(">>> {line}"));
                    self.push_line(idx, l);
                }
            }
            Event::Received { .. } | Event::RawMessage { .. } => {}
        }
    }

    fn buffer_for_sender(&mut self, server: usize, from: &str, target: &str) -> usize {
        let our = self.our_nick(server);
        if is_channel(target) {
            self.ensure_buffer(server, target.to_string(), Kind::Channel)
        } else if target == our {
            self.ensure_buffer(server, from.to_string(), Kind::Query)
        } else {
            self.ensure_buffer(server, target.to_string(), Kind::Query)
        }
    }

    fn server_text_line(&self, text: &str) -> Line {
        Line::from_spans(vec![
            Span::new(format!("[{}] ", now_hhmm()), Style::default().dim()),
            Span::new(format!("-!- {text}"), Style::fg(Color::DarkGray)),
        ])
    }

    fn add_nick(&mut self, buf: usize, nick: &str) {
        if !self.buffers[buf].nicks.iter().any(|n| n == nick) {
            self.buffers[buf].nicks.push(nick.to_string());
            self.buffers[buf].nicks.sort();
        }
    }

    fn remove_nick(&mut self, buf: usize, nick: &str) {
        self.buffers[buf].nicks.retain(|n| n != nick);
    }

    // ---- loop --------------------------------------------------------------

    pub async fn run<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        mut net_rx: mpsc::UnboundedReceiver<(usize, Event)>,
        mut term_rx: mpsc::UnboundedReceiver<TermEvent>,
    ) -> Result<()> {
        loop {
            terminal.draw(|frame| crate::ui::draw(frame, self))?;
            if self.should_quit {
                for state in &self.servers {
                    state.handle.quit("cricr");
                }
                break;
            }
            tokio::select! {
                Some((server, event)) = net_rx.recv() => {
                    self.handle_event(server, event);
                }
                Some(term_event) = term_rx.recv() => {
                    if let TermEvent::Key(key) = term_event {
                        self.handle_key(key);
                    }
                }
                else => break,
            }
        }
        Ok(())
    }
}

fn is_channel(target: &str) -> bool {
    matches!(
        target.chars().next(),
        Some('#') | Some('&') | Some('!') | Some('+')
    )
}

fn strip_prefixes(nick: &str) -> &str {
    nick.trim_start_matches(['@', '+', '%', '&', '~'])
}

fn action_body(text: &str) -> Option<&str> {
    let inner = text.strip_prefix('\u{1}')?.strip_suffix('\u{1}')?;
    inner.strip_prefix("ACTION ")
}

fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut end = first.chars().count();
    for item in &items[1..] {
        let limit = first
            .chars()
            .zip(item.chars())
            .take_while(|(a, b)| a == b)
            .count();
        end = end.min(limit);
    }
    first.chars().take(end).collect()
}

fn now_hhmm() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60)
}

fn timestamp(tags: &[Tag]) -> String {
    for tag in tags {
        if tag.key == "time" {
            if let Some(value) = &tag.value {
                if value.len() >= 16 && value.as_bytes().get(10) == Some(&b'T') {
                    return value[11..16].to_string();
                }
            }
        }
    }
    now_hhmm()
}

fn nick_color(nick: &str) -> Color {
    const PALETTE: &[Color] = &[
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::LightRed,
    ];
    let sum: usize = nick.bytes().map(|b| b as usize).sum();
    PALETTE[sum % PALETTE.len()]
}

/// Advance a point by one cell, for the next search starting position.
fn step_point(pos: Pos, direction: SearchDirection) -> Pos {
    match direction {
        SearchDirection::Forward => Pos::new(pos.row, pos.col + 1),
        SearchDirection::Backward => Pos::new(pos.row, pos.col.saturating_sub(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_prefix_works() {
        let items = vec!["hello".to_string(), "help".to_string(), "helm".to_string()];
        assert_eq!(common_prefix(&items), "hel");
    }

    #[test]
    fn channel_detection() {
        assert!(is_channel("#rust"));
        assert!(is_channel("&local"));
        assert!(!is_channel("nick"));
    }

    #[test]
    fn action_parsing() {
        assert_eq!(action_body("\u{1}ACTION waves\u{1}"), Some("waves"));
        assert_eq!(action_body("hello"), None);
    }
}
