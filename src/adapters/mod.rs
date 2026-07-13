//! External control and persistence adapters.
//!
//! The visible CLI is the first adapter. HTTP, journal persistence, and MCP are
//! added only at their roadmap gates.

pub mod config;
pub mod config_path;
pub mod control_http;
#[cfg(windows)]
pub mod foreground;
pub mod journal;
pub mod runtime_token;
