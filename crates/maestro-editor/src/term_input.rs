//! Keys and pastes for a program running in a terminal. Terminal agents in
//! the agent panel (see [`crate::agents::panel`]) and the shell pane (see
//! [`crate::shell`]) send their input through these.

use std::io;

use maestro_term::Terminal;

use crate::key::{KeyCode, KeyEvent};

/// True when `key` is `C-\`, the key that moves the keys between the editor
/// and a terminal. Terminals send it as the byte 0x1c, which crossterm
/// reports as `C-4`, so both count.
pub fn is_toggle(key: KeyEvent) -> bool {
    key.mods.ctrl && !key.mods.alt && matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4'))
}

/// Sends `key` to the program as a terminal emulator would, in application
/// cursor mode when the program asked for it. As in a terminal emulator,
/// typing returns the view to the live screen.
pub(crate) fn send_key(t: &Terminal, key: KeyEvent) -> io::Result<()> {
    let tk = term_key(key.code);
    t.scroll(0);
    let mods = maestro_term::TermMods {
        ctrl: key.mods.ctrl,
        alt: key.mods.alt,
        shift: key.mods.shift,
    };
    t.write(&maestro_term::encode_key(tk, mods, t.application_cursor()))
}

/// Sends pasted text to the program, bracketed when it turned bracketed
/// paste on.
pub(crate) fn send_paste(t: &Terminal, text: &str) -> io::Result<()> {
    t.write(&maestro_term::encode_paste(text, t.bracketed_paste()))
}

fn term_key(code: KeyCode) -> maestro_term::TermKey {
    use maestro_term::TermKey as T;
    match code {
        KeyCode::Char(c) => T::Char(c),
        KeyCode::Enter => T::Enter,
        KeyCode::Esc => T::Esc,
        KeyCode::Backspace => T::Backspace,
        KeyCode::Delete => T::Delete,
        KeyCode::Tab => T::Tab,
        KeyCode::BackTab => T::BackTab,
        KeyCode::Left => T::Left,
        KeyCode::Right => T::Right,
        KeyCode::Up => T::Up,
        KeyCode::Down => T::Down,
        KeyCode::Home => T::Home,
        KeyCode::End => T::End,
        KeyCode::PageUp => T::PageUp,
        KeyCode::PageDown => T::PageDown,
        KeyCode::Insert => T::Insert,
        KeyCode::F(n) => T::F(n),
    }
}
