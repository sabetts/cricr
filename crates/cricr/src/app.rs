//! Application state and the event loop.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Datelike, Local, TimeZone};
use crossterm::event::{Event as TermEvent, KeyEventKind};
use ratatui::backend::Backend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use cricr_irc::commands::{self, LocalCommand, ParsedInput};
use cricr_irc::message::Tag;
use cricr_irc::{ClientCommand, ClientConfig, ClientHandle, Event};

use crate::buffer::{
    capitalize_text, transpose_chars_at, transpose_words_at, Buffer, Color, Line, Pos, Span, Style,
};
use crate::config::Config;
use crate::history;
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
    /// The `*Help*` buffer (also a free text buffer, but killable).
    Help,
}

/// The minibuffer prompt, if one is active.
#[derive(Debug, Clone)]
pub enum Prompt {
    None,
    SwitchBuffer {
        input: String,
        /// Cursor position within `input`, as a char index.
        cursor: usize,
        selected: usize,
    },
    BrowseUrl {
        input: String,
        cursor: usize,
        selected: usize,
    },
    /// Set the channel topic; the current topic is shown as a default
    /// (`C-c C-t`).
    Topic {
        input: String,
        cursor: usize,
        default: String,
    },
    /// Kill (and part from) a buffer chosen from a list (`C-x k`).
    KillBuffer {
        input: String,
        cursor: usize,
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
    Help,
}

/// A numeric argument being built with `C-u`, `M-<digit>`, or `M--`
/// (Emacs's universal argument).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArgState {
    /// Unsigned magnitude.
    magnitude: u32,
    /// `+1` or `-1`.
    sign: i32,
    /// Whether a digit has been typed (so further digits append rather than
    /// replace the initial `C-u`/`M--` value).
    digits: bool,
}

impl ArgState {
    /// The signed value, clamped to `i32`.
    fn value(self) -> i32 {
        let mag = self.magnitude.min(i32::MAX as u32) as i32;
        self.sign * mag
    }
}

/// The kind of the previous editing command, for kill/yank chaining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LastCommand {
    #[default]
    Other,
    Kill,
    Yank,
}

/// State for cycling through `M-/` (dabbrev) expansions.
#[derive(Debug, Clone, Default)]
struct Dabbrev {
    expansions: Vec<String>,
    index: usize,
}

/// One connection's shared state.
pub struct ServerState {
    pub name: String,
    pub handle: ClientHandle,
    pub our_nick: String,
    pub connected: bool,
    pub buffer: usize,
    /// Template used to (re)spawn the connection (channels are refreshed from
    /// `channels` on each attempt).
    client_config: ClientConfig,
    /// Channels we are currently joined to, and thus rejoin after a reconnect.
    channels: BTreeSet<String>,
    /// Failed reconnect attempts since the last successful connection.
    attempt: u32,
    /// When the next reconnect attempt is due, if one is scheduled.
    next_at: Option<Instant>,
}

/// One entry in a buffer's logical (unwrapped) scrollback log, used by the
/// on-disk history cache.
#[derive(Debug, Clone)]
pub struct HistoryLine {
    pub time: i64,
    pub line: Line,
}

/// One buffer (target).
pub struct Chan {
    pub server: usize,
    pub target: String,
    pub kind: Kind,
    pub buffer: Buffer,
    /// Unwrapped logical lines not yet written to the history cache
    /// (channel/query only). The on-disk cache is append-only and grows
    /// without bound; this only holds the lines awaiting the next flush.
    pub history: Vec<HistoryLine>,
    /// Whether `history` has lines not yet written to disk.
    pub history_dirty: bool,
    pub nicks: Vec<String>,
    pub unread: usize,
    /// Whether our nick was mentioned since the last visit.
    pub mentioned: bool,
    /// Row of the first unread line since the last visit.
    pub first_unread: Option<usize>,
    /// Row of the inserted "read up to here" separator line, if present.
    pub marker_row: Option<usize>,
    /// Set when the unread separator was created/moved by the idle rule while
    /// this buffer was the one being viewed (so the terminal title badges it).
    idle_marked: bool,
    /// When set, activity in this buffer is not tracked (`C-c C-i`).
    pub ignore_activity: bool,
    /// Monotonic stamp of the most recent activity, for mode-line ordering.
    pub last_activity: u64,
    /// When this buffer last saw activity, for the idle-based unread marker.
    last_activity_at: Instant,
    /// Monotonic stamp of the most recent visit, for buffer-list ordering.
    pub last_visited: u64,
    /// Last timestamp printed, so repeats can be blanked.
    pub last_timestamp: String,
    /// Year*12 + month of the last line seen, for month date separators.
    last_month: Option<i32>,
    pub topic: Option<String>,
}

/// A quiet spell after which activity in the buffer you are already viewing
/// still gets an unread separator, so returning to the terminal shows where
/// output resumed.
const IDLE_MARK: Duration = Duration::from_secs(5 * 60);

/// How long a detected paste keeps folding newlines. On Windows bracketed
/// paste is unavailable, so a pasted newline arrives as a plain Enter key
/// press; a burst that carries a newline *followed by more input* is a paste,
/// and this latch keeps a paste split across drains folding.
const PASTE_LATCH: Duration = Duration::from_millis(30);

/// How long to wait before echoing an incomplete prefix key (`C-c-`), like
/// Emacs's `echo-keystrokes` (default one second).
const PREFIX_ECHO_DELAY: Duration = Duration::from_secs(1);

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
    "/nickserv",
    "/ns",
    "/whois",
    "/who",
    "/names",
    "/away",
    "/quote",
    "/close",
    "/clear",
    "/disconnect",
    "/reconnect",
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
    /// Candidates from `dabbrev-completion`, shown until the next keystroke.
    pub completions: Option<Vec<String>>,
    pub kill_ring: KillRing,
    /// Height (in rows) of the buffer view, from the last draw.
    pub viewport_rows: usize,
    /// Quiet spell after which activity in the current buffer is marked unread.
    idle_mark: Duration,
    /// In-progress `M-/` (dabbrev) expansion, for cycling.
    dabbrev: Option<Dabbrev>,
    /// While set, key events are treated as part of an in-progress paste.
    pasting_until: Option<Instant>,
    /// Whether the key currently being handled arrived in a paste burst.
    rapid_key: bool,
    pub scrollback: usize,
    pub fill_column: usize,
    /// Whether kills/copies are written to the system clipboard (OSC 52).
    pub clipboard: bool,
    pub theme: Theme,
    /// Colors used to draw the point, independent of the text under it.
    pub cursor_style: Style,
    pub should_quit: bool,
    pub prompt: Prompt,
    /// Height of the minibuffer during the current prompt, never shrinking as
    /// the candidate list narrows (extra rows are padded with blank lines).
    minibuffer_rows: u16,
    previous: usize,
    activity_seq: u64,
    visit_seq: u64,
    /// Buffer the current `C-c C-SPC` run started from, so the run can return
    /// there once every activity buffer has been visited.
    next_active_origin: Option<usize>,
    /// Set while `next_active` is switching, so `switch_to` knows not to end
    /// the run.
    in_next_active: bool,
    prefix: Prefix,
    /// A numeric argument being built (Emacs universal argument), if any.
    numeric_arg: Option<ArgState>,
    /// A pending prefix key hint (`C-c-`), shown after an idle delay the way
    /// Emacs's `echo-keystrokes` does.
    prefix_echo: Option<(String, Instant)>,
    quoted: bool,
    last_command: LastCommand,
    /// (start, end) positions of the text inserted by the last `yank`.
    last_yank: Option<(Pos, Pos)>,
    /// Pending `M-w` indication: (position to restore, deadline).
    copy_flash: Option<(Pos, Instant)>,
    /// Row of the first line inserted by the most recent `push_line`.
    last_push_row: usize,
    /// Scrollback cache directory (`None` when history caching is disabled).
    history_dir: Option<PathBuf>,
    /// The terminal title last written, so it is only emitted when it changes.
    last_title: String,
    event_tx: mpsc::UnboundedSender<(usize, Event)>,
}

impl App {
    pub fn new(config: Config, event_tx: mpsc::UnboundedSender<(usize, Event)>) -> Self {
        let scrollback = config.ui.scrollback_lines;
        let fill_column = config.ui.fill_column;
        let clipboard = config.ui.clipboard;
        let theme = Theme::for_background(config.ui.background);
        let cursor_style = Style::fg(Color::parse(&config.ui.cursor_fg).unwrap_or(Color::Black))
            .bg(Color::parse(&config.ui.cursor_bg).unwrap_or(Color::Yellow));
        let history_dir = history::dir(&config);
        let mut app = App {
            config,
            servers: Vec::new(),
            buffers: Vec::new(),
            current: 0,
            history: History::default(),
            search: Search::default(),
            message: None,
            completions: None,
            kill_ring: KillRing::new(),
            viewport_rows: 20,
            idle_mark: IDLE_MARK,
            dabbrev: None,
            pasting_until: None,
            rapid_key: false,
            scrollback,
            fill_column,
            clipboard,
            theme,
            cursor_style,
            should_quit: false,
            prompt: Prompt::None,
            minibuffer_rows: 1,
            previous: 0,
            activity_seq: 0,
            visit_seq: 0,
            next_active_origin: None,
            in_next_active: false,
            prefix: Prefix::None,
            numeric_arg: None,
            prefix_echo: None,
            quoted: false,
            last_command: LastCommand::Other,
            last_yank: None,
            copy_flash: None,
            last_push_row: 0,
            history_dir,
            last_title: String::new(),
            event_tx,
        };
        app.connect_all();
        // Emacs always has a *scratch* buffer; start there unless a server
        // buffer exists so connection output is visible immediately.
        let scratch = app.new_buffer(0, "*scratch*".to_string(), Kind::Scratch);
        app.buffers[scratch]
            .buffer
            .set_content(scratch_lines().into_iter().map(Line::plain).collect());
        app.current = app.servers.first().map(|s| s.buffer).unwrap_or(scratch);
        app.previous = scratch;
        app
    }

    fn connect_all(&mut self) {
        let servers = self.config.servers.clone();
        for server in &servers {
            match self.config.client_config(server) {
                Ok(client_config) => {
                    let index = self.servers.len();
                    let buf = self.new_buffer(index, server.name.clone(), Kind::Server);
                    let channels = client_config.channels.iter().cloned().collect();
                    let handle = self.spawn_client_events(index, client_config.clone());
                    self.servers.push(ServerState {
                        name: server.name.clone(),
                        handle,
                        our_nick: server
                            .nick
                            .clone()
                            .unwrap_or_else(|| self.config.defaults.nick.clone()),
                        connected: false,
                        buffer: buf,
                        client_config,
                        channels,
                        attempt: 0,
                        next_at: None,
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

    /// Spawn a connection task for `config` and forward its events under
    /// `server`.
    fn spawn_client_events(&self, server: usize, config: ClientConfig) -> ClientHandle {
        let (handle, rx) = cricr_irc::spawn(config);
        let forward = self.event_tx.clone();
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(event) = rx.recv().await {
                let _ = forward.send((server, event));
            }
        });
        handle
    }

    /// (Re)connect `server`, rejoining the channels we were in.
    fn spawn_connection(&mut self, server: usize) {
        let mut config = self.servers[server].client_config.clone();
        config.channels = self.servers[server].channels.iter().cloned().collect();
        let handle = self.spawn_client_events(server, config);
        self.servers[server].handle = handle;
        self.servers[server].connected = false;
        // Membership from the previous session is stale; NAMES repopulates it.
        for buf in self
            .buffers
            .iter_mut()
            .filter(|b| b.server == server && b.kind == Kind::Channel)
        {
            buf.nicks.clear();
        }
    }

    /// Schedule the next reconnect attempt for `server`, with exponential
    /// backoff, and report it in every buffer of the server.
    fn schedule_reconnect(&mut self, server: usize) {
        if !self.config.reconnect.enabled {
            return;
        }
        let attempt = self.servers[server].attempt + 1;
        self.servers[server].attempt = attempt;
        let delay = reconnect_delay(
            attempt,
            self.config.reconnect.initial_secs,
            self.config.reconnect.max_secs,
        );
        self.servers[server].next_at = Some(Instant::now() + Duration::from_secs_f64(delay));
        let name = self.servers[server].name.clone();
        let secs = (delay.round() as u64).max(1);
        let text = format!("Reconnecting to {name} in {secs}s (attempt {attempt})");
        self.broadcast_message(server, &text);
    }

    /// Earliest scheduled reconnect deadline across all servers, if any.
    fn next_reconnect_deadline(&self) -> Option<Instant> {
        self.servers.iter().filter_map(|s| s.next_at).min()
    }

    /// Start any reconnect attempts whose deadline has passed.
    fn run_due_reconnects(&mut self) {
        let now = Instant::now();
        let due: Vec<usize> = self
            .servers
            .iter()
            .enumerate()
            .filter(|(_, s)| s.next_at.is_some_and(|t| t <= now))
            .map(|(i, _)| i)
            .collect();
        for server in due {
            self.servers[server].next_at = None;
            self.spawn_connection(server);
        }
    }

    fn new_buffer(&mut self, server: usize, target: String, kind: Kind) -> usize {
        let mut buffer = if matches!(kind, Kind::Scratch | Kind::Help) {
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
            history: Vec::new(),
            history_dirty: false,
            nicks: Vec::new(),
            unread: 0,
            mentioned: false,
            first_unread: None,
            marker_row: None,
            idle_marked: false,
            ignore_activity: false,
            last_activity: 0,
            last_activity_at: Instant::now(),
            last_visited: 0,
            last_timestamp: String::new(),
            last_month: None,
            topic: None,
        });
        let idx = self.buffers.len() - 1;
        self.load_cached_history(idx);
        idx
    }

    /// Preload a channel/query buffer from the history cache, if enabled.
    fn load_cached_history(&mut self, idx: usize) {
        let Some(dir) = self.history_dir.clone() else {
            return;
        };
        if !matches!(self.buffers[idx].kind, Kind::Channel | Kind::Query) {
            return;
        }
        let Some(server) = self.servers.get(self.buffers[idx].server) else {
            return;
        };
        let server = server.name.clone();
        let target = self.buffers[idx].target.clone();
        // Only the visible tail is needed for display; the file keeps growing.
        let entries = history::load(&dir, &server, &target, self.scrollback.max(1));
        if entries.is_empty() {
            return;
        }
        // Render/re-wrap in order so the timestamp column dedups as it did live.
        self.buffers[idx].last_timestamp.clear();
        self.buffers[idx].last_month = None;
        let mut wrapped: Vec<Line> = Vec::new();
        for (time, body) in entries {
            if let Some(marker) = self.take_date_marker(idx, time) {
                wrapped.push(marker);
            }
            let line = self.render_log_line(idx, time, &body);
            wrapped.extend(line.wrap(self.fill_column));
        }
        let had_history = !wrapped.is_empty();
        self.buffers[idx].buffer.load_scrollback(wrapped);
        // Cap the visible scrollback (the cache itself is not truncated).
        self.trim_scrollback(idx);
        if had_history {
            // Separate the preloaded history from anything that arrives next,
            // so it's clear where the cached history ends.
            let row = self.buffers[idx].buffer.last_row();
            let marker = self.marker_line();
            self.buffers[idx].buffer.insert_line(row, marker);
            self.buffers[idx].marker_row = Some(row);
            self.buffers[idx].first_unread = Some(row + 1);
        }
    }

    /// Write every dirty channel/query buffer's scrollback to disk.
    fn save_all_history(&mut self) {
        let Some(dir) = self.history_dir.clone() else {
            return;
        };
        let indices: Vec<usize> = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, b)| b.history_dirty && matches!(b.kind, Kind::Channel | Kind::Query))
            .map(|(i, _)| i)
            .collect();
        for idx in indices {
            self.save_history_buffer(&dir, idx);
        }
    }

    fn save_history_buffer(&mut self, dir: &Path, idx: usize) {
        let Some(server) = self
            .servers
            .get(self.buffers[idx].server)
            .map(|s| s.name.clone())
        else {
            return;
        };
        let target = self.buffers[idx].target.clone();
        let entries: Vec<history::Entry> = self.buffers[idx]
            .history
            .iter()
            .map(|h| (h.time, line_body(&h.line)))
            .collect();
        // The cache is append-only, so only the lines added since the last
        // flush need writing; they are then dropped from memory.
        match history::append(dir, &server, &target, &entries) {
            Ok(()) => {
                self.buffers[idx].history.clear();
                self.buffers[idx].history_dirty = false;
            }
            Err(err) => {
                // Leave the lines (and the dirty flag) so the next flush retries.
                self.message = Some(format!("Could not write history for {target}: {err}"));
            }
        }
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
        self.servers
            .get(server)
            .map(|s| s.our_nick.clone())
            .unwrap_or_default()
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
        // Record the unwrapped logical line for the history cache before it is
        // split into physical (wrapped) lines.
        if self.history_dir.is_some()
            && matches!(self.buffers[buf].kind, Kind::Channel | Kind::Query)
        {
            let time = line.time.unwrap_or_else(now_epoch);
            self.buffers[buf].history.push(HistoryLine {
                time,
                line: line.clone(),
            });
            self.buffers[buf].history_dirty = true;
        }
        // A date separator when the month changes (channel/query scrollback).
        if let Some(time) = line.time {
            if matches!(self.buffers[buf].kind, Kind::Channel | Kind::Query) {
                if let Some(marker) = self.take_date_marker(buf, time) {
                    self.buffers[buf].buffer.push_line(marker);
                }
            }
        }
        self.last_push_row = self.buffers[buf].buffer.len().saturating_sub(1);
        for wrapped in line.wrap(self.fill_column) {
            self.buffers[buf].buffer.push_line(wrapped);
        }
        self.trim_scrollback(buf);
    }

    /// Record `time` as the buffer's latest month and return a date separator
    /// line when it differs from the previous one. The first line of a buffer
    /// gets no separator (only month *changes* are marked).
    fn take_date_marker(&mut self, buf: usize, time: i64) -> Option<Line> {
        let month = month_index(time);
        let marker = match self.buffers[buf].last_month {
            None => None,
            Some(prev) if prev == month => None,
            Some(_) => Some(self.date_marker_line(time)),
        };
        self.buffers[buf].last_month = Some(month);
        marker
    }

    /// A `Apr 2026 ------` separator for the month containing `time`.
    fn date_marker_line(&self, time: i64) -> Line {
        let label = format_local(time, "%b %Y");
        let dashes = self
            .fill_column
            .saturating_sub(label.chars().count() + 1)
            .max(1);
        Line::styled(
            format!("{label} {}", "-".repeat(dashes)),
            Style::fg(Color::DarkGray),
        )
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
            // The line just pushed (recorded in `last_push_row` before the
            // trim) shifted up with everything else.
            self.last_push_row = self.last_push_row.saturating_sub(excess);
        }
    }

