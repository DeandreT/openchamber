//! Transport from the native desktop to an OpenChamber server.
//!
//! Owns endpoint identity, authentication, typed wire parsing, request
//! cancellation and event-stream decoding. Wire shapes stay private; callers
//! receive core records and domain events. See
//! `docs/rust-desktop/OPENCODE_ADAPTER.md` for the contract.

pub mod attachment;
pub mod authentication;
pub mod behavior;
pub mod catalog;
pub mod client;
pub mod compatibility;
pub mod composer_catalog;
pub mod composer_preparation;
mod context_payload;
pub mod control_stream;
pub mod credential;
pub mod endpoint;
pub mod error;
pub mod event;
pub mod files;
pub mod git;
pub mod identity;
pub mod message_queue;
pub mod message_search;
#[cfg(test)]
mod parity_tests;
pub mod permission_policy;
pub mod project;
pub mod project_admission;
mod projection;
pub mod projects;
pub mod prompt;
pub mod prompt_parts;
pub mod public_asset;
pub mod queue_frozen_send;
pub mod queue_manifest;
mod request;
mod request_wire;
pub mod requests;
pub mod review;
pub mod session_actions;
pub mod session_create;
pub mod session_folders;
pub mod session_goal;
pub mod session_warming;
pub mod small_model;
pub mod snippets;
pub mod source_control;
pub mod starters;
pub mod stream;
pub mod system;
pub mod terminal;
#[cfg(test)]
mod test_server;
mod tool_facts;
mod wire;
pub mod worktree;
