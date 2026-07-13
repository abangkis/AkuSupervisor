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
