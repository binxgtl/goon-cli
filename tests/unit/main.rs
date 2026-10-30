//! Library-level tests, ported from the TypeScript `node:test` unit suite. CLI-level behaviour
//! stays covered by `tests/*.test.mjs`, which run against the native binary.
#![allow(clippy::type_complexity)]
mod audit;
mod cli_support;
mod common;
mod context;
mod context_budget;
mod coord;
mod cost;
mod dag;
mod embedder;
mod eventlog;
mod features;
mod files;
mod harness_verify;
mod mcp;
mod memory;
mod memory_integration;
mod policy;
mod rag;
mod reasoning;
mod reflection;
mod review_hardening;
mod risk;
mod runtime;
mod scheduler;
mod scope_loop;
mod session;
mod stub_provider;
mod subagents;
mod syntax;
mod verify;
mod workflow;
mod workflow_classifier;
