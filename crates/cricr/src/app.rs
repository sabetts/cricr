//! Application state and the event loop.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
use crate::killring::KillRing;
use crate::search::{Search, SearchDirection};
use crate::theme::Theme;

/// What kind of conversation a buffer represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Server,
    Channel,
    Query,
    /// A free text buffer (no prompt), like Emacs's `*scratch*`.
    Scratch,
}

/// The minibuffer prompt, if one is active.
#[derive(Debug, Clone)]
pub enum Prompt {
    None,
    SwitchBuffer {
        input: String,
        selected: usize,
    },
    BrowseUrl {
        input: String,
        selected: usize,
    },
    /// Set the channel topic; the current topic is shown as a default
    /// (`C-c C-t`).
    Topic {
        input: String,
        default: String,
    },
    /// Kill (and part from) a buffer chosen from a list (`C-x k`).
    KillBuffer {
        input: String,
        selected: usize,
    },
}

/// A pending multi-key prefix (`C-x`, `C-c`, or `C-c C-f`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Prefix {
    #[default]
    None,
    CtrlX,
    CtrlC,
    CtrlCF,
}

/// The kind of the previous editing command, for kill/yank chaining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LastCommand {
    #[default]
    Other,
    Kill,
    Yank,
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
    /// Whether our nick was mentioned since the last visit.
    pub mentioned: bool,
    /// Row of the first unread line since the last visit.
    pub first_unread: Option<usize>,
    /// Row of the inserted "read up to here" separator line, if present.
    pub marker_row: Option<usize>,
    /// When set, activity in this buffer is not tracked (`C-c C-i`).
    pub ignore_activity: bool,
    /// Monotonic stamp of the most recent activity, for mode-line ordering.
    pub last_activity: u64,
    /// Monotonic stamp of the most recent visit, for buffer-list ordering.
    pub last_visited: u64,
    /// Last timestamp printed, so repeats can be blanked.
    pub last_timestamp: String,
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
    pub kill_ring: KillRing,
    /// Height (in rows) of the buffer view, from the last draw.
    pub viewport_rows: usize,
    pub scrollback: usize,
    pub fill_column: usize,
    pub theme: Theme,
    pub should_quit: bool,
    pub prompt: Prompt,
    previous: usize,
    activity_seq: u64,
    visit_seq: u64,
    prefix: Prefix,
    quoted: bool,
    last_command: LastCommand,
    /// (start, end) positions of the text inserted by the last `yank`.
    last_yank: Option<(Pos, Pos)>,
    /// Pending `M-w` indication: (position to restore, deadline).
    copy_flash: Option<(Pos, Instant)>,
    /// Row of the first line inserted by the most recent `push_line`.
    last_push_row: usize,
    event_tx: mpsc::UnboundedSender<(usize, Event)>,
}

