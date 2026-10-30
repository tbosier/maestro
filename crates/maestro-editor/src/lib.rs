//! Editor state, modal commands and keymaps for Maestro.
//!
//! This crate depends on `maestro-core`, `maestro-config` and `maestro-lsp`
//! (which runs language servers on its own threads). It has no
//! terminal, network or Git dependency: a front end feeds it [`KeyEvent`]s
//! and draws what it holds.

pub mod commands;
pub mod editor;
pub mod key;
pub mod keymap;
pub mod layout;
pub mod lsp;
pub mod lsp_commands;
pub mod motion;
pub mod picker;
pub mod prompt;
pub mod recovery;
pub mod registers;
pub mod search;
pub mod typed;

pub use editor::{DocumentId, Editor, Mode, MouseKind, Presentation, Severity, View, ViewId};
pub use key::{KeyCode, KeyEvent, Modifiers};
pub use layout::Rect;
