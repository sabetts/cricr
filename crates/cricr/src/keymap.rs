//! Translate terminal key events into Emacs-style editor actions.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// An editor action produced from a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    SelfInsert(char),
    Send,
    ForwardChar,
    BackwardChar,
    NextLine,
    PrevLine,
    BeginningOfLine,
    EndOfLine,
    ForwardWord,
    BackwardWord,
    UpcaseWord,
    DowncaseWord,
    BeginningOfBuffer,
    EndOfBuffer,
    PageUp,
    PageDown,
    DeleteBackward,
    DeleteForward,
    BackwardKillWord,
    KillLine,
    SetMark,
    KillRegion,
    CopyRegion,
    Yank,
    YankPop,
    HistoryPrev,
    HistoryNext,
    Complete,
    SearchForward,
    SearchBackward,
    QuotedInsert,
    Abort,
}

/// Map a key press to an action.
pub fn translate(key: KeyEvent) -> Action {
    use Action::*;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            'f' => ForwardChar,
            'b' => BackwardChar,
            'n' => NextLine,
            'p' => PrevLine,
            'a' => BeginningOfLine,
            'e' => EndOfLine,
            'v' => PageDown,
            's' => SearchForward,
            'r' => SearchBackward,
            'd' => DeleteForward,
            'k' => KillLine,
            'w' => KillRegion,
            'y' => Yank,
            'g' => Abort,
            'q' => QuotedInsert,
            ' ' => SetMark,
            _ => None,
        },
        KeyCode::Char(c) if alt => match c {
            'f' | 'F' => ForwardWord,
            'b' | 'B' => BackwardWord,
            'u' | 'U' => UpcaseWord,
            'l' | 'L' => DowncaseWord,
            'v' | 'V' => PageUp,
            '<' => BeginningOfBuffer,
            '>' => EndOfBuffer,
            'p' | 'P' => HistoryPrev,
            'n' | 'N' => HistoryNext,
            'w' | 'W' => CopyRegion,
            'y' | 'Y' => YankPop,
            _ => None,
        },
        KeyCode::Char(c) => SelfInsert(c),
        KeyCode::Enter => Send,
        KeyCode::Tab => Complete,
        // M-DEL / M-Backspace: kill the word before point.
        KeyCode::Backspace if alt => BackwardKillWord,
        KeyCode::Backspace => DeleteBackward,
        KeyCode::Delete => DeleteForward,
        KeyCode::Up => PrevLine,
        KeyCode::Down => NextLine,
        KeyCode::Left => BackwardChar,
        KeyCode::Right => ForwardChar,
        KeyCode::Home => BeginningOfLine,
        KeyCode::End => EndOfLine,
        KeyCode::PageUp => PageUp,
        KeyCode::PageDown => PageDown,
        _ => None,
    }
}

/// Render a key the way Emacs echoes it (`C-x`, `M-f`, `C-x C-f`, `TAB`).
/// Returns `None` for keys we don't want to report as undefined.
pub fn describe(key: KeyEvent) -> Option<String> {
    use KeyCode::*;
    let base = match key.code {
        Char(' ') | Char('\0') => "SPC".to_string(),
        Char(c) => c.to_string(),
        Enter => "RET".to_string(),
        Tab => "TAB".to_string(),
        Backspace | Delete => "DEL".to_string(),
        Up => "<up>".to_string(),
        Down => "<down>".to_string(),
        Left => "<left>".to_string(),
        Right => "<right>".to_string(),
        Home => "<home>".to_string(),
        End => "<end>".to_string(),
        PageUp => "<prior>".to_string(),
        PageDown => "<next>".to_string(),
        F(n) => format!("<f{n}>"),
        _ => return None,
    };
    let mut out = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str("C-");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        out.push_str("M-");
    }
    out.push_str(&base);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn describe_renders_emacs_names() {
        assert_eq!(
            describe(key(KeyCode::Char('f'), KeyModifiers::CONTROL)).unwrap(),
            "C-f"
        );
        assert_eq!(
            describe(key(KeyCode::Char('b'), KeyModifiers::ALT)).unwrap(),
            "M-b"
        );
        assert_eq!(
            describe(key(KeyCode::Enter, KeyModifiers::NONE)).unwrap(),
            "RET"
        );
        assert_eq!(
            describe(key(KeyCode::Char(' '), KeyModifiers::CONTROL)).unwrap(),
            "C-SPC"
        );
        assert_eq!(describe(key(KeyCode::Esc, KeyModifiers::NONE)), None);
    }

    #[test]
    fn control_movement() {
        assert_eq!(
            translate(key(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Action::ForwardChar
        );
        assert_eq!(
            translate(key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::SearchForward
        );
    }

    #[test]
    fn alt_word_motion() {
        assert_eq!(
            translate(key(KeyCode::Char('f'), KeyModifiers::ALT)),
            Action::ForwardWord
        );
        assert_eq!(
            translate(key(KeyCode::Char('<'), KeyModifiers::ALT)),
            Action::BeginningOfBuffer
        );
    }

    #[test]
    fn alt_case_words() {
        assert_eq!(
            translate(key(KeyCode::Char('u'), KeyModifiers::ALT)),
            Action::UpcaseWord
        );
        assert_eq!(
            translate(key(KeyCode::Char('l'), KeyModifiers::ALT)),
            Action::DowncaseWord
        );
    }

    #[test]
    fn plain_char_inserts() {
        assert_eq!(
            translate(key(KeyCode::Char('a'), KeyModifiers::NONE)),
            Action::SelfInsert('a')
        );
    }

    #[test]
    fn enter_sends() {
        assert_eq!(
            translate(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Send
        );
    }

    #[test]
    fn alt_backspace_kills_word() {
        assert_eq!(
            translate(key(KeyCode::Backspace, KeyModifiers::ALT)),
            Action::BackwardKillWord
        );
        assert_eq!(
            translate(key(KeyCode::Backspace, KeyModifiers::NONE)),
            Action::DeleteBackward
        );
    }
}