impl App {
    pub fn new(config: Config, event_tx: mpsc::UnboundedSender<(usize, Event)>) -> Self {
        let scrollback = config.ui.scrollback_lines;
        let fill_column = config.ui.fill_column;
        let theme = Theme::for_background(config.ui.background);
        let mut app = App {
            config,
            servers: Vec::new(),
            buffers: Vec::new(),
            current: 0,
            history: History::default(),
            search: Search::default(),
            message: None,
            kill_ring: KillRing::new(),
            viewport_rows: 20,
            scrollback,
            fill_column,
            theme,
            should_quit: false,
            prompt: Prompt::None,
            previous: 0,
            activity_seq: 0,
            visit_seq: 0,
            prefix: Prefix::None,
            quoted: false,
            last_command: LastCommand::Other,
            last_yank: None,
            copy_flash: None,
            last_push_row: 0,
            event_tx,
        };
        app.connect_all();
        // Emacs always has a *scratch* buffer; start there unless a server
        // buffer exists so connection output is visible immediately.
        let scratch = app.new_buffer(0, "*scratch*".to_string(), Kind::Scratch);
        app.current = app.servers.first().map(|s| s.buffer).unwrap_or(scratch);
        app.previous = scratch;
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
        let mut buffer = if kind == Kind::Scratch {
            Buffer::text()
        } else {
            Buffer::new("> ")
        };
        buffer.set_prompt_style(self.theme.prompt);
        self.buffers.push(Chan {
            server,
            target,
            kind,
            buffer,
            nicks: Vec::new(),
            unread: 0,
            mentioned: false,
            first_unread: None,
            marker_row: None,
            ignore_activity: false,
            last_activity: 0,
            last_visited: 0,
            last_timestamp: String::new(),
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
        self.last_push_row = self.buffers[buf].buffer.len().saturating_sub(1);
        for wrapped in line.wrap(self.fill_column) {
            self.buffers[buf].buffer.push_line(wrapped);
        }
        self.trim_scrollback(buf);
    }

    fn trim_scrollback(&mut self, buf: usize) {
        let limit = self.scrollback.max(1);
        let excess = self.buffers[buf].buffer.len().saturating_sub(limit);
        if excess > 0 {
            self.buffers[buf].buffer.truncate_front(excess);
            if let Some(row) = &mut self.buffers[buf].first_unread {
                *row = row.saturating_sub(excess);
            }
            if let Some(row) = &mut self.buffers[buf].marker_row {
                *row = row.saturating_sub(excess);
            }
        }
    }

    fn bump_activity(&mut self, buf: usize, mentioned: bool) {
        if buf != self.current
            && self.buffers[buf].kind != Kind::Scratch
            && !self.buffers[buf].ignore_activity
        {
            self.activity_seq += 1;
            self.buffers[buf].unread += 1;
            self.buffers[buf].last_activity = self.activity_seq;
            if self.buffers[buf].first_unread.is_none() {
                // Insert a "read up to here" separator before the first unread
                // message, so it's visible when the buffer is visited.
                let row = self.last_push_row;
                let marker = self.marker_line();
                self.buffers[buf].buffer.insert_line(row, marker);
                self.buffers[buf].marker_row = Some(row);
                self.buffers[buf].first_unread = Some(row + 1);
            }
            if mentioned {
                self.buffers[buf].mentioned = true;
            }
        }
    }

    fn marker_line(&self) -> Line {
        Line::styled(
            "-".repeat(self.fill_column.max(1)),
            Style::fg(Color::DarkGray),
        )
    }

    /// Remove a buffer's read-marker separator, if present.
    fn remove_marker(&mut self, buf: usize) {
        if let Some(row) = self.buffers[buf].marker_row.take() {
            if row < self.buffers[buf].buffer.len() {
                self.buffers[buf].buffer.remove_line(row);
            }
            if let Some(first) = &mut self.buffers[buf].first_unread {
                *first = first.saturating_sub(1);
            }
        }
    }

    fn switch_to(&mut self, idx: usize) {
        if idx >= self.buffers.len() {
            return;
        }
        if idx != self.current {
            // Leaving a buffer: its unread separator has been seen.
            let old = self.current;
            self.remove_marker(old);
            self.previous = old;
        }
        self.current = idx;
        self.buffers[idx].unread = 0;
        self.buffers[idx].mentioned = false;
        // Keep the read marker / first-unread of the buffer we just entered.
        self.visit_seq += 1;
        self.buffers[idx].last_visited = self.visit_seq;
        self.buffers[idx].buffer.end_of_buffer();
    }

    fn scratch_index(&self) -> Option<usize> {
        self.buffers.iter().position(|b| b.kind == Kind::Scratch)
    }

    /// Switch to the buffer with the most recent activity, or to `*scratch*`
    /// when nothing has activity (mirrors `rcirc-next-active-buffer`).
    fn next_active(&mut self) {
        let target = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(i, b)| *i != self.current && b.unread > 0)
            .max_by_key(|(_, b)| b.last_activity)
            .map(|(i, _)| i);
        match target {
            Some(i) => self.switch_to(i),
            None => {
                if let Some(s) = self.scratch_index() {
                    self.switch_to(s);
                }
            }
        }
    }

    /// Targets with pending activity, most recent first, with the mode-line
    /// track face for each (`rcirc-track-keyword`, or `rcirc-track-nick` when
    /// our nick was mentioned).
    pub fn activity_entries(&self) -> Vec<(String, Style)> {
        let targets: Vec<String> = self.buffers.iter().map(|b| b.target.clone()).collect();
        let short = abbreviate(&targets);
        let mut active: Vec<usize> = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, b)| b.unread > 0)
            .map(|(i, _)| i)
            .collect();
        active.sort_by_key(|&i| std::cmp::Reverse(self.buffers[i].last_activity));
        active
            .into_iter()
            .map(|i| {
                let style = if self.buffers[i].mentioned {
                    self.theme.track_nick
                } else {
                    self.theme.track_keyword
                };
                (short[i].clone(), style)
            })
            .collect()
    }

    /// Targets with pending activity, most recent first.
    #[allow(dead_code)]
    pub fn activity_targets(&self) -> Vec<String> {
        self.activity_entries()
            .into_iter()
            .map(|(n, _)| n)
            .collect()
    }

    /// The `rcirc-activity-string`-style mode-line indicator.
    #[allow(dead_code)]
    pub fn activity_string(&self) -> String {
        let targets = self.activity_targets();
        if targets.is_empty() {
            String::new()
        } else {
            format!("[{}]", targets.join(","))
        }
    }

    /// The name of the previously-current buffer (the `C-x b` default).
    pub fn previous_buffer_name(&self) -> String {
        self.buffers
            .get(self.previous)
            .map(|b| b.target.clone())
            .unwrap_or_default()
    }

    /// Whether activity in the current buffer is ignored.
    pub fn ignore_activity(&self) -> bool {
        self.current().ignore_activity
    }

    /// The buffer name shown in the mode line.
    pub fn mode_line_name(&self) -> String {
        self.current().target.clone()
    }

    /// The indicator shown in parentheses in the mode line: the server name
    /// for IRC buffers, or the major mode for the scratch buffer.
    pub fn mode_line_mode(&self) -> String {
        let chan = self.current();
        match chan.kind {
            Kind::Scratch => "Fundamental".to_string(),
            _ => self.server_name(chan.server),
        }
    }

    fn server_name(&self, server: usize) -> String {
        self.servers
            .get(server)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }

    // ---- input -------------------------------------------------------------

    fn submit_input(&mut self) {
        let input = self.current().buffer.input();
        if input.trim().is_empty() {
            return;
        }
        let buf = self.current;
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
                    self.echo_plain(server, target, text);
                } else {
                    if let Some(action) = action_body(&text) {
                        let nick = self.our_nick(server);
                        let line = self.action_line(server, buf, &[], &nick, action);
                        self.push_current(line);
                    } else {
                        let nick = self.our_nick(server);
                        let line = self.chat_line(server, buf, &[], &nick, &text);
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
                let mut echoed = false;
                if let ClientCommand::Privmsg { target: t, text } = &cmd {
                    if *t == target {
                        let nick = self.our_nick(server);
                        let line = if let Some(action) = action_body(text) {
                            self.action_line(server, buf, &[], &nick, action)
                        } else {
                            self.chat_line(server, buf, &[], &nick, text)
                        };
                        self.push_current(line);
                        echoed = true;
                    }
                }
                // Echo commands (e.g. `/topic`) so they don't just vanish.
                if !echoed {
                    let line = self.command_echo_line(buf, &input);
                    self.push_line(buf, line);
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
                let i = self.current;
                self.buffers[i].buffer.clear();
                self.buffers[i].last_timestamp.clear();
            }
            LocalCommand::Disconnect => {
                self.servers[server].handle.send(ClientCommand::Disconnect);
            }
            LocalCommand::Close => {
                let cur = self.current;
                self.kill_buffer(cur);
            }
            LocalCommand::Switch(name) => {
                if let Some(i) = self.buffers.iter().position(|b| b.target == name) {
                    self.switch_to(i);
                }
            }
        }
    }

    /// Kill a buffer: part from a channel (or close a query), then remove it.
    fn kill_buffer(&mut self, idx: usize) {
        if idx >= self.buffers.len() {
            return;
        }
        let kind = self.buffers[idx].kind;
        let server = self.buffers[idx].server;
        let target = self.buffers[idx].target.clone();
        match kind {
            Kind::Server => {
                self.message = Some("Cannot kill a server buffer".to_string());
                return;
            }
            Kind::Scratch => {
                self.message = Some("Cannot kill *scratch*".to_string());
                return;
            }
            Kind::Channel => {
                self.send(
                    server,
                    ClientCommand::Part {
                        channel: target,
                        reason: None,
                    },
                );
            }
            Kind::Query => {}
        }
        self.buffers.remove(idx);
        if self.current > idx {
            self.current -= 1;
        } else if self.current == idx {
            self.current = idx.min(self.buffers.len().saturating_sub(1));
        }
        if self.previous >= self.buffers.len() {
            self.previous = self.buffers.len().saturating_sub(1);
        }
        if let Some(chan) = self.buffers.get_mut(self.current) {
            chan.buffer.end_of_buffer();
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
        let line = self.system_line(idx, &text);
        self.push_line(idx, line);
    }

    // ---- key handling ------------------------------------------------------

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        if key.kind == KeyEventKind::Release {
            return;
        }
        // Any keystroke cancels a pending `M-w` indication.
        self.copy_flash = None;
        if !matches!(self.prompt, Prompt::None) {
            self.handle_prompt_key(key);
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

        // A pending prefix always consumes the next key.
        if self.prefix != Prefix::None {
            self.handle_prefix(key, ctrl);
            return;
        }

        if ctrl && key.code == KeyCode::Char('x') {
            self.prefix = Prefix::CtrlX;
            self.message = Some("C-x-".to_string());
            return;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            self.prefix = Prefix::CtrlC;
            self.message = Some("C-c-".to_string());
            return;
        }

        self.message = None;
        let action = keymap::translate(key);
        if action == Action::None {
            if let Some(desc) = keymap::describe(key) {
                self.message = Some(format!("{desc} is undefined"));
            }
        } else {
            self.perform(action);
        }
    }

    fn handle_prefix(&mut self, key: crossterm::event::KeyEvent, ctrl: bool) {
        use crossterm::event::KeyCode;
        let prefix = std::mem::take(&mut self.prefix);
        self.message = None;
        match prefix {
            Prefix::None => {}
            Prefix::CtrlX => match key.code {
                KeyCode::Char('c') if ctrl => self.should_quit = true,
                KeyCode::Char('b') => self.start_switch_buffer(),
                KeyCode::Char('k') => self.start_kill_buffer(),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-x", key),
            },
            Prefix::CtrlC => match key.code {
                KeyCode::Char('b') if ctrl => self.start_browse_url(),
                KeyCode::Char(' ') | KeyCode::Char('@') | KeyCode::Char('\0') => self.next_active(),
                KeyCode::Char('g') if ctrl => {}
                KeyCode::Char('f') if ctrl => {
                    self.prefix = Prefix::CtrlCF;
                    self.message = Some("C-c C-f-".to_string());
                }
                KeyCode::Char('n') if ctrl => self.cmd_names(),
                KeyCode::Char('i') if ctrl => self.toggle_ignore_activity(),
                KeyCode::Char('j') if ctrl => self.prefill("/join "),
                KeyCode::Char('p') if ctrl => self.prefill("/part "),
                KeyCode::Char('q') if ctrl => self.prefill("/query "),
                KeyCode::Char('t') if ctrl => self.start_topic(),
                KeyCode::Char('w') if ctrl => self.prefill("/whois "),
                KeyCode::Char('m') if ctrl => self.prefill("/msg "),
                KeyCode::Char('k') if ctrl => self.prefill("/kick "),
                KeyCode::Char('d') if ctrl => self.prefill("/mode "),
                KeyCode::Char('r') if ctrl => self.prefill("/nick "),
                KeyCode::Char('x') if ctrl => self.prefill("/quit "),
                KeyCode::Char('s') if ctrl => self.switch_to_server(),
                KeyCode::Char('a') if ctrl => self.jump_first_unread(),
                _ => self.undefined("C-c", key),
            },
            Prefix::CtrlCF => match key.code {
                KeyCode::Char('b') if ctrl => self.insert_format('\u{2}'),
                KeyCode::Char('i') if ctrl => self.insert_format('\u{1d}'),
                KeyCode::Char('u') if ctrl => self.insert_format('\u{1f}'),
                KeyCode::Char('s') if ctrl => self.insert_format('\u{1e}'),
                KeyCode::Char('f') | KeyCode::Char('t') if ctrl => self.insert_format('\u{11}'),
                KeyCode::Char('d') if ctrl => self.unformat(),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-c C-f", key),
            },
        }
    }

    fn undefined(&mut self, prefix: &str, key: crossterm::event::KeyEvent) {
        if let Some(desc) = keymap::describe(key) {
            self.message = Some(format!("{prefix} {desc} is undefined"));
        }
    }

    /// Put a command prefix into the current buffer's input line.
    fn prefill(&mut self, text: &str) {
        let i = self.current;
        if self.buffers[i].buffer.is_irc() {
            self.buffers[i].buffer.set_input(text);
        }
    }

    fn cmd_names(&mut self) {
        let (server, target) = {
            let c = self.current();
            (c.server, c.target.clone())
        };
        self.send(server, ClientCommand::Names { channel: target });
    }

    fn switch_to_server(&mut self) {
        let server = self.current().server;
        if let Some(idx) = self.servers.get(server).map(|s| s.buffer) {
            self.switch_to(idx);
        }
    }

    fn jump_first_unread(&mut self) {
        let i = self.current;
        match self.buffers[i].first_unread {
            Some(row) => {
                let row = row.min(self.buffers[i].buffer.last_row());
                self.buffers[i].buffer.set_point(Pos::new(row, 0));
            }
            None => self.buffers[i].buffer.end_of_buffer(),
        }
    }

    fn insert_format(&mut self, code: char) {
        let i = self.current;
        if !self.buffers[i].buffer.is_irc() {
            return;
        }
        if let Some((start, end)) = self.buffers[i].buffer.region() {
            self.buffers[i].buffer.set_point(end);
            self.buffers[i].buffer.insert_char('\u{f}');
            self.buffers[i].buffer.set_point(start);
            self.buffers[i].buffer.insert_char(code);
            self.buffers[i].buffer.clear_mark();
        } else {
            self.buffers[i].buffer.insert_char(code);
            self.buffers[i].buffer.insert_char('\u{f}');
            self.buffers[i].buffer.backward_char();
        }
    }

    fn unformat(&mut self) {
        const CODES: [char; 5] = ['\u{2}', '\u{1d}', '\u{1f}', '\u{1e}', '\u{11}'];
        let i = self.current;
        if !self.buffers[i].buffer.is_irc() {
            return;
        }
        let input: Vec<char> = self.buffers[i].buffer.input().chars().collect();
        let prompt_len = self.buffers[i].buffer.prompt_len();
        let cursor = self.buffers[i]
            .buffer
            .point()
            .col
            .saturating_sub(prompt_len)
            .min(input.len());
        let start = (0..cursor).rev().find(|&ci| CODES.contains(&input[ci]));
        if let Some(start) = start {
            if let Some(end) = (start + 1..input.len()).find(|&ci| input[ci] == '\u{f}') {
                let mut new = input.clone();
                new.remove(end);
                new.remove(start);
                let text: String = new.into_iter().collect();
                self.buffers[i].buffer.set_input(&text);
            }
        }
    }

    fn start_switch_buffer(&mut self) {
        self.prompt = Prompt::SwitchBuffer {
            input: String::new(),
            selected: 0,
        };
    }

    fn start_browse_url(&mut self) {
        self.prompt = Prompt::BrowseUrl {
            input: String::new(),
            selected: 0,
        };
    }

    fn start_kill_buffer(&mut self) {
        self.prompt = Prompt::KillBuffer {
            input: String::new(),
            selected: 0,
        };
    }

    /// Buffers matching `input` (case-insensitive substring), most recently
    /// visited first. `include_current` keeps the current buffer in the list.
    fn buffer_candidates(&self, input: &str, include_current: bool) -> Vec<usize> {
        let needle = input.to_lowercase();
        let mut v: Vec<usize> = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(i, b)| {
                (include_current || *i != self.current)
                    && (needle.is_empty() || b.target.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();
        v.sort_by_key(|&i| std::cmp::Reverse(self.buffers[i].last_visited));
        v.truncate(5);
        v
    }

    /// Buffers matching `input`, excluding the current one (for switching).
    fn switch_candidates(&self, input: &str) -> Vec<usize> {
        self.buffer_candidates(input, false)
    }

    /// Buffers matching `input`, including the current one (for killing).
    fn kill_candidates(&self, input: &str) -> Vec<usize> {
        self.buffer_candidates(input, true)
    }

    /// URLs in the current buffer matching `input`, most recent first.
    fn url_candidates(&self, input: &str) -> Vec<String> {
        let needle = input.to_lowercase();
        self.buffer_urls()
            .into_iter()
            .filter(|u| needle.is_empty() || u.to_lowercase().contains(&needle))
            .take(5)
            .collect()
    }

    fn buffer_urls(&self) -> Vec<String> {
        let buf = &self.current().buffer;
        let mut urls: Vec<String> = Vec::new();
        for row in 0..buf.len() {
            for token in buf.line(row).text().split_whitespace() {
                if let Some(url) = extract_url(token) {
                    if !urls.contains(&url) {
                        urls.push(url);
                    }
                }
            }
        }
        urls.reverse();
        urls
    }

    fn prompt_move(&mut self, count: usize, delta: isize) {
        let selected = match &mut self.prompt {
            Prompt::SwitchBuffer { selected, .. }
            | Prompt::BrowseUrl { selected, .. }
            | Prompt::KillBuffer { selected, .. } => selected,
            Prompt::None | Prompt::Topic { .. } => return,
        };
        if count == 0 {
            *selected = 0;
            return;
        }
        let next = *selected as isize + delta;
        *selected = next.clamp(0, count as isize - 1) as usize;
    }

    fn prompt_push(&mut self, c: char) {
        match &mut self.prompt {
            Prompt::SwitchBuffer { input, selected }
            | Prompt::BrowseUrl { input, selected }
            | Prompt::KillBuffer { input, selected } => {
                input.push(c);
                *selected = 0;
            }
            Prompt::Topic { input, .. } => input.push(c),
            Prompt::None => {}
        }
    }

    fn prompt_backspace(&mut self) {
        match &mut self.prompt {
            Prompt::SwitchBuffer { input, selected }
            | Prompt::BrowseUrl { input, selected }
            | Prompt::KillBuffer { input, selected } => {
                input.pop();
                *selected = 0;
            }
            Prompt::Topic { input, .. } => {
                input.pop();
            }
            Prompt::None => {}
        }
    }

    fn prompt_complete(&mut self) {
        let candidates = match &self.prompt {
            Prompt::SwitchBuffer { input, .. } => self
                .switch_candidates(input)
                .into_iter()
                .map(|i| self.buffers[i].target.clone())
                .collect::<Vec<_>>(),
            Prompt::BrowseUrl { input, .. } => self.url_candidates(input),
            Prompt::KillBuffer { input, .. } => self
                .kill_candidates(input)
                .into_iter()
                .map(|i| self.buffers[i].target.clone())
                .collect::<Vec<_>>(),
            Prompt::None | Prompt::Topic { .. } => return,
        };
        if candidates.is_empty() {
            return;
        }
        let completion = if candidates.len() == 1 {
            candidates[0].clone()
        } else {
            common_prefix(&candidates)
        };
        if let Prompt::SwitchBuffer { input, selected }
        | Prompt::BrowseUrl { input, selected }
        | Prompt::KillBuffer { input, selected } = &mut self.prompt
        {
            *input = completion;
            *selected = 0;
        }
    }

    fn handle_prompt_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let count = match &self.prompt {
            Prompt::SwitchBuffer { input, .. } => self.switch_candidates(input).len(),
            Prompt::BrowseUrl { input, .. } => self.url_candidates(input).len(),
            Prompt::KillBuffer { input, .. } => self.kill_candidates(input).len(),
            Prompt::None | Prompt::Topic { .. } => 0,
        };
        match key.code {
            KeyCode::Char('g') if ctrl => self.prompt = Prompt::None,
            KeyCode::Char('c') if ctrl => self.prompt = Prompt::None,
            KeyCode::Esc => self.prompt = Prompt::None,
            KeyCode::Enter => self.commit_prompt(),
            KeyCode::Tab => self.prompt_complete(),
            KeyCode::Char('n') if ctrl => self.prompt_move(count, 1),
            KeyCode::Char('p') if ctrl => self.prompt_move(count, -1),
            KeyCode::Down => self.prompt_move(count, 1),
            KeyCode::Up => self.prompt_move(count, -1),
            KeyCode::Backspace => self.prompt_backspace(),
            KeyCode::Char(c) if !ctrl => self.prompt_push(c),
            _ => {}
        }
    }

    fn commit_prompt(&mut self) {
        match std::mem::replace(&mut self.prompt, Prompt::None) {
            Prompt::None => {}
            Prompt::SwitchBuffer { input, selected } => {
                let candidates = self.switch_candidates(&input);
                if let Some(&i) = candidates.get(selected) {
                    self.switch_to(i);
                } else if input.trim().is_empty() {
                    let prev = self.previous;
                    if prev < self.buffers.len() {
                        self.switch_to(prev);
                    }
                } else if let Some(i) = self.buffers.iter().position(|b| b.target == input.trim()) {
                    self.switch_to(i);
                } else {
                    self.message = Some(format!("No such buffer: {}", input.trim()));
                }
            }
            Prompt::BrowseUrl { input, selected } => {
                let urls = self.url_candidates(&input);
                let url = urls
                    .get(selected)
                    .cloned()
                    .or_else(|| (!input.trim().is_empty()).then(|| input.trim().to_string()));
                if let Some(url) = url {
                    self.open_url(&url);
                }
            }
            Prompt::KillBuffer { input, selected } => {
                let candidates = self.kill_candidates(&input);
                if let Some(&i) = candidates.get(selected) {
                    self.kill_buffer(i);
                } else if let Some(i) = self.buffers.iter().position(|b| b.target == input.trim()) {
                    self.kill_buffer(i);
                } else if input.trim().is_empty() {
                    let cur = self.current;
                    self.kill_buffer(cur);
                } else {
                    self.message = Some(format!("No such buffer: {}", input.trim()));
                }
            }
            Prompt::Topic { input, default } => {
                let (server, kind, target) = {
                    let c = self.current();
                    (c.server, c.kind, c.target.clone())
                };
                if kind != Kind::Channel {
                    self.message = Some("Not a channel buffer".to_string());
                    return;
                }
                // Empty input means "keep the default" (the current topic).
                let text = if input.trim().is_empty() {
                    default
                } else {
                    input
                };
                let current = self.current().topic.clone().unwrap_or_default();
                if text.is_empty() {
                    // Nothing at all: query the topic.
                    self.send(
                        server,
                        ClientCommand::Topic {
                            channel: target,
                            topic: None,
                        },
                    );
                } else if text != current {
                    self.send(
                        server,
                        ClientCommand::Topic {
                            channel: target,
                            topic: Some(text),
                        },
                    );
                }
            }
        }
    }

    fn start_topic(&mut self) {
        let topic = self.current().topic.clone().unwrap_or_default();
        self.prompt = Prompt::Topic {
            input: String::new(),
            default: topic,
        };
    }

    fn toggle_ignore_activity(&mut self) {
        let i = self.current;
        self.buffers[i].ignore_activity = !self.buffers[i].ignore_activity;
        // Clear any activity already recorded for this buffer.
        if self.buffers[i].ignore_activity {
            self.remove_marker(i);
            self.buffers[i].unread = 0;
            self.buffers[i].mentioned = false;
            self.buffers[i].first_unread = None;
        }
        self.message = Some(
            if self.buffers[i].ignore_activity {
                "Ignore activity in this buffer"
            } else {
                "Notice activity in this buffer"
            }
            .to_string(),
        );
    }

    fn open_url(&mut self, url: &str) {
        let url = normalize_url(url);
        if let Err(err) = platform_open(&url) {
            self.message = Some(format!("Could not open {url}: {err}"));
        }
    }

    /// The active prompt as (prompt line, candidate list, selected index).
    pub fn prompt_view(&self) -> Option<(String, Vec<String>, usize)> {
        match &self.prompt {
            Prompt::None => None,
            Prompt::SwitchBuffer { input, selected } => {
                let names = self
                    .switch_candidates(input)
                    .into_iter()
                    .map(|i| self.buffers[i].target.clone())
                    .collect();
                Some((
                    format!(
                        "Switch to buffer (default {}): {input}",
                        self.previous_buffer_name()
                    ),
                    names,
                    *selected,
                ))
            }
            Prompt::BrowseUrl { input, selected } => Some((
                format!("Browse URL: {input}"),
                self.url_candidates(input),
                *selected,
            )),
            Prompt::KillBuffer { input, selected } => {
                let names = self
                    .kill_candidates(input)
                    .into_iter()
                    .map(|i| self.buffers[i].target.clone())
                    .collect();
                Some((
                    format!("Kill buffer (default {}): {input}", self.mode_line_name()),
                    names,
                    *selected,
                ))
            }
            Prompt::Topic { input, default } => {
                let prompt = if default.is_empty() {
                    format!("Topic: {input}")
                } else {
                    format!("Topic (default {default}): {input}")
                };
                Some((prompt, Vec::new(), 0))
            }
        }
    }

    /// Height of the minibuffer area, expanded to show prompt candidates.
    pub fn minibuffer_height(&self) -> u16 {
        let extra = self
            .prompt_view()
            .map(|(_, candidates, _)| candidates.len().min(5))
            .unwrap_or(0);
        (1 + extra) as u16
    }

    fn perform(&mut self, action: Action) {
        use Action::*;
        let prev = self.last_command;
        self.last_command = LastCommand::Other;
        match action {
            None => {}
            SelfInsert(c) => self.current_mut().buffer.insert_char(c),
            Send => {
                if self.current().kind == Kind::Scratch {
                    self.current_mut().buffer.insert_newline();
                } else {
                    self.submit_input();
                }
            }
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
            BackwardKillWord => {
                let killed = self.current_mut().buffer.backward_kill_word();
                self.kill_ring.push(&killed, prev == LastCommand::Kill);
                self.last_command = LastCommand::Kill;
            }
            KillLine => {
                let killed = self.current_mut().buffer.kill_line();
                self.kill_ring.push(&killed, prev == LastCommand::Kill);
                self.last_command = LastCommand::Kill;
            }
            SetMark => self.current_mut().buffer.set_mark(),
            KillRegion => self.kill_region(prev),
            CopyRegion => self.copy_region(prev),
            Yank => self.yank(),
            YankPop => self.yank_pop(prev),
            HistoryPrev => self.history_prev(),
            HistoryNext => self.history_next(),
            Complete => {
                if self.current().kind == Kind::Scratch {
                    self.current_mut().buffer.insert_str("    ");
                } else {
                    self.complete();
                }
            }
            SearchForward => self.start_search(SearchDirection::Forward),
            SearchBackward => self.start_search(SearchDirection::Backward),
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

    /// A screenful minus two lines, like Emacs `next-screen-context-lines`.
    fn page_lines(&self) -> usize {
        self.viewport_rows.saturating_sub(2).max(1)
    }

    fn page_up(&mut self) {
        let n = self.page_lines();
        self.current_mut().buffer.page_backward(n);
    }

    fn page_down(&mut self) {
        let n = self.page_lines();
        self.current_mut().buffer.page_forward(n);
    }

    /// `M-w`: save the region to the kill ring (no deletion).
    fn copy_region(&mut self, prev: LastCommand) {
        let i = self.current;
        let Some((start, end)) = self.buffers[i].buffer.region() else {
            self.message = Some("The mark is not set now, so there is no region".to_string());
            return;
        };
        let Some(text) = self.buffers[i].buffer.region_text() else {
            return;
        };
        self.kill_ring.push(&text, prev == LastCommand::Kill);
        self.last_command = LastCommand::Kill;

        if start == end {
            return;
        }
        // Emacs `indicate-copied-region`: flash the far end of the region, or
        // echo `Saved text until "…"` if it is off screen.
        let point = self.buffers[i].buffer.point();
        let other = if point == end { start } else { end };
        if self.visible_row(i, other.row) {
            self.copy_flash = Some((point, Instant::now() + Duration::from_millis(220)));
            self.buffers[i].buffer.set_point(other);
        } else {
            let snippet = snippet(&text);
            self.message = Some(format!("Saved text until \"{snippet}\""));
        }
    }

    /// Whether `row` is currently within the buffer viewport.
    fn visible_row(&self, buf: usize, row: usize) -> bool {
        let height = self.viewport_rows.max(1);
        let point = self.buffers[buf].buffer.point();
        let mut top = self.buffers[buf].buffer.scroll();
        if point.row < top {
            top = point.row;
        }
        if point.row >= top + height {
            top = point.row + 1 - height;
        }
        row >= top && row < top + height
    }

    /// `C-w`: kill the region (delete and save to the kill ring).
    fn kill_region(&mut self, prev: LastCommand) {
        let i = self.current;
        let Some((start, end)) = self.buffers[i].buffer.region() else {
            self.message = Some("The mark is not set now, so there is no region".to_string());
            return;
        };
        let Some(text) = self.buffers[i].buffer.region_text() else {
            return;
        };
        if self.buffers[i].buffer.delete_region(start, end) {
            self.kill_ring.push(&text, prev == LastCommand::Kill);
            self.last_command = LastCommand::Kill;
        } else {
            self.message = Some("Text is read-only".to_string());
        }
    }

    /// `C-y`: insert the most recent kill.
    fn yank(&mut self) {
        let Some(text) = self.kill_ring.yank() else {
            self.message = Some("Kill ring is empty".to_string());
            return;
        };
        let i = self.current;
        if !self.buffers[i].buffer.editable() {
            self.message = Some("Text is read-only".to_string());
            return;
        }
        let start = self.buffers[i].buffer.point();
        self.buffers[i].buffer.insert_str(&text);
        let end = self.buffers[i].buffer.point();
        self.last_yank = Some((start, end));
        self.last_command = LastCommand::Yank;
    }

    /// `M-y`: replace the just-yanked text with the previous kill.
    fn yank_pop(&mut self, prev: LastCommand) {
        if prev != LastCommand::Yank {
            self.message = Some("Previous command was not a yank".to_string());
            return;
        }
        let Some((start, end)) = self.last_yank else {
            return;
        };
        let i = self.current;
        if !self.buffers[i].buffer.delete_region(start, end) {
            return;
        }
        let Some(text) = self.kill_ring.yank_pop() else {
            return;
        };
        self.buffers[i].buffer.set_point(start);
        let start = self.buffers[i].buffer.point();
        self.buffers[i].buffer.insert_str(&text);
        let end = self.buffers[i].buffer.point();
        self.last_yank = Some((start, end));
        self.last_command = LastCommand::Yank;
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
            self.theme.my_nick
        } else {
            self.theme.other_nick
        }
    }

    /// The 6-column timestamp field for `buf`. Repeated timestamps are blanked
    /// so only the first line of a run carries the time.
    fn time_prefix(&mut self, buf: usize, ts: &str) -> String {
        if self.buffers[buf].last_timestamp == ts {
            "      ".to_string()
        } else {
            self.buffers[buf].last_timestamp = ts.to_string();
            format!("{ts} ")
        }
    }

    /// Message body as spans: whole-word occurrences of our nick get
    /// `rcirc-nick-in-message`, URLs get `rcirc-url`. Returns whether our nick
    /// was mentioned.
    fn message_spans(&self, server: usize, text: &str) -> (Vec<Span>, bool) {
        let our = self.our_nick(server);
        let mut mentioned = false;
        let mut spans = Vec::new();
        let mut run = String::new();
        let mut run_space: Option<bool> = None;
        for ch in text.chars() {
            let space = ch.is_whitespace();
            match run_space {
                Some(s) if s == space => run.push(ch),
                Some(s) => {
                    self.emit_run(&mut run, s, &our, &mut mentioned, &mut spans);
                    run.push(ch);
                    run_space = Some(space);
                }
                None => {
                    run.push(ch);
                    run_space = Some(space);
                }
            }
        }
        if let Some(s) = run_space {
            self.emit_run(&mut run, s, &our, &mut mentioned, &mut spans);
        }
        (spans, mentioned)
    }

    fn emit_run(
        &self,
        run: &mut String,
        space: bool,
        our: &str,
        mentioned: &mut bool,
        spans: &mut Vec<Span>,
    ) {
        if run.is_empty() {
            return;
        }
        let text = std::mem::take(run);
        if space {
            spans.push(Span::new(text, Style::default()));
        } else {
            spans.extend(self.token_spans(&text, our, mentioned));
        }
    }

    fn token_spans(&self, token: &str, our: &str, mentioned: &mut bool) -> Vec<Span> {
        if let Some(url) = extract_url(token) {
            if let Some(start) = token.find(&url) {
                let mut spans = Vec::new();
                let before = &token[..start];
                let after = &token[start + url.len()..];
                if !before.is_empty() {
                    spans.extend(self.nick_spans(before, our, mentioned));
                }
                spans.push(Span::new(url, self.theme.url));
                if !after.is_empty() {
                    spans.extend(self.nick_spans(after, our, mentioned));
                }
                return spans;
            }
        }
        self.nick_spans(token, our, mentioned)
    }

    /// Highlight whole-word occurrences of `our` within `text`.
    fn nick_spans(&self, text: &str, our: &str, mentioned: &mut bool) -> Vec<Span> {
        let chars: Vec<char> = text.chars().collect();
        let needle: Vec<char> = our.to_lowercase().chars().collect();
        if our.is_empty() || needle.len() > chars.len() {
            return vec![Span::new(text.to_string(), Style::default())];
        }
        let lowered: Vec<char> = text.to_lowercase().chars().collect();
        let mut spans = Vec::new();
        let mut last = 0;
        let mut i = 0;
        while i + needle.len() <= chars.len() {
            let hit = lowered[i..i + needle.len()] == needle[..];
            let before_ok = i == 0 || !is_word_char(chars[i - 1]);
            let after_ok =
                i + needle.len() >= chars.len() || !is_word_char(chars[i + needle.len()]);
            if hit && before_ok && after_ok {
                if i > last {
                    spans.push(Span::new(
                        chars[last..i].iter().collect::<String>(),
                        Style::default(),
                    ));
                }
                spans.push(Span::new(
                    chars[i..i + needle.len()].iter().collect::<String>(),
                    self.theme.nick_in_message,
                ));
                *mentioned = true;
                last = i + needle.len();
                i = last;
            } else {
                i += 1;
            }
        }
        if last < chars.len() {
            spans.push(Span::new(
                chars[last..].iter().collect::<String>(),
                Style::default(),
            ));
        }
        if spans.is_empty() {
            spans.push(Span::new(text.to_string(), Style::default()));
        }
        spans
    }

    fn chat_line(
        &mut self,
        server: usize,
        buf: usize,
        tags: &[Tag],
        nick: &str,
        text: &str,
    ) -> Line {
        let ts = timestamp(tags);
        let prefix = self.time_prefix(buf, &ts);
        let nstyle = self.nick_style(server, nick);
        let (mut body, mentioned) = self.message_spans(server, text);
        if mentioned {
            for span in &mut body {
                span.style = span.style.bold();
            }
        }
        let indent = 6 + nick.chars().count() + 3;
        let mut spans = vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new(format!("<{nick}>"), nstyle),
            Span::new(" ".to_string(), Style::default()),
        ];
        spans.extend(body);
        Line::from_spans(spans).indented(indent)
    }

    fn action_line(
        &mut self,
        server: usize,
        buf: usize,
        tags: &[Tag],
        nick: &str,
        body: &str,
    ) -> Line {
        let ts = timestamp(tags);
        let prefix = self.time_prefix(buf, &ts);
        let nstyle = self.nick_style(server, nick);
        let (body, _) = self.message_spans(server, body);
        let mut spans = vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new("[".to_string(), Style::default()),
            Span::new(nick.to_string(), nstyle),
            Span::new(" ".to_string(), Style::default()),
        ];
        let indent = 6 + nick.chars().count() + 2;
        spans.extend(body);
        spans.push(Span::new("]".to_string(), Style::default()));
        Line::from_spans(spans).indented(indent)
    }

    fn notice_line(
        &mut self,
        server: usize,
        buf: usize,
        tags: &[Tag],
        nick: &str,
        text: &str,
    ) -> Line {
        let ts = timestamp(tags);
        let prefix = self.time_prefix(buf, &ts);
        let nstyle = self.nick_style(server, nick);
        let (body, _) = self.message_spans(server, text);
        let indent = 6 + nick.chars().count() + 3;
        let mut spans = vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new("-".to_string(), nstyle),
            Span::new(nick.to_string(), nstyle),
            Span::new("- ".to_string(), nstyle),
        ];
        spans.extend(body);
        Line::from_spans(spans).indented(indent)
    }

    /// A server/event line in rcirc's default format: `*** [sender] RESPONSE
    /// text`, rendered in the `rcirc-server` face.
    fn server_line(
        &mut self,
        buf: usize,
        sender: Option<&str>,
        response: &str,
        text: &str,
    ) -> Line {
        let ts = now_hhmm();
        let prefix = self.time_prefix(buf, &ts);
        let mut head = String::from("***");
        if let Some(sender) = sender {
            head.push(' ');
            head.push_str(sender);
        }
        if !response.is_empty() {
            head.push(' ');
            head.push_str(response);
        }
        let indent = 6 + head.chars().count() + usize::from(!text.is_empty());
        let body = if text.is_empty() {
            head
        } else {
            format!("{head} {text}")
        };
        Line::from_spans(vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new(body, self.theme.server),
        ])
        .indented(indent)
    }

    /// A connection/system message (`*** text`).
    fn system_line(&mut self, buf: usize, text: &str) -> Line {
        self.server_line(buf, None, "", text)
    }

    /// Echo a user-issued command (`>>> /topic ...`) in the current buffer.
    fn command_echo_line(&mut self, buf: usize, text: &str) -> Line {
        let ts = now_hhmm();
        let prefix = self.time_prefix(buf, &ts);
        Line::from_spans(vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new(format!(">>> {text}"), Style::fg(Color::Gray)),
        ])
        .indented(10)
    }

    /// An error/warning line (`!!! text`) in `font-lock-warning-face`.
    fn error_line(&mut self, buf: usize, text: &str) -> Line {
        let ts = now_hhmm();
        let prefix = self.time_prefix(buf, &ts);
        Line::from_spans(vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new(format!("!!! {text}"), self.theme.warning),
        ])
        .indented(10)
    }

    /// Print an error into every buffer of `server` (mirrors rcirc's sentinel
    /// printing an `ERROR` response in all buffers on disconnect).
    fn broadcast_error(&mut self, server: usize, message: &str) {
        let text = format!("{}: {message}", self.server_name(server));
        let targets: Vec<usize> = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, b)| b.server == server && b.kind != Kind::Scratch)
            .map(|(i, _)| i)
            .collect();
        for idx in targets {
            let line = self.error_line(idx, &text);
            self.push_line(idx, line);
        }
    }

    // ---- network events ----------------------------------------------------

    pub fn handle_event(&mut self, server: usize, event: Event) {
        match event {
            Event::Connecting { server: name } => {
                let idx = self.servers[server].buffer;
                let line = self.system_line(idx, &format!("Connecting to {name}..."));
                self.push_line(idx, line);
            }
            Event::Connected => {
                let idx = self.servers[server].buffer;
                let line = self.system_line(idx, "Connected");
                self.push_line(idx, line);
                self.servers[server].connected = true;
            }
            Event::TlsEstablished => {
                let idx = self.servers[server].buffer;
                let line = self.system_line(idx, "TLS established");
                self.push_line(idx, line);
            }
            Event::Registered { nick } => {
                self.servers[server].our_nick = nick.clone();
                let idx = self.servers[server].buffer;
                let line = self.system_line(idx, &format!("Registered as {nick}"));
                self.push_line(idx, line);
            }
            Event::Disconnected { reason } => {
                self.servers[server].connected = false;
                self.broadcast_error(server, &reason);
            }
            Event::ConnectionError { error } => {
                self.servers[server].connected = false;
                self.broadcast_error(server, &error);
            }
            Event::Privmsg {
                from,
                target,
                text,
                tags,
            } => {
                let idx = self.route_message(server, &from, &target);
                let mentioned = self.message_spans(server, &text).1;
                let line = self.chat_line(server, idx, &tags, &from, &text);
                self.push_line(idx, line);
                self.bump_activity(idx, mentioned);
            }
            Event::Notice {
                from,
                target,
                text,
                tags,
            } => {
                let idx = self.route_message(server, &from, &target);
                let mentioned = self.message_spans(server, &text).1;
                let line = self.notice_line(server, idx, &tags, &from, &text);
                self.push_line(idx, line);
                self.bump_activity(idx, mentioned);
            }
            Event::Action {
                from,
                target,
                text,
                tags,
            } => {
                let idx = self.route_message(server, &from, &target);
                let mentioned = self.message_spans(server, &text).1;
                let line = self.action_line(server, idx, &tags, &from, &text);
                self.push_line(idx, line);
                self.bump_activity(idx, mentioned);
            }
            Event::Ctcp {
                from,
                target,
                request,
                body,
            } => {
                let idx = self.route_message(server, &from, &target);
                let text = if body.is_empty() {
                    request
                } else {
                    format!("{request} {body}")
                };
                let line = self.server_line(idx, Some(&from), "CTCP", &text);
                self.push_line(idx, line);
            }
            Event::Join { channel, nick } => {
                let idx = self.ensure_buffer(server, channel, Kind::Channel);
                self.add_nick(idx, &nick);
                let line = self.server_line(idx, Some(&nick), "JOIN", "");
                self.push_line(idx, line);
                // Our own /join should surface the new channel as activity.
                if nick == self.our_nick(server) {
                    self.bump_activity(idx, false);
                }
            }
            Event::Part {
                channel,
                nick,
                reason,
            } => {
                // Do not recreate a channel buffer we've killed/parted.
                if let Some(idx) = self.find_buffer(server, &channel, Kind::Channel) {
                    self.remove_nick(idx, &nick);
                    let line =
                        self.server_line(idx, Some(&nick), "PART", &format!("{channel} {reason}"));
                    self.push_line(idx, line);
                }
            }
            Event::Kick {
                channel,
                nick,
                by,
                reason,
            } => {
                if let Some(idx) = self.find_buffer(server, &channel, Kind::Channel) {
                    self.remove_nick(idx, &nick);
                    let text = format!("{nick} {channel} {reason}");
                    let line = self.server_line(idx, Some(&by), "KICK", text.trim_end());
                    self.push_line(idx, line);
                }
            }
            Event::Quit { nick, reason } => {
                for idx in 0..self.buffers.len() {
                    if self.buffers[idx].kind == Kind::Channel
                        && self.buffers[idx].server == server
                        && self.buffers[idx].nicks.iter().any(|n| n == &nick)
                    {
                        self.remove_nick(idx, &nick);
                        let line = self.server_line(idx, Some(&nick), "QUIT", &reason);
                        self.push_line(idx, line);
                    }
                }
            }
            Event::Nick { old, new } => {
                let ours = old == self.our_nick(server);
                if ours {
                    self.servers[server].our_nick = new.clone();
                }
                for idx in 0..self.buffers.len() {
                    if self.buffers[idx].server == server {
                        if let Some(pos) = self.buffers[idx].nicks.iter().position(|n| n == &old) {
                            self.buffers[idx].nicks[pos] = new.clone();
                        }
                    }
                }
                let idx = self.servers[server].buffer;
                let line = self.server_line(idx, Some(&old), "NICK", &new);
                self.push_line(idx, line);
                // Our own /nick should register as activity.
                if ours {
                    self.bump_activity(idx, false);
                }
            }
            Event::TopicChanged { channel, topic, by } => {
                let idx = self.ensure_buffer(server, channel.clone(), Kind::Channel);
                self.buffers[idx].topic = Some(topic.clone());
                let line = self.server_line(idx, Some(&by), "TOPIC", &topic);
                self.push_line(idx, line);
                self.bump_activity(idx, false);
            }
            Event::TopicReply { channel, topic, .. } => {
                let idx = self.ensure_buffer(server, channel.clone(), Kind::Channel);
                self.buffers[idx].topic = Some(topic.clone());
                let line = self.server_line(idx, None, "TOPIC", &format!("{channel} :{topic}"));
                self.push_line(idx, line);
                self.bump_activity(idx, false);
            }
            Event::Mode { target, modes } => {
                // Channel modes go to the channel buffer; user modes (e.g. our
                // own `MODE nick +Ziw`) go to the server buffer.
                let idx = if is_channel(&target) {
                    Some(self.ensure_buffer(server, target.clone(), Kind::Channel))
                } else {
                    self.servers.get(server).map(|s| s.buffer)
                };
                if let Some(idx) = idx {
                    let line = self.server_line(idx, None, "MODE", &format!("{target} {modes}"));
                    self.push_line(idx, line);
                }
            }
            Event::Names { channel, nicks } => {
                let idx = self.ensure_buffer(server, channel, Kind::Channel);
                for nick in nicks {
                    self.add_nick(idx, &nick);
                }
            }
            Event::NamesEnd { channel } => {
                let idx = self.ensure_buffer(server, channel, Kind::Channel);
                let names = self.buffers[idx].nicks.join(" ");
                let line = self.server_line(idx, None, "NAMES", &names);
                self.push_line(idx, line);
            }
            Event::Whois { nick, line } => {
                let idx = self.servers[server].buffer;
                let l = self.system_line(idx, &format!("{nick}: {line}"));
                self.push_line(idx, l);
            }
            Event::Motd { text } => {
                let idx = self.servers[server].buffer;
                if !text.is_empty() {
                    let l = self.system_line(idx, &text);
                    self.push_line(idx, l);
                }
            }
            Event::ServerText {
                command,
                target,
                text,
            } => {
                if !text.is_empty() {
                    // Show channel-related numerics (e.g. topic errors) in the
                    // channel buffer, not the server buffer.
                    let idx = target
                        .as_deref()
                        .and_then(|t| self.find_buffer(server, t, Kind::Channel))
                        .or_else(|| self.servers.get(server).map(|s| s.buffer));
                    if let Some(idx) = idx {
                        let l = self.server_line(idx, None, &command, &text);
                        self.push_line(idx, l);
                        self.bump_activity(idx, false);
                    }
                }
            }
            Event::Isupport { tokens } => {
                let idx = self.servers[server].buffer;
                let line = self.system_line(idx, &format!("Supported: {}", tokens.join(" ")));
                self.push_line(idx, line);
            }
            Event::Ping { .. } | Event::Pong { .. } => {}
            Event::CapAck { caps } => {
                let idx = self.servers[server].buffer;
                let line =
                    self.system_line(idx, &format!("Capabilities enabled: {}", caps.join(" ")));
                self.push_line(idx, line);
            }
            Event::CapNak { caps } => {
                let idx = self.servers[server].buffer;
                let line =
                    self.system_line(idx, &format!("Capabilities rejected: {}", caps.join(" ")));
                self.push_line(idx, line);
            }
            Event::CapList { .. } | Event::Authenticate => {}
            Event::LoggedIn => {
                let idx = self.servers[server].buffer;
                let line = self.system_line(idx, "SASL authentication succeeded");
                self.push_line(idx, line);
            }
            Event::StandardReply { level, text } => {
                let idx = self.servers[server].buffer;
                let line = self.system_line(idx, &format!("{level}: {text}"));
                self.push_line(idx, line);
            }
            Event::Sent { .. } | Event::Received { .. } | Event::RawMessage { .. } => {}
        }
    }

    /// Route an incoming message to a buffer. Server notices (target `*` or an
    /// empty target, or sent by the server itself) go to the server buffer;
    /// channel targets go to the channel; everything else opens a query named
    /// after the other party.
    fn route_message(&mut self, server: usize, from: &str, target: &str) -> usize {
        let route = classify(
            from,
            target,
            &self.our_nick(server),
            &self.server_name(server),
        );
        match route {
            Route::Server => self.servers[server].buffer,
            Route::Channel(channel) => self.ensure_buffer(server, channel, Kind::Channel),
            Route::Query(nick) => self.ensure_buffer(server, nick, Kind::Query),
        }
    }

    fn add_nick(&mut self, buf: usize, nick: &str) {
        let bare = strip_prefixes(nick).to_string();
        let existing = self.buffers[buf]
            .nicks
            .iter()
            .position(|n| strip_prefixes(n) == bare);
        match existing {
            // Keep the more informative (prefixed) spelling, e.g. `@nick`.
            Some(pos) => {
                if nick.chars().count() > self.buffers[buf].nicks[pos].chars().count() {
                    self.buffers[buf].nicks[pos] = nick.to_string();
                }
            }
            None => {
                self.buffers[buf].nicks.push(nick.to_string());
                self.buffers[buf].nicks.sort();
            }
        }
    }

    fn remove_nick(&mut self, buf: usize, nick: &str) {
        let bare = strip_prefixes(nick).to_string();
        self.buffers[buf]
            .nicks
            .retain(|n| strip_prefixes(n) != bare);
    }

    // ---- loop --------------------------------------------------------------

    pub async fn run<B: Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
        mut net_rx: mpsc::UnboundedReceiver<(usize, Event)>,
        mut term_rx: mpsc::UnboundedReceiver<TermEvent>,
    ) -> Result<()> {
        loop {
            if let Ok(size) = terminal.size() {
                // Buffer view height = terminal rows minus mode line + minibuffer.
                self.viewport_rows = size.height.saturating_sub(2) as usize;
            }
            terminal.draw(|frame| crate::ui::draw(frame, self))?;
            if self.should_quit {
                for state in &self.servers {
                    state.handle.quit("cricr");
                }
                break;
            }
            let flash_deadline = self.copy_flash.map(|(_, deadline)| deadline);
            tokio::select! {
                Some((server, event)) = net_rx.recv() => {
                    self.handle_event(server, event);
                    // Batch a burst of server output into a single redraw.
                    while let Ok((server, event)) = net_rx.try_recv() {
                        self.handle_event(server, event);
                    }
                }
                Some(term_event) = term_rx.recv() => {
                    self.handle_term(term_event);
                    // Batch a burst of input (e.g. a paste) into one redraw.
                    while let Ok(event) = term_rx.try_recv() {
                        self.handle_term(event);
                    }
                }
                // Restore the point after the `M-w` indication flashes.
                _ = async {
                    match flash_deadline {
                        Some(deadline) => {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    if let Some((restore, _)) = self.copy_flash.take() {
                        self.current_mut().buffer.set_point(restore);
                    }
                }
                else => break,
            }
        }
        Ok(())
    }

    fn handle_term(&mut self, event: TermEvent) {
        match event {
            TermEvent::Key(key) => self.handle_key(key),
            TermEvent::Paste(text) => self.handle_paste(text),
            _ => {}
        }
    }

    /// Insert pasted text. In an rcirc buffer newlines become spaces (the input
    /// is a single line); the scratch buffer keeps them.
    fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if !matches!(self.prompt, Prompt::None) {
            for ch in text.chars() {
                if ch != '\r' && ch != '\n' {
                    self.prompt_push(ch);
                }
            }
            return;
        }
        let i = self.current;
        if self.buffers[i].buffer.is_irc() {
            let cleaned: String = text
                .chars()
                .filter(|c| *c != '\r')
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect();
            self.buffers[i].buffer.insert_str(&cleaned);
        } else {
            self.buffers[i].buffer.insert_str(&text);
        }
    }
}

