//! A text buffer with an Emacs-style point, mark, and incremental search.
//!
//! The buffer holds the whole scrollback for one IRC target. The final line is
//! the prompt line: it begins with the prompt string and holds the editable
//! input. Point may move anywhere in the buffer (like rcirc, where the input
//! lives at the bottom of the same buffer), but editing is only permitted at
//! or after the prompt.

/// A logical position: a row index and a character column within that row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub row: usize,
    pub col: usize,
}

impl Pos {
    pub fn new(row: usize, col: usize) -> Self {
        Pos { row, col }
    }
}

/// A minimal, UI-independent color palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum Color {
    #[default]
    Default,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
}

/// A minimal, UI-independent text style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub color: Color,
    pub bold: bool,
    pub dim: bool,
    pub reverse: bool,
}

impl Style {
    pub const fn fg(color: Color) -> Self {
        Style {
            color,
            bold: false,
            dim: false,
            reverse: false,
        }
    }

    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    pub const fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }
}

/// A styled run of text within a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Span {
            text: text.into(),
            style,
        }
    }
}

/// A single display line, made of one or more styled spans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn plain(text: impl Into<String>) -> Self {
        Line {
            spans: vec![Span::new(text, Style::default())],
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Line {
            spans: vec![Span::new(text, style)],
        }
    }

    pub fn from_spans(spans: Vec<Span>) -> Self {
        Line { spans }
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for span in &self.spans {
            out.push_str(&span.text);
        }
        out
    }

    pub fn chars(&self) -> Vec<char> {
        self.text().chars().collect()
    }

    pub fn char_len(&self) -> usize {
        self.spans.iter().map(|s| s.text.chars().count()).sum()
    }
}

/// The buffer model: scrollback lines plus a prompt line holding the input.
#[derive(Debug, Clone)]
pub struct Buffer {
    lines: Vec<Line>,
    prompt: String,
    prompt_len: usize,
    point: Pos,
    mark: Option<Pos>,
    scroll: usize,
}

impl Buffer {
    pub fn new(prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        let prompt_len = prompt.chars().count();
        Buffer {
            lines: vec![Line::styled(prompt.clone(), Style::fg(Color::Green).bold())],
            prompt,
            prompt_len,
            point: Pos::new(0, prompt_len),
            mark: None,
            scroll: 0,
        }
    }

    pub fn prompt_len(&self) -> usize {
        self.prompt_len
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.input().is_empty()
    }

    pub fn line(&self, row: usize) -> &Line {
        &self.lines[row]
    }

