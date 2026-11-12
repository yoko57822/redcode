//! Keystroke → terminal escape sequence translation (pure).

use std::borrow::Cow;

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// Translates a keystroke into the byte sequence a terminal application expects.
///
/// Returns `None` for keystrokes that must not reach the PTY as an escape sequence
/// (anything with Cmd, and plain text input which arrives through the input handler).
pub fn to_esc(key: &Keystroke, mode: TermMode, option_as_meta: bool) -> Option<Cow<'static, str>> {
    let mods = key.modifiers;
    if mods.platform {
        return None;
    }
    let m = 1 + u8::from(mods.shift) + 2 * u8::from(mods.alt) + 4 * u8::from(mods.control);

    match key.key.as_str() {
        "tab" => {
            return Some(Cow::Borrowed(if mods.shift { "\x1b[Z" } else { "\x09" }));
        }
        "escape" => return Some(Cow::Borrowed("\x1b")),
        "enter" => {
            let seq = if mods.alt {
                "\x1b\r"
            } else if mods.shift {
                "\n"
            } else {
                "\r"
            };
            return Some(Cow::Borrowed(seq));
        }
        "backspace" => {
            let seq = if mods.control {
                "\x08"
            } else if mods.alt {
                "\x1b\x7f"
            } else {
                "\x7f"
            };
            return Some(Cow::Borrowed(seq));
        }
        "space" => {
            return if mods.control {
                Some(Cow::Borrowed("\x00"))
            } else {
                None
            };
        }
        _ => {}
    }

    if let Some(c) = cursor_key_char(&key.key) {
        return Some(if m == 1 {
            let prefix = if mode.contains(TermMode::APP_CURSOR) {
                "\x1bO"
            } else {
                "\x1b["
            };
            Cow::Owned(format!("{prefix}{c}"))
        } else {
            Cow::Owned(format!("\x1b[1;{m}{c}"))
        });
    }

    if let Some(c) = f1_f4_char(&key.key) {
        return Some(if m == 1 {
            Cow::Owned(format!("\x1bO{c}"))
        } else {
            Cow::Owned(format!("\x1b[1;{m}{c}"))
        });
    }

    if let Some(n) = tilde_code(&key.key) {
        return Some(if m == 1 {
            Cow::Owned(format!("\x1b[{n}~"))
        } else {
            Cow::Owned(format!("\x1b[{n};{m}~"))
        });
    }

    if mods.control {
        let ctrl = control_char(&key.key, mods.shift)?;
        return Some(if mods.alt && option_as_meta {
            Cow::Owned(format!("\x1b{ctrl}"))
        } else {
            Cow::Owned(ctrl.to_string())
        });
    }

    if mods.alt && option_as_meta {
        let mut chars = key.key.chars();
        let c = chars.next()?;
        if chars.next().is_some() || !c.is_ascii_graphic() {
            return None;
        }
        let c = if mods.shift {
            c.to_ascii_uppercase()
        } else {
            c
        };
        return Some(Cow::Owned(format!("\x1b{c}")));
    }

    None
}

/// Final byte for arrow keys and Home/End.
fn cursor_key_char(key: &str) -> Option<char> {
    Some(match key {
        "up" => 'A',
        "down" => 'B',
        "right" => 'C',
        "left" => 'D',
        "home" => 'H',
        "end" => 'F',
        _ => return None,
    })
}

/// Final byte for F1–F4 (SS3 form).
fn f1_f4_char(key: &str) -> Option<char> {
    Some(match key {
        "f1" => 'P',
        "f2" => 'Q',
        "f3" => 'R',
        "f4" => 'S',
        _ => return None,
    })
}

/// Numeric parameter for `CSI n ~` keys.
fn tilde_code(key: &str) -> Option<u8> {
    Some(match key {
        "insert" => 2,
        "delete" => 3,
        "pageup" => 5,
        "pagedown" => 6,
        "f5" => 15,
        "f6" => 17,
        "f7" => 18,
        "f8" => 19,
        "f9" => 20,
        "f10" => 21,
        "f11" => 23,
        "f12" => 24,
        _ => return None,
    })
}

