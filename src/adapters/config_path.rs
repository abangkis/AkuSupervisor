use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

pub const CONFIG_ENVIRONMENT_VARIABLE: &str = "AKU_SUPERVISOR_CONFIG";
const CONFIG_FILE_NAME: &str = "services.json";

/// Source selected by deterministic configuration discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigPathSource {
    Explicit,
    Environment,
    Default,
}

impl fmt::Display for ConfigPathSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Explicit => formatter.write_str("--config"),
            Self::Environment => formatter.write_str(CONFIG_ENVIRONMENT_VARIABLE),
            Self::Default => formatter.write_str("default user configuration"),
        }
    }
}

/// Existing configuration file selected for this supervisor invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfigPath {
    path: PathBuf,
    source: ConfigPathSource,
}

impl ResolvedConfigPath {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn source(&self) -> ConfigPathSource {
        self.source
    }
}

/// Resolves explicit, environment, and default configuration precedence.
///
/// # Errors
///
/// Returns [`ConfigPathError::DefaultLocationUnavailable`] if the operating
/// system's user configuration root cannot be determined, or
/// [`ConfigPathError::NotFound`] if the selected file does not exist.
pub fn resolve_config_path(
    explicit: Option<PathBuf>,
) -> Result<ResolvedConfigPath, ConfigPathError> {
    let environment = env::var_os(CONFIG_ENVIRONMENT_VARIABLE).filter(|value| !value.is_empty());
    let default = default_config_path()?;
    resolve_candidates(explicit, environment, default)
}

fn resolve_candidates(
    explicit: Option<PathBuf>,
    environment: Option<OsString>,
    default: PathBuf,
) -> Result<ResolvedConfigPath, ConfigPathError> {
    let (path, source) = if let Some(path) = explicit {
        (path, ConfigPathSource::Explicit)
    } else if let Some(path) = environment {
        (PathBuf::from(path), ConfigPathSource::Environment)
    } else {
        (default, ConfigPathSource::Default)
    };

    if path.is_file() {
        Ok(ResolvedConfigPath { path, source })
    } else {
        Err(ConfigPathError::NotFound { path, source })
    }
}

#[cfg(windows)]
fn default_config_path() -> Result<PathBuf, ConfigPathError> {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("AkuSupervisor").join(CONFIG_FILE_NAME))
        .ok_or(ConfigPathError::DefaultLocationUnavailable {
            environment_variable: "LOCALAPPDATA",
        })
}

#[cfg(target_os = "linux")]
fn default_config_path() -> Result<PathBuf, ConfigPathError> {
    if let Some(root) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root)
            .join("aku-supervisor")
            .join(CONFIG_FILE_NAME));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|root| {
            root.join(".config")
                .join("aku-supervisor")
                .join(CONFIG_FILE_NAME)
        })
        .ok_or(ConfigPathError::DefaultLocationUnavailable {
            environment_variable: "XDG_CONFIG_HOME or HOME",
        })
}

#[cfg(target_os = "macos")]
fn default_config_path() -> Result<PathBuf, ConfigPathError> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|root| {
            root.join("Library")
                .join("Application Support")
                .join("AkuSupervisor")
                .join(CONFIG_FILE_NAME)
        })
        .ok_or(ConfigPathError::DefaultLocationUnavailable {
            environment_variable: "HOME",
        })
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn default_config_path() -> Result<PathBuf, ConfigPathError> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|root| {
            root.join(".config")
                .join("aku-supervisor")
                .join(CONFIG_FILE_NAME)
        })
        .ok_or(ConfigPathError::DefaultLocationUnavailable {
            environment_variable: "HOME",
        })
}

/// Configuration discovery failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigPathError {
    DefaultLocationUnavailable {
        environment_variable: &'static str,
    },
    NotFound {
        path: PathBuf,
        source: ConfigPathSource,
    },
}

impl fmt::Display for ConfigPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefaultLocationUnavailable {
                environment_variable,
            } => write!(
                formatter,
                "cannot determine the user configuration directory; {environment_variable} is unavailable"
            ),
            Self::NotFound {
                path,
                source: ConfigPathSource::Default,
            } => write!(
                formatter,
                "no configuration found\nexpected: {}\nuse --config <path> or set {CONFIG_ENVIRONMENT_VARIABLE}",
                path.display()
            ),
            Self::NotFound { path, source } => {
                write!(
                    formatter,
                    "configuration selected by {source} does not exist: {}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ConfigPathError {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{ConfigPathError, ConfigPathSource, resolve_candidates};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn explicit_path_wins_over_environment_and_default() {
        let directory = TestDirectory::create();
        let explicit = directory.file("explicit.json");
        let environment = directory.file("environment.json");
        let default = directory.file("default.json");

        let resolved = resolve_candidates(
            Some(explicit.clone()),
            Some(environment.into_os_string()),
            default,
        )
        .expect("explicit configuration resolves");

        assert_eq!(resolved.path(), explicit);
        assert_eq!(resolved.source(), ConfigPathSource::Explicit);
    }

    #[test]
    fn environment_wins_when_explicit_path_is_absent() {
        let directory = TestDirectory::create();
        let environment = directory.file("environment.json");
        let default = directory.file("default.json");

        let resolved =
            resolve_candidates(None, Some(environment.clone().into_os_string()), default)
                .expect("environment configuration resolves");

        assert_eq!(resolved.path(), environment);
        assert_eq!(resolved.source(), ConfigPathSource::Environment);
    }

    #[test]
    fn missing_default_reports_expected_location() {
        let directory = TestDirectory::create();
        let missing = directory.path.join("missing.json");

        let error = resolve_candidates(None, None, missing.clone())
            .expect_err("missing default configuration fails closed");

        assert_eq!(
            error,
            ConfigPathError::NotFound {
                path: missing,
                source: ConfigPathSource::Default
            }
        );
        assert!(error.to_string().contains("use --config <path>"));
    }

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn create() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aku-supervisor-config-path-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.path.join(name);
            fs::write(&path, b"{}").expect("create config candidate");
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).ok();
        }
    }
}
