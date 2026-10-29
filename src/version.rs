//! The package manifest owns the version; Cargo.toml is kept in sync by the release script.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
