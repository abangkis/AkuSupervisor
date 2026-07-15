//! External control and persistence adapters.
//!
//! The visible CLI is the first adapter. HTTP, journal persistence, and MCP are
//! added only at their roadmap gates.

pub mod aku_bridge_reload;
pub mod config;
pub mod config_path;
pub mod control_http;
pub mod development_shutdown;
#[cfg(windows)]
pub mod foreground;
pub mod http_health;
mod http_response;
pub mod journal;
pub mod mcp;
pub mod mcp_proxy;
pub mod registration;
pub mod registration_mcp;
pub mod runtime_token;
pub mod service_logs;
