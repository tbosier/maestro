//! xterm encoding of key presses and pastes.

/// A key the user pressed while a terminal has focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermKey {
    /// A printable character, already shifted (`'A'`, not `'a'` with shift).
    Char(char),
    Enter,
    Esc,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    /// A function key, `F(1)` through `F(12)`. Other numbers encode to nothing.
    F(u8),
}

/// Modifier keys held with a key press.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermMods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

const ESC: u8 = 0x1b;

/// xterm encoding of a key press.
///
/// - `Ctrl` with a letter or one of `@ [ \ ] ^ _ ? space 2-8 / -` sends the C0
///   control byte; `Ctrl` with any other character sends the character.
/// - `Alt` with a character, `Enter`, `Esc`, `Backspace` or `Tab` adds an ESC
///   prefix.
/// - Arrows, `Home` and `End` send `CSI X`, or `SS3 X` when the program turned
///   on application cursor mode (DECCKM). With any modifier they send
///   `CSI 1;<mod>X`, where `<mod>` is 1 plus 1 for shift, 2 for alt and 4 for
///   ctrl.
/// - `Insert`, `Delete`, `PageUp`, `PageDown` and F5-F12 send `CSI <n>~`, or
///   `CSI <n>;<mod>~` with modifiers. F1-F4 send `SS3 P` to `SS3 S`, or
///   `CSI 1;<mod>P` with modifiers.
/// - `BackTab` and `Shift+Tab` send `CSI Z`.
pub fn encode_key(key: TermKey, mods: TermMods, application_cursor: bool) -> Vec<u8> {
    let modifier = modifier_param(mods);
    match key {
        TermKey::Char(c) => encode_char(c, mods),
        TermKey::Enter => alt_prefixed(mods.alt, b"\r"),
        TermKey::Esc => alt_prefixed(mods.alt, &[ESC]),
        TermKey::Backspace => alt_prefixed(mods.alt, &[if mods.ctrl { 0x08 } else { 0x7f }]),
        TermKey::Tab if mods.shift => b"\x1b[Z".to_vec(),
        TermKey::Tab => alt_prefixed(mods.alt, b"\t"),
        TermKey::BackTab => b"\x1b[Z".to_vec(),
        TermKey::Up => cursor_key(b'A', modifier, application_cursor),
        TermKey::Down => cursor_key(b'B', modifier, application_cursor),
        TermKey::Right => cursor_key(b'C', modifier, application_cursor),
        TermKey::Left => cursor_key(b'D', modifier, application_cursor),
        TermKey::Home => cursor_key(b'H', modifier, application_cursor),
        TermKey::End => cursor_key(b'F', modifier, application_cursor),
        TermKey::Insert => tilde_key(2, modifier),
        TermKey::Delete => tilde_key(3, modifier),
        TermKey::PageUp => tilde_key(5, modifier),
        TermKey::PageDown => tilde_key(6, modifier),
        TermKey::F(n @ 1..=4) => {
            let letter = b'P' + (n - 1);
            match modifier {
                Some(m) => format!("\x1b[1;{m}{}", letter as char).into_bytes(),
                None => vec![ESC, b'O', letter],
            }
        }
        TermKey::F(n @ 5..=12) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            tilde_key(code, modifier)
        }
        TermKey::F(_) => Vec::new(),
    }
}

/// Bracketed paste when the program enabled it, else raw text.
///
/// Bracketed text has every ESC byte removed, so pasted text cannot end the
/// paste early with its own `ESC [201~` and have the rest run as typed input.
/// Raw text has its line endings turned into carriage returns, which is what
/// the Enter key sends.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend(text.bytes().filter(|&b| b != ESC));
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// The xterm modifier parameter, or None when no modifier is held.
fn modifier_param(mods: TermMods) -> Option<u8> {
    let bits = u8::from(mods.shift) | u8::from(mods.alt) << 1 | u8::from(mods.ctrl) << 2;
    (bits != 0).then_some(bits + 1)
}

fn alt_prefixed(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if alt {
        out.push(ESC);
    }
    out.extend_from_slice(bytes);
    out
}

fn encode_char(c: char, mods: TermMods) -> Vec<u8> {
    if mods.ctrl
        && let Some(byte) = control_byte(c)
    {
        return alt_prefixed(mods.alt, &[byte]);
    }
    let mut buf = [0u8; 4];
    alt_prefixed(mods.alt, c.encode_utf8(&mut buf).as_bytes())
}

