//! Zellij-style per-character ANSI style diffing.

use std::fmt::{self, Display, Formatter, Write as FmtWrite};

use alacritty_terminal::{
    term::cell::Flags,
    vte::ansi::{Color, NamedColor},
};

use crate::terminal::TerminalCell;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnsiCode {
    Reset,
    On,
    Rgb(u8, u8, u8),
    ColorIndex(u8),
    Named(NamedColor),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CharacterStyles {
    pub foreground: Option<AnsiCode>,
    pub background: Option<AnsiCode>,
    pub bold: Option<AnsiCode>,
    pub dim: Option<AnsiCode>,
    pub italic: Option<AnsiCode>,
    pub underline: Option<AnsiCode>,
    pub reverse: Option<AnsiCode>,
}

pub const DEFAULT_STYLES: CharacterStyles = CharacterStyles {
    foreground: None,
    background: None,
    bold: None,
    dim: None,
    italic: None,
    underline: None,
    reverse: None,
};

pub const RESET_STYLES: CharacterStyles = CharacterStyles {
    foreground: Some(AnsiCode::Reset),
    background: Some(AnsiCode::Reset),
    bold: Some(AnsiCode::Reset),
    dim: Some(AnsiCode::Reset),
    italic: Some(AnsiCode::Reset),
    underline: Some(AnsiCode::Reset),
    reverse: Some(AnsiCode::Reset),
};

impl CharacterStyles {
    pub fn from_cell(cell: &TerminalCell) -> Self {
        CharacterStyles {
            foreground: Some(color_to_ansi_or_reset(cell.fg)),
            background: Some(color_to_ansi_or_reset(cell.bg)),
            bold: flag_to_ansi(cell.flags.contains(Flags::BOLD)),
            dim: flag_to_ansi(cell.flags.contains(Flags::DIM)),
            italic: flag_to_ansi(cell.flags.contains(Flags::ITALIC)),
            underline: flag_to_ansi(
                cell.flags.intersects(Flags::ALL_UNDERLINES),
            ),
            reverse: flag_to_ansi(cell.flags.contains(Flags::INVERSE)),
        }
    }

    pub fn from_copy_run(fg: Color, bg: Color, flags: u8) -> Self {
        CharacterStyles {
            foreground: Some(color_to_ansi_or_reset(fg)),
            background: Some(color_to_ansi_or_reset(bg)),
            bold: flag_to_ansi(flags & 2 != 0),
            dim: flag_to_ansi(flags & 1 != 0),
            italic: flag_to_ansi(flags & 4 != 0),
            underline: flag_to_ansi(flags & 8 != 0),
            reverse: flag_to_ansi(flags & 16 != 0),
        }
    }

    pub fn from_layout_run(fg: &str, bg: &str, flags: u8) -> Self {
        Self::from_copy_run(
            layout_color_str(fg, true),
            layout_color_str(bg, false),
            flags,
        )
    }

    pub fn with_background(mut self, bg: AnsiCode) -> Self {
        self.background = Some(bg);
        self
    }

    pub fn update_and_return_diff(
        &mut self,
        new_styles: &CharacterStyles,
    ) -> Option<CharacterStyles> {
        if self == new_styles {
            return None;
        }
        if *new_styles == RESET_STYLES {
            *self = RESET_STYLES;
            return Some(RESET_STYLES);
        }
        let mut diff = DEFAULT_STYLES;
        if self.foreground != new_styles.foreground {
            diff.foreground = new_styles.foreground;
        }
        if self.background != new_styles.background {
            diff.background = new_styles.background;
        }
        if self.bold != new_styles.bold {
            diff.bold = new_styles.bold;
        }
        if self.dim != new_styles.dim {
            diff.dim = new_styles.dim;
        }
        if self.italic != new_styles.italic {
            diff.italic = new_styles.italic;
        }
        if self.underline != new_styles.underline {
            diff.underline = new_styles.underline;
        }
        if self.reverse != new_styles.reverse {
            diff.reverse = new_styles.reverse;
        }
        *self = *new_styles;
        Some(diff)
    }
}

impl Display for CharacterStyles {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self == &RESET_STYLES {
            return write!(f, "\x1b[m");
        }
        if let Some(code) = self.foreground {
            write_ansi_fg(f, code)?;
        }
        if let Some(code) = self.background {
            write_ansi_bg(f, code)?;
        }
        if let Some(code) = self.bold {
            match code {
                AnsiCode::On => write!(f, "\x1b[1m")?,
                AnsiCode::Reset => write!(f, "\x1b[22m")?,
                _ => {}
            }
        }
        if let Some(code) = self.dim {
            match code {
                AnsiCode::On => write!(f, "\x1b[2m")?,
                AnsiCode::Reset => {
                    write!(f, "\x1b[22m")?;
                    if self.bold == Some(AnsiCode::On) {
                        write!(f, "\x1b[1m")?;
                    }
                }
                _ => {}
            }
        }
        if let Some(code) = self.italic {
            match code {
                AnsiCode::On => write!(f, "\x1b[3m")?,
                AnsiCode::Reset => write!(f, "\x1b[23m")?,
                _ => {}
            }
        }
        if let Some(code) = self.underline {
            match code {
                AnsiCode::On => write!(f, "\x1b[4m")?,
                AnsiCode::Reset => write!(f, "\x1b[24m")?,
                _ => {}
            }
        }
        if let Some(code) = self.reverse {
            match code {
                AnsiCode::On => write!(f, "\x1b[7m")?,
                AnsiCode::Reset => write!(f, "\x1b[27m")?,
                _ => {}
            }
        }
        Ok(())
    }
}

