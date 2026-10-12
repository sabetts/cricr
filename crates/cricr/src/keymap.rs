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
    BeginningOfBuffer,
    EndOfBuffer,
    PageUp,
    PageDown,
    DeleteBackward,
    DeleteForward,
    KillLine,
    SetMark,
    KillRegion,
    CopyRegion,
    Yank,
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
            'v' | 'V' => PageUp,
            '<' => BeginningOfBuffer,
            '>' => EndOfBuffer,
            'p' | 'P' => HistoryPrev,
            'n' | 'N' => HistoryNext,
            'w' | 'W' => CopyRegion,
            _ => None,
        },
        KeyCode::Char(c) => SelfInsert(c),
        KeyCode::Enter => Send,
        KeyCode::Tab => Complete,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
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
}
