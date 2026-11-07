//! Application model for the native OpenChamber desktop.
//!
//! This crate owns domain records, sidebar membership, ordered transcripts,
//! draft identity and state transitions. It has no renderer, network or
//! filesystem dependency: callers pass trusted, explicit inputs and receive
//! owned values, stable identifiers or borrowed views.

pub mod auto_review;
pub mod autocomplete;
pub mod branching;
pub mod chat_roots;
pub mod composer_selection;
pub mod context_usage;
pub mod data_url;
pub mod draft;
pub mod draft_content;
pub mod draft_starters;
pub mod file_draft;
pub mod file_tree;
pub mod folders;
pub mod form;
pub mod git_changes;
pub mod git_identity;
pub mod grouping;
pub mod ids;
pub mod load_state;
pub mod magic_prompt;
pub mod message_queue;
pub mod message_time;
pub mod model_identifier;
pub mod ordering;
pub mod ownership;
pub mod path;
pub mod pending_requests;
pub mod permission;
pub mod permission_mode;
pub mod project_actions;
pub mod provider_logo;
pub mod relative_time;
pub mod repository;
pub mod request_list;
pub mod request_owner;
pub mod scroll_memory;
pub mod search_rank;
pub mod send_config;
pub mod session;
pub mod session_actions;
pub mod session_goal;
pub mod session_index;
pub mod session_markdown;
pub mod session_moves;
pub mod session_reference;
pub mod session_search;
pub mod shortcuts;
pub mod sidebar_collection;
pub mod sidebar_rows;
pub mod sidebar_work;
pub mod source_board;
pub mod synthetic;
pub mod terminal_tabs;
pub mod tool_names;
pub mod tool_shells;
pub mod transcript;
pub mod turn_footer;
pub mod turn_stats;
