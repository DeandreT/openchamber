//! Application model for the native OpenChamber desktop.
//!
//! This crate owns domain records, sidebar membership, ordered transcripts,
//! draft identity and state transitions. It has no renderer, network or
//! filesystem dependency: callers pass trusted, explicit inputs and receive
//! owned values, stable identifiers or borrowed views.

pub mod chat_projection;
pub mod chat_roots;
pub mod draft;
pub mod grouping;
pub mod ids;
pub mod load_state;
pub mod ordering;
pub mod ownership;
pub mod path;
pub mod relative_time;
pub mod scroll_memory;
pub mod session;
pub mod session_index;
pub mod sidebar_rows;
pub mod synthetic;
pub mod transcript;
