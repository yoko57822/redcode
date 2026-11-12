//! Mouse coordinate conversion and xterm mouse report encoding (pure).

use alacritty_terminal::index::Side;
use alacritty_terminal::term::TermMode;
use gpui::{Modifiers, Pixels, Point};

/// Encodes a mouse report for the PTY.
///
/// `button` is the base code: 0 left, 1 middle, 2 right, `+32` while moving
/// (35 = motion without a button), 64/65 wheel up/down. Modifier bits
/// (shift 4, alt 8, ctrl 16) are added here. `col`/`row` are 0-based viewport cells.
/// Returns `None` when the position cannot be encoded in the active protocol.
pub fn mouse_report(
    button: u8,
    pressed: bool,
    col: usize,
    row: usize,
    mods: Modifiers,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let mod_bits =
        4 * u32::from(mods.shift) + 8 * u32::from(mods.alt) + 16 * u32::from(mods.control);

    if mode.contains(TermMode::SGR_MOUSE) {
        let b = u32::from(button) + mod_bits;
        let suffix = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{b};{};{}{suffix}", col + 1, row + 1).into_bytes());
    }

    let base = if pressed { u32::from(button) } else { 3 };
    let b = base + mod_bits;
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    let limit = if utf8 { 2015 } else { 223 };
    let (x, y) = (col + 1, row + 1);
    if x > limit || y > limit {
        return None;
    }

    let mut out = b"\x1b[M".to_vec();
    for value in [32 + b, 32 + x as u32, 32 + y as u32] {
        if utf8 && value >= 128 {
            let c = char::from_u32(value)?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        } else {
            out.push(u8::try_from(value).ok()?);
        }
    }
    Some(out)
}

/// Converts a window position to a clamped viewport cell and the cell half it falls in.
/// Returns `(col, row, side)`.
pub fn grid_cell(
    pos: Point<Pixels>,
    origin: Point<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
    cols: usize,
    rows: usize,
) -> (usize, usize, Side) {
    let cell_w = f32::from(cell_w);
    let line_h = f32::from(line_h);
    let dx = f32::from(pos.x) - f32::from(origin.x);
    let dy = f32::from(pos.y) - f32::from(origin.y);

    let col = clamp_index(dx / cell_w, cols);
    let row = clamp_index(dy / line_h, rows);
    let within = dx - col as f32 * cell_w;
    let side = if within < cell_w / 2.0 {
        Side::Left
    } else {
        Side::Right
    };
    (col, row, side)
}

fn clamp_index(value: f32, count: usize) -> usize {
    let max = count.saturating_sub(1);
    let floor = value.floor();
    if floor.is_nan() || floor <= 0.0 {
        0
    } else {
        (floor as usize).min(max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};

    #[test]
    fn sgr_left_click() {
        let mode = TermMode::SGR_MOUSE | TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            mouse_report(0, true, 4, 2, Modifiers::none(), mode).unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            mouse_report(0, false, 4, 2, Modifiers::none(), mode).unwrap(),
            b"\x1b[<0;5;3m"
        );
    }

    #[test]
    fn normal_press() {
        let bytes = mouse_report(
            0,
            true,
            4,
            2,
            Modifiers::none(),
            TermMode::MOUSE_REPORT_CLICK,
        )
        .unwrap();
        assert_eq!(&bytes[..3], b"\x1b[M");
        assert_eq!(&bytes[3..], &[32, 37, 35]);
    }

    #[test]
    fn normal_release_uses_button_three() {
        let bytes = mouse_report(
            0,
            false,
            0,
            0,
            Modifiers::none(),
            TermMode::MOUSE_REPORT_CLICK,
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[M#!!");
    }

    #[test]
    fn normal_out_of_range() {
        assert_eq!(
            mouse_report(
                0,
                true,
                230,
                0,
                Modifiers::none(),
                TermMode::MOUSE_REPORT_CLICK
            ),
            None
        );
    }

    #[test]
    fn utf8_large_column() {
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        let bytes = mouse_report(0, true, 230, 0, Modifiers::none(), mode).unwrap();
        let mut expected = b"\x1b[M ".to_vec();
        expected.extend_from_slice("\u{107}".as_bytes());
        expected.push(33);
        assert_eq!(bytes, expected);
        assert_eq!(
            mouse_report(0, true, 2015, 0, Modifiers::none(), mode),
            None
        );
    }

    #[test]
    fn sgr_wheel_with_shift() {
        let mods = Modifiers {
            shift: true,
            ..Modifiers::none()
        };
        let mode = TermMode::SGR_MOUSE | TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            mouse_report(64, true, 0, 0, mods, mode).unwrap(),
            b"\x1b[<68;1;1M"
        );
    }

    #[test]
    fn grid_cell_clamps_and_sides() {
        let origin = point(px(6.), px(4.));
        let (w, h) = (px(8.), px(16.));
        assert_eq!(
            grid_cell(point(px(6.), px(4.)), origin, w, h, 80, 24),
            (0, 0, Side::Left)
        );
        assert_eq!(
            grid_cell(
                point(px(6. + 8. * 3. + 5.), px(4. + 16. * 2. + 1.)),
                origin,
                w,
                h,
                80,
                24
            ),
            (3, 2, Side::Right)
        );
        assert_eq!(
            grid_cell(point(px(0.), px(0.)), origin, w, h, 80, 24),
            (0, 0, Side::Left)
        );
        assert_eq!(
            grid_cell(point(px(10_000.), px(10_000.)), origin, w, h, 80, 24),
            (79, 23, Side::Right)
        );
    }
}
