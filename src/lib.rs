//! Engram core library.
//!
//! Milestone 0 intentionally keeps domain behavior behind small typed modules:
//! configuration, canonical `SQLite` storage, immutable events, and project identity.

#[doc(hidden)]
pub mod agenthooks;
pub mod architect;
pub mod autostart;
pub mod blobs;
pub mod cache;
pub mod codex_trust;
pub mod config;
pub mod context;
pub mod daemon;
pub mod doctor;
pub mod effectiveness;
pub mod error;
pub mod evaluation;
pub mod events;
pub mod experience;
pub mod export;
pub mod gitref;
pub mod ids;
pub mod jobs;
pub mod judge;
pub mod learning;
pub mod lifecycle;
pub mod linkscan;
pub mod mcp;
pub mod memory;
pub mod merge;
pub mod metrics;
pub mod product;
pub mod projects;
pub mod proxy;
pub mod repository_context;
pub mod retrieval;
pub mod reverify;
pub mod scheduler;
pub mod security;
pub mod semantic;
pub mod skillpacks;
pub mod storage;
pub mod sync;
pub mod synthesis;
pub mod techstack;
pub mod usage_capture;
pub mod vault;
pub mod web;
pub mod workspaces;
pub mod world;

pub use error::{Error, Result};

/// Build identity stamped by the release installer. Development builds share
/// a fixed identity so sibling binaries produced by one Cargo invocation match.
pub const BUILD_ID: &str = match option_env!("ENGRAM_BUILD_ID") {
    Some(value) => value,
    None => "development",
};
