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
    TransposeChars,
    TransposeWords,
    DabbrevExpand,
    BeginningOfBuffer,
    EndOfBuffer,
    PageUp,
    PageDown,
    DeleteBackward,
    DeleteForward,
    Undo,
    BackwardKillWord,
    KillWord,
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
        // Terminals report `C-SPC` in several ways: a space with CONTROL,
        // `C-@` (the same character historically), NUL, or `KeyCode::Null`.
        KeyCode::Char(' ') | KeyCode::Char('@') | KeyCode::Char('\0') if ctrl => SetMark,
        KeyCode::Char('\0') | KeyCode::Null => SetMark,
        // C-M-< / C-M-> are aliases for beginning/end of buffer.
        KeyCode::Char('<') if ctrl && alt => BeginningOfBuffer,
        KeyCode::Char('>') if ctrl && alt => EndOfBuffer,
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
            't' => TransposeChars,
            // Emacs spells undo `C-_`; terminals often report it as C-- or
            // C-/, depending on the layout.
            '_' | '-' | '/' => Undo,
            // The Alacritty NUL workaround for C-SPC arrives via ConPTY as
            // Ctrl+Shift+2 on a US layout.
            #[cfg(windows)]
            '2' if key.modifiers.contains(KeyModifiers::SHIFT) => SetMark,
            _ => None,
        },
        KeyCode::Char(c) if alt => match c {
            'f' | 'F' => ForwardWord,
            'b' | 'B' => BackwardWord,
            'u' | 'U' => UpcaseWord,
            'l' | 'L' => DowncaseWord,
            't' | 'T' => TransposeWords,
            'd' | 'D' => KillWord,
            '/' => DabbrevExpand,
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
        Char(' ') | Char('\0') | Null => "SPC".to_string(),
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

/// Whether a Ctrl key is currently held. Used to recover `C-SPC` from
/// terminals (notably Alacritty on Windows) that report it as a plain space.
#[cfg(windows)]
pub(crate) fn ctrl_is_down() -> bool {
    const VK_CONTROL: i32 = 0x11;
    #[link(name = "user32")]
    extern "system" {
        fn GetAsyncKeyState(v_key: i32) -> i16;
    }
    unsafe { GetAsyncKeyState(VK_CONTROL) as u16 & 0x8000 != 0 }
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
    fn control_meta_buffer_motion() {
        assert_eq!(
            translate(key(
                KeyCode::Char('<'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            Action::BeginningOfBuffer
        );
        assert_eq!(
            translate(key(
                KeyCode::Char('>'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            Action::EndOfBuffer
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
    fn transpose_bindings() {
        assert_eq!(
            translate(key(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Action::TransposeChars
        );
        assert_eq!(
            translate(key(KeyCode::Char('t'), KeyModifiers::ALT)),
            Action::TransposeWords
        );
    }

    #[test]
    fn undo_bindings() {
        assert_eq!(
            translate(key(KeyCode::Char('_'), KeyModifiers::CONTROL)),
            Action::Undo
        );
        assert_eq!(
            translate(key(KeyCode::Char('-'), KeyModifiers::CONTROL)),
            Action::Undo
        );
        assert_eq!(
            translate(key(KeyCode::Char('/'), KeyModifiers::CONTROL)),
            Action::Undo
        );
    }

    #[test]
    fn ctrl_space_variants_set_mark() {
        assert_eq!(
            translate(key(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            Action::SetMark
        );
        assert_eq!(
            translate(key(KeyCode::Char('@'), KeyModifiers::CONTROL)),
            Action::SetMark
        );
        assert_eq!(
            translate(key(KeyCode::Char('\0'), KeyModifiers::NONE)),
            Action::SetMark
        );
        assert_eq!(
            translate(key(KeyCode::Null, KeyModifiers::NONE)),
            Action::SetMark
        );
        // Plain space and `@` still insert themselves.
        assert_eq!(
            translate(key(KeyCode::Char(' '), KeyModifiers::NONE)),
            Action::SelfInsert(' ')
        );
        assert_eq!(
            translate(key(KeyCode::Char('@'), KeyModifiers::NONE)),
            Action::SelfInsert('@')
        );
    }

    #[cfg(windows)]
    #[test]
    fn alacritty_nul_workaround_sets_mark() {
        assert_eq!(
            translate(key(
                KeyCode::Char('2'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Action::SetMark
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
    fn alt_slash_dabbrev_expands() {
        assert_eq!(
            translate(key(KeyCode::Char('/'), KeyModifiers::ALT)),
            Action::DabbrevExpand
        );
    }

    #[test]
    fn alt_d_kills_word_forward() {
        assert_eq!(
            translate(key(KeyCode::Char('d'), KeyModifiers::ALT)),
            Action::KillWord
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