    pub fn point(&self) -> Pos {
        self.point
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn last_row(&self) -> usize {
        self.lines.len() - 1
    }

    /// Insert a line of scrollback immediately before the prompt line.
    pub fn push_line(&mut self, line: Line) {
        let at = self.last_row();
        self.lines.insert(at, line);
        if self.point.row >= at {
            self.point.row += 1;
        }
        if let Some(mark) = &mut self.mark {
            if mark.row >= at {
                mark.row += 1;
            }
        }
    }

    pub fn push_plain(&mut self, text: impl Into<String>) {
        self.push_line(Line::plain(text));
    }

    /// Drop the oldest `n` scrollback lines, adjusting point/scroll/mark.
    pub fn truncate_front(&mut self, n: usize) {
        if n == 0 || self.lines.len() <= 1 {
            return;
        }
        let n = n.min(self.lines.len() - 1);
        self.lines.drain(0..n);
        self.point.row = self.point.row.saturating_sub(n);
        self.scroll = self.scroll.saturating_sub(n);
        if let Some(mark) = &mut self.mark {
            mark.row = mark.row.saturating_sub(n);
        }
    }

    pub fn clear(&mut self) {
        self.input_set("");
        self.scroll = 0;
        self.point = Pos::new(0, self.prompt_len);
    }

    /// The current input text (the prompt line after the prompt).
    pub fn input(&self) -> String {
        let chars = self.lines[self.last_row()].chars();
        chars[self.prompt_len.min(chars.len())..].iter().collect()
    }

    fn input_set(&mut self, text: &str) {
        let row = self.last_row();
        self.lines[row] = Line::from_spans(vec![
            Span::new(self.prompt.clone(), Style::fg(Color::Green).bold()),
            Span::new(text, Style::default()),
        ]);
        self.point.col = self.point.col.max(self.prompt_len);
    }

    pub fn set_input(&mut self, text: &str) {
        self.input_set(text);
        self.point = Pos::new(self.last_row(), self.prompt_len + text.chars().count());
    }

    // ---- read-only queries -------------------------------------------------

    pub fn editable(&self) -> bool {
        self.point.row == self.last_row() && self.point.col >= self.prompt_len
    }

    fn clamp_col(&self, row: usize, col: usize) -> usize {
        let len = self.lines[row].char_len();
        let mut col = col.min(len);
        if row == self.last_row() {
            col = col.max(self.prompt_len);
        }
        col
    }

    // ---- motion ------------------------------------------------------------

    pub fn forward_char(&mut self) {
        let row = self.point.row;
        let len = self.lines[row].char_len();
        if self.point.col < len {
            self.point.col += 1;
        } else if row < self.last_row() {
            self.point.row += 1;
            self.point.col = self.clamp_col(self.point.row, 0);
        }
        self.scroll_into_view();
    }

    pub fn backward_char(&mut self) {
        let row = self.point.row;
        if self.point.col > 0 {
            self.point.col -= 1;
            let min = if row == self.last_row() {
                self.prompt_len
            } else {
                0
            };
            if self.point.col < min {
                self.point.col = min;
            }
        } else if row > 0 {
            self.point.row -= 1;
            self.point.col = self.lines[self.point.row].char_len();
        }
        self.scroll_into_view();
    }

    pub fn next_line(&mut self) {
        if self.point.row < self.last_row() {
            self.point.row += 1;
            self.point.col = self.clamp_col(self.point.row, self.point.col);
        }
        self.scroll_into_view();
    }

    pub fn prev_line(&mut self) {
        if self.point.row > 0 {
            self.point.row -= 1;
            self.point.col = self.clamp_col(self.point.row, self.point.col);
        }
        self.scroll_into_view();
    }

    pub fn beginning_of_line(&mut self) {
        self.point.col = if self.point.row == self.last_row() {
            self.prompt_len
        } else {
            0
        };
    }

    pub fn end_of_line(&mut self) {
        self.point.col = self.lines[self.point.row].char_len();
        self.scroll_into_view();
    }

    pub fn beginning_of_buffer(&mut self) {
        self.point = Pos::new(0, 0);
        self.scroll = 0;
    }

    pub fn end_of_buffer(&mut self) {
        let row = self.last_row();
        self.point = Pos::new(row, self.lines[row].char_len());
        self.scroll_into_view();
    }

    pub fn forward_word(&mut self) {
        let chars = self.lines[self.point.row].chars();
        let mut col = self.point.col;
        let min = if self.point.row == self.last_row() {
            self.prompt_len
        } else {
            0
        };
        if col >= chars.len() {
            self.next_line();
            return;
        }
        while col < chars.len() && !is_word(chars[col]) {
            col += 1;
        }
        while col < chars.len() && is_word(chars[col]) {
            col += 1;
        }
        self.point.col = col.max(min);
        self.scroll_into_view();
    }

    pub fn backward_word(&mut self) {
        let chars = self.lines[self.point.row].chars();
        let min = if self.point.row == self.last_row() {
            self.prompt_len
        } else {
            0
        };
        let mut col = self.point.col;
        while col > min && !is_word(chars[col - 1]) {
            col -= 1;
        }
        while col > min && is_word(chars[col - 1]) {
            col -= 1;
        }
        self.point.col = col;
        self.scroll_into_view();
    }

    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_add(rows);
    }

    pub fn set_point(&mut self, pos: Pos) {
        let row = pos.row.min(self.last_row());
        self.point = Pos::new(row, self.clamp_col(row, pos.col));
        self.scroll_into_view();
    }

    pub fn set_mark(&mut self) {
        self.mark = Some(self.point);
    }

    pub fn clear_mark(&mut self) {
        self.mark = None;
    }

