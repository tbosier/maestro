//! Language Server Protocol client for Maestro.
//!
//! This crate runs language server processes and speaks JSON-RPC to them on
//! plain threads. It has no terminal or editor dependency and never waits
//! for a server's answer: requests hand back a channel, notifications go to
//! a sink the caller provides. The editor (`maestro-editor`) turns those into jobs that
//! run on its main thread.
//!
//! Positions are UTF-16 code units, the protocol default, converted with
//! `maestro_core::pos`.

pub mod client;
pub mod completion;
pub mod convert;
pub mod framing;
pub mod jsonrpc;

#[cfg(feature = "fake-server")]
pub mod fake;

pub use client::{Client, Event, EventSink, Reply, StartOptions, find_executable};
pub use jsonrpc::{RequestId, RpcError};
pub use serde_json::{Value, json};
