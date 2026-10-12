//! Rendering with ratatui.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as RColor, Modifier, Style as RStyle, Stylize};
use ratatui::text::{Line as RLine, Span as RSpan, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::app::{App, Kind};
use crate::buffer::{Color, Line, Pos, Style};
use crate::search::SearchDirection;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let (main_area, sidebar_area) = if app.sidebar && area.width > 50 {
        let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(24)]).split(rows[0]);
        (cols[0], Some(cols[1]))
    } else {
        (rows[0], None)
    };

    draw_buffer(frame, app, main_area);
    if let Some(sidebar) = sidebar_area {
        draw_sidebar(frame, app, sidebar);
    }
    draw_status(frame, app, rows[1]);
    draw_minibuffer(frame, app, rows[2]);
}

fn draw_buffer(frame: &mut Frame, app: &App, area: Rect) {
    let chan = app.current();
    let buf = &chan.buffer;
    let title = buffer_title(app, chan);
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;
    if height == 0 {
        return;
    }
    let point = buf.point();
    let mut top = buf.scroll();
    if point.row < top {
        top = point.row;
    }
    if point.row >= top + height {
        top = point.row + 1 - height;
    }

    let highlights = if app.search.active && !app.search.query.is_empty() {
        buf.matches(&app.search.query)
    } else {
        Vec::new()
    };

    let mut lines: Vec<RLine> = Vec::with_capacity(height);
    for row in top..buf.len().min(top + height) {
        let mut cells = line_cells(buf.line(row));
        apply_highlights(&mut cells, row, &highlights);
        if row == point.row {
            place_cursor(&mut cells, point.col);
        }
        lines.push(to_rline(&cells));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn buffer_title(app: &App, chan: &crate::app::Chan) -> String {
    let server = app
        .servers
        .get(chan.server)
        .map(|s| s.name.as_str())
        .unwrap_or("?");
    let kind = match chan.kind {
        Kind::Server => "server",
        Kind::Channel => "channel",
        Kind::Query => "query",
    };
    let mut title = format!(" {} ({kind}) ", chan.target);
    if let Some(topic) = &chan.topic {
        if !topic.is_empty() {
            title.push_str(&format!("— {topic} "));
        }
    }
    let _ = server;
    title
}

fn line_cells(line: &Line) -> Vec<(char, Style)> {
    let mut cells = Vec::new();
    for span in &line.spans {
        for ch in span.text.chars() {
            cells.push((ch, span.style));
        }
    }
    cells
}

fn apply_highlights(cells: &mut [(char, Style)], row: usize, highlights: &[(Pos, usize)]) {
    for (pos, len) in highlights {
        if pos.row != row {
            continue;
        }
        for i in pos.col..(pos.col + len).min(cells.len()) {
            cells[i].1 = cells[i].1.reverse();
        }
    }
}

fn place_cursor(cells: &mut Vec<(char, Style)>, col: usize) {
    if col < cells.len() {
        cells[col].1 = cells[col].1.reverse();
    } else {
        cells.push((' ', Style::default().reverse()));
    }
}

fn to_rline(cells: &[(char, Style)]) -> RLine<'static> {
    let mut spans: Vec<RSpan> = Vec::new();
    let mut current = String::new();
    let mut current_style: Option<Style> = None;
    for (ch, style) in cells {
        match current_style {
            Some(s) if s == *style => current.push(*ch),
            Some(s) => {
                spans.push(RSpan::styled(std::mem::take(&mut current), rstyle(s)));
                current.push(*ch);
                current_style = Some(*style);
            }
            None => {
                current.push(*ch);
                current_style = Some(*style);
            }
        }
    }
    if let Some(s) = current_style {
        spans.push(RSpan::styled(current, rstyle(s)));
    }
    RLine::from(spans)
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let rows =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);

    let items: Vec<ListItem> = app
        .buffers
        .iter()
        .enumerate()
        .map(|(i, chan)| {
            let marker = if i == app.current {
                ">"
            } else if chan.unread > 0 {
                "*"
            } else {
                " "
            };
            let mut style = RStyle::default();
            if i == app.current {
                style = style.bold();
            }
            if chan.unread > 0 && i != app.current {
                style = style.fg(RColor::Yellow);
            }
            ListItem::new(RLine::from(format!("{marker} {}", chan.target))).style(style)
        })
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" Buffers "));
    frame.render_widget(list, rows[0]);

    let mut nicks: Vec<String> = app.current().nicks.clone();
    nicks.sort();
    let items: Vec<ListItem> = nicks
        .iter()
        .map(|n| ListItem::new(RLine::from(n.clone())))
        .collect();
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" Nicks "));
    frame.render_widget(list, rows[1]);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let chan = app.current();
    let server = app
        .servers
        .get(chan.server)
        .map(|s| s.name.as_str())
        .unwrap_or("-");
    let nick = app
        .servers
        .get(chan.server)
        .map(|s| s.our_nick.as_str())
        .unwrap_or("-");
    let status = if app
        .servers
        .get(chan.server)
        .map(|s| s.connected)
        .unwrap_or(false)
    {
        "connected"
    } else {
        "offline"
    };
    let text = format!(" {nick} @ {server}  [{status}]  — {} ", chan.target);
    let para = Paragraph::new(text).style(RStyle::default().bg(RColor::Blue).fg(RColor::White));
    frame.render_widget(para, area);
}

fn draw_minibuffer(frame: &mut Frame, app: &App, area: Rect) {
    let text = if app.search.active {
        let prefix = match app.search.direction {
            SearchDirection::Forward => "I-search",
            SearchDirection::Backward => "I-search backward",
        };
        let suffix = if app.search.failed { " [failing]" } else { "" };
        format!("{prefix}: {}{suffix}", app.search.query)
    } else if let Some(message) = &app.message {
        message.clone()
    } else {
        String::new()
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn rstyle(style: Style) -> RStyle {
    let mut out = RStyle::default().fg(rcolor(style.color));
    if style.bold {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.dim {
        out = out.add_modifier(Modifier::DIM);
    }
    if style.reverse {
        out = out.add_modifier(Modifier::REVERSED);
    }
    out
}

fn rcolor(color: Color) -> RColor {
    match color {
        Color::Default => RColor::Reset,
        Color::Black => RColor::Black,
        Color::Red => RColor::Red,
        Color::Green => RColor::Green,
        Color::Yellow => RColor::Yellow,
        Color::Blue => RColor::Blue,
        Color::Magenta => RColor::Magenta,
        Color::Cyan => RColor::Cyan,
        Color::Gray => RColor::Gray,
        Color::DarkGray => RColor::DarkGray,
        Color::LightRed => RColor::LightRed,
        Color::LightGreen => RColor::LightGreen,
        Color::LightYellow => RColor::LightYellow,
        Color::LightBlue => RColor::LightBlue,
        Color::LightMagenta => RColor::LightMagenta,
        Color::LightCyan => RColor::LightCyan,
        Color::White => RColor::White,
    }
}
