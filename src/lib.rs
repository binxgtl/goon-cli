//! goon: an independent coding harness for Termux and external model APIs.
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

/// A regex compiled once per call site.
macro_rules! re {
    ($pattern:expr) => {{
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        #[allow(clippy::regex_creation_in_loops)]
        RE.get_or_init(|| regex::Regex::new($pattern).unwrap())
    }};
}
pub(crate) use re;

pub mod approval;
pub mod cli;
pub mod config;
pub mod context;
pub mod context_budget;
pub mod coord;
pub mod cost;
pub mod embedder;
pub mod errors;
pub mod eventlog;
pub mod files;
pub mod generate;
pub mod harness;
pub mod help;
pub mod jsutil;
pub mod mcp;
pub mod mcp_client;
pub mod memory;
pub mod model;
pub mod network;
pub mod pair;
pub mod policy;
pub mod process;
pub mod prompts;
pub mod provider;
pub mod providers;
pub mod rag;
pub mod reasoning;
pub mod redact;
pub mod risk;
pub mod schema;
pub mod session;
pub mod setup;
pub mod subagents;
pub mod syntax;
pub mod term;
pub mod tools;
pub mod tui;
pub mod verify;
pub mod version;
pub mod workflow;
