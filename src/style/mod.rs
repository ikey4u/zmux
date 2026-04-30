use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};
use ratatui::style::{Color, Modifier, Style};

pub fn parse_color(s: &str) -> Color {
    let s = s.trim();
    match s {
        "default" | "" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "brightblack" | "colour8" => Color::DarkGray,
        "brightred" => Color::LightRed,
        "brightgreen" => Color::LightGreen,
        "brightyellow" => Color::LightYellow,
        "brightblue" => Color::LightBlue,
        "brightmagenta" => Color::LightMagenta,
        "brightcyan" => Color::LightCyan,
        "brightwhite" => Color::White,
        _ if s.starts_with('#') && s.len() == 7 => {
            let r = u8::from_str_radix(&s[1..3], 16).unwrap_or(0);
            let g = u8::from_str_radix(&s[3..5], 16).unwrap_or(0);
            let b = u8::from_str_radix(&s[5..7], 16).unwrap_or(0);
            Color::Rgb(r, g, b)
        }
        _ if s.starts_with("colour") || s.starts_with("color") => {
            let num_str =
                s.trim_start_matches("colour").trim_start_matches("color");
            if let Ok(n) = num_str.parse::<u8>() {
                Color::Indexed(n)
            } else {
                Color::Reset
            }
        }
        _ => Color::Reset,
    }
}

pub fn parse_style(spec: &str) -> Style {
    let mut style = Style::default();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(val) = part.strip_prefix("fg=") {
            style = style.fg(parse_color(val));
        } else if let Some(val) = part.strip_prefix("bg=") {
            style = style.bg(parse_color(val));
        } else {
            match part {
                "bold" => {
                    style = style.add_modifier(Modifier::BOLD);
                }
                "dim" => {
                    style = style.add_modifier(Modifier::DIM);
                }
                "italic" => {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                "underline" => {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                "blink" => {
                    style = style.add_modifier(Modifier::SLOW_BLINK);
                }
                "reverse" | "reverse-video" => {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                "strikethrough" => {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                }
                _ => {}
            }
        }
    }
    style
}

pub fn term_color_to_ratatui(c: TermColor) -> Color {
    match c {
        TermColor::Named(NamedColor::Foreground | NamedColor::Background) => {
            Color::Reset
        }
        TermColor::Named(NamedColor::Black) => Color::Black,
        TermColor::Named(NamedColor::Red) => Color::Red,
        TermColor::Named(NamedColor::Green) => Color::Green,
        TermColor::Named(NamedColor::Yellow) => Color::Yellow,
        TermColor::Named(NamedColor::Blue) => Color::Blue,
        TermColor::Named(NamedColor::Magenta) => Color::Magenta,
        TermColor::Named(NamedColor::Cyan) => Color::Cyan,
        TermColor::Named(NamedColor::White) => Color::Gray,
        TermColor::Named(NamedColor::BrightBlack) => Color::DarkGray,
        TermColor::Named(NamedColor::BrightRed) => Color::LightRed,
        TermColor::Named(NamedColor::BrightGreen) => Color::LightGreen,
        TermColor::Named(NamedColor::BrightYellow) => Color::LightYellow,
        TermColor::Named(NamedColor::BrightBlue) => Color::LightBlue,
        TermColor::Named(NamedColor::BrightMagenta) => Color::LightMagenta,
        TermColor::Named(NamedColor::BrightCyan) => Color::LightCyan,
        TermColor::Named(NamedColor::BrightWhite) => Color::White,
        TermColor::Named(_) => Color::Reset,
        TermColor::Indexed(i) => Color::Indexed(i),
        TermColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}
