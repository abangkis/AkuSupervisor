//! External control and persistence adapters.
//!
//! CLI, HTTP, MCP, configuration, journal, and browser adapters connect the
//! application core to local operators and external clients.

pub mod chrome_extension_reload;
pub mod config;
pub mod config_path;
pub mod console_time;
pub mod control_http;
pub mod cooperative_extensions;
mod dashboard_assets;
pub mod development_shutdown;
#[cfg(windows)]
pub mod foreground;
pub mod http_health;
mod http_response;
pub mod journal;
pub mod mcp;
pub mod mcp_contract;
pub mod mcp_proxy;
pub mod registration;
pub mod registration_events;
pub mod registration_mcp;
pub mod runtime_instance;
pub mod runtime_token;
pub mod service_logs;
pub mod supervisor_shutdown;
