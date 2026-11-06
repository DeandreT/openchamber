//! Transport from the native desktop to an OpenChamber server.
//!
//! Owns endpoint identity, authentication, typed wire parsing, request
//! cancellation and event-stream decoding. Wire shapes stay private; callers
//! receive core records and domain events. See
//! `docs/rust-desktop/OPENCODE_ADAPTER.md` for the contract.

pub mod attachment;
pub mod authentication;
pub mod catalog;
pub mod client;
pub mod composer_preparation;
pub mod control_stream;
pub mod credential;
pub mod endpoint;
pub mod error;
pub mod event;
pub mod files;
pub mod git;
pub mod identity;
pub mod message_queue;
#[cfg(test)]
mod parity_tests;
pub mod permission_policy;
pub mod project;
pub mod project_admission;
mod projection;
pub mod prompt;
pub mod prompt_parts;
pub mod public_asset;
pub mod queue_frozen_send;
pub mod queue_manifest;
mod request;
mod request_wire;
pub mod requests;
pub mod session_actions;
pub mod session_create;
pub mod starters;
pub mod stream;
pub mod system;
pub mod terminal;
mod wire;
