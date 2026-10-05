//! xterm-256 palette plus the kitchen's dark theme for the 16 ANSI colors.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use egui::Color32;

pub const BACKGROUND: Color32 = Color32::from_rgb(0x1a, 0x1b, 0x26);
pub const FOREGROUND: Color32 = Color32::WHITE;
pub const CURSOR: Color32 = Color32::from_rgb(0xc0, 0xca, 0xf5);
pub const SELECTION: Color32 = Color32::from_rgb(0x33, 0x46, 0x7c);

/// Tokyo-night-ish ANSI 0..15.
const ANSI: [Color32; 16] = [
    Color32::from_rgb(0x15, 0x16, 0x1e),
    Color32::from_rgb(0xf7, 0x76, 0x8e),
    Color32::from_rgb(0x9e, 0xce, 0x6a),
    Color32::from_rgb(0xe0, 0xaf, 0x68),
    Color32::from_rgb(0x7a, 0xa2, 0xf7),
    Color32::from_rgb(0xbb, 0x9a, 0xf7),
    Color32::from_rgb(0x7d, 0xcf, 0xff),
    Color32::from_rgb(0xa9, 0xb1, 0xd6),
    Color32::from_rgb(0x41, 0x48, 0x68),
    Color32::from_rgb(0xff, 0x89, 0x9d),
    Color32::from_rgb(0xb9, 0xf2, 0x7c),
    Color32::from_rgb(0xff, 0xc7, 0x77),
    Color32::from_rgb(0x8d, 0xb0, 0xff),
    Color32::from_rgb(0xc7, 0xa9, 0xff),
    Color32::from_rgb(0xa4, 0xda, 0xff),
    Color32::from_rgb(0xc0, 0xca, 0xf5),
];

fn indexed(i: u8) -> Color32 {
    match i {
        0..=15 => ANSI[i as usize],
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}

fn rgb(c: Rgb) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

fn dim(c: Color32) -> Color32 {
    Color32::from_rgb((c.r() as f32 * 0.66) as u8, (c.g() as f32 * 0.66) as u8, (c.b() as f32 * 0.66) as u8)
}

/// Resolve a cell color, honoring palette overrides the program set (OSC 4/10/11).
pub fn resolve(color: Color, overrides: &Colors) -> Color32 {
    match color {
        Color::Spec(c) => rgb(c),
        Color::Indexed(i) => overrides[i as usize].map(rgb).unwrap_or_else(|| indexed(i)),
        Color::Named(n) => {
            if let Some(c) = overrides[n] {
                return rgb(c);
            }
            match n {
                NamedColor::Foreground | NamedColor::BrightForeground => FOREGROUND,
                NamedColor::Background => BACKGROUND,
                NamedColor::Cursor => CURSOR,
                NamedColor::DimForeground => dim(FOREGROUND),
                NamedColor::DimBlack => dim(ANSI[0]),
                NamedColor::DimRed => dim(ANSI[1]),
                NamedColor::DimGreen => dim(ANSI[2]),
                NamedColor::DimYellow => dim(ANSI[3]),
                NamedColor::DimBlue => dim(ANSI[4]),
                NamedColor::DimMagenta => dim(ANSI[5]),
                NamedColor::DimCyan => dim(ANSI[6]),
                NamedColor::DimWhite => dim(ANSI[7]),
                other => ANSI[(other as usize).min(15)],
            }
        }
    }
}

pub fn is_default_bg(color: Color) -> bool {
    matches!(color, Color::Named(NamedColor::Background))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_gray() {
        assert_eq!(indexed(16), Color32::from_rgb(0, 0, 0));
        assert_eq!(indexed(231), Color32::from_rgb(255, 255, 255));
        assert_eq!(indexed(232), Color32::from_rgb(8, 8, 8));
    }
}