/// Where an incoming message should be shown.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Server,
    Channel(String),
    Query(String),
}

/// Decide the destination buffer for a message from `from` addressed to
/// `target`. Server-originated traffic (`target` of `*`/empty, or sent by the
/// server itself) belongs in the server buffer; not a `#channel` → a query.
fn classify(from: &str, target: &str, our: &str, server_name: &str) -> Route {
    if target == "*" || target.is_empty() || from == server_name {
        Route::Server
    } else if is_channel(target) {
        Route::Channel(target.to_string())
    } else if target == our {
        Route::Query(from.to_string())
    } else {
        Route::Query(target.to_string())
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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

/// The shortest unique prefix of each name (mirrors rcirc's prefix-tree
/// abbreviation used for the mode-line activity indicator).
fn abbreviate(targets: &[String]) -> Vec<String> {
    (0..targets.len())
        .map(|i| {
            let name = &targets[i];
            let chars: Vec<char> = name.chars().collect();
            for len in 1..=chars.len() {
                let prefix: String = chars[..len].iter().collect();
                if (0..targets.len()).all(|j| j == i || !targets[j].starts_with(&prefix)) {
                    return prefix;
                }
            }
            name.clone()
        })
        .collect()
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

/// Extract an http(s) URL from a whitespace-delimited token.
fn extract_url(token: &str) -> Option<String> {
    let start = token.find("http://").or_else(|| token.find("https://"))?;
    let trimmed = token[start..].trim_end_matches(|c: char| {
        matches!(
            c,
            ')' | '>' | ',' | '.' | ';' | '"' | '\'' | ']' | '}' | '\u{1}'
        )
    });
    (trimmed.len() > "https://".len()).then(|| trimmed.to_string())
}

/// A one-line, length-limited rendering of region text for the `M-w` echo.
fn snippet(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > 40 {
        joined.chars().take(40).collect::<String>() + "…"
    } else {
        joined
    }
}

/// Give a bare host (`example.com`) a scheme so the OS opener treats it as a
/// URL rather than a program/file.
fn normalize_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.contains("://") || trimmed.starts_with("mailto:") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

#[cfg(target_os = "windows")]
fn platform_open(url: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("cmd")
        .args(["/C", "start", "", url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

#[cfg(target_os = "macos")]
fn platform_open(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_open(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
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
    fn abbreviate_shortest_unique_prefixes() {
        let v = vec![
            "#emacs".to_string(),
            "#emacs-dev".to_string(),
            "*scratch*".to_string(),
            "bob".to_string(),
        ];
        assert_eq!(abbreviate(&v), vec!["#emacs", "#emacs-", "*", "b"]);
        assert_eq!(abbreviate(&["#cricr".to_string()]), vec!["#"]);
    }

    #[test]
    fn common_prefix_works() {
        let items = vec!["hello".to_string(), "help".to_string(), "helm".to_string()];
        assert_eq!(common_prefix(&items), "hel");
    }

    fn press(app: &mut App, code: crossterm::event::KeyCode, mods: crossterm::event::KeyModifiers) {
        app.handle_key(crossterm::event::KeyEvent::new(code, mods));
    }

    #[test]
    fn cx_b_opens_switch_prompt() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::NONE);
        assert!(matches!(app.prompt, Prompt::SwitchBuffer { .. }));
        assert!(!app.should_quit);
    }

    #[test]
    fn cc_cb_twice_does_not_quit() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(matches!(app.prompt, Prompt::BrowseUrl { .. }));
        // A second C-c cancels the prompt; the following key does nothing odd.
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(app.prompt, Prompt::None));
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(!app.should_quit, "C-c C-b twice must not quit");
    }

    #[test]
    fn only_cx_cc_quits() {
        use crossterm::event::{KeyCode, KeyModifiers};
        for a in ['c', 'x', 'g', 'b', 'n', 'p'] {
            for b in ['c', 'x', 'g', 'b', ' ', '@'] {
                let mut app = app_without_servers();
                press(&mut app, KeyCode::Char(a), KeyModifiers::CONTROL);
                press(&mut app, KeyCode::Char(b), KeyModifiers::CONTROL);
                if a == 'x' && b == 'c' {
                    assert!(app.should_quit, "C-x C-c should quit");
                } else {
                    assert!(!app.should_quit, "C-{a} C-{b} must not quit");
                }
            }
        }
    }

    #[test]
    fn unbound_key_reports_undefined() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(app
            .message
            .as_deref()
            .unwrap_or("")
            .ends_with("is undefined"));
    }

    #[test]
    fn c_x_then_unbound_reports_undefined() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert_eq!(app.message.as_deref(), Some("C-x <f1> is undefined"));
    }

    #[test]
    fn c_c_c_n_runs_names_without_quitting() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(!app.should_quit);
    }

    #[test]
    fn c_c_c_f_b_inserts_bold_code() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(app.current().buffer.input(), "\u{2}\u{f}");
    }

    #[test]
    fn toggle_ignore_activity_suppresses_activity() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        app.switch_to(a);
        app.toggle_ignore_activity();
        assert!(app.buffers[a].ignore_activity);
        assert!(app.ignore_activity());
        let scratch = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        app.switch_to(scratch);
        app.bump_activity(a, false);
        assert_eq!(app.buffers[a].unread, 0, "ignored buffer gets no activity");
    }

    #[test]
    fn c_c_c_t_prefills_current_topic() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        app.switch_to(a);
        app.buffers[a].topic = Some("hello world".to_string());
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        match &app.prompt {
            Prompt::Topic { input, default } => {
                // The current topic is a default, not inserted editable text.
                assert_eq!(input, "");
                assert_eq!(default, "hello world");
            }
            other => panic!("expected Topic prompt, got {other:?}"),
        }
        // Typing replaces (rather than appends to) the topic.
        for c in "new topic".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.prompt, Prompt::None));
    }

    #[test]
    fn page_down_and_up_move_by_a_screenful() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        for i in 0..50 {
            app.push_line(ch, Line::plain(format!("line {i}")));
        }
        app.switch_to(ch);
        app.viewport_rows = 10; // page = 8 lines
        let bottom = app.current().buffer.point().row;
        app.perform(Action::PageUp);
        assert_eq!(app.current().buffer.point().row, bottom - 8);
        app.perform(Action::PageDown);
        assert_eq!(app.current().buffer.point().row, bottom);
    }

    #[test]
    fn yank_and_yank_pop_cycle_kill_ring() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.kill_ring.push("one", false);
        app.kill_ring.push("two", false);
        app.perform(Action::Yank);
        assert_eq!(app.current().buffer.input(), "two");
        app.perform(Action::YankPop);
        assert_eq!(app.current().buffer.input(), "one");
        app.perform(Action::YankPop);
        assert_eq!(app.current().buffer.input(), "two");
    }

    #[test]
    fn copy_region_flashes_the_other_end() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello world");
        app.current_mut().buffer.set_point(Pos::new(0, 8));
        app.current_mut().buffer.set_mark();
        app.current_mut().buffer.set_point(Pos::new(0, 13));
        app.viewport_rows = 20;
        app.perform(Action::CopyRegion);
        // point was the region end, so it flashes the start
        assert_eq!(app.current().buffer.point(), Pos::new(0, 8));
        assert!(app.copy_flash.is_some());
    }

    #[test]
    fn snippet_is_single_line_and_limited() {
        assert_eq!(snippet("a\nb   c"), "a b c");
        let long = "x".repeat(100);
        assert!(snippet(&long).chars().count() <= 41);
    }

    #[test]
    fn kill_region_and_yank_roundtrip() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello world");
        // select "world"
        app.current_mut().buffer.set_point(Pos::new(0, 8));
        app.current_mut().buffer.set_mark();
        app.current_mut().buffer.set_point(Pos::new(0, 13));
        app.perform(Action::KillRegion);
        assert_eq!(app.current().buffer.input(), "hello ");
        app.perform(Action::Yank);
        assert_eq!(app.current().buffer.input(), "hello world");
    }

    #[tokio::test]
    async fn topic_command_reaches_the_wire() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (rd, mut wr) = stream.into_split();
            let mut rd = BufReader::new(rd);
            let mut buf = String::new();
            loop {
                buf.clear();
                if rd.read_line(&mut buf).await.unwrap() == 0 {
                    break;
                }
                let line = buf.trim_end().to_string();
                let upper = line.to_ascii_uppercase();
                if upper.starts_with("CAP LS") {
                    wr.write_all(b":srv CAP * LS :\r\n").await.unwrap();
                } else if upper.starts_with("CAP END") {
                    wr.write_all(b":srv 001 nick :Welcome\r\n").await.unwrap();
                } else if upper.starts_with("TOPIC ") {
                    assert!(line.contains("TOPIC #test :hello world"), "got: {line}");
                    return;
                }
            }
            panic!("no TOPIC seen");
        });

        let mut cfg = Config::default();
        cfg.servers.push(crate::config::ServerConfig {
            name: "127.0.0.1".to_string(),
            port: Some(addr.port()),
            tls: Some(false),
            nick: Some("nick".to_string()),
            user_name: None,
            full_name: None,
            password: None,
            channels: Vec::new(),
            sasl: None,
            cert: None,
            key: None,
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(cfg, tx);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let ch = app.new_buffer(0, "#test".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("/topic hello world");
        app.submit_input();

        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), server).await;
    }

    #[test]
    fn cx_k_kills_selected_buffer() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        app.new_buffer(0, "#b".to_string(), Kind::Channel);
        app.switch_to(a);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert!(matches!(app.prompt, Prompt::KillBuffer { .. }));
        for c in "#b".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.buffers.iter().all(|x| x.target != "#b"));
        assert!(!app.should_quit);
    }

    #[test]
    fn kill_scratch_is_refused() {
        let mut app = app_without_servers();
        let s = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        let before = app.buffers.len();
        app.kill_buffer(s);
        assert_eq!(app.buffers.len(), before);
        assert!(app.message.as_deref().unwrap().contains("Cannot kill"));
    }

    #[test]
    fn part_of_killed_channel_does_not_recreate_it() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        app.kill_buffer(a);
        app.handle_event(
            0,
            Event::Part {
                channel: "#a".to_string(),
                nick: "me".to_string(),
                reason: String::new(),
            },
        );
        assert!(app.buffers.iter().all(|b| b.target != "#a"));
    }

    #[test]
    fn channel_numeric_routes_to_channel_and_records_activity() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.handle_event(
            0,
            Event::ServerText {
                command: "482".to_string(),
                target: Some("#c".to_string()),
                text: "#c :You're not channel operator".to_string(),
            },
        );
        let text: Vec<String> = (0..app.buffers[ch].buffer.len())
            .map(|r| app.buffers[ch].buffer.line(r).text())
            .collect();
        assert!(text.iter().any(|l| l.contains("482")));
        assert_eq!(app.buffers[ch].unread, 1, "activity recorded");
    }

    #[test]
    fn issued_command_is_echoed_in_current_buffer() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("/topic blah");
        app.submit_input();
        let text: Vec<String> = (0..app.buffers[ch].buffer.len())
            .map(|r| app.buffers[ch].buffer.line(r).text())
            .collect();
        assert!(text.iter().any(|l| l.contains(">>> /topic blah")));
    }

    #[tokio::test]
    async fn own_join_records_activity() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let mut cfg = Config::default();
        cfg.servers.push(crate::config::ServerConfig {
            name: "127.0.0.1".to_string(),
            port: Some(addr.port()),
            tls: Some(false),
            nick: Some("me".to_string()),
            user_name: None,
            full_name: None,
            password: None,
            channels: Vec::new(),
            sasl: None,
            cert: None,
            key: None,
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(cfg, tx);

        app.handle_event(
            0,
            Event::Join {
                channel: "#new".to_string(),
                nick: "me".to_string(),
            },
        );
        let idx = app
            .buffers
            .iter()
            .position(|b| b.target == "#new")
            .expect("channel buffer created");
        assert_eq!(app.buffers[idx].unread, 1, "own join records activity");
        assert!(!app.activity_entries().is_empty());
    }

    #[tokio::test]
    async fn user_mode_goes_to_server_buffer() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let mut cfg = Config::default();
        cfg.servers.push(crate::config::ServerConfig {
            name: "127.0.0.1".to_string(),
            port: Some(addr.port()),
            tls: Some(false),
            nick: Some("me".to_string()),
            user_name: None,
            full_name: None,
            password: None,
            channels: Vec::new(),
            sasl: None,
            cert: None,
            key: None,
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(cfg, tx);
        let server_buf = app.servers[0].buffer;
        let before = app.buffers.len();

        app.handle_event(
            0,
            Event::Mode {
                target: "me".to_string(),
                modes: "+Ziw".to_string(),
            },
        );

        assert_eq!(
            app.buffers.len(),
            before,
            "user mode must not create a buffer"
        );
        let text: Vec<String> = (0..app.buffers[server_buf].buffer.len())
            .map(|r| app.buffers[server_buf].buffer.line(r).text())
            .collect();
        assert!(text.iter().any(|l| l.contains("+Ziw")));
    }

    #[tokio::test]
    async fn own_nick_change_records_activity() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let mut cfg = Config::default();
        cfg.servers.push(crate::config::ServerConfig {
            name: "127.0.0.1".to_string(),
            port: Some(addr.port()),
            tls: Some(false),
            nick: Some("me".to_string()),
            user_name: None,
            full_name: None,
            password: None,
            channels: Vec::new(),
            sasl: None,
            cert: None,
            key: None,
        });
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(cfg, tx);
        // Look at a channel so the server buffer is not current.
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        let server_buf = app.servers[0].buffer;

        app.handle_event(
            0,
            Event::Nick {
                old: "me".to_string(),
                new: "me2".to_string(),
            },
        );

        assert_eq!(app.servers[0].our_nick, "me2");
        assert_eq!(
            app.buffers[server_buf].unread, 1,
            "own /nick records activity"
        );
    }

    #[test]
    fn paste_folds_newlines_into_irc_input() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.handle_paste("hello\r\nworld".to_string());
        assert_eq!(app.current().buffer.input(), "hello world");
    }

    #[test]
    fn paste_into_scratch_keeps_newlines() {
        let mut app = app_without_servers();
        app.handle_paste("a\nb".to_string());
        assert_eq!(app.current().buffer.line(0).text(), "a");
        assert_eq!(app.current().buffer.line(1).text(), "b");
    }

    #[test]
    fn activity_inserts_read_marker_and_switch_removes_it() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        app.push_line(a, Line::plain("hello"));
        app.bump_activity(a, false);

        assert!(app.buffers[a].marker_row.is_some());
        let has_marker = (0..app.buffers[a].buffer.len())
            .any(|r| app.buffers[a].buffer.line(r).text().starts_with("---"));
        assert!(has_marker, "separator line inserted");
        // first unread sits just below the marker
        assert_eq!(
            app.buffers[a].first_unread,
            app.buffers[a].marker_row.map(|r| r + 1)
        );

        // visiting the buffer keeps the marker visible
        app.switch_to(a);
        assert!(app.buffers[a].marker_row.is_some());

        // leaving the buffer removes it
        let scratch = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        app.switch_to(scratch);
        assert!(app.buffers[a].marker_row.is_none());
        let still = (0..app.buffers[a].buffer.len())
            .any(|r| app.buffers[a].buffer.line(r).text().starts_with("---"));
        assert!(!still, "separator removed on leaving");
    }

    #[test]
    fn nick_prefixes_dedupe() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.add_nick(ch, "crictest");
        app.add_nick(ch, "@crictest");
        assert_eq!(app.buffers[ch].nicks, vec!["@crictest"]);
        app.remove_nick(ch, "crictest");
        assert!(app.buffers[ch].nicks.is_empty());
    }

    #[test]
    fn disconnect_broadcasts_to_each_buffer() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        let b = app.new_buffer(0, "#b".to_string(), Kind::Query);
        let scratch = app
            .buffers
            .iter()
            .position(|x| x.kind == Kind::Scratch)
            .unwrap();
        let before_a = app.buffers[a].buffer.len();
        let before_b = app.buffers[b].buffer.len();
        let before_scratch = app.buffers[scratch].buffer.len();

        app.broadcast_error(0, "connection closed");

        assert!(app.buffers[a].buffer.len() > before_a);
        assert!(app.buffers[b].buffer.len() > before_b);
        assert_eq!(app.buffers[scratch].buffer.len(), before_scratch);
        let text: Vec<String> = (0..app.buffers[a].buffer.len())
            .map(|r| app.buffers[a].buffer.line(r).text())
            .collect();
        assert!(text
            .iter()
            .any(|l| l.contains("!!!") && l.contains("connection closed")));
    }

    #[test]
    fn long_messages_wrap_on_push() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.fill_column = 20;
        let before = app.buffers[ch].buffer.len();
        app.push_line(ch, Line::plain("x".repeat(100)));
        assert!(app.buffers[ch].buffer.len() > before + 1);
        for row in 0..app.buffers[ch].buffer.len() {
            assert!(app.buffers[ch].buffer.line(row).char_len() <= 20);
        }
    }

    #[test]
    fn server_targets_fold_into_server_buffer() {
        let s = "irc.libera.chat";
        // registration notices: NOTICE * :***
        assert_eq!(classify(s, "*", "me", s), Route::Server);
        // a service notice addressed to us is a query with the service
        assert_eq!(
            classify("NickServ", "me", "me", s),
            Route::Query("NickServ".into())
        );
        // channel traffic
        assert_eq!(
            classify("bob", "#rust", "me", s),
            Route::Channel("#rust".into())
        );
        // a private message from another user
        assert_eq!(classify("bob", "me", "me", s), Route::Query("bob".into()));
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

    fn app_without_servers() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(Config::default(), tx)
    }

    #[test]
    fn activity_string_is_most_recent_first() {
        let mut app = app_without_servers();
        assert_eq!(app.mode_line_mode(), "Fundamental");
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        let b = app.new_buffer(0, "#b".to_string(), Kind::Query);
        app.bump_activity(a, false);
        app.bump_activity(b, true);
        assert_eq!(app.activity_string(), "[#b,#a]");
        app.switch_to(a);
        assert_eq!(app.activity_string(), "[#b]");
        // b was a mention, so its track style is the (reverse) nick face
        assert!(app.activity_entries()[0].1.reverse);
    }

    #[test]
    fn next_active_prefers_most_recent_then_scratch() {
        let mut app = app_without_servers();
        let scratch = app.current;
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        let b = app.new_buffer(0, "#b".to_string(), Kind::Channel);
        app.bump_activity(a, false);
        app.bump_activity(b, false);
        app.next_active();
        assert_eq!(app.current, b, "should jump to the most recent activity");
        app.next_active();
        assert_eq!(app.current, a, "then the next-most-recent");
        app.next_active();
        assert_eq!(app.current, scratch, "no activity left -> *scratch*");
    }

    #[test]
    fn switch_buffer_prompt_resolves_names() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        app.prompt = Prompt::SwitchBuffer {
            input: "#a".to_string(),
            selected: 0,
        };
        app.commit_prompt();
        assert_eq!(app.current, a);
        assert!(matches!(app.prompt, Prompt::None));
    }

    #[test]
    fn switch_candidates_are_substring_and_recent_first() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#alpha".to_string(), Kind::Channel);
        let b = app.new_buffer(0, "#beta".to_string(), Kind::Channel);
        // visiting b last makes it most recent
        app.switch_to(a);
        app.switch_to(b);
        app.prompt = Prompt::SwitchBuffer {
            input: String::new(),
            selected: 0,
        };
        let names: Vec<String> = app
            .switch_candidates("")
            .into_iter()
            .map(|i| app.buffers[i].target.clone())
            .collect();
        // current is #beta (excluded); recency order: #alpha then *scratch*
        assert_eq!(names, vec!["#alpha", "*scratch*"]);
        assert_eq!(app.switch_candidates("alp"), vec![a]);
    }

    #[test]
    fn nick_and_url_spans_are_marked() {
        let app = app_without_servers();
        let mut mentioned = false;
        let spans = app.nick_spans("hey crictest!", "crictest", &mut mentioned);
        assert!(mentioned);
        assert!(spans
            .iter()
            .any(|s| s.text == "crictest" && s.style == app.theme.nick_in_message));

        let mut mentioned = false;
        let url_spans = app.token_spans("see https://example.com/x.", "crictest", &mut mentioned);
        assert!(url_spans
            .iter()
            .any(|s| s.style.bold && s.text == "https://example.com/x"));
    }

    #[test]
    fn normalize_url_adds_scheme() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("  example.com/x  "), "https://example.com/x");
        assert_eq!(normalize_url("http://x"), "http://x");
        assert_eq!(normalize_url("mailto:a@b"), "mailto:a@b");
    }

    #[test]
    fn extract_url_trims_punctuation() {
        assert_eq!(
            extract_url("see (https://example.com/x).").as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(extract_url("http://a.b"), Some("http://a.b".to_string()));
        assert_eq!(extract_url("not-a-url"), None);
    }
}
