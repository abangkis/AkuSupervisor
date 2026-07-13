use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

const MAX_TAIL_LINES: usize = 1_000;
const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stdout" => Some(Self::Stdout),
            "stderr" => Some(Self::Stderr),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceLogTail {
    pub service_id: String,
    pub stream: LogStream,
    pub lines: Vec<String>,
    pub truncated_to: usize,
}

#[derive(Debug, Clone)]
pub struct ServiceLogStore {
    paths: BTreeMap<String, ServiceLogPaths>,
}

impl ServiceLogStore {
    #[must_use]
    pub fn new(
        runtime_services_directory: &Path,
        service_ids: impl IntoIterator<Item = String>,
    ) -> Self {
        let paths = service_ids
            .into_iter()
            .map(|service_id| {
                let paths = ServiceLogPaths {
                    stdout: runtime_services_directory.join(format!("{service_id}.stdout.log")),
                    stderr: runtime_services_directory.join(format!("{service_id}.stderr.log")),
                };
                (service_id, paths)
            })
            .collect();
        Self { paths }
    }

    /// Reads at most the requested number of lines from the active log file.
    ///
    /// # Errors
    ///
    /// Returns an unknown-service, metadata, or bounded-read error.
    pub fn tail(
        &self,
        service_id: &str,
        stream: LogStream,
        lines: usize,
    ) -> Result<ServiceLogTail, ServiceLogError> {
        let paths = self
            .paths
            .get(service_id)
            .ok_or_else(|| ServiceLogError::ServiceNotFound(service_id.to_owned()))?;
        let path = match stream {
            LogStream::Stdout => &paths.stdout,
            LogStream::Stderr => &paths.stderr,
        };
        let lines = lines.clamp(1, MAX_TAIL_LINES);
        let bytes = match fs::metadata(path) {
            Ok(metadata) if metadata.len() > MAX_READ_BYTES => {
                return Err(ServiceLogError::Oversized {
                    path: path.clone(),
                    bytes: metadata.len(),
                });
            }
            Ok(_) => fs::read(path).map_err(|source| ServiceLogError::Read {
                path: path.clone(),
                source,
            })?,
            Err(source) if source.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(source) => {
                return Err(ServiceLogError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut tail = text
            .lines()
            .rev()
            .take(lines)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        tail.reverse();
        Ok(ServiceLogTail {
            service_id: service_id.to_owned(),
            stream,
            lines: tail,
            truncated_to: lines,
        })
    }
}

#[derive(Debug, Clone)]
struct ServiceLogPaths {
    stdout: PathBuf,
    stderr: PathBuf,
}

#[derive(Debug)]
pub enum ServiceLogError {
    ServiceNotFound(String),
    Read { path: PathBuf, source: io::Error },
    Oversized { path: PathBuf, bytes: u64 },
}

impl fmt::Display for ServiceLogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServiceNotFound(service_id) => write!(formatter, "unknown service: {service_id}"),
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "failed to read service log {}: {source}",
                    path.display()
                )
            }
            Self::Oversized { path, bytes } => write!(
                formatter,
                "service log {} exceeds bounded read size: {bytes} bytes",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ServiceLogError {}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{LogStream, ServiceLogStore};

    #[test]
    fn tail_is_ordered_and_bounded() {
        let directory =
            std::env::temp_dir().join(format!("aku-supervisor-log-tail-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create log directory");
        fs::write(directory.join("api.stdout.log"), "one\ntwo\nthree\n")
            .expect("write log fixture");
        let store = ServiceLogStore::new(&directory, ["api".to_owned()]);

        let tail = store
            .tail("api", LogStream::Stdout, 2)
            .expect("read log tail");

        assert_eq!(tail.lines, ["two", "three"]);
        assert!(store.tail("unknown", LogStream::Stdout, 2).is_err());
        fs::remove_dir_all(directory).ok();
    }
}
