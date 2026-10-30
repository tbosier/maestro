//! The Maestro automation server.
//!
//! [`server::Server`] executes [`maestro_protocol`] requests against an
//! [`maestro_editor::Editor`]. Two transports carry them:
//!
//! * [`headless`]: newline-delimited JSON on stdin and stdout, for
//!   `maestro --headless`, with no terminal at all;
//! * `socket` (Unix only): a socket served while the interactive editor
//!   runs, for `maestro --listen`, with requests delivered to the editor
//!   thread as background jobs.
//!
//! This lives in its own crate rather than inside `maestro-editor` so the
//! editor crate stays free of JSON and sockets, and so the server can only
//! use the editor's public interface, the same one a front end uses.

pub mod cli;
pub mod headless;
pub mod server;
#[cfg(unix)]
pub mod socket;

pub use server::{Server, Session};