    pub fn region(&self) -> Option<(Pos, Pos)> {
        self.mark.map(|m| {
            if m <= self.point {
                (m, self.point)
            } else {
                (self.point, m)
            }
        })
    }

    pub fn region_text(&self) -> Option<String> {
        let (start, end) = self.region()?;
        Some(self.text_between(start, end))
    }

    pub fn text_between(&self, start: Pos, end: Pos) -> String {
        if start == end {
            return String::new();
        }
        let mut out = String::new();
        for row in start.row..=end.row {
            let chars = self.lines[row].chars();
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row { end.col } else { chars.len() };
            let from = from.min(chars.len());
            let to = to.min(chars.len());
            out.extend(&chars[from..to]);
            if row != end.row {
                out.push('\n');
            }
        }
        out
    }

    // ---- editing (input only) ---------------------------------------------

    pub fn insert_char(&mut self, c: char) {
        if !self.editable() {
            return;
        }
        let mut chars: Vec<char> = self.input().chars().collect();
        let idx = self.point.col - self.prompt_len;
        chars.insert(idx.min(chars.len()), c);
        let text: String = chars.into_iter().collect();
        self.input_set(&text);
        self.point.col += 1;
        self.scroll_into_view();
    }

    pub fn insert_str(&mut self, s: &str) {
        if !self.editable() {
            return;
        }
        let mut chars: Vec<char> = self.input().chars().collect();
        let idx = (self.point.col - self.prompt_len).min(chars.len());
        let added: Vec<char> = s.chars().collect();
        let added_len = added.len();
        for (i, c) in added.into_iter().enumerate() {
            chars.insert(idx + i, c);
        }
        let text: String = chars.into_iter().collect();
        self.input_set(&text);
        self.point.col += added_len;
        self.scroll_into_view();
    }

    pub fn delete_backward(&mut self) {
        if !self.editable() || self.point.col <= self.prompt_len {
            return;
        }
        let mut chars: Vec<char> = self.input().chars().collect();
        let idx = self.point.col - self.prompt_len - 1;
        if idx < chars.len() {
            chars.remove(idx);
        }
        let text: String = chars.into_iter().collect();
        self.input_set(&text);
        self.point.col -= 1;
        self.scroll_into_view();
    }

    pub fn delete_forward(&mut self) {
        if !self.editable() {
            return;
        }
        let mut chars: Vec<char> = self.input().chars().collect();
        let idx = self.point.col - self.prompt_len;
        if idx < chars.len() {
            chars.remove(idx);
        }
        let text: String = chars.into_iter().collect();
        self.input_set(&text);
        self.scroll_into_view();
    }

    /// Kill from point to end of the input line; returns the killed text.
    pub fn kill_line(&mut self) -> String {
        if !self.editable() {
            return String::new();
        }
        let chars: Vec<char> = self.input().chars().collect();
        let idx = self.point.col - self.prompt_len;
        let killed: String = chars[idx.min(chars.len())..].iter().collect();
        let kept: String = chars[..idx.min(chars.len())].iter().collect();
        self.input_set(&kept);
        self.scroll_into_view();
        killed
    }

    // ---- search ------------------------------------------------------------

    /// Find the next occurrence of `query` from `from` in the given direction.
    /// Returns the match start and its character length.
    pub fn search(&self, query: &str, from: Pos, forward: bool) -> Option<(Pos, usize)> {
        if query.is_empty() {
            return None;
        }
        let needle: Vec<char> = query.to_lowercase().chars().collect();
        let rows: Vec<usize> = if forward {
            (from.row..self.lines.len()).collect()
        } else {
            (0..=from.row.min(self.last_row())).rev().collect()
        };
        for row in rows {
            let hay: Vec<char> = self.lines[row].text().to_lowercase().chars().collect();
            if forward {
                let start = if row == from.row { from.col } else { 0 };
                if let Some(col) = find_in(&hay, &needle, start.min(hay.len()), true) {
                    return Some((Pos::new(row, col), needle.len()));
                }
            } else {
                let end = if row == from.row { from.col } else { hay.len() };
                if let Some(col) = find_in(&hay, &needle, end.min(hay.len()), false) {
                    return Some((Pos::new(row, col), needle.len()));
                }
            }
        }
        None
    }

