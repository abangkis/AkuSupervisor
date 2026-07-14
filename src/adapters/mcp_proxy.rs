//! Stdio compatibility proxy for MCP clients that cannot read a bearer token
//! directly from `AkuSupervisor`'s protected runtime file.

use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use serde_json::{Value, json};

use super::config::SupervisorConfig;
use super::config_path::{ConfigPathError, resolve_config_path};
use super::control_http::{ControlClientError, mcp_client_request};
use super::runtime_token::{RuntimeToken, RuntimeTokenError, resolve_token_path};

const MAX_STDIO_MESSAGE_BYTES: usize = 4 * 1024;

/// Runs a newline-delimited stdio proxy to an already-running Supervisor.
///
/// # Errors
///
/// Returns configuration, token, stdin/stdout, or loopback transport failures.
pub fn run(explicit_config: Option<PathBuf>) -> Result<(), McpProxyError> {
    let resolved = resolve_config_path(explicit_config).map_err(McpProxyError::ConfigPath)?;
    let source =
        fs::read_to_string(resolved.path()).map_err(|source| McpProxyError::ReadConfig {
            path: resolved.path().to_owned(),
            source,
        })?;
    let config = SupervisorConfig::parse_json(&source).map_err(McpProxyError::Config)?;
    config.validate().map_err(McpProxyError::Config)?;
    if !config.control.mcp.enabled {
        return Err(McpProxyError::Disabled);
    }
    let token_path = resolve_token_path(resolved.path(), &config.control.token_file);
    let token = RuntimeToken::load(&token_path).map_err(McpProxyError::Token)?;
    let address = format!("{}:{}", config.control.host, config.control.port)
        .parse::<SocketAddr>()
        .map_err(McpProxyError::Address)?;

    proxy(io::stdin().lock(), io::stdout().lock(), address, &token)
}

fn proxy(
    mut input: impl BufRead,
    mut output: impl Write,
    address: SocketAddr,
    token: &RuntimeToken,
) -> Result<(), McpProxyError> {
    let mut line = String::new();
    loop {
        line.clear();
        let count = input
            .read_line(&mut line)
            .map_err(McpProxyError::ReadStdin)?;
        if count == 0 {
            return Ok(());
        }
        let message = line.trim_end_matches(['\r', '\n']);
        if message.len() > MAX_STDIO_MESSAGE_BYTES {
            write_json_line(
                &mut output,
                &json!({
                    "jsonrpc":"2.0",
                    "id":Value::Null,
                    "error":{"code":-32600,"message":"MCP message exceeds bounded body size"}
                }),
            )?;
            continue;
        }
        let message: Value = if let Ok(message) = serde_json::from_str(message) {
            message
        } else {
            write_json_line(
                &mut output,
                &json!({
                    "jsonrpc":"2.0",
                    "id":Value::Null,
                    "error":{"code":-32700,"message":"Parse error"}
                }),
            )?;
            continue;
        };
        if let Some(response) =
            mcp_client_request(address, token, &message).map_err(McpProxyError::Transport)?
        {
            write_json_line(&mut output, &response)?;
        }
    }
}

fn write_json_line(output: &mut impl Write, value: &Value) -> Result<(), McpProxyError> {
    serde_json::to_writer(&mut *output, value).map_err(McpProxyError::Serialize)?;
    output
        .write_all(b"\n")
        .map_err(McpProxyError::WriteStdout)?;
    output.flush().map_err(McpProxyError::WriteStdout)
}

#[derive(Debug)]
pub enum McpProxyError {
    ConfigPath(ConfigPathError),
    ReadConfig { path: PathBuf, source: io::Error },
    Config(super::config::ConfigError),
    Disabled,
    Token(RuntimeTokenError),
    Address(std::net::AddrParseError),
    ReadStdin(io::Error),
    WriteStdout(io::Error),
    Serialize(serde_json::Error),
    Transport(ControlClientError),
}

impl fmt::Display for McpProxyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigPath(error) => error.fmt(formatter),
            Self::ReadConfig { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
            Self::Config(error) => error.fmt(formatter),
            Self::Disabled => formatter.write_str("read-only MCP is disabled in configuration"),
            Self::Token(error) => error.fmt(formatter),
            Self::Address(error) => write!(formatter, "invalid control address: {error}"),
            Self::ReadStdin(error) => write!(formatter, "failed to read MCP stdin: {error}"),
            Self::WriteStdout(error) => write!(formatter, "failed to write MCP stdout: {error}"),
            Self::Serialize(error) => {
                write!(formatter, "failed to serialize MCP response: {error}")
            }
            Self::Transport(error) => write!(formatter, "MCP loopback transport failed: {error}"),
        }
    }
}

impl std::error::Error for McpProxyError {}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{McpProxyError, proxy};

    #[test]
    fn malformed_stdio_input_is_answered_without_contacting_a_supervisor() {
        let input = Cursor::new(b"not-json\n");
        let mut output = Vec::new();
        let token_path =
            std::env::temp_dir().join(format!("aku-supervisor-proxy-token-{}", std::process::id()));
        std::fs::remove_file(&token_path).ok();
        let token = super::RuntimeToken::load_or_create(&token_path, || Ok("a".repeat(64)))
            .expect("create token");

        let result = proxy(
            input,
            &mut output,
            "127.0.0.1:9".parse().expect("address"),
            &token,
        );

        assert!(result.is_ok());
        let response: serde_json::Value = serde_json::from_slice(&output).expect("JSON line");
        assert_eq!(response["error"]["code"], -32700);
        assert!(!matches!(result, Err(McpProxyError::Transport(_))));
        std::fs::remove_file(token_path).ok();
    }
}
