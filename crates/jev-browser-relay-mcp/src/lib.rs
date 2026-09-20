//! Stateful MCP server for jev-browser-relay.
//!
//! The tool surface is a *task session*, not a set of browser primitives. That is the whole
//! design: a host agent starts a task, calls `run`, and hears back only when a decision genuinely
//! needs a capable model.

pub mod protocol;
pub mod server;
pub mod tools;

pub use server::{BackendFactory, HarnessFactory, RelayServer, ScriptedFactory};