    /// All non-overlapping matches of `query`, for highlighting.
    pub fn matches(&self, query: &str) -> Vec<(Pos, usize)> {
        if query.is_empty() {
            return Vec::new();
        }
        let needle: Vec<char> = query.to_lowercase().chars().collect();
        let mut out = Vec::new();
        for row in 0..self.lines.len() {
            let hay: Vec<char> = self.lines[row].text().to_lowercase().chars().collect();
            let mut start = 0;
            while let Some(col) = find_in(&hay, &needle, start, true) {
                out.push((Pos::new(row, col), needle.len()));
                start = col + needle.len().max(1);
            }
        }
        out
    }

    fn scroll_into_view(&mut self) {
        if self.point.row < self.scroll {
            self.scroll = self.point.row;
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Find `needle` inside `hay`, starting at char index `start`. When `forward`
/// is false, returns the last match strictly before `start`.
fn find_in(hay: &[char], needle: &[char], start: usize, forward: bool) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    if forward {
        let last = hay.len() - needle.len();
        for i in start..=last {
            if hay[i..i + needle.len()] == *needle {
                return Some(i);
            }
        }
    } else {
        let start = start.min(hay.len());
        if start < needle.len() {
            return None;
        }
        for i in (0..=start - needle.len()).rev() {
            if hay[i..i + needle.len()] == *needle {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_with_history() -> Buffer {
        let mut b = Buffer::new("> ");
        b.push_plain("first line");
        b.push_plain("second line here");
        b
    }

    #[test]
    fn starts_on_prompt() {
        let b = Buffer::new("> ");
        assert_eq!(b.point(), Pos::new(0, 2));
        assert!(b.editable());
    }

    #[test]
    fn typing_and_deleting() {
        let mut b = Buffer::new("> ");
        for c in "hello".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.input(), "hello");
        b.delete_backward();
        assert_eq!(b.input(), "hell");
        b.backward_char();
        b.insert_char('X');
        assert_eq!(b.input(), "helXl");
    }

    #[test]
    fn cannot_edit_history() {
        let mut b = buf_with_history();
        b.beginning_of_buffer();
        b.insert_char('x');
        // still just the history line plus prompt
        assert_eq!(b.len(), 3);
        assert!(!b.editable());
    }

    #[test]
    fn motion_over_prompt_is_clamped() {
        let mut b = buf_with_history();
        // point on prompt line (row 2)
        assert_eq!(b.point().row, 2);
        b.backward_char();
        assert_eq!(b.point().col, b.prompt_len());
    }

    #[test]
    fn inserting_history_keeps_point_on_prompt() {
        let mut b = buf_with_history();
        let before = b.point();
        b.push_plain("third");
        assert_eq!(b.point().row, before.row + 1);
        assert!(b.editable());
    }

    #[test]
    fn search_forward_backward() {
        let b = buf_with_history();
        let hit = b.search("line", Pos::new(0, 0), true).unwrap();
        assert_eq!(hit.0, Pos::new(0, 6));
        let second = b
            .search("line", Pos::new(hit.0.row, hit.0.col + 1), true)
            .unwrap();
        assert_eq!(second.0.row, 1);
        let back = b.search("first", Pos::new(1, 0), false).unwrap();
        assert_eq!(back.0, Pos::new(0, 0));
    }

    #[test]
    fn matches_all() {
        let b = buf_with_history();
        let m = b.matches("line");
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn region_text() {
        let mut b = Buffer::new("> ");
        b.set_input("abcdef");
        b.set_point(Pos::new(0, 3));
        b.set_mark();
        b.set_point(Pos::new(0, 5));
        assert_eq!(b.region_text().unwrap(), "bc");
    }

    #[test]
    fn kill_line_returns_remainder() {
        let mut b = Buffer::new("> ");
        b.set_input("hello world");
        b.set_point(Pos::new(0, 2 + 5));
        let killed = b.kill_line();
        assert_eq!(killed, " world");
        assert_eq!(b.input(), "hello");
    }
}
