//! Release builds pin the version through `GOON_VERSION` (set by the release workflow after it
//! picks the next tag); local builds use the Cargo manifest, which the release commit keeps in sync.
pub const VERSION: &str = match option_env!("GOON_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