    fn bump_activity(&mut self, buf: usize, mentioned: bool) {
        let current = buf == self.current;
        let idle = self.buffers[buf].last_activity_at.elapsed() >= self.idle_mark;
        let ignored = matches!(self.buffers[buf].kind, Kind::Scratch | Kind::Help)
            || self.buffers[buf].ignore_activity;
        // Activity in another buffer always counts as unread. Activity in the
        // buffer we are already viewing only counts after a quiet spell, so
        // returning to the terminal shows where output resumed (no mode-line
        // flag for the current buffer).
        if !ignored && (!current || idle) {
            if current {
                // Idle activity in the buffer we are viewing: drop the
                // separator before the first message of the quiet spell rather
                // than growing the unread region. Once it is showing, later
                // output in the same spell grows the unread region below it
                // instead of dragging the separator forward; the user
                // acknowledging the buffer (or leaving it) clears `idle_marked`,
                // which lets a fresh spell move the separator again.
                if !self.buffers[buf].idle_marked {
                    let mut row = self.last_push_row;
                    if let Some(old) = self.buffers[buf].marker_row {
                        // Removing a separator above the new line shifts it up.
                        if old < row {
                            row -= 1;
                        }
                        self.remove_marker(buf);
                    }
                    self.insert_marker(buf, row);
                    self.buffers[buf].idle_marked = true;
                }
            } else {
                self.activity_seq += 1;
                self.buffers[buf].unread += 1;
                self.buffers[buf].last_activity = self.activity_seq;
                if mentioned {
                    self.buffers[buf].mentioned = true;
                }
                // Insert a separator only when one isn't already showing. Gate
                // on `marker_row` (the separator actually inserted), not
                // `first_unread`, which persists after the separator is removed
                // so `C-c C-a` can still find where unread began.
                if self.buffers[buf].marker_row.is_none() {
                    let row = self.last_push_row;
                    self.insert_marker(buf, row);
                }
            }
        }
        self.buffers[buf].last_activity_at = Instant::now();
    }

    /// Insert a "read up to here" separator at `row` and record it.
    fn insert_marker(&mut self, buf: usize, row: usize) {
        let marker = self.marker_line();
        self.buffers[buf].buffer.insert_line(row, marker);
        self.buffers[buf].marker_row = Some(row);
        self.buffers[buf].first_unread = Some(row + 1);
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
        self.buffers[buf].idle_marked = false;
    }

    fn switch_to(&mut self, idx: usize) {
        if idx >= self.buffers.len() {
            return;
        }
        if !self.in_next_active {
            // Switching any other way ends a `C-c C-SPC` run.
            self.next_active_origin = None;
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
        // Follow the newest output only if we were already at the end there;
        // otherwise leave point where the user last had it.
        if self.buffers[idx].buffer.at_end() {
            self.buffers[idx].buffer.end_of_buffer();
        }
    }

    fn scratch_index(&self) -> Option<usize> {
        self.buffers.iter().position(|b| b.kind == Kind::Scratch)
    }

    /// Switch to the buffer with the most recent activity (mirrors
    /// `rcirc-next-active-buffer`). A run of consecutive `C-c C-SPC` presses
    /// remembers where it began and, once every activity buffer has been
    /// visited, returns there instead of falling through to `*scratch*`. A
    /// lone press with nothing to visit still goes to `*scratch*`.
    fn next_active(&mut self) {
        let target = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(i, b)| *i != self.current && b.unread > 0)
            .max_by_key(|(_, b)| b.last_activity)
            .map(|(i, _)| i);
        match (self.next_active_origin, target) {
            // Mid-run: hop to the next buffer with activity.
            (Some(_), Some(i)) => self.switch_active(i),
            // Run exhausted: go back to where it started.
            (Some(origin), None) => {
                self.next_active_origin = None;
                if origin != self.current && origin < self.buffers.len() {
                    self.switch_active(origin);
                }
            }
            // Starting a run: remember the origin and jump.
            (None, Some(i)) => {
                self.next_active_origin = Some(self.current);
                self.switch_active(i);
            }
            // Nothing to visit and no run: fall back to `*scratch*`.
            (None, None) => {
                if let Some(s) = self.scratch_index() {
                    self.switch_active(s);
                }
            }
        }
    }

    /// Switch buffers as part of a `next_active` run (see `in_next_active`).
    fn switch_active(&mut self, idx: usize) {
        self.in_next_active = true;
        self.switch_to(idx);
        self.in_next_active = false;
    }

    /// Targets with pending activity, most recent first, with the mode-line
    /// track face for each (`rcirc-track-keyword`, or `rcirc-track-nick` when
    /// our nick was mentioned).
    pub fn activity_entries(&self) -> Vec<(String, Style)> {
        let targets: Vec<String> = self.buffers.iter().map(|b| b.target.clone()).collect();
        let short = abbreviate(&targets);
        self.active_indices(false)
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

    /// Indices of buffers with pending activity, most recent first. When
    /// `idle` is set, a buffer whose unread separator was created/moved by the
    /// idle rule while being viewed also counts (for the terminal title, not
    /// the mode line).
    fn active_indices(&self, idle: bool) -> Vec<usize> {
        let mut active: Vec<usize> = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, b)| b.unread > 0 || (idle && b.idle_marked))
            .map(|(i, _)| i)
            .collect();
        active.sort_by_key(|&i| std::cmp::Reverse(self.buffers[i].last_activity));
        active
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

    /// The terminal title: the app name, with the buffers that have pending
    /// activity appended (e.g. `cricr [bob,#emacs]`). Unlike the mode line this
    /// also lists the buffer being viewed when it has an unread separator, so
    /// the terminal tab shows activity you may not be looking at.
    fn terminal_title(&self) -> String {
        let targets: Vec<String> = self.buffers.iter().map(|b| b.target.clone()).collect();
        let short = abbreviate(&targets);
        let active = self.active_indices(true);
        if active.is_empty() {
            return "cricr".to_string();
        }
        let shown: Vec<&str> = active.iter().take(4).map(|&i| short[i].as_str()).collect();
        let mut title = format!("cricr [{}", shown.join(","));
        if active.len() > shown.len() {
            title.push_str(&format!(" +{}", active.len() - shown.len()));
        }
        title.push(']');
        title
    }

    /// Emit the terminal title when the activity badge changes (`[ui]
    /// title_badge`).
    fn sync_title(&mut self) {
        if !self.config.ui.title_badge {
            return;
        }
        let title = self.terminal_title();
        if title == self.last_title {
            return;
        }
        self.write_title(&title);
        self.last_title = title;
    }

    /// Reset the terminal title on exit, leaving it for the shell to reclaim.
    fn reset_title(&mut self) {
        if !self.config.ui.title_badge {
            return;
        }
        self.write_title("");
        self.last_title.clear();
    }