pub fn adjust_styles_for_custom_bg_fg(
    character_styles: CharacterStyles,
    pane_default_fg: Option<AnsiCode>,
    pane_default_bg: Option<AnsiCode>,
) -> CharacterStyles {
    let mut character_styles = character_styles;
    if character_styles.foreground.is_none()
        || character_styles.foreground == Some(AnsiCode::Reset)
    {
        if let Some(fg) = pane_default_fg {
            character_styles.foreground = Some(fg);
        }
    }
    if character_styles.background.is_none()
        || character_styles.background == Some(AnsiCode::Reset)
    {
        if let Some(bg) = pane_default_bg {
            character_styles.background = Some(bg);
        }
    }
    character_styles
}

pub fn pane_default_ansi(rgb: (u8, u8, u8)) -> AnsiCode {
    AnsiCode::Rgb(rgb.0, rgb.1, rgb.2)
}

pub fn color_to_ansi(color: Color) -> Option<AnsiCode> {
    match color {
        Color::Named(NamedColor::Foreground | NamedColor::Background) => None,
        Color::Named(named) => Some(AnsiCode::Named(named)),
        Color::Indexed(index) => Some(AnsiCode::ColorIndex(index)),
        Color::Spec(rgb) => Some(AnsiCode::Rgb(rgb.r, rgb.g, rgb.b)),
    }
}

