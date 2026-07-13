use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

const TOKEN_LENGTH: usize = 64;

/// Authentication secret whose debug output never reveals its value.
#[derive(Clone, PartialEq, Eq)]
pub struct RuntimeToken(String);

impl RuntimeToken {
    /// Loads an already-created runtime token.
    ///
    /// # Errors
    ///
    /// Returns a read or validation error when the file is absent or malformed.
    pub fn load(path: &Path) -> Result<Self, RuntimeTokenError> {
        let value = fs::read_to_string(path).map_err(|source| RuntimeTokenError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(path, &value)
    }

    /// Loads the token, or atomically creates it with a supplied secure generator.
    ///
    /// # Errors
    ///
    /// Returns a filesystem, generation, or token validation error.
    pub fn load_or_create(
        path: &Path,
        generate: impl FnOnce() -> io::Result<String>,
    ) -> Result<Self, RuntimeTokenError> {
        match Self::load(path) {
            Ok(token) => return Ok(token),
            Err(RuntimeTokenError::Read { source, .. })
                if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| RuntimeTokenError::CreateDirectory {
                path: parent.to_owned(),
                source,
            })?;
        }
        let value = generate().map_err(RuntimeTokenError::Generate)?;
        let token = Self::parse(path, &value)?;
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                file.write_all(token.0.as_bytes())
                    .and_then(|()| file.write_all(b"\n"))
                    .and_then(|()| file.sync_all())
                    .map_err(|source| RuntimeTokenError::Write {
                        path: path.to_owned(),
                        source,
                    })?;
                Ok(token)
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => Self::load(path),
            Err(source) => Err(RuntimeTokenError::Write {
                path: path.to_owned(),
                source,
            }),
        }
    }

    #[must_use]
    pub fn bearer_matches(&self, candidate: &str) -> bool {
        constant_time_equal(self.0.as_bytes(), candidate.as_bytes())
    }

    #[must_use]
    pub fn expose_for_authorization_header(&self) -> &str {
        &self.0
    }

    fn parse(path: &Path, value: &str) -> Result<Self, RuntimeTokenError> {
        let value = value.trim();
        if value.len() != TOKEN_LENGTH
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(RuntimeTokenError::Invalid {
                path: path.to_owned(),
            });
        }
        Ok(Self(value.to_owned()))
    }
}

impl fmt::Debug for RuntimeToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RuntimeToken([REDACTED])")
    }
}

#[must_use]
pub fn resolve_token_path(config_path: &Path, configured_path: &Path) -> PathBuf {
    let mut resolved = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_owned();
    for component in configured_path.components() {
        if let Component::Normal(component) = component {
            resolved.push(component);
        }
    }
    resolved
}

fn constant_time_equal(expected: &[u8], candidate: &[u8]) -> bool {
    let mut difference = expected.len() ^ candidate.len();
    let maximum = expected.len().max(candidate.len());
    for index in 0..maximum {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = candidate.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

#[derive(Debug)]
pub enum RuntimeTokenError {
    Read { path: PathBuf, source: io::Error },
    CreateDirectory { path: PathBuf, source: io::Error },
    Generate(io::Error),
    Write { path: PathBuf, source: io::Error },
    Invalid { path: PathBuf },
}

impl fmt::Display for RuntimeTokenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "failed to read token {}: {source}",
                    path.display()
                )
            }
            Self::CreateDirectory { path, source } => write!(
                formatter,
                "failed to create runtime directory {}: {source}",
                path.display()
            ),
            Self::Generate(source) => {
                write!(formatter, "failed to generate runtime token: {source}")
            }
            Self::Write { path, source } => {
                write!(
                    formatter,
                    "failed to write token {}: {source}",
                    path.display()
                )
            }
            Self::Invalid { path } => write!(
                formatter,
                "runtime token {} is not a 256-bit lowercase hexadecimal token",
                path.display()
            ),
        }
    }
}

impl std::error::Error for RuntimeTokenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. }
            | Self::CreateDirectory { source, .. }
            | Self::Generate(source)
            | Self::Write { source, .. } => Some(source),
            Self::Invalid { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{RuntimeToken, resolve_token_path};

    #[test]
    fn debug_and_comparison_do_not_weaken_token_boundary() {
        let directory =
            std::env::temp_dir().join(format!("aku-supervisor-token-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create token test directory");
        let path = directory.join("control-token");
        fs::remove_file(&path).ok();
        let expected = "a".repeat(64);
        let token = RuntimeToken::load_or_create(&path, || Ok(expected.clone()))
            .expect("create valid token");

        assert!(token.bearer_matches(&expected));
        assert!(!token.bearer_matches(&format!("{expected}x")));
        assert_eq!(format!("{token:?}"), "RuntimeToken([REDACTED])");
        assert!(!format!("{token:?}").contains(&expected));

        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn configured_token_path_uses_native_components() {
        let resolved = resolve_token_path(
            Path::new("C:/config/AkuSupervisor/services.json"),
            Path::new(".runtime/control-token"),
        );

        assert_eq!(
            resolved,
            Path::new("C:/config/AkuSupervisor")
                .join(".runtime")
                .join("control-token")
        );
    }
}
