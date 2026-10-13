//! Rendering with ratatui.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color as RColor, Modifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan, Text};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, Prompt};
use crate::buffer::{Color, Line, Pos, Style};
use crate::search::SearchDirection;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let minibuffer = app.minibuffer_height().max(1);
    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(minibuffer),
    ])
    .split(frame.area());

    draw_buffer(frame, app, rows[0]);
    draw_mode_line(frame, app, rows[1]);
    draw_minibuffer(frame, app, rows[2]);
}

fn draw_buffer(frame: &mut Frame, app: &App, area: Rect) {
    let buf = &app.current().buffer;
    let height = area.height as usize;
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
        if row == point.row && matches!(app.prompt, Prompt::None) {
            place_cursor(&mut cells, point.col);
        }
        lines.push(to_rline(&cells));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn draw_mode_line(frame: &mut Frame, app: &App, area: Rect) {
    let line = app.current().buffer.point().row + 1;
    let base = RStyle::default().add_modifier(Modifier::REVERSED);
    let minor = if app.ignore_activity() { " Ignore" } else { "" };
    let mut spans = vec![RSpan::styled(
        format!(
            " {}  ({}){minor}  L{} ",
            app.mode_line_name(),
            app.mode_line_mode(),
            line
        ),
        base,
    )];

    let entries = app.activity_entries();
    if !entries.is_empty() {
        spans.push(RSpan::styled(" ", base));
        spans.push(RSpan::styled("[", base));
        for (i, (name, style)) in entries.iter().enumerate() {
            if i > 0 {
                spans.push(RSpan::styled(",", base));
            }
            spans.push(RSpan::styled(name.clone(), track_style(base, *style)));
        }
        spans.push(RSpan::styled("] ", base));
    }

    frame.render_widget(Paragraph::new(RLine::from(spans)).style(base), area);
}

/// Compose an activity entry's style over the (reversed) mode-line base:
/// keywords bold, our-mentioned nicks un-reversed so they stand out.
fn track_style(base: RStyle, style: Style) -> RStyle {
    let mut out = base;
    if style.bold {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.reverse {
        out = out.remove_modifier(Modifier::REVERSED);
    }
    out
}

fn draw_minibuffer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some((prompt, cursor, candidates, selected)) = app.prompt_view() {
        let mut lines: Vec<RLine> = Vec::with_capacity(candidates.len() + 1);
        // Show the cursor at the edit position within the prompt input.
        let mut cells: Vec<(char, Style)> = prompt.chars().map(|c| (c, Style::default())).collect();
        place_cursor(&mut cells, cursor);
        lines.push(to_rline(&cells));
        for (i, candidate) in candidates.iter().enumerate() {
            let style = if i == selected {
                RStyle::default().add_modifier(Modifier::REVERSED)
            } else {
                RStyle::default()
            };
            lines.push(RLine::from(RSpan::styled(format!("  {candidate}"), style)));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        return;
    }

    let text = if app.search.active {
        let prefix = match app.search.direction {
            SearchDirection::Forward => "I-search",
            SearchDirection::Backward => "I-search backward",
        };
        let suffix = if app.search.failed { " [failing]" } else { "" };
        format!("{prefix}: {}{suffix}", app.search.query)
    } else {
        app.message.clone().unwrap_or_default()
    };
    frame.render_widget(Paragraph::new(text), area);
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
        Color::Rgb(r, g, b) => RColor::Rgb(r, g, b),
    }
}