pub fn color_to_ansi_or_reset(color: Color) -> AnsiCode {
    match color {
        Color::Named(NamedColor::Foreground | NamedColor::Background) => {
            AnsiCode::Reset
        }
        Color::Named(named) => AnsiCode::Named(named),
        Color::Indexed(index) => AnsiCode::ColorIndex(index),
        Color::Spec(rgb) => AnsiCode::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

fn flag_to_ansi(on: bool) -> Option<AnsiCode> {
    Some(if on { AnsiCode::On } else { AnsiCode::Reset })
}

fn layout_color_str(name: &str, fg: bool) -> Color {
    use ratatui::style::Color as RtColor;

    use crate::style::parse_color;
    match parse_color(name) {
        RtColor::Reset => {
            if fg {
                Color::Named(NamedColor::Foreground)
            } else {
                Color::Named(NamedColor::Background)
            }
        }
        RtColor::Black => Color::Named(NamedColor::Black),
        RtColor::Red => Color::Named(NamedColor::Red),
        RtColor::Green => Color::Named(NamedColor::Green),
        RtColor::Yellow => Color::Named(NamedColor::Yellow),
        RtColor::Blue => Color::Named(NamedColor::Blue),
        RtColor::Magenta => Color::Named(NamedColor::Magenta),
        RtColor::Cyan => Color::Named(NamedColor::Cyan),
        RtColor::White | RtColor::Gray => Color::Named(NamedColor::White),
        RtColor::DarkGray => Color::Named(NamedColor::BrightBlack),
        RtColor::LightRed => Color::Named(NamedColor::BrightRed),
        RtColor::LightGreen => Color::Named(NamedColor::BrightGreen),
        RtColor::LightYellow => Color::Named(NamedColor::BrightYellow),
        RtColor::LightBlue => Color::Named(NamedColor::BrightBlue),
        RtColor::LightMagenta => Color::Named(NamedColor::BrightMagenta),
        RtColor::LightCyan => Color::Named(NamedColor::BrightCyan),
        RtColor::Indexed(i) => Color::Indexed(i),
        RtColor::Rgb(r, g, b) => {
            Color::Spec(alacritty_terminal::vte::ansi::Rgb { r, g, b })
        }
    }
}

/// Last non-default background on the row (from the rightmost styled cell).
pub fn row_trailing_bg(cells: &[Option<TerminalCell>]) -> AnsiCode {
    cells
        .iter()
        .rev()
        .flatten()
        .map(|cell| color_to_ansi_or_reset(cell.bg))
        .find(|bg| *bg != AnsiCode::Reset)
        .unwrap_or(AnsiCode::Reset)
}

fn named_fg_code(named: NamedColor) -> u8 {
    match named {
        NamedColor::Black => 30,
        NamedColor::Red => 31,
        NamedColor::Green => 32,
        NamedColor::Yellow => 33,
        NamedColor::Blue => 34,
        NamedColor::Magenta => 35,
        NamedColor::Cyan => 36,
        NamedColor::White => 37,
        NamedColor::BrightBlack => 90,
        NamedColor::BrightRed => 91,
        NamedColor::BrightGreen => 92,
        NamedColor::BrightYellow => 93,
        NamedColor::BrightBlue => 94,
        NamedColor::BrightMagenta => 95,
        NamedColor::BrightCyan => 96,
        NamedColor::BrightWhite => 97,
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor => 39,
        _ => 39,
    }
}

fn named_bg_code(named: NamedColor) -> u8 {
    match named {
        NamedColor::Black => 40,
        NamedColor::Red => 41,
        NamedColor::Green => 42,
        NamedColor::Yellow => 43,
        NamedColor::Blue => 44,
        NamedColor::Magenta => 45,
        NamedColor::Cyan => 46,
        NamedColor::White => 47,
        NamedColor::BrightBlack => 100,
        NamedColor::BrightRed => 101,
        NamedColor::BrightGreen => 102,
        NamedColor::BrightYellow => 103,
        NamedColor::BrightBlue => 104,
        NamedColor::BrightMagenta => 105,
        NamedColor::BrightCyan => 106,
        NamedColor::BrightWhite => 107,
        NamedColor::Foreground
        | NamedColor::Background
        | NamedColor::Cursor => 49,
        _ => 49,
    }
}

fn write_ansi_fg(f: &mut Formatter<'_>, code: AnsiCode) -> fmt::Result {
    match code {
        AnsiCode::Rgb(r, g, b) => write!(f, "\x1b[38;2;{};{};{}m", r, g, b),
        AnsiCode::ColorIndex(i) => write!(f, "\x1b[38;5;{}m", i),
        AnsiCode::Reset => write!(f, "\x1b[39m"),
        AnsiCode::Named(named) => write!(f, "\x1b[{}m", named_fg_code(named)),
        AnsiCode::On => Ok(()),
    }
}

fn write_ansi_bg(f: &mut Formatter<'_>, code: AnsiCode) -> fmt::Result {
    match code {
        AnsiCode::Rgb(r, g, b) => write!(f, "\x1b[48;2;{};{};{}m", r, g, b),
        AnsiCode::ColorIndex(i) => write!(f, "\x1b[48;5;{}m", i),
        AnsiCode::Reset => write!(f, "\x1b[49m"),
        AnsiCode::Named(named) => write!(f, "\x1b[{}m", named_bg_code(named)),
        AnsiCode::On => Ok(()),
    }
}

pub fn vte_goto(x: u16, y: u16, out: &mut String) {
    // Reset SGR after the cup so callers that re-init `current_styles` to
    // DEFAULT_STYLES stay in sync with the host terminal.
    let _ = write!(out, "\x1b[{};{}H\x1b[m", y + 1, x + 1);
}

/// Move the cursor without resetting SGR. Used for per-cell cups inside a row
/// so style diff state stays valid while still preventing host wrap.
pub fn vte_cup(x: u16, y: u16, out: &mut String) {
    let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
}

pub fn write_style_diff(
    current: &mut CharacterStyles,
    new_styles: CharacterStyles,
    out: &mut String,
) {
    if let Some(diff) = current.update_and_return_diff(&new_styles) {
        let _ = write!(out, "{diff}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dim_reset_reapplies_bold_when_bold_in_same_diff() {
        let diff = CharacterStyles {
            dim: Some(AnsiCode::Reset),
            bold: Some(AnsiCode::On),
            ..DEFAULT_STYLES
        };
        assert_eq!(format!("{diff}"), "\x1b[1m\x1b[22m\x1b[1m");
    }

    #[test]
    fn colored_then_default_fg_emits_reset_within_line() {
        let mut current = DEFAULT_STYLES;
        let mut out = String::new();
        let red = CharacterStyles {
            foreground: Some(AnsiCode::Named(NamedColor::Red)),
            background: Some(AnsiCode::Reset),
            bold: Some(AnsiCode::Reset),
            dim: Some(AnsiCode::Reset),
            italic: Some(AnsiCode::Reset),
            underline: Some(AnsiCode::Reset),
            reverse: Some(AnsiCode::Reset),
        };
        let normal = CharacterStyles::from_cell(&TerminalCell {
            text: "B".to_string(),
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            width: 1,
        });
        write_style_diff(&mut current, red, &mut out);
        out.push('A');
        write_style_diff(&mut current, normal, &mut out);
        out.push('B');
        let reset_after_a = out
            .split_once('A')
            .is_some_and(|(_, tail)| tail.starts_with("\x1b[m"));
        assert!(
            reset_after_a,
            "default fg after red must emit reset, got {out:?}"
        );
    }

    #[test]
    fn pane_default_bg_applied_for_reset_background() {
        let adjusted = adjust_styles_for_custom_bg_fg(
            CharacterStyles {
                background: Some(AnsiCode::Reset),
                ..DEFAULT_STYLES
            },
            None,
            Some(AnsiCode::Rgb(0, 26, 58)),
        );
        assert_eq!(adjusted.background, Some(AnsiCode::Rgb(0, 26, 58)));
    }
}
