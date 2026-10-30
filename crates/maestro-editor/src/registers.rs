//! Registers and the clipboard interface.
//!
//! A register holds one string per selection, so yanking three selections
//! and pasting into three selections pastes each piece into its own place.
//! `+` and `*` are the system clipboard; the front end supplies the
//! [`Clipboard`] implementation.

use std::collections::HashMap;

pub trait Clipboard {
    fn get(&mut self) -> Result<String, String>;
    fn set(&mut self, text: &str) -> Result<(), String>;
    /// Short description shown in messages, e.g. "wl-copy" or "OSC 52".
    fn name(&self) -> &str;
}

/// In-process clipboard used in tests and when no system clipboard exists.
#[derive(Debug, Default)]
pub struct MemoryClipboard(String);

impl Clipboard for MemoryClipboard {
    fn get(&mut self) -> Result<String, String> {
        Ok(self.0.clone())
    }

    fn set(&mut self, text: &str) -> Result<(), String> {
        self.0 = text.to_string();
        Ok(())
    }

    fn name(&self) -> &str {
        "internal"
    }
}

#[derive(Debug, Default)]
pub struct Registers {
    values: HashMap<char, Vec<String>>,
    /// Prompt input history by prompt kind (":" and "/").
    pub history: HashMap<char, Vec<String>>,
}

pub const DEFAULT: char = '"';

impl Registers {
    pub fn get(&self, name: char) -> Option<&[String]> {
        self.values.get(&name).map(Vec::as_slice)
    }

    pub fn set(&mut self, name: char, values: Vec<String>) {
        if name == '_' {
            return; // black hole
        }
        self.values.insert(name, values);
    }

    pub fn push_history(&mut self, kind: char, entry: &str) {
        if entry.is_empty() {
            return;
        }
        let h = self.history.entry(kind).or_default();
        h.retain(|e| e != entry);
        h.push(entry.to_string());
        if h.len() > 200 {
            h.remove(0);
        }
    }
}

pub fn is_clipboard(name: char) -> bool {
    matches!(name, '+' | '*')
}