/// The C0 control byte xterm sends for Ctrl with `c`.
fn control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0x00),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '~' | '6' => Some(0x1e),
        '_' | '/' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

fn cursor_key(letter: u8, modifier: Option<u8>, application_cursor: bool) -> Vec<u8> {
    match modifier {
        Some(m) => format!("\x1b[1;{m}{}", letter as char).into_bytes(),
        None if application_cursor => vec![ESC, b'O', letter],
        None => vec![ESC, b'[', letter],
    }
}

fn tilde_key(code: u8, modifier: Option<u8>) -> Vec<u8> {
    match modifier {
        Some(m) => format!("\x1b[{code};{m}~").into_bytes(),
        None => format!("\x1b[{code}~").into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: TermMods = TermMods {
        ctrl: false,
        alt: false,
        shift: false,
    };
    const CTRL: TermMods = TermMods {
        ctrl: true,
        alt: false,
        shift: false,
    };
    const ALT: TermMods = TermMods {
        ctrl: false,
        alt: true,
        shift: false,
    };
    const SHIFT: TermMods = TermMods {
        ctrl: false,
        alt: false,
        shift: true,
    };
    const CTRL_SHIFT: TermMods = TermMods {
        ctrl: true,
        alt: false,
        shift: true,
    };
    const ALL: TermMods = TermMods {
        ctrl: true,
        alt: true,
        shift: true,
    };

    fn key(k: TermKey, mods: TermMods) -> Vec<u8> {
        encode_key(k, mods, false)
    }

    #[test]
    fn plain_characters_are_utf8() {
        assert_eq!(key(TermKey::Char('a'), NONE), b"a");
        assert_eq!(key(TermKey::Char('A'), SHIFT), b"A");
        assert_eq!(key(TermKey::Char('中'), NONE), "中".as_bytes());
    }

    #[test]
    fn ctrl_letters_are_control_bytes() {
        assert_eq!(key(TermKey::Char('a'), CTRL), [0x01]);
        assert_eq!(key(TermKey::Char('c'), CTRL), [0x03]);
        assert_eq!(key(TermKey::Char('z'), CTRL), [0x1a]);
        assert_eq!(key(TermKey::Char('C'), CTRL_SHIFT), [0x03]);
        assert_eq!(key(TermKey::Char(' '), CTRL), [0x00]);
        assert_eq!(key(TermKey::Char('@'), CTRL), [0x00]);
        assert_eq!(key(TermKey::Char('['), CTRL), [0x1b]);
        assert_eq!(key(TermKey::Char('\\'), CTRL), [0x1c]);
        assert_eq!(key(TermKey::Char(']'), CTRL), [0x1d]);
        assert_eq!(key(TermKey::Char('^'), CTRL), [0x1e]);
        assert_eq!(key(TermKey::Char('_'), CTRL), [0x1f]);
        assert_eq!(key(TermKey::Char('?'), CTRL), [0x7f]);
        // No control byte exists: the character goes through unchanged.
        assert_eq!(key(TermKey::Char('é'), CTRL), "é".as_bytes());
        assert_eq!(key(TermKey::Char('1'), CTRL), b"1");
    }

    #[test]
    fn alt_adds_an_escape_prefix() {
        assert_eq!(key(TermKey::Char('x'), ALT), b"\x1bx");
        assert_eq!(
            key(
                TermKey::Char('b'),
                TermMods {
                    ctrl: true,
                    alt: true,
                    shift: false
                }
            ),
            [0x1b, 0x02]
        );
        assert_eq!(key(TermKey::Enter, ALT), b"\x1b\r");
        assert_eq!(key(TermKey::Backspace, ALT), b"\x1b\x7f");
        assert_eq!(key(TermKey::Esc, ALT), b"\x1b\x1b");
        assert_eq!(key(TermKey::Tab, ALT), b"\x1b\t");
    }

    #[test]
    fn editing_keys() {
        assert_eq!(key(TermKey::Enter, NONE), b"\r");
        assert_eq!(key(TermKey::Esc, NONE), b"\x1b");
        assert_eq!(key(TermKey::Backspace, NONE), b"\x7f");
        assert_eq!(key(TermKey::Backspace, CTRL), b"\x08");
        assert_eq!(key(TermKey::Tab, NONE), b"\t");
        assert_eq!(key(TermKey::Tab, SHIFT), b"\x1b[Z");
        assert_eq!(key(TermKey::BackTab, NONE), b"\x1b[Z");
        assert_eq!(key(TermKey::BackTab, SHIFT), b"\x1b[Z");
        assert_eq!(key(TermKey::Insert, NONE), b"\x1b[2~");
        assert_eq!(key(TermKey::Delete, NONE), b"\x1b[3~");
        assert_eq!(key(TermKey::PageUp, NONE), b"\x1b[5~");
        assert_eq!(key(TermKey::PageDown, NONE), b"\x1b[6~");
        assert_eq!(key(TermKey::Delete, CTRL), b"\x1b[3;5~");
        assert_eq!(key(TermKey::PageUp, SHIFT), b"\x1b[5;2~");
    }

    #[test]
    fn cursor_keys_follow_decckm() {
        assert_eq!(encode_key(TermKey::Up, NONE, false), b"\x1b[A");
        assert_eq!(encode_key(TermKey::Down, NONE, false), b"\x1b[B");
        assert_eq!(encode_key(TermKey::Right, NONE, false), b"\x1b[C");
        assert_eq!(encode_key(TermKey::Left, NONE, false), b"\x1b[D");
        assert_eq!(encode_key(TermKey::Home, NONE, false), b"\x1b[H");
        assert_eq!(encode_key(TermKey::End, NONE, false), b"\x1b[F");
        assert_eq!(encode_key(TermKey::Up, NONE, true), b"\x1bOA");
        assert_eq!(encode_key(TermKey::Left, NONE, true), b"\x1bOD");
        assert_eq!(encode_key(TermKey::Home, NONE, true), b"\x1bOH");
        assert_eq!(encode_key(TermKey::End, NONE, true), b"\x1bOF");
    }

    #[test]
    fn modified_cursor_keys_use_the_modifier_parameter() {
        assert_eq!(key(TermKey::Up, SHIFT), b"\x1b[1;2A");
        assert_eq!(key(TermKey::Left, ALT), b"\x1b[1;3D");
        assert_eq!(key(TermKey::Right, CTRL), b"\x1b[1;5C");
        assert_eq!(key(TermKey::Down, CTRL_SHIFT), b"\x1b[1;6B");
        assert_eq!(key(TermKey::Home, ALL), b"\x1b[1;8H");
        assert_eq!(key(TermKey::End, CTRL), b"\x1b[1;5F");
        // Modifiers win over application cursor mode, as in xterm.
        assert_eq!(encode_key(TermKey::Up, CTRL, true), b"\x1b[1;5A");
    }

    #[test]
    fn function_keys() {
        assert_eq!(key(TermKey::F(1), NONE), b"\x1bOP");
        assert_eq!(key(TermKey::F(2), NONE), b"\x1bOQ");
        assert_eq!(key(TermKey::F(3), NONE), b"\x1bOR");
        assert_eq!(key(TermKey::F(4), NONE), b"\x1bOS");
        assert_eq!(key(TermKey::F(1), SHIFT), b"\x1b[1;2P");
        assert_eq!(key(TermKey::F(4), CTRL), b"\x1b[1;5S");
        let expected: [&[u8]; 8] = [
            b"\x1b[15~",
            b"\x1b[17~",
            b"\x1b[18~",
            b"\x1b[19~",
            b"\x1b[20~",
            b"\x1b[21~",
            b"\x1b[23~",
            b"\x1b[24~",
        ];
        for (n, want) in (5..=12).zip(expected) {
            assert_eq!(key(TermKey::F(n), NONE), want, "F{n}");
        }
        assert_eq!(key(TermKey::F(5), CTRL), b"\x1b[15;5~");
        assert_eq!(key(TermKey::F(12), SHIFT), b"\x1b[24;2~");
        assert!(key(TermKey::F(0), NONE).is_empty());
        assert!(key(TermKey::F(13), NONE).is_empty());
    }

    #[test]
    fn paste_brackets_and_strips_escapes() {
        assert_eq!(encode_paste("ls\n", true), b"\x1b[200~ls\n\x1b[201~");
        assert_eq!(
            encode_paste("a\x1b[201~rm -rf /\n", true),
            b"\x1b[200~a[201~rm -rf /\n\x1b[201~"
        );
    }

    #[test]
    fn raw_paste_turns_newlines_into_returns() {
        assert_eq!(encode_paste("one\ntwo\r\nthree", false), b"one\rtwo\rthree");
        assert_eq!(encode_paste("plain", false), b"plain");
    }
}
