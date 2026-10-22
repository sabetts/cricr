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
    let width = area.width as usize;
    if height == 0 || width == 0 {
        return;
    }

    let point = buf.point();
    let highlights = if app.search.active && !app.search.query.is_empty() {
        buf.matches(&app.search.query)
    } else {
        Vec::new()
    };
    let current_match = if app.search.active {
        app.search.match_end.and_then(|end| {
            (end.row == point.row && end.col > point.col).then(|| (point, end.col - point.col))
        })
    } else {
        None
    };

    // The prompt line is not stored wrapped, so split it for display. Every
    // other logical line occupies one display row.
    let prompt_row = buf.last_row();
    let (mut prompt_chunks, cursor_chunk, cursor_col) = if buf.is_irc() {
        let mut cells = line_cells(buf.line(prompt_row));
        apply_highlights(
            &mut cells,
            prompt_row,
            &highlights,
            current_match,
            app.theme.lazy_highlight,
            app.theme.isearch,
        );
        let (chunk, col) = wrap_position(point.col, width);
        (wrap_cells(&cells, width), chunk, col)
    } else {
        (Vec::new(), 0, 0)
    };
    if buf.is_irc() && point.row == prompt_row && cursor_chunk >= prompt_chunks.len() {
        // Point at the start of a fresh visual row (e.g. exactly at the wrap
        // boundary), so make room for the cursor.
        prompt_chunks.resize(cursor_chunk + 1, Vec::new());
    }
    let prompt_height = prompt_chunks.len().max(1);
    let total = if buf.is_irc() {
        prompt_row + prompt_height
    } else {
        buf.len()
    };
    let cursor_display = if buf.is_irc() && point.row == prompt_row {
        prompt_row + cursor_chunk
    } else {
        point.row
    };

    let bottom_anchored = buf.is_irc() && buf.scroll() + height >= buf.len();
    let top = display_top(buf.scroll(), total, height, cursor_display, bottom_anchored);

    let mut lines: Vec<RLine> = Vec::with_capacity(height);
    for display in top..(top + height).min(total) {
        if buf.is_irc() && display >= prompt_row {
            let chunk = display - prompt_row;
            if let Some(cells) = prompt_chunks.get(chunk) {
                let mut cells = cells.clone();
                if point.row == prompt_row
                    && matches!(app.prompt, Prompt::None)
                    && chunk == cursor_chunk
                {
                    place_cursor(&mut cells, cursor_col, app.cursor_style);
                }
                lines.push(to_rline(&cells));
            }
            continue;
        }
        let row = display;
        let mut cells = line_cells(buf.line(row));
        apply_highlights(
            &mut cells,
            row,
            &highlights,
            current_match,
            app.theme.lazy_highlight,
            app.theme.isearch,
        );
        if row == point.row && matches!(app.prompt, Prompt::None) {
            place_cursor(&mut cells, point.col, app.cursor_style);
        }
        lines.push(to_rline(&cells));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// Split `cells` into hard-wrapped display rows, preserving every character.
fn wrap_cells(cells: &[(char, Style)], width: usize) -> Vec<Vec<(char, Style)>> {
    if width == 0 {
        return vec![cells.to_vec()];
    }
    if cells.is_empty() {
        return vec![Vec::new()];
    }
    cells.chunks(width).map(|chunk| chunk.to_vec()).collect()
}

/// The visual row and column of `col` when wrapped at `width`.
fn wrap_position(col: usize, width: usize) -> (usize, usize) {
    match col.checked_div(width) {
        Some(row) => (row, col % width),
        None => (0, col),
    }
}

/// The first display row to draw. When the logical window reaches the end of
/// the buffer, anchor the display to the bottom so a wrapping prompt stays
/// fully visible while point moves within it.
fn display_top(
    scroll: usize,
    total: usize,
    height: usize,
    cursor: usize,
    bottom_anchored: bool,
) -> usize {
    let mut top = scroll.min(total.saturating_sub(1));
    if bottom_anchored {
        top = total.saturating_sub(height);
    }
    if cursor < top {
        top = cursor;
    }
    if cursor >= top + height {
        top = cursor + 1 - height;
    }
    top
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
        place_cursor(&mut cells, cursor, app.cursor_style);
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

fn apply_highlights(
    cells: &mut [(char, Style)],
    row: usize,
    highlights: &[(Pos, usize)],
    current: Option<(Pos, usize)>,
    lazy: Style,
    isearch: Style,
) {
    for (pos, len) in highlights {
        if pos.row != row {
            continue;
        }
        let is_current = current
            .is_some_and(|(cpos, clen)| cpos.row == pos.row && cpos.col == pos.col && clen == *len);
        for i in pos.col..(pos.col + len).min(cells.len()) {
            cells[i].1.reverse = false;
            if is_current {
                cells[i].1.color = isearch.color;
                cells[i].1.bg = isearch.bg;
            } else {
                cells[i].1.bg = lazy.bg;
            }
        }
    }
}

fn place_cursor(cells: &mut Vec<(char, Style)>, col: usize, style: Style) {
    if col < cells.len() {
        cells[col].1 = style;
    } else {
        cells.push((' ', style));
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
    let mut out = RStyle::default()
        .fg(rcolor(style.color))
        .bg(rcolor(style.bg));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_cells_without_losing_characters() {
        let cells: Vec<(char, Style)> = "abcdef".chars().map(|c| (c, Style::default())).collect();
        let chunks = wrap_cells(&cells, 4);
        assert_eq!(chunks.len(), 2);
        let text: String = chunks.iter().flatten().map(|(c, _)| *c).collect();
        assert_eq!(text, "abcdef");
        assert_eq!(chunks[0].len(), 4);
        assert_eq!(chunks[1].len(), 2);
    }

    #[test]
    fn wrap_position_maps_cursor_to_visual_row() {
        assert_eq!(wrap_position(0, 4), (0, 0));
        assert_eq!(wrap_position(3, 4), (0, 3));
        assert_eq!(wrap_position(4, 4), (1, 0));
        assert_eq!(wrap_position(7, 4), (1, 3));
    }

    #[test]
    fn bottom_anchored_window_stays_put_within_wrapped_prompt() {
        // A three-row prompt ending at display row 24 stays fully visible as
        // the cursor moves from its start to its end.
        assert_eq!(display_top(13, 25, 10, 22, true), 15);
        assert_eq!(display_top(13, 25, 10, 24, true), 15);
        // Moving above the window's top edge does scroll it.
        assert_eq!(display_top(13, 25, 10, 14, true), 14);
    }

    #[test]
    fn window_scrolls_only_at_the_edges() {
        assert_eq!(display_top(5, 25, 10, 5, false), 5);
        assert_eq!(display_top(5, 25, 10, 14, false), 5);
        assert_eq!(display_top(5, 25, 10, 16, false), 7);
    }

    #[test]
    fn cursor_style_is_independent_of_the_text_under_it() {
        let mut cells = vec![('a', Style::fg(Color::Green).bold())];
        let cursor = Style::fg(Color::Black).bg(Color::Yellow);
        place_cursor(&mut cells, 0, cursor);
        assert_eq!(cells[0].1, cursor);
    }

    #[test]
    fn isearch_uses_lazy_highlight_for_other_matches() {
        let mut cells = vec![('a', Style::default()); 8];
        let lazy = Style::fg(Color::White).bg(Color::Rgb(0x66, 0x8b, 0x8b));
        let isearch = Style::fg(Color::Rgb(0x8b, 0x23, 0x23)).bg(Color::Rgb(0xee, 0x79, 0x9f));
        let highlights = vec![(Pos::new(0, 0), 2), (Pos::new(0, 4), 2)];
        apply_highlights(
            &mut cells,
            0,
            &highlights,
            Some((Pos::new(0, 0), 2)),
            lazy,
            isearch,
        );
        assert_eq!(cells[0].1.bg, isearch.bg);
        assert_eq!(cells[0].1.color, isearch.color);
        assert_eq!(cells[4].1.bg, lazy.bg);
        assert_eq!(cells[4].1.color, Style::default().color);
    }
}
