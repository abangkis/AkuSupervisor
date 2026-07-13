//! Opt-in shutdown signal used by the local development watcher.
//!
//! This adapter deliberately exposes no network endpoint. The watcher selects
//! one tightly named local file through an environment variable, and the
//! foreground loop consumes that file as a request to follow its normal,
//! graceful shutdown path.

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Environment variable that enables the development-only shutdown signal.
pub const DEVELOPMENT_SHUTDOWN_FILE_ENV: &str = "AKU_SUPERVISOR_DEV_SHUTDOWN_FILE";

const REQUEST_FILE_NAME: &str = "shutdown-request";
const MAX_REASON_BYTES: u64 = 1_024;
const MAX_REASON_CAPACITY: usize = 1_024;
const DEFAULT_REASON: &str = "source changed";

/// A disabled or tightly scoped local development shutdown signal.
#[derive(Debug)]
pub struct DevelopmentShutdown {
    request_path: Option<PathBuf>,
}

impl DevelopmentShutdown {
    /// Resolves the opt-in development signal from the process environment.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured path is not absolute or does not
    /// use the fixed `shutdown-request` filename.
    pub fn from_environment() -> Result<Self, DevelopmentShutdownError> {
        Self::from_optional_path(env::var_os(DEVELOPMENT_SHUTDOWN_FILE_ENV).map(PathBuf::from))
    }

    fn from_optional_path(request_path: Option<PathBuf>) -> Result<Self, DevelopmentShutdownError> {
        if let Some(path) = &request_path {
            validate_request_path(path)?;
        }
        Ok(Self { request_path })
    }

    /// Returns the configured request-file path when watcher mode is enabled.
    #[must_use]
    pub fn request_path(&self) -> Option<&Path> {
        self.request_path.as_deref()
    }

    /// Consumes one pending shutdown request and returns its bounded reason.
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be inspected, read, or removed,
    /// or if it exceeds the bounded payload size.
    pub fn take_request(&self) -> Result<Option<String>, DevelopmentShutdownError> {
        let Some(path) = &self.request_path else {
            return Ok(None);
        };

        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(DevelopmentShutdownError::Io {
                    path: path.clone(),
                    operation: "inspect",
                    source,
                });
            }
        };
        if metadata.len() > MAX_REASON_BYTES {
            return Err(DevelopmentShutdownError::RequestTooLarge {
                path: path.clone(),
                bytes: metadata.len(),
            });
        }

        let file = fs::File::open(path).map_err(|source| DevelopmentShutdownError::Io {
            path: path.clone(),
            operation: "read",
            source,
        })?;
        let mut payload = Vec::with_capacity(MAX_REASON_CAPACITY);
        file.take(MAX_REASON_BYTES + 1)
            .read_to_end(&mut payload)
            .map_err(|source| DevelopmentShutdownError::Io {
                path: path.clone(),
                operation: "read",
                source,
            })?;
        if payload.len() as u64 > MAX_REASON_BYTES {
            return Err(DevelopmentShutdownError::RequestTooLarge {
                path: path.clone(),
                bytes: payload.len() as u64,
            });
        }
        fs::remove_file(path).map_err(|source| DevelopmentShutdownError::Io {
            path: path.clone(),
            operation: "remove",
            source,
        })?;

        let reason = String::from_utf8_lossy(&payload).trim().to_owned();
        Ok(Some(if reason.is_empty() {
            DEFAULT_REASON.to_owned()
        } else {
            reason
        }))
    }
}

fn validate_request_path(path: &Path) -> Result<(), DevelopmentShutdownError> {
    if !path.is_absolute() || path.file_name() != Some(OsStr::new(REQUEST_FILE_NAME)) {
        return Err(DevelopmentShutdownError::InvalidPath(path.to_owned()));
    }
    Ok(())
}

/// Development shutdown signal configuration or I/O failure.
#[derive(Debug)]
pub enum DevelopmentShutdownError {
    InvalidPath(PathBuf),
    RequestTooLarge {
        path: PathBuf,
        bytes: u64,
    },
    Io {
        path: PathBuf,
        operation: &'static str,
        source: io::Error,
    },
}

impl fmt::Display for DevelopmentShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(path) => write!(
                formatter,
                "{DEVELOPMENT_SHUTDOWN_FILE_ENV} must be an absolute path ending in \
                 {REQUEST_FILE_NAME}: {}",
                path.display()
            ),
            Self::RequestTooLarge { path, bytes } => write!(
                formatter,
                "development shutdown request {} is {bytes} bytes; maximum is \
                 {MAX_REASON_BYTES}",
                path.display()
            ),
            Self::Io {
                path,
                operation,
                source,
            } => write!(
                formatter,
                "failed to {operation} development shutdown request {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for DevelopmentShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidPath(_) | Self::RequestTooLarge { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DevelopmentShutdown, DevelopmentShutdownError};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn signal_is_disabled_without_a_path() {
        let signal = DevelopmentShutdown::from_optional_path(None).expect("disabled signal");
        assert!(signal.request_path().is_none());
        assert!(
            signal
                .take_request()
                .expect("poll disabled signal")
                .is_none()
        );
    }

    #[test]
    fn signal_rejects_unbounded_or_ambiguous_paths() {
        assert!(matches!(
            DevelopmentShutdown::from_optional_path(Some("shutdown-request".into())),
            Err(DevelopmentShutdownError::InvalidPath(_))
        ));
        assert!(matches!(
            DevelopmentShutdown::from_optional_path(Some(
                std::env::temp_dir().join("different-name")
            )),
            Err(DevelopmentShutdownError::InvalidPath(_))
        ));
    }

    #[test]
    fn signal_consumes_one_bounded_reason() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "aku-supervisor-development-shutdown-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("shutdown-request");
        fs::write(&path, "source changed: src/main.rs\n").expect("write request");
        let signal = DevelopmentShutdown::from_optional_path(Some(path.clone()))
            .expect("valid shutdown signal");

        assert_eq!(
            signal.take_request().expect("consume request").as_deref(),
            Some("source changed: src/main.rs")
        );
        assert!(!path.exists());
        assert!(
            signal
                .take_request()
                .expect("poll consumed signal")
                .is_none()
        );

        fs::remove_dir(directory).expect("remove test directory");
    }
}
