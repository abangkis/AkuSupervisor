//! `AkuSupervisor` lifecycle library.
//!
//! Protocol adapters and platform code depend on the application and domain
//! layers. The domain layer must not depend on an adapter or operating system.

pub mod adapters;
pub mod application;
pub mod cli;
pub mod domain;
pub mod platform;

/// Package version embedded by Cargo at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Source revision embedded by the build script, with `-dirty` when the
/// compiled source tree had uncommitted changes.
pub const BUILD_COMMIT: &str = env!("AKU_SUPERVISOR_BUILD_COMMIT");

/// Returns the human-facing binary identity used by `--version` and startup
/// diagnostics. Protocol contracts continue to expose semantic `VERSION`
/// separately from this build provenance.
#[must_use]
pub fn version_banner() -> String {
    format!("aku-supervisor {VERSION} ({BUILD_COMMIT})")
}
