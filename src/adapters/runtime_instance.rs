//! Portable single-instance identity and development-watcher coordination.
//!
//! File locks are process-owned and released by the operating system even
//! after an abrupt process exit. Persisted JSON remains available for
//! diagnostics, while the lock itself is the authority.

use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const WATCHER_ID_ENVIRONMENT: &str = "AKU_SUPERVISOR_WATCHER_ID";
pub const WATCHER_PID_ENVIRONMENT: &str = "AKU_SUPERVISOR_WATCHER_PID";
pub const INSTANCE_FILE_NAME: &str = "supervisor-instance.json";
pub const INSTANCE_LEASE_FILE_NAME: &str = "supervisor-instance.lock";
pub const WATCHER_LEASE_FILE_NAME: &str = "development-watcher.lock";
pub const WATCHER_IDENTITY_FILE_NAME: &str = "development-watcher.json";

/// How the foreground Supervisor was launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    Stable,
    DevelopmentWatcher,
}

/// Public, non-secret identity of one foreground Supervisor process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInstance {
    pub schema_version: u32,
    pub instance_id: String,
    pub process_id: u32,
    pub executable: PathBuf,
    pub mode: RuntimeMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watcher_process_id: Option<u32>,
    pub started_at_unix_ms: u64,
    pub version: String,
    pub config_fingerprint: String,
    pub control_api: String,
}

impl RuntimeInstance {
    /// Constructs an identity from the active process and validated config.
    ///
    /// # Errors
    ///
    /// Returns an executable-path lookup error or malformed watcher identity.
    pub fn current(
        mode: RuntimeMode,
        config_fingerprint: String,
        control_host: &str,
        control_port: u16,
    ) -> Result<Self, RuntimeInstanceError> {
        let process_id = std::process::id();
        let started_at_unix_ms = unix_time_ms();
        let watcher_process_id = match mode {
            RuntimeMode::Stable => None,
            RuntimeMode::DevelopmentWatcher => Some(
                env::var(WATCHER_PID_ENVIRONMENT)
                    .ok()
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|value| *value > 0)
                    .ok_or(RuntimeInstanceError::MissingWatcherAuthority)?,
            ),
        };
        Ok(Self {
            schema_version: 1,
            instance_id: format!("{process_id}-{}", unix_time_ns()),
            process_id,
            executable: env::current_exe().map_err(RuntimeInstanceError::CurrentExecutable)?,
            mode,
            watcher_process_id,
            started_at_unix_ms,
            version: crate::VERSION.to_owned(),
            config_fingerprint,
            control_api: format!("http://{control_host}:{control_port}"),
        })
    }
}

/// Exclusive runtime ownership retained for the lifetime of one Supervisor.
#[derive(Debug)]
pub struct RuntimeInstanceLease {
    identity: RuntimeInstance,
    path: PathBuf,
    _lock: File,
}

impl RuntimeInstanceLease {
    /// Acquires watcher authority and the single-Supervisor instance lock.
    ///
    /// # Errors
    ///
    /// Returns structured ownership or filesystem failures.
    pub fn acquire(
        runtime_directory: &Path,
        identity: RuntimeInstance,
    ) -> Result<Self, RuntimeInstanceError> {
        fs::create_dir_all(runtime_directory).map_err(|source| {
            RuntimeInstanceError::CreateDirectory {
                path: runtime_directory.to_owned(),
                source,
            }
        })?;
        verify_watcher_authority(runtime_directory, &identity)?;

        let path = runtime_directory.join(INSTANCE_FILE_NAME);
        let lock_path = runtime_directory.join(INSTANCE_LEASE_FILE_NAME);
        let lock = open_lock_file(&lock_path)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(RuntimeInstanceError::AlreadyRunning {
                    path: lock_path,
                    owner: File::open(&path)
                        .ok()
                        .and_then(|mut identity| read_identity(&mut identity).ok())
                        .map(Box::new),
                });
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(RuntimeInstanceError::Lock {
                    path: lock_path,
                    source,
                });
            }
        }
        let mut identity_file = open_lock_file(&path)?;
        write_json(&mut identity_file, &path, &identity)?;
        Ok(Self {
            identity,
            path,
            _lock: lock,
        })
    }

    #[must_use]
    pub const fn identity(&self) -> &RuntimeInstance {
        &self.identity
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn verify_watcher_authority(
    runtime_directory: &Path,
    identity: &RuntimeInstance,
) -> Result<(), RuntimeInstanceError> {
    let path = runtime_directory.join(WATCHER_LEASE_FILE_NAME);
    let file = open_lock_file(&path)?;
    match file.try_lock() {
        Ok(()) if identity.mode == RuntimeMode::Stable => Ok(()),
        Ok(()) => Err(RuntimeInstanceError::MissingWatcherAuthority),
        Err(std::fs::TryLockError::WouldBlock) => {
            let watcher_path = runtime_directory.join(WATCHER_IDENTITY_FILE_NAME);
            let mut watcher_file =
                File::open(&watcher_path).map_err(|source| RuntimeInstanceError::ReadWatcher {
                    path: watcher_path.clone(),
                    source,
                })?;
            let watcher = read_watcher_identity(&mut watcher_file).map_err(|source| {
                RuntimeInstanceError::ReadWatcher {
                    path: watcher_path,
                    source,
                }
            })?;
            let supplied_watcher_id = env::var(WATCHER_ID_ENVIRONMENT).ok();
            let authorized_process = identity.watcher_process_id;
            if identity.mode == RuntimeMode::DevelopmentWatcher
                && supplied_watcher_id.as_deref() == Some(watcher.watcher_id.as_str())
                && authorized_process == Some(watcher.watcher_process_id)
            {
                Ok(())
            } else {
                Err(RuntimeInstanceError::WatcherActive {
                    path,
                    watcher_process_id: watcher.watcher_process_id,
                })
            }
        }
        Err(std::fs::TryLockError::Error(source)) => {
            Err(RuntimeInstanceError::Lock { path, source })
        }
    }
}

fn open_lock_file(path: &Path) -> Result<File, RuntimeInstanceError> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|source| RuntimeInstanceError::Open {
            path: path.to_owned(),
            source,
        })
}