    fn write_title(&self, title: &str) {
        let mut out = std::io::stdout();
        let _ = std::io::Write::write_all(&mut out, title_sequence(title).as_bytes());
        let _ = std::io::Write::flush(&mut out);
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
            Kind::Help => "Help".to_string(),
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
                    // Show the sent message in `t`'s own buffer, creating a
                    // query buffer for a nick (as rcirc's `rcirc-send-message`
                    // does). The current buffer is left unchanged.
                    let idx = if *t == target {
                        buf
                    } else {
                        let kind = if is_channel(t) {
                            Kind::Channel
                        } else {
                            Kind::Query
                        };
                        self.ensure_buffer(server, t.clone(), kind)
                    };
                    let nick = self.our_nick(server);
                    let line = if let Some(action) = action_body(text) {
                        self.action_line(server, idx, &[], &nick, action)
                    } else {
                        self.chat_line(server, idx, &[], &nick, text)
                    };
                    self.push_line(idx, line);
                    // Sending to a buffer other than the current one is
                    // activity there, just like any other printed message.
                    self.bump_activity(idx, false);
                    echoed = true;
                }
                // Echo commands (e.g. `/topic`) so they don't just vanish.
                if !echoed {
                    let line = self.command_echo_line(buf, &input);
                    self.push_line(buf, line);
                }
                // `/join`: jump straight to the channel buffer (creating it if
                // needed, which also preloads any cached history).
                let joining = match &cmd {
                    ClientCommand::Join { channel, .. } => Some(channel.clone()),
                    _ => None,
                };
                let quitting = matches!(cmd, ClientCommand::Quit { .. });
                self.send(server, cmd);
                if let Some(channel) = joining {
                    let idx = self.ensure_buffer(server, channel, Kind::Channel);
                    self.switch_to(idx);
                }
                if quitting {
                    self.should_quit = true;
                }
            }
            ParsedInput::Local(local) => {
                // Echo client-local commands too (e.g. `/reconnect`).
                let line = self.command_echo_line(buf, &input);
                self.push_line(buf, line);
                self.handle_local(server, local);
            }
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
                self.buffers[i].last_month = None;
            }
            LocalCommand::Disconnect => {
                // An explicit disconnect should not queue a retry.
                self.servers[server].next_at = None;
                self.servers[server].handle.send(ClientCommand::Disconnect);
            }
            LocalCommand::Reconnect => {
                self.servers[server].attempt = 0;
                if self.servers[server].connected {
                    // Close the live connection; its `Quit` result does not
                    // schedule a retry, so queue an immediate reconnect behind it.
                    self.servers[server].next_at = Some(Instant::now());
                    self.servers[server].handle.send(ClientCommand::Quit {
                        reason: Some("reconnect".into()),
                    });
                } else {
                    self.servers[server].next_at = None;
                    self.spawn_connection(server);
                }
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
            Kind::Query | Kind::Help => {}
        }
        // Preserve the latest lines before the buffer (and its log) goes away.
        if self.buffers[idx].history_dirty {
            if let Some(dir) = self.history_dir.clone() {
                self.save_history_buffer(&dir, idx);
            }
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
        // Any keystroke acknowledges the current buffer, including prefix and
        // prompt keys that never reach `perform`.
        self.note_user_activity();
        // Any keystroke cancels a pending `M-w` indication.
        self.copy_flash = None;
        // ...and the completion list from `dabbrev-completion`.
        self.completions = None;
        // ...and cancels an overdue-but-unshown prefix hint.
        self.prefix_echo = None;
        if !matches!(self.prompt, Prompt::None) {
            self.handle_prompt_key(key);
            return;
        }
        if self.search.active {
            // Motion commands exit isearch and then perform their normal
            // action from the current match, like Emacs: forward searches
            // continue from the end of the match, backward ones from its start.
            if isearch_motion(key) {
                let direction = self.search.direction;
                let match_end = self.search.match_end;
                self.search.accept();
                if direction == SearchDirection::Forward {
                    if let Some(end) = match_end {
                        self.current_mut().buffer.set_point(end);
                    }
                }
            } else {
                self.handle_search_key(key);
                return;
            }
        }
        if self.quoted {
            self.quoted = false;
            self.message = None;
            if let KeyCode::Char(c) = key.code {
                self.current_mut().buffer.insert_char(c);
            } else if key.code == KeyCode::Tab {
                self.current_mut().buffer.insert_char('\t');
            } else if key.code == KeyCode::Enter {
                // An IRC input line is single-line, so a quoted newline becomes
                // a space; free text buffers get a real line break.
                if self.current().buffer.is_irc() {
                    self.current_mut().buffer.insert_char(' ');
                } else {
                    self.current_mut().buffer.insert_newline();
                }
            }
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Universal argument entry (`C-u`, `M-<digit>`, `M--`, digits).
        if self.handle_argument(key, ctrl) {
            return;
        }

        // A pending prefix always consumes the next key.
        if self.prefix != Prefix::None {
            self.numeric_arg = None;
            self.handle_prefix(key, ctrl);
            return;
        }

        if ctrl && key.code == KeyCode::Char('x') {
            self.numeric_arg = None;
            self.start_prefix(Prefix::CtrlX, "C-x-");
            return;
        }
        if ctrl && key.code == KeyCode::Char('c') {
            self.numeric_arg = None;
            self.start_prefix(Prefix::CtrlC, "C-c-");
            return;
        }
        // C-h (help) prefix. Some terminals report Ctrl+H as Backspace.
        if ctrl && matches!(key.code, KeyCode::Char('h') | KeyCode::Backspace) {
            self.numeric_arg = None;
            self.start_prefix(Prefix::Help, "C-h-");
            return;
        }

        self.message = None;
        let action = keymap::translate(key);
        // A newline that arrived inside a paste burst must not send.
        if action == Action::Send && self.rapid_key {
            self.paste_newline();
            return;
        }
        if action == Action::None {
            self.numeric_arg = None;
            if let Some(desc) = keymap::describe(key) {
                self.message = Some(format!("{desc} is undefined"));
            }
        } else {
            self.perform(action);
        }
    }

    /// Handle keys that build the universal/numeric argument. Returns `true`
    /// when the key was consumed.
    fn handle_argument(&mut self, key: crossterm::event::KeyEvent, ctrl: bool) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // C-u: default 4, and each additional C-u multiplies by 4.
            KeyCode::Char('u') if ctrl => {
                let arg = self.numeric_arg.get_or_insert(ArgState {
                    magnitude: 1,
                    sign: 1,
                    digits: false,
                });
                arg.magnitude = arg.magnitude.saturating_mul(4);
                self.show_argument();
                true
            }
            // M--: negative argument (default -1), or flip the sign.
            KeyCode::Char('-') if alt => {
                match &mut self.numeric_arg {
                    Some(arg) => arg.sign = -arg.sign,
                    None => {
                        self.numeric_arg = Some(ArgState {
                            magnitude: 1,
                            sign: -1,
                            digits: false,
                        })
                    }
                }
                self.show_argument();
                true
            }
            // A bare `-` while building an argument flips the sign.
            KeyCode::Char('-') if !ctrl && self.numeric_arg.is_some() => {
                if let Some(arg) = &mut self.numeric_arg {
                    arg.sign = -arg.sign;
                }
                self.show_argument();
                true
            }
            // M-<digit> starts a numeric argument; digits extend it.
            KeyCode::Char(d) if d.is_ascii_digit() && alt => {
                self.push_argument_digit(d);
                self.show_argument();
                true
            }
            KeyCode::Char(d) if d.is_ascii_digit() && !ctrl && self.numeric_arg.is_some() => {
                self.push_argument_digit(d);
                self.show_argument();
                true
            }
            _ => false,
        }
    }

    fn push_argument_digit(&mut self, d: char) {
        let n = d.to_digit(10).unwrap_or(0);
        match &mut self.numeric_arg {
            Some(arg) if arg.digits => {
                arg.magnitude = arg.magnitude.saturating_mul(10).saturating_add(n);
            }
            Some(arg) => {
                arg.magnitude = n;
                arg.digits = true;
            }
            None => {
                self.numeric_arg = Some(ArgState {
                    magnitude: n,
                    sign: 1,
                    digits: true,
                })
            }
        }
    }

    fn show_argument(&mut self) {
        if let Some(arg) = self.numeric_arg {
            self.message = Some(format!("C-u {}-", arg.value()));
        }
    }

    /// Consume the pending numeric argument, returning it (default `1`).
    fn take_arg(&mut self) -> i32 {
        self.numeric_arg.take().map(ArgState::value).unwrap_or(1)
    }

    /// Begin an incomplete key sequence and arrange for its hint to be
    /// echoed only if the user pauses.
    fn start_prefix(&mut self, prefix: Prefix, hint: &str) {
        self.prefix = prefix;
        self.message = None;
        self.start_echo(hint);
    }

    fn start_echo(&mut self, hint: &str) {
        self.prefix_echo = Some((hint.to_string(), Instant::now() + PREFIX_ECHO_DELAY));
    }

    /// Show the pending prefix hint once its delay has elapsed.
    fn echo_pending_prefix(&mut self, now: Instant) {
        if let Some((hint, deadline)) = &self.prefix_echo {
            if *deadline <= now {
                let hint = hint.clone();
                self.prefix_echo = None;
                self.message = Some(hint);
            }
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
                KeyCode::Char('x') if ctrl => self.exchange_point_and_mark(),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-x", key),
            },
            Prefix::CtrlC => match key.code {
                KeyCode::Char('b') if ctrl => self.start_browse_url(),
                KeyCode::Char(' ') | KeyCode::Char('@') | KeyCode::Char('\0') => self.next_active(),
                KeyCode::Char('g') if ctrl => {}
                KeyCode::Char('f') if ctrl => {
                    self.start_prefix(Prefix::CtrlCF, "C-c C-f-");
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
            Prefix::Help => match key.code {
                KeyCode::Char('b') => self.show_help(),
                KeyCode::Char('g') if ctrl => {}
                _ => self.undefined("C-h", key),
            },
        }
    }

    /// `C-h b`: show the key bindings in a `*Help*` buffer.
    fn show_help(&mut self) {
        let lines: Vec<Line> = help_lines().into_iter().map(Line::plain).collect();
        let idx = match self.buffers.iter().position(|b| b.target == "*Help*") {
            Some(idx) => {
                self.buffers[idx].buffer.set_content(lines);
                idx
            }
            None => {
                let idx = self.new_buffer(0, "*Help*".to_string(), Kind::Help);
                self.buffers[idx].buffer.set_content(lines);
                idx
            }
        };
        self.switch_to(idx);
        // Show the list from the top (switch_to moves point to the end).
        self.buffers[idx].buffer.beginning_of_buffer();
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
            cursor: 0,
            selected: 0,
        };
    }

    fn start_browse_url(&mut self) {
        self.prompt = Prompt::BrowseUrl {
            input: String::new(),
            cursor: 0,
            selected: 0,
        };
    }

    fn start_kill_buffer(&mut self) {
        self.prompt = Prompt::KillBuffer {
            input: String::new(),
            cursor: 0,
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

    /// The URL under point on the current line, if any.
    fn url_at_point(&self) -> Option<String> {
        let buf = &self.current().buffer;
        let point = buf.point();
        let text = buf.line(point.row).text();
        let chars: Vec<char> = text.chars().collect();
        let mut idx = 0;
        while idx < chars.len() {
            if chars[idx].is_whitespace() {
                idx += 1;
                continue;
            }
            let start = idx;
            while idx < chars.len() && !chars[idx].is_whitespace() {
                idx += 1;
            }
            let token: String = chars[start..idx].iter().collect();
            let Some(url) = extract_url(&token) else {
                continue;
            };
            let byte = token.find(&url).unwrap_or(0);
            let col = start + token[..byte].chars().count();
            if point.col >= col && point.col < col + url.chars().count() {
                return Some(url);
            }
        }
        None
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

    /// Apply `f` to the active prompt's editable text and cursor. Returns
    /// `None` when no prompt is active.
    fn prompt_edit<R>(&mut self, f: impl FnOnce(&mut String, &mut usize) -> R) -> Option<R> {
        let (input, cursor) = match &mut self.prompt {
            Prompt::SwitchBuffer { input, cursor, .. }
            | Prompt::BrowseUrl { input, cursor, .. }
            | Prompt::KillBuffer { input, cursor, .. }
            | Prompt::Topic { input, cursor, .. } => (input, cursor),
            Prompt::None => return None,
        };
        Some(f(input, cursor))
    }

    /// Reset the candidate selection after the input text changes.
    fn prompt_reset_selection(&mut self) {
        match &mut self.prompt {
            Prompt::SwitchBuffer { selected, .. }
            | Prompt::BrowseUrl { selected, .. }
            | Prompt::KillBuffer { selected, .. } => *selected = 0,
            _ => {}
        }
    }

    fn prompt_insert(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.prompt_insert_str(c.encode_utf8(&mut buf));
    }

    /// Insert `text` at the cursor.
    fn prompt_insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self
            .prompt_edit(|input, cursor| {
                let idx = (*cursor).min(input.chars().count());
                input.insert_str(char_to_byte(input, idx), text);
                *cursor = idx + text.chars().count();
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    fn prompt_delete_backward(&mut self) {
        if self
            .prompt_edit(|input, cursor| {
                if *cursor > 0 {
                    let start = char_to_byte(input, *cursor - 1);
                    let end = char_to_byte(input, *cursor);
                    input.replace_range(start..end, "");
                    *cursor -= 1;
                }
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    fn prompt_delete_forward(&mut self) {
        if self
            .prompt_edit(|input, cursor| {
                if *cursor < input.chars().count() {
                    let start = char_to_byte(input, *cursor);
                    let end = char_to_byte(input, *cursor + 1);
                    input.replace_range(start..end, "");
                }
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    fn prompt_move_cursor(&mut self, delta: isize) {
        self.prompt_edit(|input, cursor| {
            let len = input.chars().count();
            *cursor = (*cursor as isize + delta).clamp(0, len as isize) as usize;
        });
        self.last_command = LastCommand::Other;
    }

    fn prompt_cursor_start(&mut self) {
        self.prompt_edit(|_, cursor| *cursor = 0);
        self.last_command = LastCommand::Other;
    }

    fn prompt_cursor_end(&mut self) {
        self.prompt_edit(|input, cursor| *cursor = input.chars().count());
        self.last_command = LastCommand::Other;
    }

    fn prompt_forward_word(&mut self) {
        self.prompt_edit(|input, cursor| {
            let chars: Vec<char> = input.chars().collect();
            let mut i = *cursor;
            while i < chars.len() && !is_word_char(chars[i]) {
                i += 1;
            }
            while i < chars.len() && is_word_char(chars[i]) {
                i += 1;
            }
            *cursor = i;
        });
        self.last_command = LastCommand::Other;
    }

    fn prompt_backward_word(&mut self) {
        self.prompt_edit(|input, cursor| {
            let chars: Vec<char> = input.chars().collect();
            let mut i = *cursor;
            while i > 0 && !is_word_char(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && is_word_char(chars[i - 1]) {
                i -= 1;
            }
            *cursor = i;
        });
        self.last_command = LastCommand::Other;
    }

    fn prompt_upcase_word(&mut self) {
        self.prompt_case_word(|word| word.iter().flat_map(|c| c.to_uppercase()).collect());
    }

    fn prompt_downcase_word(&mut self) {
        self.prompt_case_word(|word| word.iter().flat_map(|c| c.to_lowercase()).collect());
    }

    fn prompt_capitalize_word(&mut self) {
        self.prompt_case_word(capitalize_text);
    }

    /// Transpose the characters around the cursor (`C-t`).
    fn prompt_transpose_chars(&mut self) {
        if self
            .prompt_edit(|input, cursor| {
                let mut chars: Vec<char> = input.chars().collect();
                if let Some(new_cursor) = transpose_chars_at(&mut chars, *cursor) {
                    *input = chars.into_iter().collect();
                    *cursor = new_cursor;
                }
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    /// Transpose the words around the cursor (`M-t`).
    fn prompt_transpose_words(&mut self) {
        if self
            .prompt_edit(|input, cursor| {
                let mut chars: Vec<char> = input.chars().collect();
                if let Some(new_cursor) = transpose_words_at(&mut chars, *cursor) {
                    *input = chars.into_iter().collect();
                    *cursor = new_cursor;
                }
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    /// Change the case of the word after the cursor, moving the cursor past it.
    fn prompt_case_word(&mut self, transform: impl Fn(&[char]) -> String) {
        if self
            .prompt_edit(|input, cursor| {
                let chars: Vec<char> = input.chars().collect();
                let mut i = (*cursor).min(chars.len());
                while i < chars.len() && !is_word_char(chars[i]) {
                    i += 1;
                }
                let start = i;
                while i < chars.len() && is_word_char(chars[i]) {
                    i += 1;
                }
                let end = i;
                if start == end {
                    *cursor = end;
                    return;
                }
                let changed = transform(&chars[start..end]);
                let new_len = changed.chars().count();
                let mut out: Vec<char> = Vec::with_capacity(chars.len());
                out.extend_from_slice(&chars[..start]);
                out.extend(changed.chars());
                out.extend_from_slice(&chars[end..]);
                *input = out.into_iter().collect();
                *cursor = start + new_len;
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Other;
        }
    }

    /// Kill from the cursor to the end of the input (`C-k`).
    fn prompt_kill_line(&mut self) {
        let killed = self.prompt_edit(|input, cursor| {
            let at = char_to_byte(input, (*cursor).min(input.chars().count()));
            let killed = input[at..].to_string();
            input.truncate(at);
            killed
        });
        if let Some(killed) = killed {
            let append = self.last_command == LastCommand::Kill;
            self.kill_ring.push(&killed, append);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Kill;
        }
    }

    /// Kill the word before the cursor (`M-DEL`).
    fn prompt_backward_kill_word(&mut self) {
        let killed = self.prompt_edit(|input, cursor| {
            let chars: Vec<char> = input.chars().collect();
            let mut i = *cursor;
            while i > 0 && !is_word_char(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && is_word_char(chars[i - 1]) {
                i -= 1;
            }
            let start = char_to_byte(input, i);
            let end = char_to_byte(input, *cursor);
            let killed = input[start..end].to_string();
            input.replace_range(start..end, "");
            *cursor = i;
            killed
        });
        if let Some(killed) = killed {
            let append = self.last_command == LastCommand::Kill;
            self.kill_ring.push(&killed, append);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Kill;
        }
    }

    /// Kill from the cursor to the end of the next word (`M-d`).
    fn prompt_kill_word(&mut self) {
        let killed = self.prompt_edit(|input, cursor| {
            let chars: Vec<char> = input.chars().collect();
            let mut i = *cursor;
            while i < chars.len() && !is_word_char(chars[i]) {
                i += 1;
            }
            while i < chars.len() && is_word_char(chars[i]) {
                i += 1;
            }
            let start = char_to_byte(input, *cursor);
            let end = char_to_byte(input, i);
            let killed = input[start..end].to_string();
            input.replace_range(start..end, "");
            killed
        });
        if let Some(killed) = killed {
            let append = self.last_command == LastCommand::Kill;
            self.kill_ring.push(&killed, append);
            self.prompt_reset_selection();
            self.last_command = LastCommand::Kill;
        }
    }

    /// Insert the most recent kill at the cursor (`C-y`).
    fn prompt_yank(&mut self) {
        let text = match self.kill_ring.current() {
            Some(text) => text,
            None => return,
        };
        if self
            .prompt_edit(|input, cursor| {
                let idx = (*cursor).min(input.chars().count());
                input.insert_str(char_to_byte(input, idx), &text);
                *cursor = idx + text.chars().count();
            })
            .is_some()
        {
            self.prompt_reset_selection();
            self.last_command = LastCommand::Yank;
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
        if let Prompt::SwitchBuffer {
            input,
            cursor,
            selected,
        }
        | Prompt::BrowseUrl {
            input,
            cursor,
            selected,
        }
        | Prompt::KillBuffer {
            input,
            cursor,
            selected,
        } = &mut self.prompt
        {
            *cursor = completion.chars().count();
            *input = completion;
            *selected = 0;
        }
    }

    fn handle_prompt_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        match key.code {
            KeyCode::Esc => {
                self.prompt = Prompt::None;
                return;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.prompt = Prompt::None;
                return;
            }
            KeyCode::Enter => {
                // Ignore a newline that arrived inside a paste burst.
                if !self.rapid_key {
                    self.commit_prompt();
                }
                return;
            }
            KeyCode::Tab => {
                self.prompt_complete();
                return;
            }
            _ => {}
        }
        let count = match &self.prompt {
            Prompt::SwitchBuffer { input, .. } => self.switch_candidates(input).len(),
            Prompt::BrowseUrl { input, .. } => self.url_candidates(input).len(),
            Prompt::KillBuffer { input, .. } => self.kill_candidates(input).len(),
            Prompt::None | Prompt::Topic { .. } => 0,
        };
        use Action::*;
        let action = keymap::translate(key);
        if action != DabbrevExpand {
            self.dabbrev = Option::None;
        }
        match action {
            Abort => self.prompt = Prompt::None,
            SelfInsert(c) => self.prompt_insert(c),
            DeleteBackward => self.prompt_delete_backward(),
            DeleteForward => self.prompt_delete_forward(),
            BackwardChar => self.prompt_move_cursor(-1),
            ForwardChar => self.prompt_move_cursor(1),
            BeginningOfLine => self.prompt_cursor_start(),
            EndOfLine => self.prompt_cursor_end(),
            BackwardWord => self.prompt_backward_word(),
            ForwardWord => self.prompt_forward_word(),
            UpcaseWord => self.prompt_upcase_word(),
            DowncaseWord => self.prompt_downcase_word(),
            CapitalizeWord => self.prompt_capitalize_word(),
            TransposeChars => self.prompt_transpose_chars(),
            TransposeWords => self.prompt_transpose_words(),
            DabbrevExpand => self.prompt_dabbrev_expand(),
            DabbrevComplete => self.prompt_dabbrev_complete(),
            BackwardKillWord => self.prompt_backward_kill_word(),
            KillWord => self.prompt_kill_word(),
            KillLine => self.prompt_kill_line(),
            Yank => self.prompt_yank(),
            PrevLine => self.prompt_move(count, -1),
            NextLine => self.prompt_move(count, 1),
            _ => {}
        }
    }

    fn commit_prompt(&mut self) {
        match std::mem::replace(&mut self.prompt, Prompt::None) {
            Prompt::None => {}
            Prompt::SwitchBuffer {
                input, selected, ..
            } => {
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
            Prompt::BrowseUrl {
                input, selected, ..
            } => {
                let urls = self.url_candidates(&input);
                let url = urls
                    .get(selected)
                    .cloned()
                    .or_else(|| (!input.trim().is_empty()).then(|| input.trim().to_string()));
                if let Some(url) = url {
                    self.open_url(&url);
                }
            }
            Prompt::KillBuffer {
                input, selected, ..
            } => {
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
            Prompt::Topic { input, default, .. } => {
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
            cursor: 0,
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

    /// The active prompt as (prompt line, cursor column, candidate list,
    /// selected index).
    pub fn prompt_view(&self) -> Option<(String, usize, Vec<String>, usize)> {
        match &self.prompt {
            Prompt::None => None,
            Prompt::SwitchBuffer {
                input,
                cursor,
                selected,
            } => {
                let names = self
                    .switch_candidates(input)
                    .into_iter()
                    .map(|i| self.buffers[i].target.clone())
                    .collect();
                let prefix = format!(
                    "Switch to buffer (default {}): ",
                    self.previous_buffer_name()
                );
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    names,
                    *selected,
                ))
            }
            Prompt::BrowseUrl {
                input,
                cursor,
                selected,
            } => {
                let prefix = "Browse URL: ";
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    self.url_candidates(input),
                    *selected,
                ))
            }
            Prompt::KillBuffer {
                input,
                cursor,
                selected,
            } => {
                let names = self
                    .kill_candidates(input)
                    .into_iter()
                    .map(|i| self.buffers[i].target.clone())
                    .collect();
                let prefix = format!("Kill buffer (default {}): ", self.mode_line_name());
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    names,
                    *selected,
                ))
            }
            Prompt::Topic {
                input,
                cursor,
                default,
            } => {
                let prefix = if default.is_empty() {
                    "Topic: ".to_string()
                } else {
                    format!("Topic (default {default}): ")
                };
                Some((
                    format!("{prefix}{input}"),
                    prefix.chars().count() + *cursor,
                    Vec::new(),
                    0,
                ))
            }
        }
    }

    /// Height of the minibuffer area, expanded to show prompt candidates.
    ///
    /// Within a single prompt the height only ever grows: narrowing the
    /// candidate list keeps the previously reserved rows (padded with blank
    /// lines) rather than shrinking the echo area. The height resets once the
    /// prompt closes.
    pub fn minibuffer_height(&mut self) -> u16 {
        let desired = if let Some((_, _, candidates, _)) = self.prompt_view() {
            (1 + candidates.len().min(5)) as u16
        } else if let Some(candidates) = &self.completions {
            (1 + candidates.len().min(5)) as u16
        } else {
            1
        };
        self.minibuffer_rows = if matches!(self.prompt, Prompt::None) {
            desired
        } else {
            self.minibuffer_rows.max(desired)
        };
        self.minibuffer_rows
    }

    /// Clear the current buffer's terminal-title activity badge (the unread
    /// separator line itself is left in place) and restart the quiet timer, so
    /// a reply moments later isn't treated as idle activity.
    fn note_user_activity(&mut self) {
        let cur = self.current;
        self.buffers[cur].idle_marked = false;
        self.buffers[cur].last_activity_at = Instant::now();
    }

    fn perform(&mut self, action: Action) {
        use Action::*;
        if action != DabbrevExpand {
            self.dabbrev = Option::None;
        }
        let prev = self.last_command;
        self.last_command = LastCommand::Other;
        self.note_user_activity();
        // Any real command ends a `C-c C-SPC` run.
        self.next_active_origin = Option::None;
        // A pending numeric argument (Emacs universal argument), default 1.
        let n = self.take_arg();
        match action {
            None => {}
            SelfInsert(c) => {
                for _ in 0..n.max(0) {
                    self.current_mut().buffer.insert_char(c);
                }
            }
            Send => {
                if self.current().buffer.is_irc() && !self.current().buffer.editable() {
                    // RET on a URL in the scrollback opens it.
                    if let Some(url) = self.url_at_point() {
                        self.open_url(&url);
                        return;
                    }
                }
                if matches!(self.current().kind, Kind::Scratch | Kind::Help) {
                    self.current_mut().buffer.insert_newline();
                } else {
                    self.submit_input();
                }
            }
            ForwardChar => self.move_chars(n),
            BackwardChar => self.move_chars(-n),
            NextLine => self.move_lines(n),
            PrevLine => self.move_lines(-n),
            BeginningOfLine => self.current_mut().buffer.beginning_of_line(),
            EndOfLine => self.current_mut().buffer.end_of_line(),
            ForwardWord => self.move_words(n),
            BackwardWord => self.move_words(-n),
            UpcaseWord => self.case_words(n, true),
            DowncaseWord => self.case_words(n, false),
            CapitalizeWord => self.capitalize_words(n),
            TransposeChars => self.current_mut().buffer.transpose_chars(),
            TransposeWords => self.current_mut().buffer.transpose_words(),
            DabbrevExpand => self.dabbrev_expand(),
            DabbrevComplete => self.dabbrev_complete(),
            BeginningOfBuffer => self.current_mut().buffer.beginning_of_buffer(),
            EndOfBuffer => self.current_mut().buffer.end_of_buffer(),
            PageUp => self.page_up(),
            PageDown => self.page_down(),
            DeleteBackward => self.delete_chars(-n),
            DeleteForward => self.delete_chars(n),
            Undo => self.undo(),
            Redo => self.redo(),
            BackwardKillWord => {
                let killed = self.current_mut().buffer.backward_kill_word();
                self.kill_new(&killed, prev == LastCommand::Kill);
                self.last_command = LastCommand::Kill;
            }
            KillWord => {
                let killed = self.current_mut().buffer.kill_word();
                self.kill_new(&killed, prev == LastCommand::Kill);
                self.last_command = LastCommand::Kill;
            }
            KillLine => {
                let killed = self.current_mut().buffer.kill_line();
                self.kill_new(&killed, prev == LastCommand::Kill);
                self.last_command = LastCommand::Kill;
            }
            SetMark => {
                self.current_mut().buffer.set_mark();
                self.message = Some("Mark set".to_string());
            }
            KillRegion => self.kill_region(prev),
            CopyRegion => self.copy_region(prev),
            Yank => self.yank(),
            YankPop => self.yank_pop(prev),
            HistoryPrev => self.history_prev(),
            HistoryNext => self.history_next(),
            Complete => {
                if matches!(self.current().kind, Kind::Scratch | Kind::Help) {
                    self.current_mut().buffer.insert_str("    ");
                } else {
                    self.complete();
                }
            }
            SearchForward => self.start_search(SearchDirection::Forward),
            SearchBackward => self.start_search(SearchDirection::Backward),
            QuotedInsert => {
                self.quoted = true;
                self.start_echo("C-q-");
            }
            Abort => {
                self.current_mut().buffer.clear_mark();
                self.message = Option::None;
            }
        }
    }

    /// Move point by `n` characters (negative moves backward).
    fn move_chars(&mut self, n: i32) {
        for _ in 0..n.abs() {
            if n >= 0 {
                self.current_mut().buffer.forward_char();
            } else {
                self.current_mut().buffer.backward_char();
            }
        }
    }

    /// Move point by `n` lines (negative moves up).
    fn move_lines(&mut self, n: i32) {
        for _ in 0..n.abs() {
            if n >= 0 {
                let at_last = self.current().buffer.point().row == self.current().buffer.last_row();
                self.current_mut().buffer.next_line();
                if at_last {
                    self.message = Some("End of buffer".to_string());
                }
            } else {
                self.current_mut().buffer.prev_line();
            }
        }
    }

    /// Move point by `n` words (negative moves backward).
    fn move_words(&mut self, n: i32) {
        for _ in 0..n.abs() {
            if n >= 0 {
                self.current_mut().buffer.forward_word();
            } else {
                self.current_mut().buffer.backward_word();
            }
        }
    }

    /// Change the case of `n` words (negative affects words before point).
    fn case_words(&mut self, n: i32, up: bool) {
        for _ in 0..n.abs() {
            match (n >= 0, up) {
                (true, true) => self.current_mut().buffer.upcase_word(),
                (true, false) => self.current_mut().buffer.downcase_word(),
                (false, true) => self.current_mut().buffer.backward_upcase_word(),
                (false, false) => self.current_mut().buffer.backward_downcase_word(),
            }
        }
    }

    /// Capitalize `n` words (negative affects words before point).
    fn capitalize_words(&mut self, n: i32) {
        for _ in 0..n.abs() {
            if n >= 0 {
                self.current_mut().buffer.capitalize_word();
            } else {
                self.current_mut().buffer.backward_capitalize_word();
            }
        }
    }

    /// `M-/`: expand the word before point from another word in the buffer,
    /// cycling through candidates on repeated invocations.
    fn dabbrev_expand(&mut self) {
        let i = self.current;
        if !self.buffers[i].buffer.editable() {
            return;
        }
        // Continue cycling when the word before point is the last expansion.
        if let Some(prev) = self.dabbrev_current() {
            if self.buffers[i].buffer.word_before_point() == prev {
                self.dabbrev_advance_buffer(i);
                return;
            }
        }
        let prefix = self.buffers[i].buffer.word_before_point();
        if prefix.is_empty() {
            self.dabbrev = None;
            return;
        }
        let candidates = self.buffers[i].buffer.dabbrev_candidates(&prefix);
        let Some(first) = candidates.first().cloned() else {
            self.dabbrev = None;
            self.message = Some(format!("No dabbrev expansion for \"{prefix}\""));
            return;
        };
        self.buffers[i].buffer.replace_word_before_point(&first);
        self.dabbrev = Some(Dabbrev {
            expansions: candidates,
            index: 0,
        });
    }

    /// `C-M-/`: complete the word before point from every match in the
    /// buffer, inserting the common prefix and listing the candidates.
    fn dabbrev_complete(&mut self) {
        let i = self.current;
        if !self.buffers[i].buffer.editable() {
            return;
        }
        let prefix = self.buffers[i].buffer.word_before_point();
        if prefix.is_empty() {
            self.message = Some("No possible abbreviation preceding point".to_string());
            return;
        }
        let candidates = self.buffers[i].buffer.dabbrev_candidates(&prefix);
        if candidates.is_empty() {
            self.message = Some(format!("No dynamic expansion for \"{prefix}\" found"));
            return;
        }
        let completion = if candidates.len() == 1 {
            candidates[0].clone()
        } else {
            common_prefix(&candidates)
        };
        if completion.chars().count() > prefix.chars().count() {
            self.buffers[i]
                .buffer
                .replace_word_before_point(&completion);
        }
        if candidates.len() > 1 {
            self.completions = Some(candidates);
        }
    }

    /// `C-M-/` in the minibuffer: complete from the buffer being edited.
    fn prompt_dabbrev_complete(&mut self) {
        let cur = self.current;
        let prefix = self.prompt_word_before_cursor();
        if prefix.is_empty() {
            self.message = Some("No possible abbreviation preceding point".to_string());
            return;
        }
        let candidates = self.buffers[cur].buffer.dabbrev_candidates(&prefix);
        if candidates.is_empty() {
            self.message = Some(format!("No dynamic expansion for \"{prefix}\" found"));
            return;
        }
        let completion = if candidates.len() == 1 {
            candidates[0].clone()
        } else {
            common_prefix(&candidates)
        };
        if completion.chars().count() > prefix.chars().count() {
            self.prompt_replace_word_before_cursor(&completion);
        }
        if candidates.len() > 1 {
            self.message = Some(format!("Completions: {}", candidates.join("  ")));
        }
    }

    /// The word the current `M-/` cycle last inserted, if any.
    fn dabbrev_current(&self) -> Option<String> {
        self.dabbrev
            .as_ref()
            .and_then(|d| d.expansions.get(d.index).cloned())
    }

    /// Replace the word before point with the next `M-/` candidate.
    fn dabbrev_advance_buffer(&mut self, i: usize) {
        let (next, word) = {
            let d = self.dabbrev.as_ref().unwrap();
            let next = (d.index + 1) % d.expansions.len();
            (next, d.expansions[next].clone())
        };
        self.buffers[i].buffer.replace_word_before_point(&word);
        if let Some(d) = self.dabbrev.as_mut() {
            d.index = next;
        }
    }

    /// `M-/` in the minibuffer: expand the word before the cursor, drawing
    /// candidates from the buffer being edited (like Emacs searching buffers).
    fn prompt_dabbrev_expand(&mut self) {
        let cur = self.current;
        if let Some(prev) = self.dabbrev_current() {
            if self.prompt_word_before_cursor() == prev {
                let (next, word) = {
                    let d = self.dabbrev.as_ref().unwrap();
                    let next = (d.index + 1) % d.expansions.len();
                    (next, d.expansions[next].clone())
                };
                self.prompt_replace_word_before_cursor(&word);
                if let Some(d) = self.dabbrev.as_mut() {
                    d.index = next;
                }
                return;
            }
        }
        let prefix = self.prompt_word_before_cursor();
        if prefix.is_empty() {
            self.dabbrev = None;
            return;
        }
        let candidates = self.buffers[cur].buffer.dabbrev_candidates(&prefix);
        let Some(first) = candidates.first().cloned() else {
            self.dabbrev = None;
            return;
        };
        self.prompt_replace_word_before_cursor(&first);
        self.dabbrev = Some(Dabbrev {
            expansions: candidates,
            index: 0,
        });
    }

    fn prompt_word_before_cursor(&mut self) -> String {
        self.prompt_edit(|input, cursor| {
            let chars: Vec<char> = input.chars().collect();
            let end = (*cursor).min(chars.len());
            let mut start = end;
            while start > 0 && is_word_char(chars[start - 1]) {
                start -= 1;
            }
            chars[start..end].iter().collect()
        })
        .unwrap_or_default()
    }

    fn prompt_replace_word_before_cursor(&mut self, word: &str) {
        self.prompt_edit(|input, cursor| {
            let chars: Vec<char> = input.chars().collect();
            let end = (*cursor).min(chars.len());
            let mut start = end;
            while start > 0 && is_word_char(chars[start - 1]) {
                start -= 1;
            }
            let s = char_to_byte(input, start);
            let e = char_to_byte(input, end);
            input.replace_range(s..e, word);
            *cursor = start + word.chars().count();
        });
    }

    /// Delete `n` characters (negative deletes backward).
    fn delete_chars(&mut self, n: i32) {
        for _ in 0..n.abs() {
            if n >= 0 {
                self.current_mut().buffer.delete_forward();
            } else {
                self.current_mut().buffer.delete_backward();
            }
        }
    }

    /// `C-_` / `C--`: undo the last edit to the input line. Incoming
    /// scrollback does not clear the undo history.
    fn undo(&mut self) {
        if self.current_mut().buffer.undo() {
            self.message = Some("Undo".to_string());
        } else {
            self.message = Some("No further undo information".to_string());
        }
    }

    /// `C-M-_` / `C-?`: redo the last undone input-line edit.
    fn redo(&mut self) {
        if self.current_mut().buffer.redo() {
            self.message = Some("Redo".to_string());
        } else {
            self.message = Some("No undone changes to redo".to_string());
        }
    }

    /// A screenful minus two lines, like Emacs `next-screen-context-lines`.
    fn page_lines(&self) -> usize {
        self.viewport_rows.saturating_sub(2).max(1)
    }

    /// Scroll the current buffer's window so point stays visible, moving the
    /// window only when point reaches the top or bottom edge (Emacs-style).
    fn ensure_scroll_visible(&mut self) {
        let height = self.viewport_rows.max(1);
        let cur = self.current;
        self.buffers[cur].buffer.ensure_point_visible(height);
    }

    fn page_up(&mut self) {
        let n = self.page_lines();
        self.current_mut().buffer.page_backward(n);
    }

    fn page_down(&mut self) {
        let n = self.page_lines();
        self.current_mut().buffer.page_forward(n);
    }

    /// Add text to the kill ring and mirror the result to the system
    /// clipboard (OSC 52).
    fn kill_new(&mut self, text: &str, append: bool) {
        self.kill_ring.push(text, append);
        if let Some(top) = self.kill_ring.current() {
            self.set_clipboard(&top);
        }
    }

    fn set_clipboard(&self, text: &str) {
        if !self.clipboard {
            return;
        }
        if let Some(sequence) = osc52_sequence(text) {
            let mut out = std::io::stdout();
            let _ = std::io::Write::write_all(&mut out, sequence.as_bytes());
            let _ = std::io::Write::flush(&mut out);
        }
    }

    /// `C-x C-x`: swap point and mark.
    fn exchange_point_and_mark(&mut self) {
        if !self.current_mut().buffer.exchange_point_and_mark() {
            self.message = Some("No mark set in this buffer".to_string());
        }
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
        self.kill_new(&text, prev == LastCommand::Kill);
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
            self.kill_new(&text, prev == LastCommand::Kill);
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
            Some((pos, len)) => {
                self.current_mut().buffer.set_point(pos);
                self.search.match_end = Some(Pos::new(pos.row, pos.col + len));
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
            KeyCode::Enter => {
                let end = if self.search.direction == SearchDirection::Forward {
                    self.search.match_end
                } else {
                    None
                };
                self.search.accept();
                if let Some(end) = end {
                    self.current_mut().buffer.set_point(end);
                }
            }
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

    /// The timestamp field for `buf`, rendered from the absolute `time` with
    /// the configured format. Repeated timestamps are blanked so only the first
    /// line of a run carries the time.
    fn time_prefix(&mut self, buf: usize, time: i64) -> String {
        let fmt = self.config.ui.timestamp_format.clone();
        let ts = format_local(time, &fmt);
        if self.buffers[buf].last_timestamp == ts {
            " ".repeat(ts.chars().count() + 1)
        } else {
            self.buffers[buf].last_timestamp = ts.clone();
            format!("{ts} ")
        }
    }

    /// Message body as spans: whole-word occurrences of our nick get
    /// `rcirc-nick-in-message`, URLs get `rcirc-url`. Returns whether our nick
    /// was mentioned.
    fn message_spans(&self, server: usize, text: &str) -> (Vec<Span>, bool) {
        let our = self.our_nick(server);
        self.markup_spans(&our, text, Style::default())
    }

    /// Tokenize `text`, applying `rcirc-url` (bold) to URLs and
    /// `rcirc-nick-in-message` to whole-word occurrences of `our`. Non-marked
    /// text uses `base` (default for chat, server color for server lines).
    fn markup_spans(&self, our: &str, text: &str, base: Style) -> (Vec<Span>, bool) {
        let mut mentioned = false;
        let mut spans = Vec::new();
        let mut run = String::new();
        let mut run_space: Option<bool> = None;
        for ch in text.chars() {
            let space = ch.is_whitespace();
            match run_space {
                Some(s) if s == space => run.push(ch),
                Some(s) => {
                    self.emit_run(&mut run, s, our, base, &mut mentioned, &mut spans);
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
            self.emit_run(&mut run, s, our, base, &mut mentioned, &mut spans);
        }
        (spans, mentioned)
    }

    fn emit_run(
        &self,
        run: &mut String,
        space: bool,
        our: &str,
        base: Style,
        mentioned: &mut bool,
        spans: &mut Vec<Span>,
    ) {
        if run.is_empty() {
            return;
        }
        let text = std::mem::take(run);
        if space {
            spans.push(Span::new(text, base));
        } else {
            spans.extend(self.token_spans(&text, our, base, mentioned));
        }
    }

    fn token_spans(&self, token: &str, our: &str, base: Style, mentioned: &mut bool) -> Vec<Span> {
        if let Some(url) = extract_url(token) {
            if let Some(start) = token.find(&url) {
                let mut spans = Vec::new();
                let before = &token[..start];
                let after = &token[start + url.len()..];
                if !before.is_empty() {
                    spans.extend(self.nick_spans(before, our, base, mentioned));
                }
                spans.push(Span::new(url, self.theme.url));
                if !after.is_empty() {
                    spans.extend(self.nick_spans(after, our, base, mentioned));
                }
                return spans;
            }
        }
        self.nick_spans(token, our, base, mentioned)
    }

    /// Highlight whole-word occurrences of `our` within `text`.
    fn nick_spans(&self, text: &str, our: &str, base: Style, mentioned: &mut bool) -> Vec<Span> {
        let chars: Vec<char> = text.chars().collect();
        let needle: Vec<char> = our.to_lowercase().chars().collect();
        if our.is_empty() || needle.len() > chars.len() {
            return vec![Span::new(text.to_string(), base)];
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
                    spans.push(Span::new(chars[last..i].iter().collect::<String>(), base));
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
            spans.push(Span::new(chars[last..].iter().collect::<String>(), base));
        }
        if spans.is_empty() {
            spans.push(Span::new(text.to_string(), base));
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
        let time = event_time(tags);
        let prefix = self.time_prefix(buf, time);
        let nstyle = self.nick_style(server, nick);
        let (mut body, mentioned) = self.message_spans(server, text);
        if mentioned {
            for span in &mut body {
                span.style = span.style.bold();
            }
        }
        let indent = prefix.chars().count() + nick.chars().count() + 3;
        let mut spans = vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new(format!("<{nick}>"), nstyle),
            Span::new(" ".to_string(), Style::default()),
        ];
        spans.extend(body);
        let mut line = Line::from_spans(spans).indented(indent);
        line.time = Some(time);
        line
    }

    fn action_line(
        &mut self,
        server: usize,
        buf: usize,
        tags: &[Tag],
        nick: &str,
        body: &str,
    ) -> Line {
        let time = event_time(tags);
        let prefix = self.time_prefix(buf, time);
        let nstyle = self.nick_style(server, nick);
        let (body, _) = self.message_spans(server, body);
        let indent = prefix.chars().count() + nick.chars().count() + 2;
        let mut spans = vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new("[".to_string(), Style::default()),
            Span::new(nick.to_string(), nstyle),
            Span::new(" ".to_string(), Style::default()),
        ];
        spans.extend(body);
        spans.push(Span::new("]".to_string(), Style::default()));
        let mut line = Line::from_spans(spans).indented(indent);
        line.time = Some(time);
        line
    }

    fn notice_line(
        &mut self,
        server: usize,
        buf: usize,
        tags: &[Tag],
        nick: &str,
        text: &str,
    ) -> Line {
        let time = event_time(tags);
        let prefix = self.time_prefix(buf, time);
        let nstyle = self.nick_style(server, nick);
        let (body, _) = self.message_spans(server, text);
        let indent = prefix.chars().count() + nick.chars().count() + 3;
        let mut spans = vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new("-".to_string(), nstyle),
            Span::new(nick.to_string(), nstyle),
            Span::new("- ".to_string(), nstyle),
        ];
        spans.extend(body);
        let mut line = Line::from_spans(spans).indented(indent);
        line.time = Some(time);
        line
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
        let time = now_epoch();
        let prefix = self.time_prefix(buf, time);
        let mut head = String::from("***");
        if let Some(sender) = sender {
            head.push(' ');
            head.push_str(sender);
        }
        if !response.is_empty() {
            head.push(' ');
            head.push_str(response);
        }
        let indent = prefix.chars().count() + head.chars().count() + usize::from(!text.is_empty());
        let mut spans = vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new(head, self.theme.server),
        ];
        if !text.is_empty() {
            spans.push(Span::new(" ".to_string(), self.theme.server));
            // URLs/nick markup applies here too (rcirc-markup-urls runs on all
            // responses), with the server face as the base color.
            let our = self.our_nick(self.buffers[buf].server);
            let (body, _) = self.markup_spans(&our, text, self.theme.server);
            spans.extend(body);
        }
        let mut line = Line::from_spans(spans).indented(indent);
        line.time = Some(time);
        line
    }

    /// A connection/system message (`*** text`).
    fn system_line(&mut self, buf: usize, text: &str) -> Line {
        self.server_line(buf, None, "", text)
    }

    /// Echo a user-issued command (`>>> /topic ...`) in the current buffer.
    fn command_echo_line(&mut self, buf: usize, text: &str) -> Line {
        let time = now_epoch();
        let prefix = self.time_prefix(buf, time);
        let indent = prefix.chars().count() + 4;
        let mut line = Line::from_spans(vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new(format!(">>> {text}"), Style::fg(Color::Gray)),
        ])
        .indented(indent);
        line.time = Some(time);
        line
    }

    /// An error/warning line (`!!! text`) in `font-lock-warning-face`.
    fn error_line(&mut self, buf: usize, text: &str) -> Line {
        let time = now_epoch();
        let prefix = self.time_prefix(buf, time);
        let indent = prefix.chars().count() + 4;
        let mut line = Line::from_spans(vec![
            Span::new(prefix, self.theme.timestamp),
            Span::new(format!("!!! {text}"), self.theme.warning),
        ])
        .indented(indent);
        line.time = Some(time);
        line
    }

    /// Rebuild a logical line from the cache: render the timestamp column from
    /// the absolute `time`, recover the body's face from its rcirc prefix.
    fn render_log_line(&mut self, buf: usize, time: i64, body: &str) -> Line {
        let server = self.buffers[buf].server;
        let prefix = self.time_prefix(buf, time);
        let prefix_w = prefix.chars().count();
        let ts_style = self.theme.timestamp;
        let mut spans = vec![Span::new(prefix, ts_style)];
        let indent;

        if let Some(rest) = body.strip_prefix('<') {
            if let Some(close) = rest.find('>') {
                let nick = rest[..close].to_string();
                let text = rest[close + 1..]
                    .strip_prefix(' ')
                    .unwrap_or(&rest[close + 1..]);
                let nstyle = self.nick_style(server, &nick);
                let (mut b, mentioned) = self.message_spans(server, text);
                if mentioned {
                    for span in &mut b {
                        span.style = span.style.bold();
                    }
                }
                indent = prefix_w + nick.chars().count() + 3;
                spans.push(Span::new(format!("<{nick}>"), nstyle));
                spans.push(Span::new(" ".to_string(), Style::default()));
                spans.extend(b);
                return self.finish_log_line(spans, indent, time);
            }
        } else if let Some(inner) = body.strip_prefix('[').and_then(|b| b.strip_suffix(']')) {
            if let Some((nick, text)) = inner.split_once(' ') {
                let nstyle = self.nick_style(server, nick);
                let (b, _) = self.message_spans(server, text);
                indent = prefix_w + nick.chars().count() + 2;
                spans.push(Span::new("[".to_string(), Style::default()));
                spans.push(Span::new(nick.to_string(), nstyle));
                spans.push(Span::new(" ".to_string(), Style::default()));
                spans.extend(b);
                spans.push(Span::new("]".to_string(), Style::default()));
                return self.finish_log_line(spans, indent, time);
            }
        } else if let Some(rest) = body.strip_prefix('-') {
            if let Some(idx) = rest.find("- ") {
                let nick = rest[..idx].to_string();
                let text = &rest[idx + 2..];
                let nstyle = self.nick_style(server, &nick);
                let (b, _) = self.message_spans(server, text);
                indent = prefix_w + nick.chars().count() + 3;
                spans.push(Span::new("-".to_string(), nstyle));
                spans.push(Span::new(nick.to_string(), nstyle));
                spans.push(Span::new("- ".to_string(), nstyle));
                spans.extend(b);
                return self.finish_log_line(spans, indent, time);
            }
        }

        // Server/system (`*** ...`), command echo (`>>> ...`), error (`!!! ...`),
        // or anything else: render the remainder with an appropriate base face.
        if let Some(rest) = body.strip_prefix("*** ") {
            let our = self.our_nick(server);
            let (b, _) = self.markup_spans(&our, rest, self.theme.server);
            indent = prefix_w + 4;
            spans.push(Span::new("*** ".to_string(), self.theme.server));
            spans.extend(b);
        } else if body.starts_with(">>> ") {
            indent = prefix_w + 4;
            spans.push(Span::new(body.to_string(), Style::fg(Color::Gray)));
        } else if body.starts_with("!!! ") {
            indent = prefix_w + 4;
            spans.push(Span::new(body.to_string(), self.theme.warning));
        } else {
            indent = prefix_w;
            spans.push(Span::new(body.to_string(), Style::default()));
        }
        self.finish_log_line(spans, indent, time)
    }

    fn finish_log_line(&self, spans: Vec<Span>, indent: usize, time: i64) -> Line {
        let mut line = Line::from_spans(spans).indented(indent);
        line.time = Some(time);
        line
    }

    /// Print an error into every buffer of `server` (mirrors rcirc's sentinel
    /// printing an `ERROR` response in all buffers on disconnect).
    fn broadcast_error(&mut self, server: usize, message: &str) {
        let text = format!("{}: {message}", self.server_name(server));
        self.broadcast_message(server, &text);
    }

    /// Print `text` verbatim as an `!!!` line into every buffer of `server`.
    fn broadcast_message(&mut self, server: usize, text: &str) {
        let targets: Vec<usize> = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, b)| b.server == server && b.kind != Kind::Scratch)
            .map(|(i, _)| i)
            .collect();
        for idx in targets {
            let line = self.error_line(idx, text);
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
                let reconnected = self.servers[server].attempt > 0;
                self.servers[server].attempt = 0;
                self.servers[server].next_at = None;
                let idx = self.servers[server].buffer;
                let text = if reconnected {
                    "Reconnected"
                } else {
                    "Connected"
                };
                let line = self.system_line(idx, text);
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
                // `Quit` is emitted for our own /disconnect and /quit.
                if reason != "Quit" {
                    self.schedule_reconnect(server);
                }
            }
            Event::ConnectionError { error } => {
                self.servers[server].connected = false;
                // Report the underlying failure only for the first error of an
                // outage; each retry is summarized by `schedule_reconnect`
                // ("Reconnecting ... (attempt N)") rather than repeating the
                // same connect error every few seconds.
                if self.servers[server].attempt == 0 {
                    self.broadcast_error(server, &error);
                }
                self.schedule_reconnect(server);
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
                let idx = self.ensure_buffer(server, channel.clone(), Kind::Channel);
                self.add_nick(idx, &nick);
                let line = self.server_line(idx, Some(&nick), "JOIN", "");
                self.push_line(idx, line);
                // Our own /join should surface the new channel as activity,
                // and marks it to be rejoined after a reconnect.
                if nick == self.our_nick(server) {
                    self.servers[server].channels.insert(channel);
                    self.bump_activity(idx, false);
                }
            }
            Event::Part {
                channel,
                nick,
                reason,
            } => {
                if nick == self.our_nick(server) {
                    self.servers[server].channels.remove(&channel);
                }
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
                if nick == self.our_nick(server) {
                    self.servers[server].channels.remove(&channel);
                }
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
                let line = self.server_line(idx, None, "TOPIC", &topic);
                self.push_line(idx, line);
                self.bump_activity(idx, false);
            }
            Event::TopicSet {
                channel,
                setter,
                time,
            } => {
                let idx = self.ensure_buffer(server, channel.clone(), Kind::Channel);
                let when = format_time(time);
                let text = format!("Topic set by {setter} on {when}");
                let line = self.server_line(idx, None, "", &text);
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
        // Flush the scrollback cache periodically (interval is configurable).
        let sync_secs = self.config.history.sync_secs.max(1);
        let mut history_flush = tokio::time::interval(Duration::from_secs(sync_secs));
        history_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        history_flush.tick().await; // consume the immediate first tick

        loop {
            let minibuffer = self.minibuffer_height();
            if let Ok(size) = terminal.size() {
                // Buffer view height = terminal rows minus the mode line and
                // the (possibly multi-row) minibuffer.
                self.viewport_rows = (size.height as usize).saturating_sub(1 + minibuffer as usize);
            }
            self.ensure_scroll_visible();
            terminal.draw(|frame| crate::ui::draw(frame, self))?;
            self.sync_title();
            if self.should_quit {
                self.reset_title();
                self.save_all_history();
                for state in &self.servers {
                    state.handle.quit("cricr");
                }
                break;
            }
            let flash_deadline = self.copy_flash.map(|(_, deadline)| deadline);
            let prefix_deadline = self.prefix_echo.as_ref().map(|(_, deadline)| *deadline);
            let reconnect_deadline = self.next_reconnect_deadline();
            tokio::select! {
                Some((server, event)) = net_rx.recv() => {
                    self.handle_event(server, event);
                    // Batch a burst of server output into a single redraw.
                    while let Ok((server, event)) = net_rx.try_recv() {
                        self.handle_event(server, event);
                    }
                }
                Some(term_event) = term_rx.recv() => {
                    // Batch a burst of input (e.g. a paste) into one redraw,
                    // so the whole burst can be classified at once.
                    let mut batch = vec![term_event];
                    while let Ok(event) = term_rx.try_recv() {
                        batch.push(event);
                    }
                    self.handle_term_batch(batch);
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
                // Echo an incomplete prefix key after the user pauses.
                _ = async {
                    match prefix_deadline {
                        Some(deadline) => {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.echo_pending_prefix(Instant::now());
                }
                // Fire any due reconnect attempts.
                _ = async {
                    match reconnect_deadline {
                        Some(deadline) => {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                        }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.run_due_reconnects();
                }
                // Periodically flush dirty scrollback to the history cache.
                _ = history_flush.tick() => {
                    self.save_all_history();
                }
                else => break,
            }
        }
        Ok(())
    }

    fn handle_term_batch(&mut self, mut batch: Vec<TermEvent>) {
        let now = Instant::now();
        // Alacritty on Windows reports Ctrl+Space as a plain Space. Only
        // promote a lone space; a paste burst holds typed text that must keep
        // its spaces even while Ctrl is still held after Ctrl+V.
        #[cfg(windows)]
        promote_alacritty_ctrl_space(&mut batch, keymap::ctrl_is_down());
        // A burst carrying a newline followed by more input is a paste (e.g. on
        // Windows, where bracketed paste is not available); the latch keeps a
        // paste that is split across drains folding.
        let multiline = is_multiline_paste(&batch);
        let pasting = multiline || self.pasting_until.is_some_and(|t| now < t);
        if pasting {
            self.pasting_until = Some(now + PASTE_LATCH);
        }
        for event in batch {
            match event {
                TermEvent::Key(key) => {
                    self.rapid_key = pasting;
                    self.handle_key(key);
                }
                TermEvent::Paste(text) => self.handle_paste(text),
                _ => {}
            }
        }
        self.rapid_key = false;
    }

    /// A newline that arrived inside a paste burst: fold it into the input
    /// rather than sending (a space for rcirc buffers, a real newline for free
    /// text buffers).
    fn paste_newline(&mut self) {
        let i = self.current;
        if self.buffers[i].buffer.is_irc() {
            self.buffers[i].buffer.insert_char(' ');
        } else {
            self.buffers[i].buffer.insert_newline();
        }
    }

    /// Insert pasted text. In an rcirc buffer newlines become spaces (the input
    /// is a single line); the scratch buffer keeps them.
    fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        // Prompt input: drop newlines.
        if !matches!(self.prompt, Prompt::None) {
            let filtered: String = text.chars().filter(|c| *c != '\r' && *c != '\n').collect();
            if filtered.is_empty() {
                return;
            }
            self.prompt_insert_str(&filtered);
            self.kill_ring.push(&filtered, false);
            return;
        }
        let i = self.current;
        let inserted = if self.buffers[i].buffer.is_irc() {
            text.chars()
                .filter(|c| *c != '\r')
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect::<String>()
        } else {
            text
        };
        self.buffers[i].buffer.insert_str(&inserted);
        // Make pasted text available to C-y / M-y.
        self.kill_ring.push(&inserted, false);
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

/// Whether `key` is a motion command that should leave isearch and run.
fn isearch_motion(key: crossterm::event::KeyEvent) -> bool {
    use Action::*;
    matches!(
        keymap::translate(key),
        ForwardChar
            | BackwardChar
            | NextLine
            | PrevLine
            | BeginningOfLine
            | EndOfLine
            | ForwardWord
            | BackwardWord
            | BeginningOfBuffer
            | EndOfBuffer
            | PageUp
            | PageDown
    )
}

/// Byte offset of the `idx`-th char in `s`, clamped to the end.
fn char_to_byte(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map(|(i, _)| i).unwrap_or(s.len())
}

/// Exponential backoff delay (in seconds) for reconnect `attempt` (1-based),
/// capped at `max`.
fn reconnect_delay(attempt: u32, initial: f64, max: f64) -> f64 {
    let initial = initial.max(0.1);
    let max = max.max(initial);
    let exp = attempt.saturating_sub(1).min(20) as i32;
    (initial * 2f64.powi(exp)).min(max)
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

/// The introductory text placed in the `*scratch*` buffer at startup.
fn scratch_lines() -> Vec<String> {
    let path = crate::config::default_path();
    [
        "cricr — a standalone IRC client with Emacs-style editing".to_string(),
        String::new(),
        "Welcome to the *scratch* buffer: a free-text buffer for notes and".to_string(),
        "drafts. Nothing typed here is sent to a server.".to_string(),
        String::new(),
        "Configuration".to_string(),
        "  By default cricr reads its config from:".to_string(),
        format!("      {}", path.display()),
        String::new(),
        "  A commented template is written there on first run. Edit it to add".to_string(),
        "  servers, SASL/NickServ credentials, channels, and UI options. Pass".to_string(),
        "  `cricr --config <file>` to use a different file.".to_string(),
        String::new(),
        "Discovering commands".to_string(),
        "  C-h b          list all key bindings in a *Help* buffer".to_string(),
        "  C-x b          switch buffers".to_string(),
        "  C-x C-c        quit".to_string(),
        "  Commands typed in a buffer start with `/` (e.g. /join, /whois).".to_string(),
        String::new(),
        "Notes".to_string(),
        "  Pasted text is added to the kill ring (recall with C-y / M-y).".to_string(),
        "  C-w / M-w / C-k mirror the kill to the system clipboard (OSC 52).".to_string(),
    ]
    .to_vec()
}

/// The text of the `*Help*` buffer (`C-h b`).
fn help_lines() -> Vec<String> {
    [
        "cricr key bindings",
        "",
        "Movement",
        "  C-f / C-b      forward / backward character",
        "  C-n / C-p      next / previous line",
        "  M-f / M-b      forward / backward word",
        "  C-a / C-e      beginning / end of line",
        "  M-< / M->      beginning / end of buffer",
        "  C-v / M-v      scroll up / down a screen",
        "  C-s / C-r      incremental search forward / backward",
        "",
        "Editing",
        "  RET            send input",
        "  TAB            complete nick or /command",
        "  M-p / M-n      previous / next input",
        "  M-u / M-l / M-c  upcase / downcase / capitalize word",
        "  M-/            dabbrev-expand (complete from a word in the buffer)",
        "  C-M-/          dabbrev-completion (complete prefix and list matches)",
        "  C-k            kill to end of line",
        "  M-Backspace    kill word backward",
        "  M-d            kill word forward",
        "  C-d            delete character forward",
        "  C-_ / C--      undo an edit on the input line",
        "  C-M-_ / C-?    redo an undone edit",
        "  C-SPC          set mark",
        "  C-w / M-w      kill / copy region",
        "  C-y / M-y      yank / yank-pop",
        "  C-q            quoted insert",
        "",
        "Buffers",
        "  C-x b          switch buffer",
        "  C-x k          kill buffer (parts a channel)",
        "  C-x C-x        exchange point and mark",
        "  C-x C-c        quit",
        "  C-c C-SPC      jump to the next buffer with activity",
        "  C-c C-a        jump to the first unread line",
        "  C-c C-s        switch to the server buffer",
        "",
        "IRC commands",
        "  C-c C-n        /names",
        "  C-c C-t        set the channel topic",
        "  C-c C-b        browse a URL found in the buffer",
        "  C-c C-i        toggle ignoring activity",
        "  C-c C-j/p/q/w/m/k/d/r/x  /join /part /query /whois /msg /kick /mode /nick /quit",
        "  C-c C-f C-b/i/u/s/f/t    bold / italic / underline / strike / fixed",
        "  C-c C-f C-d    remove formatting",
        "",
        "Help",
        "  C-h b          describe bindings (this buffer)",
        "  C-g            abort the current command",
        "",
        "Arguments",
        "  C-u            numeric argument (4); repeat to multiply (C-u C-u = 16)",
        "  M-<digit>      start a numeric argument, e.g. M-3",
        "  M--            negative argument (M-- M-u upcases the previous word)",
        "  C-u 3 C-f      forward 3 characters; C-u C-n moves down 4 lines",
        "",
        "Notes",
        "  Pasted text is added to the kill ring (recall with C-y / M-y).",
        "  C-w / M-w / C-k mirror the kill to the system clipboard (OSC 52).",
        "  Your terminal may capture Ctrl+V for paste; rebind it (e.g. to",
        "  Ctrl+Shift+V) if you want C-v to scroll a page.",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The OSC 52 escape sequence that sets the system clipboard to `text`
/// (`None` for empty text).
fn osc52_sequence(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    use base64::Engine;
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    Some(format!("\u{1b}]52;c;{payload}\u{7}"))
}

/// The OSC 2 escape sequence that sets the terminal title. Control characters
/// are stripped so a buffer name can never inject an escape sequence, and the
/// result is length-capped.
fn title_sequence(title: &str) -> String {
    let sanitized: String = title
        .chars()
        .filter(|c| !c.is_control() && *c != '\u{7f}')
        .take(120)
        .collect();
    format!("\u{1b}]2;{sanitized}\u{7}")
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

/// Whether a drained burst of terminal events is a multiline paste: it carries
/// an Enter that is not the last key press, i.e. a newline with more input after
/// it. A typed Enter is always the last key in its burst.
fn is_multiline_paste(batch: &[TermEvent]) -> bool {
    let keys: Vec<&crossterm::event::KeyEvent> = batch
        .iter()
        .filter_map(|event| match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => Some(key),
            _ => None,
        })
        .collect();
    keys.iter()
        .enumerate()
        .any(|(i, key)| key.code == crossterm::event::KeyCode::Enter && i + 1 < keys.len())
}

/// Promote a lone plain Space to `C-SPC` when Alacritty on Windows has lost the
/// control modifier. Multi-key bursts (pastes) are left alone so their spaces
/// are inserted normally.
#[cfg(windows)]
fn promote_alacritty_ctrl_space(batch: &mut [TermEvent], ctrl_down: bool) {
    if !ctrl_down {
        return;
    }
    let mut found = None;
    for (i, event) in batch.iter().enumerate() {
        match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                if found.is_some() {
                    return;
                }
                found = Some(i);
            }
            TermEvent::Paste(_) => return,
            _ => {}
        }
    }
    let Some(i) = found else {
        return;
    };
    if let TermEvent::Key(key) = &mut batch[i] {
        if key.code == crossterm::event::KeyCode::Char(' ')
            && !key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
            && !key.modifiers.contains(crossterm::event::KeyModifiers::ALT)
        {
            key.modifiers |= crossterm::event::KeyModifiers::CONTROL;
        }
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

/// Format a unix timestamp in local time, e.g. `Sun Sep 20 20:59:52 2026`.
fn format_time(unix: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix, 0).single() {
        Some(dt) => dt.format("%a %b %-d %H:%M:%S %Y").to_string(),
        None => unix.to_string(),
    }
}

/// The current time as Unix epoch seconds.
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format `time` (epoch seconds) in local time with `fmt` (e.g. `"%H:%M"`).
fn format_local(time: i64, fmt: &str) -> String {
    Local
        .timestamp_opt(time, 0)
        .single()
        .map(|dt| dt.format(fmt).to_string())
        .unwrap_or_default()
}

/// A monotonic month index (year * 12 + month) for `time` in local time, so
/// consecutive months compare unequal. `i32::MIN` when the time is invalid.
fn month_index(time: i64) -> i32 {
    Local
        .timestamp_opt(time, 0)
        .single()
        .map(|dt| dt.year() * 12 + dt.month0() as i32)
        .unwrap_or(i32::MIN)
}

/// A cached logical line's body: its text minus the timestamp column (which is
/// the line's first span and is re-rendered on load).
fn line_body(line: &Line) -> String {
    if line.spans.len() <= 1 {
        return line.text();
    }
    let mut out = String::new();
    for span in &line.spans[1..] {
        out.push_str(&span.text);
    }
    out
}

/// The absolute time of an event: the IRCv3 `server-time` tag when present,
/// else the current local time.
fn event_time(tags: &[Tag]) -> i64 {
    for tag in tags {
        if tag.key == "time" {
            if let Some(value) = &tag.value {
                if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
                    return dt.timestamp();
                }
            }
        }
    }
    now_epoch()
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
    fn quoted_newline_is_a_space_in_irc_but_a_break_in_scratch() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("ab");
        press(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.buffers[ch].buffer.input(), "ab ");
        assert!(!app.buffers[ch].buffer.input().contains('\n'));

        let scratch = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        app.switch_to(scratch);
        let before = app.buffers[scratch].buffer.len();
        app.current_mut().buffer.beginning_of_buffer();
        app.current_mut().buffer.insert_str("xy");
        press(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.buffers[scratch].buffer.len(), before + 1);
        assert_eq!(app.buffers[scratch].buffer.line(0).text(), "xy");
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
    fn prefix_echo_waits_for_idle_delay() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(app.prefix, Prefix::CtrlC));
        // Nothing is echoed immediately.
        assert_eq!(app.message, None);
        assert!(app.prefix_echo.is_some());

        app.echo_pending_prefix(Instant::now() + PREFIX_ECHO_DELAY);
        assert_eq!(app.message.as_deref(), Some("C-c-"));
        assert!(app.prefix_echo.is_none());
    }

    #[test]
    fn completing_a_prefix_cancels_its_echo() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(matches!(app.prompt, Prompt::BrowseUrl { .. }));
        assert!(app.prefix_echo.is_none());
        assert_ne!(app.message.as_deref(), Some("C-c-"));
    }

    #[test]
    fn enter_accepts_forward_search_at_match_end() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        {
            let b = &mut app.current_mut().buffer;
            b.push_plain("alpha");
            b.push_plain("beta");
            b.push_plain("gamma");
            b.beginning_of_buffer();
        }
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        for c in "beta".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.current().buffer.point(), Pos::new(1, 0));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.search.active);
        assert_eq!(app.current().buffer.point(), Pos::new(1, 4));
    }

    #[test]
    fn motion_key_exits_isearch_and_moves() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        {
            let b = &mut app.current_mut().buffer;
            b.push_plain("alpha");
            b.push_plain("beta");
            b.push_plain("gamma");
            b.beginning_of_buffer();
        }
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        for c in "beta".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(app.search.active);
        let found = app.current().buffer.point();
        // C-n leaves isearch at the end of the match and moves down a line.
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(!app.search.active);
        assert_eq!(app.current().buffer.point(), Pos::new(found.row + 1, 4));
    }

    #[test]
    fn forward_search_motion_starts_at_match_end() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        {
            let b = &mut app.current_mut().buffer;
            b.push_plain("alpha");
            b.push_plain("beta");
            b.push_plain("gamma");
            b.beginning_of_buffer();
        }
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        for c in "beta".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.current().buffer.point(), Pos::new(1, 0));
        // Motion after a forward search starts at the end of "beta".
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(app.current().buffer.point(), Pos::new(1, 3));
    }

    #[test]
    fn backward_search_motion_starts_at_match_start() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        {
            let b = &mut app.current_mut().buffer;
            b.push_plain("alpha");
            b.push_plain("beta");
            b.push_plain("gamma");
            b.end_of_buffer();
        }
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        for c in "beta".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.current().buffer.point(), Pos::new(1, 0));
        // Motion after a backward search starts at the match's beginning.
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(app.current().buffer.point(), Pos::new(1, 1));
    }

    #[test]
    fn control_meta_beginning_of_buffer_exits_isearch() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        {
            let b = &mut app.current_mut().buffer;
            b.push_plain("alpha");
            b.push_plain("beta");
            b.push_plain("gamma");
            b.beginning_of_buffer();
        }
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        for c in "gamma".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(app.search.active);
        press(
            &mut app,
            KeyCode::Char('<'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(!app.search.active);
        assert_eq!(app.current().buffer.point(), Pos::new(0, 0));
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
            Prompt::Topic { input, default, .. } => {
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
    fn window_scrolls_only_at_the_edges() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        for i in 0..20 {
            app.push_line(ch, Line::plain(format!("line {i}")));
        }
        app.switch_to(ch);
        app.viewport_rows = 5;
        app.current_mut().buffer.end_of_buffer();
        app.ensure_scroll_visible();
        let bottom = app.current().buffer.scroll();
        // At the end, the window shows the last `height` rows.
        assert_eq!(bottom, app.current().buffer.last_row() + 1 - 5);

        // Moving up one line keeps the window put; point moves within it.
        app.perform(Action::PrevLine);
        app.ensure_scroll_visible();
        assert_eq!(
            app.current().buffer.scroll(),
            bottom,
            "should not scroll yet"
        );

        // Only when point reaches the top edge does the window scroll by one.
        for _ in 0..4 {
            app.perform(Action::PrevLine);
            app.ensure_scroll_visible();
        }
        assert_eq!(app.current().buffer.scroll(), bottom - 1);
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
    fn c_minus_undoes_input_edits_after_new_output() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello");
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.current().buffer.input(), "hell");
        app.current_mut()
            .buffer
            .push_plain("someone said something");
        press(&mut app, KeyCode::Char('-'), KeyModifiers::CONTROL);
        assert_eq!(app.current().buffer.input(), "hello");
        assert_eq!(app.message.as_deref(), Some("Undo"));
    }

    #[test]
    fn undo_without_history_reports_it() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        press(&mut app, KeyCode::Char('-'), KeyModifiers::CONTROL);
        assert_eq!(app.message.as_deref(), Some("No further undo information"));
    }

    #[test]
    fn ctrl_meta_underscore_redoes_input_edits() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello");
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('-'), KeyModifiers::CONTROL);
        assert_eq!(app.current().buffer.input(), "hello");
        press(
            &mut app,
            KeyCode::Char('_'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(app.current().buffer.input(), "hell");
        assert_eq!(app.message.as_deref(), Some("Redo"));
    }

    #[test]
    fn redo_without_an_undo_reports_it() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        press(
            &mut app,
            KeyCode::Char('_'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(app.message.as_deref(), Some("No undone changes to redo"));
    }

    #[test]
    fn c_x_c_x_exchanges_point_and_mark() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello world");
        app.current_mut().buffer.set_point(Pos::new(0, 8));
        app.current_mut().buffer.set_mark();
        app.current_mut().buffer.set_point(Pos::new(0, 13));
        press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(app.current().buffer.point(), Pos::new(0, 8));
        assert_eq!(app.current().buffer.region_text().as_deref(), Some("world"));
    }

    #[test]
    fn c_x_c_x_without_mark_reports_it() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(app.message.as_deref(), Some("No mark set in this buffer"));
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

    #[test]
    fn ctrl_space_sets_mark_for_copy_region() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello world");
        // Mark the end, move back over "world", then copy the region.
        press(&mut app, KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(app.message.as_deref(), Some("Mark set"));
        // NUL is another common encoding of C-SPC / C-@.
        press(&mut app, KeyCode::Char('\0'), KeyModifiers::NONE);
        assert_eq!(app.message.as_deref(), Some("Mark set"));
        for _ in 0..5 {
            press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('w'), KeyModifiers::ALT);
        assert_eq!(app.kill_ring.current().as_deref(), Some("world"));
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

        let mut cfg = test_config();
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
            nickserv: None,
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
    fn c_h_b_shows_help_buffer() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        press(&mut app, KeyCode::Char('h'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::NONE);
        let help = app
            .buffers
            .iter()
            .position(|b| b.target == "*Help*")
            .expect("help buffer created");
        let text: String = (0..app.buffers[help].buffer.len())
            .map(|r| app.buffers[help].buffer.line(r).text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("cricr key bindings"));
        assert!(text.contains("C-x b"));
        assert_eq!(app.current, help);
        // The list is shown from the top.
        assert_eq!(app.buffers[help].buffer.point(), Pos::new(0, 0));
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
    fn help_buffer_is_killable() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        press(&mut app, KeyCode::Char('h'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::NONE);
        let help = app
            .buffers
            .iter()
            .position(|b| b.target == "*Help*")
            .unwrap();
        let before = app.buffers.len();
        app.kill_buffer(help);
        assert_eq!(app.buffers.len(), before - 1);
        assert!(app.buffers.iter().all(|b| b.target != "*Help*"));
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
    fn topic_set_line_is_rendered() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.handle_event(
            0,
            Event::TopicSet {
                channel: "#c".to_string(),
                setter: "rcy".to_string(),
                time: 1_758_407_992,
            },
        );
        let text: Vec<String> = (0..app.buffers[ch].buffer.len())
            .map(|r| app.buffers[ch].buffer.line(r).text())
            .collect();
        assert!(text.iter().any(|l| l.contains("Topic set by rcy on")));
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
    async fn local_command_is_echoed() {
        let mut app = app_with_unreachable_server();
        let buf = app.servers[0].buffer;
        app.current_mut().buffer.set_input("/reconnect");
        app.submit_input();
        let text: Vec<String> = (0..app.buffers[buf].buffer.len())
            .map(|r| app.buffers[buf].buffer.line(r).text())
            .collect();
        assert!(text.iter().any(|l| l.contains(">>> /reconnect")));
    }

    #[test]
    fn slash_join_switches_to_the_channel() {
        let mut app = app_without_servers();
        let start = app.new_buffer(0, "#start".to_string(), Kind::Channel);
        app.switch_to(start);
        app.current_mut().buffer.set_input("/join #new");
        app.submit_input();
        let idx = app
            .buffers
            .iter()
            .position(|b| b.target == "#new" && b.kind == Kind::Channel)
            .expect("channel buffer created");
        assert_eq!(app.current, idx, "should switch to the joined channel");
    }

    #[test]
    fn slash_msg_creates_query_buffer_and_echoes() {
        let mut app = app_without_servers();
        let start = app.new_buffer(0, "#chan".to_string(), Kind::Channel);
        app.switch_to(start);
        app.current_mut().buffer.set_input("/msg bob hi there");
        app.submit_input();
        let idx = app
            .buffers
            .iter()
            .position(|b| b.target == "bob" && b.kind == Kind::Query)
            .expect("query buffer created for the nick");
        let text: Vec<String> = (0..app.buffers[idx].buffer.len())
            .map(|r| app.buffers[idx].buffer.line(r).text())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("hi there")),
            "sent message should appear in the query buffer: {text:?}"
        );
        assert_eq!(app.current, start, "stays in the originating buffer");
        // The sent line shows up as activity in the mode line.
        assert_eq!(app.buffers[idx].unread, 1, "query buffer has activity");
        let targets: Vec<String> = app.buffers.iter().map(|b| b.target.clone()).collect();
        let short = abbreviate(&targets);
        let bob = short[app.buffers.iter().position(|b| b.target == "bob").unwrap()].clone();
        assert!(
            app.activity_entries().iter().any(|(name, _)| *name == bob),
            "mode line should list the query buffer ({bob})"
        );
    }

    #[tokio::test]
    async fn own_join_records_activity() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let mut cfg = test_config();
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
            nickserv: None,
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

        let mut cfg = test_config();
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
            nickserv: None,
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

        let mut cfg = test_config();
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
            nickserv: None,
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
    fn paste_preserves_spaces() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.handle_paste("two  spaces and one".to_string());
        assert_eq!(app.current().buffer.input(), "two  spaces and one");
    }

    #[test]
    fn pasted_newline_does_not_send() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello");
        // A newline arriving in a paste burst folds into the input.
        app.rapid_key = true;
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.current().buffer.input(), "hello ");
        // A typed Enter still sends.
        app.rapid_key = false;
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.current().buffer.input(), "");
    }

    #[test]
    fn multiline_paste_burst_is_recognized() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ch = |c: char| TermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let enter = || TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // A typed letter then Enter: Enter is last, so it's not a paste.
        assert!(!is_multiline_paste(&[ch('a'), enter()]));
        // A multiline paste: the newline is followed by more text.
        assert!(is_multiline_paste(&[ch('a'), enter(), ch('b')]));
    }

    #[cfg(windows)]
    #[test]
    fn alacritty_space_promotion_skips_paste_bursts() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let space = || TermEvent::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let ch = |c: char| TermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));

        let mut lone = vec![space()];
        promote_alacritty_ctrl_space(&mut lone, true);
        let TermEvent::Key(key) = &lone[0] else {
            panic!("expected key");
        };
        assert!(key.modifiers.contains(KeyModifiers::CONTROL));

        // A pasted burst must keep its spaces as typed text.
        let mut burst = vec![ch('r'), ch('e'), space(), ch('d')];
        promote_alacritty_ctrl_space(&mut burst, true);
        let TermEvent::Key(key) = &burst[2] else {
            panic!("expected key");
        };
        assert!(!key.modifiers.contains(KeyModifiers::CONTROL));

        // Without Ctrl held, a lone space stays a space.
        let mut plain = vec![space()];
        promote_alacritty_ctrl_space(&mut plain, false);
        let TermEvent::Key(key) = &plain[0] else {
            panic!("expected key");
        };
        assert!(!key.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn burst_letter_enter_sends_but_multiline_paste_folds() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |c: char| TermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let enter = || TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);

        // Typing "!" then Enter quickly: Enter is the last key, so it sends.
        app.current_mut().buffer.set_input("hi");
        app.handle_term_batch(vec![key('!'), enter()]);
        assert_eq!(app.current().buffer.input(), "");

        // A multiline paste: the newline folds instead of sending.
        app.current_mut().buffer.set_input("a");
        app.handle_term_batch(vec![enter(), key('b')]);
        assert_eq!(app.current().buffer.input(), "a b");
    }

    #[test]
    fn paste_adds_to_kill_ring() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.handle_paste("hello\nworld".to_string());
        assert_eq!(app.current().buffer.input(), "hello world");
        assert_eq!(app.kill_ring.current().as_deref(), Some("hello world"));
    }

    #[test]
    fn osc52_sequence_encodes_text() {
        assert_eq!(
            osc52_sequence("hi").as_deref(),
            Some("\u{1b}]52;c;aGk=\u{7}")
        );
        assert_eq!(osc52_sequence(""), None);
    }

    #[test]
    fn title_sequence_sanitizes_and_wraps() {
        assert_eq!(title_sequence("cricr"), "\u{1b}]2;cricr\u{7}");
        // Control characters (a stray BEL/ESC) can't inject escapes.
        assert_eq!(title_sequence("a\u{7}b\u{1b}c"), "\u{1b}]2;abc\u{7}");
    }

    #[test]
    fn terminal_title_lists_active_buffers() {
        let mut app = app_without_servers();
        app.config.ui.title_badge = true;
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        let b = app.new_buffer(0, "bob".to_string(), Kind::Query);
        app.switch_to(a);
        assert_eq!(app.terminal_title(), "cricr");

        app.bump_activity(b, false);
        let title = app.terminal_title();
        assert!(title.starts_with("cricr ["), "unexpected title: {title}");
        assert!(title.contains('b'), "activity buffer missing: {title}");

        // Visiting the buffer clears its activity, dropping it from the title.
        app.switch_to(b);
        assert_eq!(app.terminal_title(), "cricr");
    }

    #[test]
    fn idle_marker_badges_the_current_buffer_title() {
        let mut app = app_without_servers();
        app.config.ui.title_badge = true;
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        assert_eq!(app.terminal_title(), "cricr");

        // An idle post in the visible buffer drops the separator and badges it.
        app.idle_mark = Duration::ZERO;
        app.push_line(ch, Line::plain("hi"));
        app.bump_activity(ch, false);
        let title = app.terminal_title();
        assert!(title.starts_with("cricr ["), "expected a badge: {title}");

        // Leaving the buffer clears the badge (and the separator).
        let scratch = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        app.switch_to(scratch);
        assert_eq!(app.terminal_title(), "cricr");
    }

    #[test]
    fn activity_clears_the_current_buffer_badge() {
        let mut app = app_without_servers();
        app.config.ui.title_badge = true;
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        // An idle post badges the buffer being viewed.
        app.idle_mark = Duration::ZERO;
        app.push_line(ch, Line::plain("hi"));
        app.bump_activity(ch, false);
        assert!(app.buffers[ch].idle_marked);
        assert!(app.terminal_title().starts_with("cricr ["));

        // Being active in the buffer clears the badge (the separator stays).
        app.perform(Action::PrevLine);
        assert!(!app.buffers[ch].idle_marked);
        assert_eq!(app.terminal_title(), "cricr");
        assert!(
            app.buffers[ch].marker_row.is_some(),
            "separator should remain"
        );
    }

    #[test]
    fn prefix_commands_clear_the_current_buffer_badge() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        app.config.ui.title_badge = true;
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.idle_mark = Duration::ZERO;
        app.push_line(ch, Line::plain("hi"));
        app.bump_activity(ch, false);
        assert!(app.buffers[ch].idle_marked);
        assert!(app.terminal_title().starts_with("cricr ["));

        // C-c C-b opens the browse prompt; the prefix itself acknowledges the
        // buffer even though it never reaches `perform`.
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(matches!(app.prompt, Prompt::BrowseUrl { .. }));
        assert!(!app.buffers[ch].idle_marked);
        assert_eq!(app.terminal_title(), "cricr");
    }

    #[test]
    fn user_activity_resets_the_idle_timer() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        // The buffer has been quiet for a long time...
        app.buffers[ch].last_activity_at = Instant::now()
            .checked_sub(Duration::from_secs(600))
            .unwrap_or_else(Instant::now);
        // ...then the user posts.
        app.perform(Action::Send);
        // A reply moments later is not idle activity: no separator, no badge.
        app.push_line(ch, Line::plain("reply"));
        app.bump_activity(ch, false);
        assert!(
            app.buffers[ch].marker_row.is_none(),
            "a reply right after posting must not be marked unread"
        );
        assert!(!app.buffers[ch].idle_marked);
    }

    #[test]
    fn title_badge_off_does_not_touch_title() {
        let mut app = app_without_servers();
        app.config.ui.title_badge = false;
        let b = app.new_buffer(0, "bob".to_string(), Kind::Query);
        app.bump_activity(b, false);
        // Disabled: sync is a no-op and nothing is cached (or written).
        app.sync_title();
        assert!(app.last_title.is_empty());
    }

    #[test]
    fn paste_into_scratch_keeps_newlines() {
        let mut app = app_without_servers();
        // Clear the seeded startup text so the paste lands at the top.
        app.current_mut().buffer.clear();
        app.handle_paste("a\nb".to_string());
        assert_eq!(app.current().buffer.line(0).text(), "a");
        assert_eq!(app.current().buffer.line(1).text(), "b");
    }

    #[test]
    fn scratch_buffer_shows_startup_help() {
        let app = app_without_servers();
        let scratch = app.current();
        let text: String = (0..scratch.buffer.len())
            .map(|r| scratch.buffer.line(r).text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("C-h b"), "should mention C-h b for bindings");
        assert!(
            text.contains("cricr --config"),
            "should explain config file"
        );
        assert!(text.to_lowercase().contains("configuration"));
    }

    fn hist_config(dir: &Path) -> Config {
        let mut cfg: Config = toml::from_str(
            "[history]\nenabled = true\n[[servers]]\nname = \"127.0.0.1\"\nport = 1\ntls = false\nnick = \"nick\"\n",
        )
        .unwrap();
        cfg.history.dir = Some(dir.to_path_buf());
        cfg
    }

    fn push_history_line(app: &mut App, ch: usize, body: &str, time: i64) {
        let mut line = Line::styled(format!("<alice> {body}"), Style::default());
        line.time = Some(time);
        app.push_line(ch, line);
    }

    /// An epoch timestamp for noon local time on the given date.
    fn ts(year: i32, month: u32, day: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp()
    }

    fn buffer_text(app: &App, idx: usize) -> Vec<String> {
        (0..app.buffers[idx].buffer.len())
            .map(|r| app.buffers[idx].buffer.line(r).text())
            .collect()
    }

    #[test]
    fn month_change_inserts_date_separator() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        push_history_line(&mut app, ch, "one", ts(2026, 4, 10));
        push_history_line(&mut app, ch, "two", ts(2026, 4, 20));
        push_history_line(&mut app, ch, "three", ts(2026, 5, 2));
        let text = buffer_text(&app, ch);
        // Only the month *change* is marked; the first line is not.
        assert!(!text.iter().any(|l| l.starts_with("Apr 2026")), "{text:?}");
        let separators: Vec<&String> = text.iter().filter(|l| l.starts_with("May 2026")).collect();
        assert_eq!(separators.len(), 1, "{text:?}");
        assert!(separators[0].contains("---"), "{text:?}");
    }

    #[tokio::test]
    async fn history_load_inserts_date_separators() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-months-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = hist_config(&dir);
        {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new(cfg.clone(), tx);
            let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
            push_history_line(&mut app, ch, "apr", ts(2026, 4, 1));
            push_history_line(&mut app, ch, "may", ts(2026, 5, 1));
            app.save_all_history();
        }

        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(cfg, tx);
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        let text = buffer_text(&app, ch);
        assert!(
            text.iter().any(|l| l.starts_with("May 2026")),
            "date separator missing on reload: {text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn history_is_saved_and_prefilled_on_join() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = hist_config(&dir);

        {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new(cfg.clone(), tx);
            let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
            let mut line = Line::styled("<alice> hello there", Style::default());
            line.time = Some(1_700_000_000);
            app.push_line(ch, line);
            app.save_all_history();
        }

        // A fresh app loads the cache when the channel buffer is created.
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(cfg, tx);
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        let text: Vec<String> = (0..app.buffers[ch].buffer.len())
            .map(|r| app.buffers[ch].buffer.line(r).text())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("hello there")),
            "preloaded history missing: {text:?}"
        );
        // An unread separator marks where the preloaded history ends.
        let marker = app.buffers[ch]
            .marker_row
            .expect("history separator inserted");
        assert!(
            text[marker].starts_with("---"),
            "not a separator: {:?}",
            text[marker]
        );
        let hello = text.iter().position(|l| l.contains("hello there")).unwrap();
        assert!(marker > hello, "separator should follow the history");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn history_keeps_preloaded_lines_across_saves() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = hist_config(&dir);

        // First session writes one line.
        {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new(cfg.clone(), tx);
            let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
            let mut line = Line::styled("<alice> first", Style::default());
            line.time = Some(1_700_000_000);
            app.push_line(ch, line);
            app.save_all_history();
        }

        // Second session loads it, adds a line, and saves again. The save must
        // include the preloaded line, not just the newly added one.
        {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new(cfg.clone(), tx);
            let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
            let mut line = Line::styled("<bob> second", Style::default());
            line.time = Some(1_700_000_060);
            app.push_line(ch, line);
            app.save_all_history();
        }

        // Third session still sees both lines.
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(cfg, tx);
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        let text: Vec<String> = (0..app.buffers[ch].buffer.len())
            .map(|r| app.buffers[ch].buffer.line(r).text())
            .collect();
        assert!(
            text.iter().any(|l| l.contains("first")),
            "preloaded history dropped after a later save: {text:?}"
        );
        assert!(text.iter().any(|l| l.contains("second")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn history_appends_new_lines_across_saves() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-append-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = hist_config(&dir);

        {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new(cfg.clone(), tx);
            let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
            push_history_line(&mut app, ch, "one", 1_700_000_000);
            app.save_all_history();
        }
        {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new(cfg.clone(), tx);
            let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
            push_history_line(&mut app, ch, "two", 1_700_000_001);
            app.save_all_history();
            push_history_line(&mut app, ch, "three", 1_700_000_002);
            app.save_all_history();
        }

        let entries = history::load(&dir, "127.0.0.1", "#c", 10);
        assert_eq!(entries.len(), 3, "entries: {entries:?}");
        for word in ["one", "two", "three"] {
            let n = entries.iter().filter(|(_, b)| b.contains(word)).count();
            assert_eq!(n, 1, "{word} should appear once: {entries:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn history_grows_beyond_scrollback() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-grow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = hist_config(&dir);
        cfg.ui.scrollback_lines = 2;

        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(cfg, tx);
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        push_history_line(&mut app, ch, "one", 1_700_000_000);
        push_history_line(&mut app, ch, "two", 1_700_000_001);
        push_history_line(&mut app, ch, "three", 1_700_000_002);
        app.save_all_history();

        // The on-screen scrollback is capped...
        assert!(
            app.buffers[ch].buffer.len() <= 2,
            "display should be capped at scrollback_lines"
        );
        // ...but the cache keeps every line.
        let entries = history::load(&dir, "127.0.0.1", "#c", 10);
        assert_eq!(entries.len(), 3, "cache must not truncate: {entries:?}");
        assert!(entries.iter().any(|(_, b)| b.contains("one")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn history_reload_rewraps_to_current_width() {
        let dir = std::env::temp_dir().join(format!("cricr-hist-wrap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = hist_config(&dir);
        cfg.ui.fill_column = 70;

        let long = format!("<alice> {}", "word ".repeat(40));
        {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new(cfg.clone(), tx);
            let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
            let mut line = Line::styled(long.clone(), Style::default());
            line.time = Some(1_700_000_000);
            app.push_line(ch, line);
            app.save_all_history();
        }

        // Reloading at a much narrower width wraps the one logical line.
        cfg.ui.fill_column = 20;
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new(cfg, tx);
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        assert!(
            app.buffers[ch].buffer.len() > 3,
            "expected the long line to wrap into several rows"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_capture_is_skipped_when_disabled() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        let mut line = Line::styled("<alice> hi", Style::default());
        line.time = Some(1_700_000_000);
        app.push_line(ch, line);
        assert!(app.buffers[ch].history.is_empty());
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
    fn current_buffer_marks_unread_only_after_idle() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.idle_mark = Duration::from_secs(3600); // not idle

        // Fresh activity in the buffer we are viewing is not marked.
        app.push_line(ch, Line::plain("now"));
        app.bump_activity(ch, false);
        assert!(
            app.buffers[ch].marker_row.is_none(),
            "no marker while active"
        );
        assert_eq!(app.buffers[ch].unread, 0);

        // After a quiet spell, activity in the current buffer is marked.
        app.idle_mark = Duration::ZERO; // always idle
        app.push_line(ch, Line::plain("later"));
        app.bump_activity(ch, false);
        assert!(app.buffers[ch].marker_row.is_some(), "marker after idle");
        assert_eq!(
            app.buffers[ch].first_unread,
            app.buffers[ch].marker_row.map(|r| r + 1)
        );
        // The current buffer stays out of the mode-line activity list.
        assert_eq!(app.buffers[ch].unread, 0);
    }

    #[test]
    fn idle_post_moves_the_current_buffer_marker() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.idle_mark = Duration::ZERO; // always idle

        // A first idle post drops the separator.
        app.push_line(ch, Line::plain("first"));
        app.bump_activity(ch, false);
        let first = app.buffers[ch].marker_row.expect("separator inserted");

        // Pressing a key does not move it.
        app.perform(Action::PrevLine);
        assert_eq!(
            app.buffers[ch].marker_row,
            Some(first),
            "keys must not move the separator"
        );

        // A later idle post moves the separator down to just before it.
        app.push_line(ch, Line::plain("second"));
        app.bump_activity(ch, false);
        let moved = app.buffers[ch].marker_row.expect("separator kept");
        assert!(moved > first, "separator should move to the new post");
        assert_eq!(app.buffers[ch].first_unread, Some(moved + 1));
        assert!(app.buffers[ch].buffer.line(moved).text().starts_with("---"));
        assert!(app.buffers[ch]
            .buffer
            .line(moved + 1)
            .text()
            .contains("second"));
    }

    #[test]
    fn idle_separator_stays_put_while_the_buffer_stays_idle() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.idle_mark = Duration::from_secs(300);
        let quiet = || {
            Instant::now()
                .checked_sub(Duration::from_secs(600))
                .unwrap_or_else(Instant::now)
        };

        // The first post after a quiet spell drops the separator.
        app.buffers[ch].last_activity_at = quiet();
        app.push_line(ch, Line::plain("post 0"));
        app.bump_activity(ch, false);
        let first = app.buffers[ch].marker_row.expect("separator inserted");
        assert!(app.buffers[ch].idle_marked);

        // Later output while the user stays away grows the unread region below
        // the separator instead of dragging it forward, even across further
        // quiet spells.
        for i in 1..4 {
            app.buffers[ch].last_activity_at = quiet();
            app.push_line(ch, Line::plain(format!("post {i}")));
            app.bump_activity(ch, false);
            assert_eq!(
                app.buffers[ch].marker_row,
                Some(first),
                "separator must not drift while the user is idle"
            );
        }
        assert!(app.buffers[ch]
            .buffer
            .line(first + 1)
            .text()
            .contains("post 0"));

        // Acknowledging the buffer lets a fresh quiet spell move it.
        app.perform(Action::PrevLine);
        app.buffers[ch].last_activity_at = quiet();
        app.push_line(ch, Line::plain("post 4"));
        app.bump_activity(ch, false);
        assert!(
            app.buffers[ch].marker_row.unwrap() > first,
            "separator moves once the buffer has been acknowledged"
        );
    }

    #[test]
    fn separator_lands_above_the_new_line_when_scrollback_is_trimmed() {
        let mut app = app_without_servers();
        app.scrollback = 4;
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        let scratch = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        app.switch_to(scratch);
        for i in 0..6 {
            app.push_line(a, Line::plain(format!("line {i}")));
        }
        // The buffer is now at the scrollback limit; new unread arrives.
        app.push_line(a, Line::plain("LAST"));
        app.bump_activity(a, false);
        let row = app.buffers[a].marker_row.expect("separator inserted");
        assert!(app.buffers[a].buffer.line(row + 1).text().contains("LAST"));
    }

    #[test]
    fn second_unread_activity_inserts_a_new_separator() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        let scratch = app
            .buffers
            .iter()
            .position(|b| b.kind == Kind::Scratch)
            .unwrap();
        app.switch_to(scratch);

        app.push_line(a, Line::plain("one"));
        app.bump_activity(a, false);
        assert!(app.buffers[a].marker_row.is_some(), "first separator");

        // Visiting and leaving the buffer removes its separator...
        app.switch_to(a);
        app.switch_to(scratch);
        assert!(app.buffers[a].marker_row.is_none());

        // ...but a later message must drop a fresh one.
        app.push_line(a, Line::plain("two"));
        app.bump_activity(a, false);
        let row = app.buffers[a]
            .marker_row
            .expect("separator after new activity");
        assert!(app.buffers[a].buffer.line(row + 1).text().contains("two"));
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

    /// A config with the scrollback cache disabled, so tests stay hermetic
    /// (never read or write the real user cache directory).
    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.history.enabled = false;
        cfg
    }

    fn app_without_servers() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(test_config(), tx)
    }

    /// An app with one server pointed at an unreachable port, so the client
    /// task spawns (and fails) without needing a real server.
    fn app_with_unreachable_server() -> App {
        let cfg: Config = toml::from_str(
            "[history]\nenabled = false\n[[servers]]\nname = \"127.0.0.1\"\nport = 1\ntls = false\nnick = \"nick\"\n",
        )
        .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(cfg, tx)
    }

    #[test]
    fn reconnect_delay_is_exponential_and_capped() {
        assert_eq!(reconnect_delay(1, 1.0, 120.0), 1.0);
        assert_eq!(reconnect_delay(2, 1.0, 120.0), 2.0);
        assert_eq!(reconnect_delay(3, 1.0, 120.0), 4.0);
        assert_eq!(reconnect_delay(8, 1.0, 120.0), 120.0); // 128 -> capped
        assert_eq!(reconnect_delay(3, 5.0, 20.0), 20.0); // 20 -> capped
    }

    #[tokio::test]
    async fn unexpected_disconnect_schedules_reconnect() {
        let mut app = app_with_unreachable_server();
        assert_eq!(app.servers.len(), 1);
        app.handle_event(
            0,
            Event::Disconnected {
                reason: "Connection closed".into(),
            },
        );
        assert!(app.servers[0].next_at.is_some());
        assert_eq!(app.servers[0].attempt, 1);

        app.handle_event(
            0,
            Event::ConnectionError {
                error: "boom".into(),
            },
        );
        assert_eq!(app.servers[0].attempt, 2);
    }

    #[tokio::test]
    async fn reconnect_failures_report_the_attempt_number() {
        let mut app = app_with_unreachable_server();
        let server_buf = app.servers[0].buffer;

        // The first failure of an outage shows the underlying error once, and
        // each retry reports its attempt number instead of repeating it.
        app.handle_event(
            0,
            Event::ConnectionError {
                error: "boom".into(),
            },
        );
        app.handle_event(
            0,
            Event::ConnectionError {
                error: "boom".into(),
            },
        );
        app.handle_event(
            0,
            Event::ConnectionError {
                error: "boom".into(),
            },
        );

        let text = buffer_text(&app, server_buf).join("\n");
        assert_eq!(
            text.matches("boom").count(),
            1,
            "raw error shown once: {text}"
        );
        assert!(text.contains("Reconnecting to 127.0.0.1 in"), "{text}");
        for attempt in 1..=3 {
            assert!(
                text.contains(&format!("(attempt {attempt})")),
                "missing attempt {attempt}: {text}"
            );
        }
        assert_eq!(app.servers[0].attempt, 3);
    }

    #[tokio::test]
    async fn quit_and_disabled_config_do_not_reconnect() {
        let mut app = app_with_unreachable_server();
        app.handle_event(
            0,
            Event::Disconnected {
                reason: "Quit".into(),
            },
        );
        assert!(app.servers[0].next_at.is_none());
        assert_eq!(app.servers[0].attempt, 0);

        app.config.reconnect.enabled = false;
        app.handle_event(
            0,
            Event::ConnectionError {
                error: "boom".into(),
            },
        );
        assert!(app.servers[0].next_at.is_none());
    }

    #[tokio::test]
    async fn connected_event_resets_backoff() {
        let mut app = app_with_unreachable_server();
        app.handle_event(
            0,
            Event::Disconnected {
                reason: "dropped".into(),
            },
        );
        assert_eq!(app.servers[0].attempt, 1);
        app.handle_event(0, Event::Connected);
        assert_eq!(app.servers[0].attempt, 0);
        assert!(app.servers[0].next_at.is_none());
        assert!(app.servers[0].connected);
    }

    #[tokio::test]
    async fn join_part_kick_track_channels_for_reconnect() {
        let mut app = app_with_unreachable_server();
        let nick = app.servers[0].our_nick.clone();
        app.handle_event(
            0,
            Event::Join {
                channel: "#a".into(),
                nick: nick.clone(),
            },
        );
        assert!(app.servers[0].channels.contains("#a"));

        // Someone else's join is not remembered.
        app.handle_event(
            0,
            Event::Join {
                channel: "#b".into(),
                nick: "someone".into(),
            },
        );
        assert!(!app.servers[0].channels.contains("#b"));

        app.handle_event(
            0,
            Event::Part {
                channel: "#a".into(),
                nick: nick.clone(),
                reason: String::new(),
            },
        );
        assert!(!app.servers[0].channels.contains("#a"));

        app.handle_event(
            0,
            Event::Join {
                channel: "#c".into(),
                nick: nick.clone(),
            },
        );
        app.handle_event(
            0,
            Event::Kick {
                channel: "#c".into(),
                nick,
                by: "op".into(),
                reason: String::new(),
            },
        );
        assert!(!app.servers[0].channels.contains("#c"));
    }

    #[tokio::test]
    async fn reconnect_command_clears_pending_attempt() {
        use cricr_irc::commands::LocalCommand;
        let mut app = app_with_unreachable_server();
        app.servers[0].attempt = 3;
        app.servers[0].next_at = Some(Instant::now() + Duration::from_secs(60));
        app.handle_local(0, LocalCommand::Reconnect);
        assert_eq!(app.servers[0].attempt, 0);
        assert!(app.servers[0].next_at.is_none());
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
    fn next_active_run_returns_to_where_it_started() {
        let mut app = app_without_servers();
        let scratch = app.current;
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        let b = app.new_buffer(0, "#b".to_string(), Kind::Channel);
        let c = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(a);
        app.bump_activity(b, false);
        app.bump_activity(c, false);

        app.next_active();
        assert_eq!(app.current, c, "most recent activity");
        app.next_active();
        assert_eq!(app.current, b, "next-most-recent");
        app.next_active();
        assert_eq!(
            app.current, a,
            "run exhausted -> back to the start, not *scratch*"
        );
        // A fresh run with nothing to visit still falls back to *scratch*.
        app.next_active();
        assert_eq!(app.current, scratch);
    }

    #[test]
    fn next_active_run_ends_on_another_command() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        let b = app.new_buffer(0, "#b".to_string(), Kind::Channel);
        let c = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(a);
        app.bump_activity(b, false);
        app.bump_activity(c, false);

        app.next_active();
        assert_eq!(app.current, c, "most recent activity");
        // Typing in the buffer ends the run...
        app.perform(Action::SelfInsert('x'));
        app.next_active();
        assert_eq!(app.current, b, "a new run starts from #c");
        app.next_active();
        assert_eq!(app.current, c, "returns to where this run started");
    }

    #[test]
    fn switching_preserves_point_unless_it_was_at_the_end() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        let b = app.new_buffer(0, "#b".to_string(), Kind::Channel);
        for i in 0..20 {
            app.push_line(a, Line::plain(format!("line {i}")));
        }

        // Left in the middle: returning keeps the same point.
        app.switch_to(a);
        app.current_mut().buffer.beginning_of_buffer();
        let top = app.buffers[a].buffer.point();
        app.switch_to(b);
        app.switch_to(a);
        assert_eq!(app.buffers[a].buffer.point(), top);

        // Left at the end: returning follows new output to the new end.
        app.current_mut().buffer.end_of_buffer();
        app.switch_to(b);
        app.push_line(a, Line::plain("newest"));
        app.switch_to(a);
        assert!(app.buffers[a].buffer.at_end());
        assert_eq!(
            app.buffers[a].buffer.point().row,
            app.buffers[a].buffer.last_row()
        );
    }

    #[test]
    fn switch_buffer_prompt_resolves_names() {
        let mut app = app_without_servers();
        let a = app.new_buffer(0, "#a".to_string(), Kind::Channel);
        app.prompt = Prompt::SwitchBuffer {
            input: "#a".to_string(),
            cursor: 2,
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
            cursor: 0,
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

    fn prompt_input(app: &App) -> String {
        match &app.prompt {
            Prompt::SwitchBuffer { input, .. }
            | Prompt::BrowseUrl { input, .. }
            | Prompt::KillBuffer { input, .. }
            | Prompt::Topic { input, .. } => input.clone(),
            Prompt::None => String::new(),
        }
    }

    fn prompt_cursor(app: &App) -> usize {
        match &app.prompt {
            Prompt::SwitchBuffer { cursor, .. }
            | Prompt::BrowseUrl { cursor, .. }
            | Prompt::KillBuffer { cursor, .. }
            | Prompt::Topic { cursor, .. } => *cursor,
            Prompt::None => 0,
        }
    }

    #[test]
    fn prompt_motion_and_insert_in_the_middle() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        app.start_browse_url();
        for c in "hello".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(prompt_cursor(&app), 3);
        press(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(prompt_input(&app), "helXlo");
        assert_eq!(prompt_cursor(&app), 4);
        press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(prompt_cursor(&app), 0);
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(prompt_cursor(&app), 6);
    }

    #[test]
    fn prompt_forward_and_backward_delete() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        app.start_browse_url();
        for c in "abc".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        // Backspace at end removes 'c'.
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(prompt_input(&app), "ab");
        // C-a then C-d removes 'a' (forward delete).
        press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(prompt_input(&app), "b");
    }

    #[test]
    fn prompt_word_motion_and_kill_yank() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        app.start_browse_url();
        for c in "one two".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::ALT);
        assert_eq!(prompt_cursor(&app), 3);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(prompt_input(&app), "one");
        assert_eq!(app.kill_ring.current().as_deref(), Some(" two"));
        press(&mut app, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(prompt_input(&app), "one two");
        // M-b moves back to the start of "two".
        press(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        assert_eq!(prompt_cursor(&app), 4);
    }

    #[test]
    fn prompt_kill_word_forward() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        app.start_browse_url();
        for c in "one two".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        // M-d kills the word at point.
        press(&mut app, KeyCode::Char('d'), KeyModifiers::ALT);
        assert_eq!(prompt_input(&app), " two");
        assert_eq!(app.kill_ring.current().as_deref(), Some("one"));
    }

    #[test]
    fn prompt_upcase_and_downcase_word() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        app.start_browse_url();
        for c in "hello world".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('u'), KeyModifiers::ALT);
        assert_eq!(prompt_input(&app), "HELLO world");
        assert_eq!(prompt_cursor(&app), 5);
        press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::ALT);
        assert_eq!(prompt_input(&app), "hello world");
        press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::ALT);
        assert_eq!(prompt_input(&app), "Hello world");
        assert_eq!(prompt_cursor(&app), 5);
    }

    #[test]
    fn upcase_word_action_edits_buffer_input() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello there");
        app.current_mut().buffer.beginning_of_line();
        app.perform(Action::UpcaseWord);
        assert_eq!(app.current().buffer.input(), "HELLO there");
        app.current_mut().buffer.beginning_of_line();
        app.perform(Action::DowncaseWord);
        assert_eq!(app.current().buffer.input(), "hello there");
        app.current_mut().buffer.set_input("hELLO wORLD");
        app.current_mut().buffer.beginning_of_line();
        app.perform(Action::CapitalizeWord);
        assert_eq!(app.current().buffer.input(), "Hello wORLD");
    }

    #[test]
    fn transpose_actions_edit_buffer_input() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("teh");
        app.perform(Action::TransposeChars);
        assert_eq!(app.current().buffer.input(), "the");
        app.current_mut().buffer.set_input("hello world");
        app.perform(Action::TransposeWords);
        assert_eq!(app.current().buffer.input(), "world hello");
    }

    #[test]
    fn prompt_transpose_bindings() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        app.start_browse_url();
        for c in "teh".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(prompt_input(&app), "the");
        assert_eq!(prompt_cursor(&app), 3);

        let mut app = app_without_servers();
        app.start_browse_url();
        for c in "hello world".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Char('t'), KeyModifiers::ALT);
        assert_eq!(prompt_input(&app), "world hello");
        assert_eq!(prompt_cursor(&app), 11);
    }

    #[test]
    fn dabbrev_expand_cycles_candidates() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("regina reginald regi");
        // Point is at the end; M-/ expands the word "regi".
        app.perform(Action::DabbrevExpand);
        assert_eq!(app.current().buffer.input(), "regina reginald reginald");
        // A second M-/ cycles to the next candidate.
        app.perform(Action::DabbrevExpand);
        assert_eq!(app.current().buffer.input(), "regina reginald regina");
        // A third cycles back to the first.
        app.perform(Action::DabbrevExpand);
        assert_eq!(app.current().buffer.input(), "regina reginald reginald");
    }

    #[test]
    fn dabbrev_completion_inserts_common_prefix_and_lists_matches() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut()
            .buffer
            .set_input("configuration configure conf");
        app.perform(Action::DabbrevComplete);
        assert_eq!(
            app.current().buffer.input(),
            "configuration configure configur"
        );
        let completions = app.completions.as_ref().unwrap();
        assert_eq!(completions.len(), 2);
        assert!(completions.contains(&"configuration".to_string()));
        assert!(completions.contains(&"configure".to_string()));
        assert_eq!(app.minibuffer_height(), 3);
    }

    #[test]
    fn dabbrev_completion_inserts_a_unique_match() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello hel");
        app.perform(Action::DabbrevComplete);
        assert_eq!(app.current().buffer.input(), "hello hello");
        assert!(app.completions.is_none());
    }

    #[test]
    fn dabbrev_completion_without_matches_reports_it() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("zzz");
        app.perform(Action::DabbrevComplete);
        assert_eq!(app.current().buffer.input(), "zzz");
        assert_eq!(
            app.message.as_deref(),
            Some("No dynamic expansion for \"zzz\" found")
        );
    }

    #[test]
    fn c_m_slash_runs_dabbrev_completion_and_next_key_hides_it() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut()
            .buffer
            .set_input("configuration configure conf");
        press(
            &mut app,
            KeyCode::Char('/'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(
            app.current().buffer.input(),
            "configuration configure configur"
        );
        assert!(app.completions.is_some());
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(app.completions.is_none());
        assert_eq!(
            app.current().buffer.input(),
            "configuration configure configurx"
        );
    }

    #[test]
    fn c_n_on_last_line_reports_end_of_buffer() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello");
        app.current_mut().buffer.beginning_of_line();
        let base = app.current().buffer.prompt_len();
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(app.current().buffer.point().col, base + 5);
        assert_eq!(app.message.as_deref(), Some("End of buffer"));
    }

    #[test]
    fn c_u_c_n_moves_down_four_lines() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        for i in 0..10 {
            app.push_line(ch, Line::plain(format!("line {i}")));
        }
        app.switch_to(ch);
        app.current_mut().buffer.beginning_of_buffer();
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(app.current().buffer.point().row, 4);
    }

    #[test]
    fn c_u_3_c_f_moves_forward_three() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("abcdef");
        app.current_mut().buffer.beginning_of_line();
        let base = app.current().buffer.prompt_len();
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('3'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(app.current().buffer.point().col, base + 3);
    }

    #[test]
    fn m_minus_m_u_upcases_previous_word() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        app.current_mut().buffer.set_input("hello world");
        let base = app.current().buffer.prompt_len();
        press(&mut app, KeyCode::Char('-'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('u'), KeyModifiers::ALT);
        assert_eq!(app.current().buffer.input(), "hello WORLD");
        // Point stays where it was (at the end of the line).
        assert_eq!(app.current().buffer.point().col, base + 11);

        app.current_mut().buffer.set_input("hello WORLD");
        press(&mut app, KeyCode::Char('-'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::ALT);
        assert_eq!(app.current().buffer.input(), "hello world");
        assert_eq!(app.current().buffer.point().col, base + 11);

        app.current_mut().buffer.set_input("hello WORLD");
        press(&mut app, KeyCode::Char('-'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::ALT);
        assert_eq!(app.current().buffer.input(), "hello World");
        assert_eq!(app.current().buffer.point().col, base + 11);
    }

    #[test]
    fn numeric_argument_repeats_self_insert_and_multiplies() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        // C-u 3 x -> "xxx"
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('3'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(app.current().buffer.input(), "xxx");
        app.current_mut().buffer.set_input("");
        // C-u C-u z -> 16 z's
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('z'), KeyModifiers::NONE);
        assert_eq!(app.current().buffer.input(), "z".repeat(16));
    }

    #[test]
    fn nick_and_url_spans_are_marked() {
        let app = app_without_servers();
        let mut mentioned = false;
        let base = Style::default();
        let spans = app.nick_spans("hey crictest!", "crictest", base, &mut mentioned);
        assert!(mentioned);
        assert!(spans
            .iter()
            .any(|s| s.text == "crictest" && s.style == app.theme.nick_in_message));

        let mut mentioned = false;
        let url_spans = app.token_spans(
            "see https://example.com/x.",
            "crictest",
            base,
            &mut mentioned,
        );
        assert!(url_spans
            .iter()
            .any(|s| s.style.bold && s.text == "https://example.com/x"));
    }

    #[test]
    fn server_line_marks_urls() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        let line = app.server_line(ch, None, "TOPIC", "see https://example.com/x now");
        assert!(line
            .spans
            .iter()
            .any(|s| s.text == "https://example.com/x" && s.style.bold));
    }

    #[test]
    fn url_at_point_finds_the_url_under_cursor() {
        let mut app = app_without_servers();
        let ch = app.new_buffer(0, "#c".to_string(), Kind::Channel);
        app.switch_to(ch);
        {
            let b = &mut app.current_mut().buffer;
            b.push_plain("see https://example.com/x now");
            b.set_point(Pos::new(0, 8));
        }
        assert_eq!(app.url_at_point().as_deref(), Some("https://example.com/x"));
        // Whitespace outside the URL does not count.
        app.current_mut().buffer.set_point(Pos::new(0, 3));
        assert_eq!(app.url_at_point(), None);
        app.current_mut().buffer.set_point(Pos::new(0, 27));
        assert_eq!(app.url_at_point(), None);
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
