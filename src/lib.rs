//! Local Engineering Brain (LEB) core library.
//!
//! Milestone 0 intentionally keeps domain behavior behind small typed modules:
//! configuration, canonical `SQLite` storage, immutable events, and project identity.

#[doc(hidden)]
pub mod autostart;
pub mod blobs;
pub mod cache;
pub mod config;
pub mod context;
pub mod daemon;
pub mod doctor;
pub mod error;
pub mod evaluation;
pub mod events;
pub mod experience;
pub mod export;
pub mod ids;
pub mod jobs;
pub mod learning;
pub mod mcp;
pub mod memory;
pub mod metrics;
pub mod projects;
pub mod proxy;
pub mod retrieval;
pub mod scheduler;
pub mod security;
pub mod semantic;
pub mod storage;
pub mod synthesis;
pub mod vault;
pub mod world;

pub use error::{Error, Result};

/// Build identity stamped by the release installer. Development builds share
/// a fixed identity so sibling binaries produced by one Cargo invocation match.
pub const BUILD_ID: &str = match option_env!("LEB_BUILD_ID") {
    Some(value) => value,
    None => "development",
};
