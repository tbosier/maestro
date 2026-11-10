//! Editor state, modal commands and keymaps for Maestro.
//!
//! This crate depends on `maestro-core`, `maestro-config`, `maestro-git`,
//! `maestro-lsp` and `maestro-syntax`. It has no terminal or network
//! dependency: a front end feeds it [`KeyEvent`]s and draws what it holds.
//! Git runs only on background threads (see [`git`]); language servers run
//! on their own threads (see [`lsp`]); large parses run in the background
//! (see [`syntax`]). Coding agents, their terminals, tests and review run
//! through `maestro-agent`, `maestro-term`, `maestro-tasks` and
//! `maestro-git` (see [`agents`]). The GitHub inbox runs the GitHub CLI
//! through `maestro-forge` on background threads (see [`forge`]). The shell
//! pane runs the user's shell in a `maestro-term` terminal (see [`shell`]).

pub mod agents;
pub mod commands;
pub mod comment;
pub mod editor;
pub mod forge;
pub mod git;
pub mod git_hunks;
pub mod git_screen;
pub mod key;
pub mod keymap;
pub mod layout;
pub mod lsp;
pub mod lsp_commands;
pub mod messages;
pub mod motion;
pub mod navigation;
pub mod picker;
pub mod prompt;
pub mod proposals;
pub mod python_indent;
pub mod recovery;
pub mod registers;
pub mod search;
pub mod shell;
pub mod structural;
pub mod syntax;
pub mod syntax_commands;
pub mod term_input;
pub mod typed;
pub mod workspaces;

pub use editor::{DocumentId, Editor, Mode, MouseKind, Presentation, Severity, View, ViewId};
pub use key::{KeyCode, KeyEvent, Modifiers};
pub use layout::Rect;
