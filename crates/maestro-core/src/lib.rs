//! Editing core for Maestro.
//!
//! This crate owns the document model and nothing else: no terminal, no
//! async runtime, no Git, no language servers. Everything here is testable as
//! plain data.
//!
//! # Position conventions
//!
//! Internally every position is a **char index** (a Unicode scalar value
//! offset into the rope), stored as `usize`. This matches Ropey's native
//! indexing. Other coordinate systems only appear at boundaries and use
//! explicit types from [`pos`]:
//!
//! * [`pos::ByteOffset`] for file IO and regex matches,
//! * [`pos::LspPosition`] (line + UTF-16 code unit) for language servers,
//! * [`pos::LineCol`] (line + grapheme-aware display column) for rendering.
//!
//! Conversions between them live in [`pos`] and are covered by tests.

pub mod change;
pub mod document;
pub mod history;
pub mod io;
pub mod line_ending;
pub mod pos;
pub mod selection;
pub mod text;

pub use change::{Assoc, Change, ChangeError, ChangeSet, Transaction};
pub use document::{Document, DocumentVersion, ReadOnlyReason};
pub use line_ending::LineEnding;
pub use ropey::{Rope, RopeSlice};
pub use selection::{Range, Selection};