/// Control character for `ctrl-<key>`. Shifted US-layout digits/punctuation
/// (`shift-2` = `@`, `shift-6` = `^`, `shift--` = `_`, `shift-/` = `?`) are accepted too.
fn control_char(key: &str, shift: bool) -> Option<char> {
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let c = match (c, shift) {
        ('2', true) => '@',
        ('6', true) => '^',
        ('-', true) => '_',
        ('/', true) => '?',
        (c, _) => c.to_ascii_lowercase(),
    };
    Some(match c {
        'a'..='z' => (c as u8 - b'a' + 1) as char,
        '@' => '\x00',
        '[' => '\x1b',
        '\\' => '\x1c',
        ']' => '\x1d',
        '^' => '\x1e',
        '_' => '\x1f',
        '?' => '\x7f',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(source: &'static str, mode: TermMode, option_as_meta: bool) -> Option<String> {
        to_esc(&Keystroke::parse(source).unwrap(), mode, option_as_meta).map(Cow::into_owned)
    }

    #[test]
    fn arrows_follow_app_cursor_mode() {
        assert_eq!(esc("up", TermMode::NONE, true).as_deref(), Some("\x1b[A"));
        assert_eq!(
            esc("up", TermMode::APP_CURSOR, true).as_deref(),
            Some("\x1bOA")
        );
    }

    #[test]
    fn control_letters() {
        assert_eq!(esc("ctrl-c", TermMode::NONE, true).as_deref(), Some("\x03"));
        assert_eq!(
            esc("ctrl-shift-a", TermMode::NONE, true).as_deref(),
            Some("\x01")
        );
        assert_eq!(esc("ctrl-[", TermMode::NONE, true).as_deref(), Some("\x1b"));
    }

    #[test]
    fn shift_tab() {
        assert_eq!(
            esc("shift-tab", TermMode::NONE, true).as_deref(),
            Some("\x1b[Z")
        );
        assert_eq!(esc("tab", TermMode::NONE, true).as_deref(), Some("\x09"));
    }

    #[test]
    fn modified_arrow() {
        assert_eq!(
            esc("ctrl-shift-up", TermMode::NONE, true).as_deref(),
            Some("\x1b[1;6A")
        );
        assert_eq!(
            esc("ctrl-shift-up", TermMode::APP_CURSOR, true).as_deref(),
            Some("\x1b[1;6A")
        );
    }

    #[test]
    fn option_as_meta() {
        assert_eq!(esc("alt-b", TermMode::NONE, true).as_deref(), Some("\x1bb"));
        assert_eq!(
            esc("alt-shift-b", TermMode::NONE, true).as_deref(),
            Some("\x1bB")
        );
        assert_eq!(esc("alt-b", TermMode::NONE, false), None);
        assert_eq!(
            esc("alt-ctrl-c", TermMode::NONE, true).as_deref(),
            Some("\x1b\x03")
        );
    }

    #[test]
    fn cmd_is_never_sent() {
        assert_eq!(esc("cmd-c", TermMode::NONE, true), None);
        assert_eq!(esc("cmd-up", TermMode::NONE, true), None);
    }

    #[test]
    fn function_keys() {
        assert_eq!(esc("f5", TermMode::NONE, true).as_deref(), Some("\x1b[15~"));
        assert_eq!(esc("f1", TermMode::NONE, true).as_deref(), Some("\x1bOP"));
        assert_eq!(
            esc("shift-f1", TermMode::NONE, true).as_deref(),
            Some("\x1b[1;2P")
        );
        assert_eq!(
            esc("ctrl-delete", TermMode::NONE, true).as_deref(),
            Some("\x1b[3;5~")
        );
    }

    #[test]
    fn plain_text_goes_to_input_handler() {
        assert_eq!(esc("a", TermMode::NONE, true), None);
        assert_eq!(esc("shift-a", TermMode::NONE, true), None);
        assert_eq!(esc("space", TermMode::NONE, true), None);
    }
}
