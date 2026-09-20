//! Browser adapters for jev-browser-relay.
//!
//! Two backends behind one trait:
//!
//! - [`HarnessBackend`] drives real Chrome through the Browser Harness daemon, speaking its
//!   Unix-socket CDP protocol natively from Rust.
//! - [`ScriptedBackend`] is a deterministic in-memory page model, so tests and benchmarks run
//!   the identical loop with no browser and no network.

pub mod harness;
pub mod harness_backend;
pub mod provision;
pub mod scripted;
pub mod state;

pub use harness::HarnessIpc;
pub use harness_backend::HarnessBackend;
pub use provision::{
    ensure, ensure_daemon, install_plan, is_harness_installed, reclaim_orphaned_tabs, ProvisionOutcome,
};
pub use scripted::{ExecutedAction, Fixture, ScriptedBackend};

/// Default viewport for a relay session, matching jev-ultrafast's.
pub const DEFAULT_VIEWPORT: (u32, u32) = (1120, 780);
