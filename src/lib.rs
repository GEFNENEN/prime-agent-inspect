//! Read-only analyzer for prime-agent session files.
//!
//! The tool answers three questions about a session corpus: what is in it,
//! where the tokens went, and what each compaction cost. It parses session
//! JSONL itself rather than linking `pa-types`, so a format bump upstream
//! cannot break the analyzer at compile time.

pub mod cache;
pub mod calibrate;
pub mod context;
pub mod global_state;
pub mod model;
pub mod overhead;
pub mod session;
pub mod http;
pub mod system_prompt;
pub mod token_est;
pub mod web;
