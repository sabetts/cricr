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
    /// A 24-bit truecolor value.
    Rgb(u8, u8, u8),
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

    pub const fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Style {
            color: Color::Rgb(r, g, b),
            bold: false,
            dim: false,
            reverse: false,
        }
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
#[derive(Debug, Clone, Default)]
pub struct Line {
    pub spans: Vec<Span>,
    /// When wrapping, continuation lines are indented by this many columns
    /// (the width of the line's prefix, e.g. timestamp + `<nick> `).
    pub indent: usize,
    /// Absolute time (Unix epoch seconds) this line was produced, if known.
    /// Used only by the scrollback cache; not part of line equality.
    pub time: Option<i64>,
}

impl PartialEq for Line {
    fn eq(&self, other: &Self) -> bool {
        self.spans == other.spans && self.indent == other.indent
    }
}

impl Eq for Line {}

impl Line {
    pub fn plain(text: impl Into<String>) -> Self {
        Line {
            spans: vec![Span::new(text, Style::default())],
            indent: 0,
            time: None,
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Line {
            spans: vec![Span::new(text, style)],
            indent: 0,
            time: None,
        }
    }

    pub fn from_spans(spans: Vec<Span>) -> Self {
        Line {
            spans,
            indent: 0,
            time: None,
        }
    }

    /// Set the continuation indent used when wrapping.
    pub fn indented(mut self, indent: usize) -> Self {
        self.indent = indent;
        self
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

    /// Word-wrap this line to `width` columns. Continuation lines are indented
    /// by `self.indent`. Returns the original line unchanged if it fits.
    ///
    /// Words are kept whole where possible, and URLs are never split (a URL
    /// that is itself wider than the line overflows it) so they stay copyable
    /// and openable.
    pub fn wrap(&self, width: usize) -> Vec<Line> {
        if width == 0 {
            return vec![self.clone()];
        }
        let cells: Vec<(char, Style)> = self
            .spans
            .iter()
            .flat_map(|s| s.text.chars().map(move |c| (c, s.style)))
            .collect();
        if cells.len() <= width {
            return vec![self.clone()];
        }

        let indent = self.indent.min(width.saturating_sub(1));
        let mut chunks: Vec<&[(char, Style)]> = Vec::new();
        let mut start = 0;
        let mut limit = width;
        // On the first line, never break inside the prefix (columns < indent).
        let mut min_break = indent;
        while start < cells.len() {
            if cells.len() - start <= limit {
                chunks.push(&cells[start..]);
                break;
            }
            let window_end = start + limit;
            // `brk` is the index just past the last space within the window.
            let brk = (start + 1..=window_end)
                .rev()
                .find(|&i| cells[i - 1].0 == ' ' && i > min_break);
            if let Some(i) = brk {
                chunks.push(&cells[start..i - 1]);
                start = i;
            } else {
                // No space to break on inside the window. Identify the token
                // that runs past the edge.
                let word_start = (start..window_end)
                    .rev()
                    .find(|&i| cells[i].0 == ' ')
                    .map(|i| (i + 1).max(min_break))
                    .unwrap_or(start);
                let word_end = (word_start..cells.len())
                    .find(|&i| cells[i].0 == ' ')
                    .unwrap_or(cells.len());
                if is_url(&cells[word_start..word_end]) {
                    // Never split a URL or give it its own line; keep it on the
                    // current line and let it overflow.
                    chunks.push(&cells[start..word_end]);
                    start = (word_end + 1).min(cells.len());
                } else if word_start > start {
                    chunks.push(&cells[start..word_start]);
                    start = word_start;
                } else {
                    chunks.push(&cells[start..window_end]);
                    start = window_end;
                }
            }
            limit = width.saturating_sub(indent);
            min_break = 0;
        }

        chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                let mut line = cells_to_line(chunk);
                if i > 0 && indent > 0 {
                    let mut spans = vec![Span::new(" ".repeat(indent), Style::default())];
                    spans.append(&mut line.spans);
                    line = Line::from_spans(spans);
                }
                line
            })
            .collect()
    }
}

