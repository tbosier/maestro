//! Terminal-independent key events and the textual key notation used in
//! keymaps and config (`C-r`, `A-;`, `space`, `ret`, `S-tab`, ...).

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyCode {
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
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub mods: Modifiers,
}

impl KeyEvent {
    /// Builds a key event, normalizing shift on characters: the character
    /// itself carries case, so `S-a` and `A` are the same key.
    pub fn new(code: KeyCode, mut mods: Modifiers) -> Self {
        if let KeyCode::Char(_) = code {
            mods.shift = false;
        }
        if code == KeyCode::BackTab {
            mods.shift = false;
        }
        Self { code, mods }
    }

    pub fn plain(code: KeyCode) -> Self {
        Self::new(code, Modifiers::default())
    }

    pub fn char(c: char) -> Self {
        Self::plain(KeyCode::Char(c))
    }

    pub fn ctrl(c: char) -> Self {
        Self::new(
            KeyCode::Char(c),
            Modifiers {
                ctrl: true,
                ..Default::default()
            },
        )
    }

    pub fn alt(c: char) -> Self {
        Self::new(
            KeyCode::Char(c),
            Modifiers {
                alt: true,
                ..Default::default()
            },
        )
    }

    /// The character this key types in insert mode, if any.
    pub fn typed_char(&self) -> Option<char> {
        match self.code {
            KeyCode::Char(c) if !self.mods.ctrl && !self.mods.alt => Some(c),
            _ => None,
        }
    }
}

impl fmt::Display for KeyEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.ctrl {
            f.write_str("C-")?;
        }
        if self.mods.alt {
            f.write_str("A-")?;
        }
        if self.mods.shift {
            f.write_str("S-")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("space"),
            KeyCode::Char('-') if self.mods != Modifiers::default() => f.write_str("minus"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Enter => f.write_str("ret"),
            KeyCode::Esc => f.write_str("esc"),
            KeyCode::Backspace => f.write_str("backspace"),
            KeyCode::Delete => f.write_str("del"),
            KeyCode::Tab => f.write_str("tab"),
            KeyCode::BackTab => f.write_str("S-tab"),
            KeyCode::Left => f.write_str("left"),
            KeyCode::Right => f.write_str("right"),
            KeyCode::Up => f.write_str("up"),
            KeyCode::Down => f.write_str("down"),
            KeyCode::Home => f.write_str("home"),
            KeyCode::End => f.write_str("end"),
            KeyCode::PageUp => f.write_str("pageup"),
            KeyCode::PageDown => f.write_str("pagedown"),
            KeyCode::Insert => f.write_str("ins"),
            KeyCode::F(n) => write!(f, "F{n}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid key notation: {0:?}")]
pub struct KeyParseError(pub String);

impl FromStr for KeyEvent {
    type Err = KeyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || KeyParseError(s.to_string());
        let mut mods = Modifiers::default();
        let mut rest = s;
        loop {
            if rest.len() > 2 && (rest.starts_with("C-") || rest.starts_with("c-")) {
                mods.ctrl = true;
            } else if rest.len() > 2 && (rest.starts_with("A-") || rest.starts_with("a-")) {
                mods.alt = true;
            } else if rest.len() > 2 && (rest.starts_with("S-") || rest.starts_with("s-")) {
                mods.shift = true;
            } else {
                break;
            }
            rest = &rest[2..];
        }
        let code = match rest {
            "space" => KeyCode::Char(' '),
            "minus" => KeyCode::Char('-'),
            "lt" => KeyCode::Char('<'),
            "gt" => KeyCode::Char('>'),
            "ret" | "enter" => KeyCode::Enter,
            "esc" => KeyCode::Esc,
            "backspace" => KeyCode::Backspace,
            "del" => KeyCode::Delete,
            "tab" if mods.shift => KeyCode::BackTab,
            "tab" => KeyCode::Tab,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "ins" => KeyCode::Insert,
            f if f.len() > 1 && f.starts_with('F') => {
                KeyCode::F(f[1..].parse().map_err(|_| err())?)
            }
            c => {
                let mut chars = c.chars();
                let ch = chars.next().ok_or_else(err)?;
                if chars.next().is_some() {
                    return Err(err());
                }
                KeyCode::Char(ch)
            }
        };
        Ok(KeyEvent::new(code, mods))
    }
}

/// Parses a space-separated key sequence such as `"space w v"`.
pub fn parse_sequence(s: &str) -> Result<Vec<KeyEvent>, KeyParseError> {
    s.split_whitespace().map(str::parse).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display_round_trip() {
        for s in [
            "a", "A", "C-r", "A-;", "space", "ret", "esc", "S-tab", "C-A-x", "F5", "pagedown",
        ] {
            let k: KeyEvent = s.parse().unwrap();
            assert_eq!(k.to_string(), s, "round trip of {s}");
        }
        assert_eq!(parse_sequence("space w v").unwrap().len(), 3);
        assert!("xyz".parse::<KeyEvent>().is_err());
    }

    #[test]
    fn shift_is_folded_into_chars() {
        let k = KeyEvent::new(
            KeyCode::Char('X'),
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(k, KeyEvent::char('X'));
    }
}
