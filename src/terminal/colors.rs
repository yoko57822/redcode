//! Terminal color palette: ANSI 16 colors, xterm 256-color cube/gray ramp and theme colors.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, Rgb};
use gpui::{App, Hsla, Rgba};
use gpui_kit::component::ActiveTheme as _;

const DARK_ANSI: [Rgb; 16] = [
    hex(0x000000),
    hex(0xcd3131),
    hex(0x0dbc79),
    hex(0xe5e510),
    hex(0x2472c8),
    hex(0xbc3fbc),
    hex(0x11a8cd),
    hex(0xe5e5e5),
    hex(0x666666),
    hex(0xf14c4c),
    hex(0x23d18b),
    hex(0xf5f543),
    hex(0x3b8eea),
    hex(0xd670d6),
    hex(0x29b8db),
    hex(0xffffff),
];

const LIGHT_ANSI: [Rgb; 16] = [
    hex(0x000000),
    hex(0xcd3131),
    hex(0x00bc00),
    hex(0x949800),
    hex(0x0451a5),
    hex(0xbc05bc),
    hex(0x0598bc),
    hex(0x555555),
    hex(0x666666),
    hex(0xcd3131),
    hex(0x14ce14),
    hex(0xb5ba00),
    hex(0x0451a5),
    hex(0xbc05bc),
    hex(0x0598bc),
    hex(0xa5a5a5),
];

/// Brightness factor for dim colors.
const DIM: f32 = 0.66;

const fn hex(value: u32) -> Rgb {
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

/// Converts an 8-bit RGB color into an opaque GPUI color.
pub fn rgb_to_hsla(rgb: Rgb) -> Hsla {
    Rgba {
        r: f32::from(rgb.r) / 255.0,
        g: f32::from(rgb.g) / 255.0,
        b: f32::from(rgb.b) / 255.0,
        a: 1.0,
    }
    .into()
}

/// Converts a GPUI color into 8-bit RGB (alpha is discarded).
pub fn hsla_to_rgb(color: Hsla) -> Rgb {
    let rgba: Rgba = color.into();
    let channel = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    Rgb {
        r: channel(rgba.r),
        g: channel(rgba.g),
        b: channel(rgba.b),
    }
}

#[derive(Clone, Debug)]
pub struct Palette {
    pub ansi: [Rgb; 16],
    pub foreground: Hsla,
    pub background: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
}

impl Palette {
    /// Builds the palette from the active gpui-component theme.
    pub fn from_theme(cx: &App) -> Self {
        let theme = cx.theme();
        Self {
            ansi: if theme.is_dark() {
                DARK_ANSI
            } else {
                LIGHT_ANSI
            },
            foreground: theme.foreground,
            background: theme.background,
            cursor: theme.caret,
            selection: theme.selection,
        }
    }

    /// RGB value for an alacritty color index (see `alacritty_terminal::term::color`).
    /// Out-of-range indices resolve to the foreground.
    pub fn rgb(&self, index: usize) -> Rgb {
        match index {
            0..16 => self.ansi[index],
            16..232 => {
                let i = index - 16;
                let component = |v: usize| if v == 0 { 0 } else { (55 + 40 * v) as u8 };
                Rgb {
                    r: component(i / 36),
                    g: component((i / 6) % 6),
                    b: component(i % 6),
                }
            }
            232..256 => {
                let v = (8 + 10 * (index - 232)) as u8;
                Rgb { r: v, g: v, b: v }
            }
            256 => hsla_to_rgb(self.foreground),
            257 => hsla_to_rgb(self.background),
            258 => hsla_to_rgb(self.cursor),
            259..267 => self.ansi[index - 259] * DIM,
            267 => hsla_to_rgb(self.foreground),
            268 => hsla_to_rgb(self.foreground) * DIM,
            _ => hsla_to_rgb(self.foreground),
        }
    }

    /// Resolves a cell color, preferring colors set at runtime via OSC sequences.
    pub fn resolve(&self, color: Color, overrides: &Colors) -> Hsla {
        let index = match color {
            Color::Spec(rgb) => return rgb_to_hsla(rgb),
            Color::Indexed(i) => usize::from(i),
            Color::Named(named) => named as usize,
        };
        match overrides[index] {
            Some(rgb) => rgb_to_hsla(rgb),
            None => match index {
                256 | 267 => self.foreground,
                257 => self.background,
                258 => self.cursor,
                _ => rgb_to_hsla(self.rgb(index)),
            },
        }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            ansi: DARK_ANSI,
            foreground: rgb_to_hsla(hex(0xe5e5e5)),
            background: rgb_to_hsla(hex(0x1e1e1e)),
            cursor: rgb_to_hsla(hex(0xffffff)),
            selection: rgb_to_hsla(hex(0x264f78)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::NamedColor;

    fn triple(rgb: Rgb) -> (u8, u8, u8) {
        (rgb.r, rgb.g, rgb.b)
    }

    #[test]
    fn color_cube() {
        let p = Palette::for_test();
        assert_eq!(triple(p.rgb(16)), (0, 0, 0));
        assert_eq!(triple(p.rgb(231)), (255, 255, 255));
        assert_eq!(triple(p.rgb(196)), (255, 0, 0));
    }

    #[test]
    fn gray_ramp() {
        let p = Palette::for_test();
        assert_eq!(triple(p.rgb(232)), (8, 8, 8));
        assert_eq!(triple(p.rgb(255)), (238, 238, 238));
    }

    #[test]
    fn ansi_and_special_indices() {
        let p = Palette::for_test();
        assert_eq!(triple(p.rgb(1)), (0xcd, 0x31, 0x31));
        assert_eq!(triple(p.rgb(256)), (0xe5, 0xe5, 0xe5));
        assert_eq!(triple(p.rgb(257)), (0x1e, 0x1e, 0x1e));
        assert_eq!(triple(p.rgb(1000)), (0xe5, 0xe5, 0xe5));
    }

    #[test]
    fn resolve_prefers_overrides() {
        let p = Palette::for_test();
        let mut overrides = Colors::default();
        assert_eq!(
            hsla_to_rgb(p.resolve(Color::Indexed(196), &overrides)),
            Rgb { r: 255, g: 0, b: 0 }
        );
        overrides[196] = Some(hex(0x123456));
        assert_eq!(
            hsla_to_rgb(p.resolve(Color::Indexed(196), &overrides)),
            hex(0x123456)
        );
        assert_eq!(
            p.resolve(Color::Named(NamedColor::Background), &overrides),
            p.background
        );
        overrides[NamedColor::Background] = Some(hex(0x010203));
        assert_eq!(
            hsla_to_rgb(p.resolve(Color::Named(NamedColor::Background), &overrides)),
            hex(0x010203)
        );
        assert_eq!(
            hsla_to_rgb(p.resolve(Color::Spec(hex(0xabcdef)), &overrides)),
            hex(0xabcdef)
        );
    }
}