/// Whether a wrapped word is an http(s) URL, which is never broken across
/// lines (so it stays copyable and openable).
fn is_url(cells: &[(char, Style)]) -> bool {
    let text: String = cells.iter().map(|(c, _)| *c).collect();
    text.contains("http://") || text.contains("https://")
}

fn cells_to_line(cells: &[(char, Style)]) -> Line {
    let mut spans: Vec<Span> = Vec::new();
    let mut cur = String::new();
    let mut cur_style: Option<Style> = None;
    for (ch, style) in cells {
        match cur_style {
            Some(s) if s == *style => cur.push(*ch),
            Some(_) => {
                if !cur.is_empty() {
                    spans.push(Span::new(std::mem::take(&mut cur), cur_style.unwrap()));
                }
                cur.push(*ch);
                cur_style = Some(*style);
            }
            None => {
                cur.push(*ch);
                cur_style = Some(*style);
            }
        }
    }
    if let Some(style) = cur_style {
        if !cur.is_empty() {
            spans.push(Span::new(cur, style));
        }
    }
    Line::from_spans(spans)
}

/// The buffer model: scrollback lines plus a prompt line holding the input.
#[derive(Debug, Clone)]
pub struct Buffer {
    lines: Vec<Line>,
    /// `Some(prompt)` for an rcirc-style buffer (input editable only on the
    /// prompt line); `None` for a free text buffer (like `*scratch*`).
    prompt: Option<String>,
    prompt_len: usize,
    prompt_style: Style,
    point: Pos,
    mark: Option<Pos>,
    scroll: usize,
}

impl Buffer {
    pub fn new(prompt: impl Into<String>) -> Self {
        let prompt = prompt.into();
        let prompt_len = prompt.chars().count();
        let prompt_style = Style::fg(Color::Green).bold();
        Buffer {
            lines: vec![Line::styled(prompt.clone(), prompt_style)],
            prompt: Some(prompt),
            prompt_len,
            prompt_style,
            point: Pos::new(0, prompt_len),
            mark: None,
            scroll: 0,
        }
    }

    /// A free text buffer with no prompt (the whole buffer is editable).
    pub fn text() -> Self {
        Buffer {
            lines: vec![Line::plain("")],
            prompt: None,
            prompt_len: 0,
            prompt_style: Style::default(),
            point: Pos::new(0, 0),
            mark: None,
            scroll: 0,
        }
    }

    /// Replace all content (used for read-only-ish help buffers).
    pub fn set_content(&mut self, lines: Vec<Line>) {
        self.lines = if lines.is_empty() {
            vec![Line::plain("")]
        } else {
            lines
        };
        self.point = Pos::new(0, 0);
        self.scroll = 0;
        self.mark = None;
    }

    /// Replace the scrollback with cached `history`, keeping a fresh prompt
    /// line (used when preloading an rcirc buffer from the history cache).
    pub fn load_scrollback(&mut self, history: Vec<Line>) {
        let prompt = self.prompt.clone().unwrap_or_default();
        self.lines = Vec::with_capacity(history.len() + 1);
        self.lines.extend(history);
        self.lines.push(Line::styled(prompt, self.prompt_style));
        self.point = Pos::new(self.last_row(), self.prompt_len);
        self.scroll = 0;
        self.mark = None;
    }

    /// Set the style used to render the prompt (e.g. `rcirc-prompt`).
    pub fn set_prompt_style(&mut self, style: Style) {
        self.prompt_style = style;
        if self.is_irc() {
            let input = self.input();
            self.input_set(&input);
        }
    }

    /// Whether this is an rcirc-style buffer with a prompt line.
    pub fn is_irc(&self) -> bool {
        self.prompt.is_some()
    }

