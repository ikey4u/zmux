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

pub fn vt_color_to_ratatui(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(0) => Color::Black,
        vt100::Color::Idx(1) => Color::Red,
        vt100::Color::Idx(2) => Color::Green,
        vt100::Color::Idx(3) => Color::Yellow,
        vt100::Color::Idx(4) => Color::Blue,
        vt100::Color::Idx(5) => Color::Magenta,
        vt100::Color::Idx(6) => Color::Cyan,
        vt100::Color::Idx(7) => Color::Gray,
        vt100::Color::Idx(8) => Color::DarkGray,
        vt100::Color::Idx(9) => Color::LightRed,
        vt100::Color::Idx(10) => Color::LightGreen,
        vt100::Color::Idx(11) => Color::LightYellow,
        vt100::Color::Idx(12) => Color::LightBlue,
        vt100::Color::Idx(13) => Color::LightMagenta,
        vt100::Color::Idx(14) => Color::LightCyan,
        vt100::Color::Idx(15) => Color::White,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
