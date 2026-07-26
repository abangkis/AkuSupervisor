//! Stdio compatibility proxy for MCP clients that cannot read a bearer token
//! directly from `AkuSupervisor`'s protected runtime file.

use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};

use super::config::is_runtime_token_path;
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
    let config = parse_proxy_profile(&source).map_err(McpProxyError::Config)?;
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpProxyProfile {
    control: McpProxyControl,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpProxyControl {
    host: String,
    port: u16,
    token_file: PathBuf,
    #[serde(default)]
    mcp: McpProxySettings,
}

#[derive(Debug, Default, Deserialize)]
struct McpProxySettings {
    #[serde(default)]
    enabled: bool,
}

fn parse_proxy_profile(source: &str) -> Result<McpProxyProfile, String> {
    let profile: McpProxyProfile =
        serde_json::from_str(source).map_err(|error| format!("invalid JSON: {error}"))?;
    if profile.control.host != "127.0.0.1" {
        return Err("control.host must be exactly 127.0.0.1".to_owned());
    }
    if profile.control.port == 0 {
        return Err("control.port must be non-zero".to_owned());
    }
    if !is_runtime_token_path(&profile.control.token_file) {
        return Err("control.tokenFile must be a relative path beneath .runtime".to_owned());
    }
    Ok(profile)
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
    Config(String),
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
            Self::Config(error) => write!(formatter, "MCP proxy configuration is invalid: {error}"),
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

    use super::{McpProxyError, parse_proxy_profile, proxy};

    #[test]
    fn proxy_profile_ignores_unrelated_supervisor_contract_changes() {
        let profile = parse_proxy_profile(
            r#"{
                "version": 999,
                "control": {
                    "host": "127.0.0.1",
                    "port": 47820,
                    "tokenFile": ".runtime/control-token",
                    "mcp": {"enabled": true, "futureSetting": "ignored"},
                    "futureControlSetting": true
                },
                "cooperativeActions": {"futureAction": {"shape": "unknown"}},
                "services": {"future-service": {"shape": "unknown"}},
                "futureTopLevel": true
            }"#,
        )
        .expect("proxy should parse only its required control projection");

        assert_eq!(profile.control.host, "127.0.0.1");
        assert_eq!(profile.control.port, 47820);
        assert!(profile.control.mcp.enabled);
    }

    #[test]
    fn proxy_profile_retains_loopback_and_token_path_security_boundaries() {
        for source in [
            r#"{"control":{"host":"0.0.0.0","port":47820,"tokenFile":".runtime/control-token","mcp":{"enabled":true}}}"#,
            r#"{"control":{"host":"127.0.0.1","port":47820,"tokenFile":"../control-token","mcp":{"enabled":true}}}"#,
        ] {
            assert!(parse_proxy_profile(source).is_err());
        }
    }

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
