//! Background colours of the rail. Status hues stay ANSI-16, so the
//! terminal's theme draws them; the surfaces behind cards are truecolor,
//! derived from the terminal's own background and foreground so they sit
//! right on a dark or a light theme alike.

pub type Rgb = (u8, u8, u8);

/// `#rrggbb` or `rrggbb`; `None` for anything else. The text comes from
/// Zellij's report of the terminal, so it is checked, not trusted.
pub fn parse_hex(text: &str) -> Option<Rgb> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some((channel(0)?, channel(2)?, channel(4)?))
}

/// `a` moved toward `b` by `t` (0 to 1), per channel.
fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// The rail's surfaces: a panel a step darker than the terminal, cards
/// stepping up from it, and highlights stepping past the terminal toward
/// its foreground.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub rail: Rgb,
    /// A tab with nothing running.
    pub idle: Rgb,
    /// A tab with something running.
    pub busy: Rgb,
    /// The tab in view.
    pub active: Rgb,
    /// A row that just started needing you.
    pub flash: Rgb,
    /// The row under the cursor.
    pub selected: Rgb,
}

/// Used until Zellij reports the terminal's colours: a neutral dark.
const FALLBACK: (Rgb, Rgb) = ((26, 27, 38), (192, 202, 220));

impl Theme {
    pub fn new(bg: Rgb, fg: Rgb) -> Self {
        let sum = |(r, g, b): Rgb| r as u32 + g as u32 + b as u32;
        // Darkening a light background much makes a muddy slab.
        let recede = if sum(bg) <= sum(fg) { 0.30 } else { 0.08 };
        let rail = blend(bg, (0, 0, 0), recede);
        Theme {
            rail,
            idle: blend(rail, bg, 0.30),
            busy: blend(rail, bg, 0.72),
            active: blend(bg, fg, 0.14),
            flash: blend(bg, fg, 0.30),
            selected: blend(bg, fg, 0.24),
        }
    }

    /// From Zellij's `default_bg`/`default_fg` strings, when both parse.
    pub fn from_hex(bg: Option<&str>, fg: Option<&str>) -> Self {
        match (bg.and_then(parse_hex), fg.and_then(parse_hex)) {
            (Some(bg), Some(fg)) => Theme::new(bg, fg),
            _ => Theme::default(),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::new(FALLBACK.0, FALLBACK.1)
    }
}

/// The escape that sets `color` as the background.
pub fn bg(color: Rgb) -> String {
    format!("\x1b[48;2;{};{};{}m", color.0, color.1, color.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_six_hex_digits() {
        assert_eq!(parse_hex("#1a1b26"), Some((0x1a, 0x1b, 0x26)));
        assert_eq!(parse_hex("ffffff"), Some((255, 255, 255)));
        for bad in ["", "#fff", "#gggggg", "#1a1b2é", "1a1b26ff"] {
            assert_eq!(parse_hex(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn surfaces_step_up_from_the_panel_on_dark_and_light() {
        let sum = |(r, g, b): Rgb| r as u32 + g as u32 + b as u32;
        let dark = Theme::new((30, 30, 30), (220, 220, 220));
        assert!(sum(dark.rail) < sum(dark.idle) && sum(dark.idle) < sum(dark.busy));
        assert!(sum(dark.busy) < sum(dark.active) && sum(dark.active) < sum(dark.flash));
        let light = Theme::new((250, 250, 250), (20, 20, 20));
        assert!(sum(light.rail) > 600, "a light panel stays light");
        assert_eq!(
            Theme::from_hex(Some("#zz"), Some("#000000")),
            Theme::default()
        );
    }
}
