//! Library-level tests, ported from the TypeScript `node:test` unit suite. CLI-level behaviour
//! stays covered by `tests/*.test.mjs`, which run against the native binary.
mod common;
mod risk;
mod reasoning;
mod syntax;
mod workflow_classifier;
mod review_hardening;
mod policy;
mod files;
mod context_budget;
mod cost;