    /// The minimum column point may occupy on `row`.
    fn min_col(&self, row: usize) -> usize {
        if self.is_irc() && row == self.last_row() {
            self.prompt_len
        } else {
            0
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

    /// Append a line of scrollback. For rcirc buffers the prompt line always
    /// stays last, so the new line is inserted just before it.
    pub fn push_line(&mut self, line: Line) {
        if self.is_irc() {
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
        } else {
            self.lines.push(line);
        }
    }

    pub fn push_plain(&mut self, text: impl Into<String>) {
        self.push_line(Line::plain(text));
    }

    /// Insert a line at an arbitrary row, keeping point/scroll/mark attached
    /// to the same content.
    pub fn insert_line(&mut self, row: usize, line: Line) {
        let row = row.min(self.lines.len());
        self.lines.insert(row, line);
        if self.point.row >= row {
            self.point.row += 1;
        }
        if let Some(mark) = &mut self.mark {
            if mark.row >= row {
                mark.row += 1;
            }
        }
        if self.scroll >= row {
            self.scroll += 1;
        }
    }

    /// Remove the line at `row`, adjusting point/scroll/mark. Returns it.
    pub fn remove_line(&mut self, row: usize) -> Option<Line> {
        if row >= self.lines.len() {
            return None;
        }
        let removed = self.lines.remove(row);
        let last = self.lines.len() - 1;
        if self.point.row > row {
            self.point.row -= 1;
        }
        self.point.row = self.point.row.min(last);
        self.point.col = self.clamp_col(self.point.row, self.point.col);
        if let Some(mark) = &mut self.mark {
            if mark.row > row {
                mark.row -= 1;
            }
            mark.row = mark.row.min(last);
        }
        if self.scroll > row {
            self.scroll -= 1;
        }
        Some(removed)
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
        if self.is_irc() {
            self.input_set("");
        } else {
            self.lines = vec![Line::plain("")];
        }
        self.scroll = 0;
        self.point = Pos::new(0, self.prompt_len);
    }

    /// The current input text (the prompt line after the prompt). Empty for
    /// free text buffers.
    pub fn input(&self) -> String {
        if !self.is_irc() {
            return String::new();
        }
        let chars = self.lines[self.last_row()].chars();
        chars[self.prompt_len.min(chars.len())..].iter().collect()
    }

    fn input_set(&mut self, text: &str) {
        let row = self.last_row();
        let prompt = self.prompt.clone().unwrap_or_default();
        self.lines[row] = Line::from_spans(vec![
            Span::new(prompt, self.prompt_style),
            Span::new(text, Style::default()),
        ]);
        self.point.col = self.point.col.max(self.prompt_len);
    }

    pub fn set_input(&mut self, text: &str) {
        if !self.is_irc() {
            return;
        }
        self.input_set(text);
        self.point = Pos::new(self.last_row(), self.prompt_len + text.chars().count());
    }

    // ---- read-only queries -------------------------------------------------

    pub fn editable(&self) -> bool {
        if !self.is_irc() {
            return true;
        }
        self.point.row == self.last_row() && self.point.col >= self.prompt_len
    }

    fn clamp_col(&self, row: usize, col: usize) -> usize {
        let len = self.lines[row].char_len();
        col.min(len).max(self.min_col(row))
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
            let min = self.min_col(row);
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
        self.point.col = self.min_col(self.point.row);
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

    /// Whether point is at the very end of the buffer (following new output).
    pub fn at_end(&self) -> bool {
        let row = self.last_row();
        self.point.row == row && self.point.col == self.lines[row].char_len()
    }

    pub fn forward_word(&mut self) {
        let chars = self.lines[self.point.row].chars();
        let mut col = self.point.col;
        let min = self.min_col(self.point.row);
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
        let min = self.min_col(self.point.row);
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

    /// Upcase the word after point (Emacs `upcase-word`), moving point past it.
    pub fn upcase_word(&mut self) {
        self.case_word(|word| word.iter().flat_map(|c| c.to_uppercase()).collect());
    }

    /// Downcase the word after point (Emacs `downcase-word`).
    pub fn downcase_word(&mut self) {
        self.case_word(|word| word.iter().flat_map(|c| c.to_lowercase()).collect());
    }

    /// Transform the word after point with `transform`, leaving point at the
    /// end of the (possibly length-changing) result.
    fn case_word(&mut self, transform: impl Fn(&[char]) -> String) {
        if !self.editable() {
            return;
        }
        if self.is_irc() {
            let base = self.prompt_len;
            let chars: Vec<char> = self.input().chars().collect();
            let mut col = self.point.col.saturating_sub(base).min(chars.len());
            while col < chars.len() && !is_word(chars[col]) {
                col += 1;
            }
            let start = col;
            while col < chars.len() && is_word(chars[col]) {
                col += 1;
            }
            let end = col;
            if start == end {
                self.point.col = base + end;
                return;
            }
            let changed = transform(&chars[start..end]);
            let new_len = changed.chars().count();
            let mut out: Vec<char> = Vec::with_capacity(chars.len());
            out.extend_from_slice(&chars[..start]);
            out.extend(changed.chars());
            out.extend_from_slice(&chars[end..]);
            let text: String = out.into_iter().collect();
            self.input_set(&text);
            self.point = Pos::new(self.last_row(), base + start + new_len);
        } else {
            let row = self.point.row;
            let chars: Vec<char> = self.lines[row].chars();
            let mut col = self.point.col.min(chars.len());
            while col < chars.len() && !is_word(chars[col]) {
                col += 1;
            }
            let start = col;
            while col < chars.len() && is_word(chars[col]) {
                col += 1;
            }
            let end = col;
            if start == end {
                self.point.col = end;
                return;
            }
            let changed = transform(&chars[start..end]);
            let new_len = changed.chars().count();
            let mut out: Vec<char> = Vec::with_capacity(chars.len());
            out.extend_from_slice(&chars[..start]);
            out.extend(changed.chars());
            out.extend_from_slice(&chars[end..]);
            self.lines[row] = Line::plain(out.into_iter().collect::<String>());
            self.point.col = start + new_len;
        }
        self.scroll_into_view();
    }

    /// Upcase the word before point (Emacs `upcase-word` with a negative
    /// argument), leaving point at the start of that word.
    pub fn backward_upcase_word(&mut self) {
        self.backward_case_word(|word| word.iter().flat_map(|c| c.to_uppercase()).collect());
    }

    /// Downcase the word before point (`downcase-word` with a negative arg).
    pub fn backward_downcase_word(&mut self) {
        self.backward_case_word(|word| word.iter().flat_map(|c| c.to_lowercase()).collect());
    }

    /// Transform the word before point with `transform`, leaving point at the
    /// start of that word.
    fn backward_case_word(&mut self, transform: impl Fn(&[char]) -> String) {
        if !self.editable() {
            return;
        }
        if self.is_irc() {
            let base = self.prompt_len;
            let chars: Vec<char> = self.input().chars().collect();
            let end = self.point.col.saturating_sub(base).min(chars.len());
            let mut i = end;
            while i > 0 && !is_word(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && is_word(chars[i - 1]) {
                i -= 1;
            }
            let start = i;
            if start == end {
                self.point.col = base + start;
                return;
            }
            let changed = transform(&chars[start..end]);
            let mut out: Vec<char> = Vec::with_capacity(chars.len());
            out.extend_from_slice(&chars[..start]);
            out.extend(changed.chars());
            out.extend_from_slice(&chars[end..]);
            let text: String = out.into_iter().collect();
            self.input_set(&text);
            self.point = Pos::new(self.last_row(), base + start);
        } else {
            let row = self.point.row;
            let chars: Vec<char> = self.lines[row].chars();
            let end = self.point.col.min(chars.len());
            let mut i = end;
            while i > 0 && !is_word(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && is_word(chars[i - 1]) {
                i -= 1;
            }
            let start = i;
            if start == end {
                self.point.col = start;
                return;
            }
            let changed = transform(&chars[start..end]);
            let mut out: Vec<char> = Vec::with_capacity(chars.len());
            out.extend_from_slice(&chars[..start]);
            out.extend(changed.chars());
            out.extend_from_slice(&chars[end..]);
            self.lines[row] = Line::plain(out.into_iter().collect::<String>());
            self.point.col = start;
        }
        self.scroll_into_view();
    }

    /// Emacs `scroll-up-command` (C-v): move point forward a screenful and
    /// scroll the view with it.
    pub fn page_forward(&mut self, rows: usize) {
        let target = (self.point.row + rows).min(self.last_row());
        let delta = target - self.point.row;
        self.point.row = target;
        self.point.col = self.clamp_col(self.point.row, self.point.col);
        self.scroll = (self.scroll + delta).min(self.point.row);
    }

    /// Emacs `scroll-down-command` (M-v): move point back a screenful.
    pub fn page_backward(&mut self, rows: usize) {
        let target = self.point.row.saturating_sub(rows);
        let delta = self.point.row - target;
        self.point.row = target;
        self.point.col = self.clamp_col(self.point.row, self.point.col);
        self.scroll = self.scroll.saturating_sub(delta);
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

    // ---- editing -----------------------------------------------------------

    pub fn insert_char(&mut self, c: char) {
        if !self.editable() {
            return;
        }
        if self.is_irc() {
            let mut chars: Vec<char> = self.input().chars().collect();
            let idx = self.point.col - self.prompt_len;
            chars.insert(idx.min(chars.len()), c);
            let text: String = chars.into_iter().collect();
            self.input_set(&text);
            self.point.col += 1;
        } else {
            let row = self.point.row;
            let mut chars = self.lines[row].chars();
            let idx = self.point.col.min(chars.len());
            chars.insert(idx, c);
            self.lines[row] = Line::plain(chars.into_iter().collect::<String>());
            self.point.col += 1;
        }
        self.scroll_into_view();
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' && !self.is_irc() {
                self.insert_newline();
            } else {
                self.insert_char(c);
            }
        }
    }

    /// Insert a line break at point (free text buffers only).
    pub fn insert_newline(&mut self) {
        if self.is_irc() {
            return;
        }
        let row = self.point.row;
        let chars = self.lines[row].chars();
        let idx = self.point.col.min(chars.len());
        let left: String = chars[..idx].iter().collect();
        let right: String = chars[idx..].iter().collect();
        self.lines[row] = Line::plain(left);
        self.lines.insert(row + 1, Line::plain(right));
        self.point = Pos::new(row + 1, 0);
        self.scroll_into_view();
    }

    pub fn delete_backward(&mut self) {
        if !self.editable() {
            return;
        }
        if self.is_irc() {
            if self.point.col > self.prompt_len {
                let mut chars: Vec<char> = self.input().chars().collect();
                let idx = self.point.col - self.prompt_len - 1;
                chars.remove(idx);
                let text: String = chars.into_iter().collect();
                self.input_set(&text);
                self.point.col -= 1;
            }
        } else if self.point.col > 0 {
            let row = self.point.row;
            let mut chars = self.lines[row].chars();
            chars.remove(self.point.col - 1);
            self.lines[row] = Line::plain(chars.into_iter().collect::<String>());
            self.point.col -= 1;
        } else if self.point.row > 0 {
            let row = self.point.row;
            let prev_len = self.lines[row - 1].char_len();
            let current = self.lines[row].text();
            let mut prev = self.lines[row - 1].chars();
            prev.extend(current.chars());
            self.lines[row - 1] = Line::plain(prev.into_iter().collect::<String>());
            self.lines.remove(row);
            self.point = Pos::new(row - 1, prev_len);
        }
        self.scroll_into_view();
    }

    pub fn delete_forward(&mut self) {
        if !self.editable() {
            return;
        }
        if self.is_irc() {
            let mut chars: Vec<char> = self.input().chars().collect();
            let idx = self.point.col - self.prompt_len;
            if idx < chars.len() {
                chars.remove(idx);
            }
            let text: String = chars.into_iter().collect();
            self.input_set(&text);
        } else {
            let row = self.point.row;
            let len = self.lines[row].char_len();
            if self.point.col < len {
                let mut chars = self.lines[row].chars();
                chars.remove(self.point.col);
                self.lines[row] = Line::plain(chars.into_iter().collect::<String>());
            } else if row + 1 < self.lines.len() {
                let next = self.lines[row + 1].text();
                let mut chars = self.lines[row].chars();
                chars.extend(next.chars());
                self.lines[row] = Line::plain(chars.into_iter().collect::<String>());
                self.lines.remove(row + 1);
            }
        }
        self.scroll_into_view();
    }

    /// Kill from point to end of line; returns the killed text.
    pub fn kill_line(&mut self) -> String {
        if !self.editable() {
            return String::new();
        }
        if self.is_irc() {
            let chars: Vec<char> = self.input().chars().collect();
            let idx = (self.point.col - self.prompt_len).min(chars.len());
            let killed: String = chars[idx..].iter().collect();
            let kept: String = chars[..idx].iter().collect();
            self.input_set(&kept);
            self.scroll_into_view();
            killed
        } else {
            let row = self.point.row;
            let chars = self.lines[row].chars();
            let idx = self.point.col.min(chars.len());
            let killed: String = chars[idx..].iter().collect();
            let kept: String = chars[..idx].iter().collect();
            self.lines[row] = Line::plain(kept);
            self.scroll_into_view();
            killed
        }
    }

    fn region_editable(&self, start: Pos, end: Pos) -> bool {
        if self.is_irc() {
            start.row == self.last_row()
                && end.row == self.last_row()
                && start.col >= self.prompt_len
                && end.col >= self.prompt_len
        } else {
            true
        }
    }

    /// Delete the text between `start` and `end` (ordered, `start <= end`).
    /// Returns false if the region is not editable (e.g. rcirc history).
    pub fn delete_region(&mut self, start: Pos, end: Pos) -> bool {
        if start == end {
            return true;
        }
        if start > end || !self.region_editable(start, end) {
            return false;
        }
        if self.is_irc() {
            let base = self.prompt_len;
            let mut chars: Vec<char> = self.input().chars().collect();
            let s = (start.col - base).min(chars.len());
            let e = (end.col - base).min(chars.len());
            chars.drain(s..e);
            let text: String = chars.into_iter().collect();
            self.input_set(&text);
            self.point = Pos::new(self.last_row(), base + s);
        } else if start.row == end.row {
            let mut chars = self.lines[start.row].chars();
            chars.drain(start.col..end.col.min(chars.len()));
            self.lines[start.row] = Line::plain(chars.into_iter().collect::<String>());
            self.point = Pos::new(start.row, start.col);
        } else {
            let left: String = self.lines[start.row].chars()[..start.col].iter().collect();
            let right_chars = self.lines[end.row].chars();
            let right: String = right_chars[end.col.min(right_chars.len())..]
                .iter()
                .collect();
            self.lines[start.row] = Line::plain(format!("{left}{right}"));
            for _ in 0..(end.row - start.row) {
                self.lines.remove(start.row + 1);
            }
            self.point = Pos::new(start.row, start.col);
        }
        self.mark = None;
        self.point.col = self.clamp_col(self.point.row, self.point.col);
        self.scroll_into_view();
        true
    }

    /// Kill the word before point (`M-DEL`), returning the killed text.
    pub fn backward_kill_word(&mut self) -> String {
        if !self.editable() {
            return String::new();
        }
        let row = self.point.row;
        let min = self.min_col(row);
        let chars = self.lines[row].chars();
        let mut col = self.point.col.min(chars.len());
        while col > min && !is_word(chars[col - 1]) {
            col -= 1;
        }
        while col > min && is_word(chars[col - 1]) {
            col -= 1;
        }
        if col == self.point.col {
            // At the line start: join with the previous line (kills the newline).
            if self.is_irc() || row == 0 {
                return String::new();
            }
            self.delete_backward();
            return "\n".to_string();
        }
        let start = Pos::new(row, col);
        let end = self.point;
        let text = self.text_between(start, end);
        self.delete_region(start, end);
        text
    }

    /// Kill from point to the end of the next word (`M-d`), returning the
    /// killed text.
    pub fn kill_word(&mut self) -> String {
        if !self.editable() {
            return String::new();
        }
        let row = self.point.row;
        let min = self.min_col(row);
        let chars = self.lines[row].chars();
        let mut col = self.point.col.min(chars.len());
        while col < chars.len() && !is_word(chars[col]) {
            col += 1;
        }
        while col < chars.len() && is_word(chars[col]) {
            col += 1;
        }
        col = col.max(min);
        if col == self.point.col {
            // At the line end: join with the next line (kills the newline).
            if self.is_irc() || row == self.last_row() {
                return String::new();
            }
            self.delete_forward();
            return "\n".to_string();
        }
        let start = self.point;
        let end = Pos::new(row, col);
        let text = self.text_between(start, end);
        self.delete_region(start, end);
        text
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

    /// Keep point within a window `height` rows tall, scrolling only when point
    /// reaches the top or bottom edge (Emacs-style). Unlike `scroll_into_view`,
    /// this also scrolls *down* when point falls below the window.
    pub fn ensure_point_visible(&mut self, height: usize) {
        let height = height.max(1);
        if self.point.row < self.scroll {
            self.scroll = self.point.row;
        } else if self.point.row >= self.scroll + height {
            self.scroll = self.point.row + 1 - height;
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
    fn upcase_and_downcase_word() {
        let mut b = Buffer::new("> ");
        for c in "hello world".chars() {
            b.insert_char(c);
        }
        b.beginning_of_line();
        b.upcase_word();
        assert_eq!(b.input(), "HELLO world");
        assert_eq!(b.point().col, 2 + 5);
        b.beginning_of_line();
        b.downcase_word();
        assert_eq!(b.input(), "hello world");
    }

    #[test]
    fn case_word_in_free_text() {
        let mut b = Buffer::text();
        b.insert_str("foo bar");
        b.beginning_of_buffer();
        b.upcase_word();
        assert_eq!(b.line(0).text(), "FOO bar");
    }

    #[test]
    fn backward_upcase_word() {
        let mut b = Buffer::new("> ");
        for c in "hello world".chars() {
            b.insert_char(c);
        }
        b.backward_upcase_word();
        assert_eq!(b.input(), "hello WORLD");
        assert_eq!(b.point().col, 2 + 6);
    }

    #[test]
    fn backward_downcase_word() {
        let mut b = Buffer::new("> ");
        for c in "HELLO WORLD".chars() {
            b.insert_char(c);
        }
        b.backward_downcase_word();
        assert_eq!(b.input(), "HELLO world");
        assert_eq!(b.point().col, 2 + 6);
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

    #[test]
    fn text_buffer_is_editable_everywhere() {
        let mut b = Buffer::text();
        assert!(!b.is_irc());
        assert!(b.editable());
        for c in "hello".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.line(0).text(), "hello");
        b.insert_newline();
        b.insert_char('x');
        assert_eq!(b.line(1).text(), "x");
        assert_eq!(b.len(), 2);
        // point may sit anywhere
        b.set_point(Pos::new(0, 2));
        assert!(b.editable());
    }

    #[test]
    fn text_backspace_joins_lines() {
        let mut b = Buffer::text();
        b.insert_char('a');
        b.insert_newline();
        b.insert_char('b');
        b.delete_backward();
        assert_eq!(b.line(1).text(), "");
        b.delete_backward();
        assert_eq!(b.len(), 1);
        assert_eq!(b.line(0).text(), "a");
        assert_eq!(b.point(), Pos::new(0, 1));
    }

    #[test]
    fn text_forward_delete_joins_lines() {
        let mut b = Buffer::text();
        b.insert_char('a');
        b.insert_newline();
        b.insert_char('b');
        b.set_point(Pos::new(0, 1));
        b.delete_forward();
        assert_eq!(b.len(), 1);
        assert_eq!(b.line(0).text(), "ab");
    }

    #[test]
    fn backward_kill_word_deletes_word() {
        let mut b = Buffer::new("> ");
        b.set_input("hello world");
        assert_eq!(b.backward_kill_word(), "world");
        assert_eq!(b.input(), "hello ");
        assert_eq!(b.backward_kill_word(), "hello ");
        assert_eq!(b.input(), "");
        assert_eq!(b.backward_kill_word(), "");
    }

    #[test]
    fn kill_word_deletes_forward() {
        let mut b = Buffer::new("> ");
        b.set_input("hello world");
        b.beginning_of_line();
        // Like Emacs, point moves to the end of the word (before the space).
        assert_eq!(b.kill_word(), "hello");
        assert_eq!(b.input(), " world");
        assert_eq!(b.kill_word(), " world");
        assert_eq!(b.input(), "");
        assert_eq!(b.kill_word(), "");
    }

    #[test]
    fn delete_region_in_input() {
        let mut b = Buffer::new("> ");
        b.set_input("hello world");
        b.set_point(Pos::new(0, 8));
        b.set_mark();
        b.set_point(Pos::new(0, 13));
        let (start, end) = b.region().unwrap();
        assert_eq!(b.region_text().unwrap(), "world");
        assert!(b.delete_region(start, end));
        assert_eq!(b.input(), "hello ");
    }

    #[test]
    fn delete_region_rejects_read_only_history() {
        let mut b = Buffer::new("> ");
        b.push_plain("history line");
        b.set_point(Pos::new(0, 0));
        b.set_mark();
        b.set_point(Pos::new(0, 4));
        let (start, end) = b.region().unwrap();
        assert!(!b.delete_region(start, end));
    }

    #[test]
    fn delete_region_multiline_text() {
        let mut b = Buffer::text();
        b.insert_str("abc\ndef");
        b.set_point(Pos::new(0, 1));
        b.set_mark();
        b.set_point(Pos::new(1, 2));
        let (start, end) = b.region().unwrap();
        assert!(b.delete_region(start, end));
        assert_eq!(b.line(0).text(), "af");
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn paging_moves_point_and_scroll() {
        let mut b = Buffer::new("> ");
        for i in 0..50 {
            b.push_plain(format!("line {i}"));
        }
        b.end_of_buffer();
        let before = b.point().row;
        b.page_backward(8);
        assert_eq!(b.point().row, before - 8);
        b.page_forward(8);
        assert_eq!(b.point().row, before);
    }

    #[test]
    fn short_line_is_not_wrapped() {
        let line = Line::plain("hello");
        assert_eq!(line.wrap(70).len(), 1);
    }

    #[test]
    fn wraps_long_line_at_width() {
        let line = Line::plain("a".repeat(80)).indented(4);
        let wrapped = line.wrap(40);
        assert!(wrapped.len() >= 2);
        for l in &wrapped {
            assert!(l.char_len() <= 40, "line too long: {}", l.char_len());
        }
        // continuation lines carry the indent
        assert!(wrapped[1].text().starts_with("    "));
    }

    #[test]
    fn wraps_on_word_boundaries() {
        let line = Line::plain("word ".repeat(20).trim_end());
        let wrapped = line.wrap(30);
        assert!(wrapped.len() > 1);
        for l in &wrapped {
            assert!(l.char_len() <= 30);
            assert!(!l.text().starts_with(' '));
            assert!(!l.text().ends_with(' '));
        }
    }

    #[test]
    fn wrapping_preserves_styles() {
        let line = Line::from_spans(vec![
            Span::new("nick: ", Style::rgb(1, 2, 3)),
            Span::new("y".repeat(80), Style::default()),
        ])
        .indented(6);
        let wrapped = line.wrap(40);
        assert!(wrapped[0].text().starts_with("nick: "));
        assert_eq!(wrapped[0].spans[0].style, Style::rgb(1, 2, 3));
        assert!(wrapped[1].text().starts_with("      "));
    }

    #[test]
    fn urls_are_never_split() {
        let prefix = "<nick> ";
        // A URL that does not fit after the text moves whole to the next line.
        let url = format!("https://example.com/{}", "a".repeat(60));
        let line = Line::plain(format!("{prefix}see {url}")).indented(prefix.chars().count());
        let wrapped = line.wrap(40);
        assert!(
            wrapped.iter().any(|l| l.text().contains(&url)),
            "URL split across lines: {wrapped:?}"
        );

        // A URL wider than the line stays on the line it started on (no extra
        // break) and overflows intact rather than being cut.
        let long = format!("https://example.com/{}", "b".repeat(80));
        let line = Line::plain(format!("{prefix}{long}")).indented(prefix.chars().count());
        let wrapped = line.wrap(40);
        assert_eq!(
            wrapped.len(),
            1,
            "overflowing URL should not get its own line: {wrapped:?}"
        );
        assert!(wrapped[0].text().contains(&long));
    }

    #[test]
    fn text_insert_str_with_newlines() {
        let mut b = Buffer::text();
        b.insert_str("a\nb");
        assert_eq!(b.len(), 2);
        assert_eq!(b.line(0).text(), "a");
        assert_eq!(b.line(1).text(), "b");
    }
}