fn write_json(
    file: &mut File,
    path: &Path,
    value: &impl Serialize,
) -> Result<(), RuntimeInstanceError> {
    let encoded = serde_json::to_vec_pretty(value).map_err(RuntimeInstanceError::Serialize)?;
    file.set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| file.write_all(&encoded))
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.flush())
        .map_err(|source| RuntimeInstanceError::Write {
            path: path.to_owned(),
            source,
        })
}

fn read_identity(file: &mut File) -> Result<RuntimeInstance, io::Error> {
    read_json(file)
}

fn read_watcher_identity(file: &mut File) -> Result<WatcherIdentity, io::Error> {
    read_json(file)
}

fn read_json<Value: for<'de> Deserialize<'de>>(file: &mut File) -> Result<Value, io::Error> {
    file.seek(SeekFrom::Start(0))?;
    let mut encoded = String::new();
    file.read_to_string(&mut encoded)?;
    serde_json::from_str(&encoded).map_err(io::Error::other)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WatcherIdentity {
    watcher_id: String,
    watcher_process_id: u32,
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn unix_time_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

/// Runtime instance ownership or metadata failure.
#[derive(Debug)]
pub enum RuntimeInstanceError {
    CurrentExecutable(io::Error),
    MissingWatcherAuthority,
    CreateDirectory {
        path: PathBuf,
        source: io::Error,
    },
    Open {
        path: PathBuf,
        source: io::Error,
    },
    Lock {
        path: PathBuf,
        source: io::Error,
    },
    ReadWatcher {
        path: PathBuf,
        source: io::Error,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Serialize(serde_json::Error),
    AlreadyRunning {
        path: PathBuf,
        owner: Option<Box<RuntimeInstance>>,
    },
    WatcherActive {
        path: PathBuf,
        watcher_process_id: u32,
    },
}

impl fmt::Display for RuntimeInstanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentExecutable(error) => {
                write!(formatter, "failed to resolve current executable: {error}")
            }
            Self::MissingWatcherAuthority => {
                formatter.write_str("development mode requires a matching active watcher lease")
            }
            Self::CreateDirectory { path, source } => {
                write!(formatter, "failed to create {}: {source}", path.display())
            }
            Self::Open { path, source } => {
                write!(
                    formatter,
                    "failed to open runtime lease {}: {source}",
                    path.display()
                )
            }
            Self::Lock { path, source } => {
                write!(
                    formatter,
                    "failed to lock runtime lease {}: {source}",
                    path.display()
                )
            }
            Self::ReadWatcher { path, source } => write!(
                formatter,
                "failed to read active watcher identity {}: {source}",
                path.display()
            ),
            Self::Write { path, source } => {
                write!(
                    formatter,
                    "failed to write runtime identity {}: {source}",
                    path.display()
                )
            }
            Self::Serialize(error) => {
                write!(formatter, "failed to serialize runtime identity: {error}")
            }
            Self::AlreadyRunning {
                path,
                owner: Some(owner),
            } => write!(
                formatter,
                "another AkuSupervisor instance is active: PID {} {} ({:?}); identity: {}",
                owner.process_id,
                owner.executable.display(),
                owner.mode,
                path.display()
            ),
            Self::AlreadyRunning { path, owner: None } => write!(
                formatter,
                "another AkuSupervisor instance holds {}; its identity could not be read",
                path.display()
            ),
            Self::WatcherActive {
                path,
                watcher_process_id,
            } => write!(
                formatter,
                "development watcher PID {watcher_process_id} owns {}; start Supervisor through that watcher",
                path.display()
            ),
        }
    }
}

impl std::error::Error for RuntimeInstanceError {}

#[cfg(test)]
mod tests {
    use super::{RuntimeInstance, RuntimeInstanceError, RuntimeInstanceLease, RuntimeMode};

    fn directory(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "aku-supervisor-instance-{name}-{}",
            std::process::id()
        ))
    }

    fn identity(label: &str) -> RuntimeInstance {
        RuntimeInstance {
            schema_version: 1,
            instance_id: label.to_owned(),
            process_id: std::process::id(),
            executable: "aku-supervisor".into(),
            mode: RuntimeMode::Stable,
            watcher_process_id: None,
            started_at_unix_ms: 1,
            version: "test".to_owned(),
            config_fingerprint: "sha256:test".to_owned(),
            control_api: "http://127.0.0.1:1".to_owned(),
        }
    }

    #[test]
    fn lease_is_exclusive_and_reclaims_after_owner_drop() {
        let directory = directory("exclusive");
        std::fs::create_dir_all(&directory).expect("create runtime directory");
        let first = RuntimeInstanceLease::acquire(&directory, identity("first"))
            .expect("acquire first lease");
        let conflict = RuntimeInstanceLease::acquire(&directory, identity("second"))
            .expect_err("second lease must fail");
        assert!(matches!(
            conflict,
            RuntimeInstanceError::AlreadyRunning {
                owner: Some(owner),
                ..
            } if owner.instance_id == "first"
        ));
        drop(first);
        let second = RuntimeInstanceLease::acquire(&directory, identity("second"))
            .expect("reclaim released lease");
        assert_eq!(second.identity().instance_id, "second");
        drop(second);
        std::fs::remove_dir_all(directory).ok();
    }
}
