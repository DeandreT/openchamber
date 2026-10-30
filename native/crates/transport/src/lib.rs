//! Transport from the native desktop to an OpenChamber server.
//!
//! Owns endpoint identity, authentication, typed wire parsing, request
//! cancellation and event-stream decoding. Wire shapes stay private; callers
//! receive core records and domain events. See
//! `docs/rust-desktop/OPENCODE_ADAPTER.md` for the contract.

pub mod catalog;
pub mod client;
pub mod credential;
pub mod endpoint;
pub mod error;
pub mod event;
pub mod identity;
#[cfg(test)]
mod parity_tests;
pub mod permission_policy;
pub mod project;
mod projection;
pub mod prompt;
pub mod prompt_parts;
pub mod public_asset;
mod request_wire;
pub mod requests;
pub mod session_actions;
pub mod session_create;
pub mod stream;
mod wire;
