//! goon: an independent coding harness for Termux and external model APIs.
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

pub mod approval;
pub mod config;
pub mod context;
pub mod coord;
pub mod context_budget;
pub mod cost;
pub mod errors;
pub mod eventlog;
pub mod files;
pub mod generate;
pub mod harness;
pub mod help;
pub mod jsutil;
pub mod mcp;
pub mod network;
pub mod mcp_client;
pub mod model;
pub mod policy;
pub mod process;
pub mod prompts;
pub mod provider;
pub mod providers;
pub mod reasoning;
pub mod redact;
pub mod risk;
pub mod schema;
pub mod session;
pub mod subagents;
pub mod setup;
pub mod syntax;
pub mod term;
pub mod tools;
pub mod verify;
pub mod workflow;
pub mod version;
