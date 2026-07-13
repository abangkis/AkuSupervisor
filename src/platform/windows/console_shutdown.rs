#![allow(unsafe_code)]

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};

static INSTALLED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Process-wide Windows console interruption observer.
///
/// Only one observer may be installed because Windows console handlers are
/// process-wide. Both Ctrl+C and Ctrl+Break set a lock-free flag; lifecycle
/// cleanup remains in normal application code rather than inside the handler.
#[derive(Debug)]
pub struct ConsoleShutdown {
    installed: bool,
}

impl ConsoleShutdown {
    /// Installs the process-wide console handler.
    ///
    /// # Errors
    ///
    /// Returns [`ConsoleShutdownError::AlreadyInstalled`] if another observer
    /// is active, or [`ConsoleShutdownError::Install`] if Windows rejects the
    /// handler registration.
    pub fn install() -> Result<Self, ConsoleShutdownError> {
        if INSTALLED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ConsoleShutdownError::AlreadyInstalled);
        }

        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        if unsafe { SetConsoleCtrlHandler(Some(console_handler), 1) } == 0 {
            INSTALLED.store(false, Ordering::Release);
            return Err(ConsoleShutdownError::Install(io::Error::last_os_error()));
        }

        Ok(Self { installed: true })
    }

    /// Returns whether Ctrl+C or Ctrl+Break has requested shutdown.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        SHUTDOWN_REQUESTED.load(Ordering::Acquire)
    }
}

impl Drop for ConsoleShutdown {
    fn drop(&mut self) {
        if self.installed {
            let removed = unsafe { SetConsoleCtrlHandler(Some(console_handler), 0) } != 0;
            if removed {
                self.installed = false;
                INSTALLED.store(false, Ordering::Release);
                SHUTDOWN_REQUESTED.store(false, Ordering::Release);
            }
        }
    }
}

unsafe extern "system" fn console_handler(control_type: u32) -> i32 {
    if matches!(control_type, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        SHUTDOWN_REQUESTED.store(true, Ordering::Release);
        1
    } else {
        0
    }
}

/// Console handler installation failure.
#[derive(Debug)]
pub enum ConsoleShutdownError {
    AlreadyInstalled,
    Install(io::Error),
}

impl fmt::Display for ConsoleShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInstalled => {
                formatter.write_str("a console shutdown observer is already installed")
            }
            Self::Install(error) => write!(formatter, "failed to install console handler: {error}"),
        }
    }
}

impl std::error::Error for ConsoleShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Install(error) => Some(error),
            Self::AlreadyInstalled => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CTRL_C_EVENT, ConsoleShutdown, console_handler};

    #[test]
    fn ctrl_c_handler_only_sets_shutdown_flag() {
        let shutdown = ConsoleShutdown::install().expect("install console handler");
        assert!(!shutdown.is_requested());

        let handled = unsafe { console_handler(CTRL_C_EVENT) };

        assert_eq!(handled, 1);
        assert!(shutdown.is_requested());
    }
}
