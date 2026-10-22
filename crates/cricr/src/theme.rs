//! rcirc face colors, resolved against Emacs' color list.
//!
//! The faces and their light/dark-background variants come from the
//! `defface` forms at the end of `contrib/rcirc.el`; the hex values are the
//! Emacs color names (e.g. `LightSkyBlue`) as listed in `colors_list.txt`.

use serde::Deserialize;

use crate::buffer::{Color, Style};

/// Which background the terminal uses, selecting the rcirc face variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    #[default]
    Dark,
    Light,
}

/// The set of rcirc faces used for rendering.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub my_nick: Style,
    pub other_nick: Style,
    #[allow(dead_code)]
    pub bright_nick: Style,
    #[allow(dead_code)]
    pub dim_nick: Style,
    pub server: Style,
    #[allow(dead_code)]
    pub server_prefix: Style,
    pub timestamp: Style,
    pub prompt: Style,
    pub nick_in_message: Style,
    pub url: Style,
    #[allow(dead_code)]
    pub keyword: Style,
    /// `font-lock-warning-face`, used for `!!! ` error/disconnect lines.
    pub warning: Style,
    pub track_nick: Style,
    pub track_keyword: Style,
    /// `lazy-highlight`: other matches during isearch.
    pub lazy_highlight: Style,
    /// `isearch`: the current match.
    pub isearch: Style,
}

impl Theme {
    pub fn for_background(background: Background) -> Theme {
        match background {
            Background::Dark => Theme {
                my_nick: Style::rgb(0x87, 0xce, 0xfa),     // LightSkyBlue
                other_nick: Style::rgb(0xee, 0xdd, 0x82),  // LightGoldenrod
                bright_nick: Style::rgb(0x7f, 0xff, 0xd4), // Aquamarine
                dim_nick: Style::default(),
                server: Style::rgb(0xff, 0x7f, 0x24), // chocolate1
                server_prefix: Style::rgb(0xff, 0x7f, 0x24),
                timestamp: Style::default(),
                prompt: Style::rgb(0x00, 0xff, 0xff), // cyan1
                nick_in_message: Style::rgb(0x00, 0xff, 0xff), // Cyan1
                url: Style::default().bold(),
                keyword: Style::default().reverse(),
                // font-lock-warning-face (dark bg): Red1, bold.
                warning: Style::rgb(0xff, 0x00, 0x00).bold(),
                track_nick: Style::default().reverse(),
                track_keyword: Style::default().bold(),
                // isearch.el: (:background "paleturquoise4" :distant-foreground "white")
                lazy_highlight: Style::fg(Color::White).bg(Color::Rgb(0x66, 0x8b, 0x8b)),
                // isearch.el: (:background "palevioletred2" :foreground "brown4")
                isearch: Style::fg(Color::Rgb(0x8b, 0x23, 0x23)).bg(Color::Rgb(0xee, 0x79, 0x9f)),
            },
            Background::Light => Theme {
                my_nick: Style::rgb(0x00, 0x00, 0xff),     // Blue1
                other_nick: Style::rgb(0xb8, 0x86, 0x0b),  // DarkGoldenrod
                bright_nick: Style::rgb(0x5f, 0x9e, 0xa0), // CadetBlue
                dim_nick: Style::default(),
                server: Style::rgb(0xb2, 0x22, 0x22), // Firebrick
                server_prefix: Style::rgb(0xb2, 0x22, 0x22),
                timestamp: Style::default(),
                prompt: Style::rgb(0x00, 0x00, 0x8b), // dark blue
                nick_in_message: Style::rgb(0xa0, 0x20, 0xf0), // Purple
                url: Style::default().bold(),
                keyword: Style::default().reverse(),
                warning: Style::rgb(0x8b, 0x00, 0x00).bold(), // dark red
                track_nick: Style::default().reverse(),
                track_keyword: Style::default().bold(),
                // isearch.el: (:background "paleturquoise" :distant-foreground "black")
                lazy_highlight: Style::fg(Color::Black).bg(Color::Rgb(0xaf, 0xee, 0xee)),
                // isearch.el: (:background "magenta3" :foreground "lightskyblue1")
                isearch: Style::fg(Color::Rgb(0xb0, 0xe2, 0xff)).bg(Color::Rgb(0xcd, 0x00, 0xcd)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_uses_light_variants() {
        let theme = Theme::for_background(Background::Dark);
        assert_eq!(theme.my_nick, Style::rgb(0x87, 0xce, 0xfa));
        assert_eq!(theme.other_nick, Style::rgb(0xee, 0xdd, 0x82));
        assert!(theme.url.bold);
        assert!(theme.track_nick.reverse);
        assert_eq!(theme.lazy_highlight.bg, Color::Rgb(0x66, 0x8b, 0x8b));
        assert_eq!(theme.isearch.bg, Color::Rgb(0xee, 0x79, 0x9f));
        assert_eq!(theme.isearch.color, Color::Rgb(0x8b, 0x23, 0x23));
    }

    #[test]
    fn light_uses_dark_variants() {
        let theme = Theme::for_background(Background::Light);
        assert_eq!(theme.my_nick, Style::rgb(0x00, 0x00, 0xff));
        assert_eq!(theme.server, Style::rgb(0xb2, 0x22, 0x22));
        assert_eq!(theme.lazy_highlight.bg, Color::Rgb(0xaf, 0xee, 0xee));
        assert_eq!(theme.isearch.bg, Color::Rgb(0xcd, 0x00, 0xcd));
    }
}
