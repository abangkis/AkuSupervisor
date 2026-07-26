//! Optional cooperative-action composition.
//!
//! The foreground lifecycle host knows only the platform-neutral
//! `CooperativeActionControl` trait. Product-specific adapters are selected in
//! this module so ordinary service supervision and future platform hosts do not
//! acquire a product-specific browser-extension dependency.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::adapters::chrome_extension_reload::ChromeExtensionReloadClient;
use crate::adapters::config::SupervisorConfig;
use crate::application::{CooperativeActionControl, CooperativeActionError};

pub struct CooperativeExtensions {
    pub control: Option<Arc<dyn CooperativeActionControl>>,
    pub audit_path: PathBuf,
}

impl std::fmt::Debug for CooperativeExtensions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CooperativeExtensions")
            .field("control_configured", &self.control.is_some())
            .field("audit_path", &self.audit_path)
            .finish()
    }
}

/// Builds configured cooperative adapters without exposing their concrete
/// implementation to the foreground lifecycle host.
///
/// # Errors
///
/// Returns the selected adapter's typed construction error when its loopback
/// contract or audit sink is invalid.
pub fn build(
    config: &SupervisorConfig,
    runtime_directory: &Path,
    fingerprint: &str,
) -> Result<CooperativeExtensions, CooperativeActionError> {
    let audit_path = runtime_directory.join("cooperative-actions.jsonl");
    let control = config
        .cooperative_actions
        .chrome_extension_reload
        .as_ref()
        .map(|reload| {
            ChromeExtensionReloadClient::new(
                &reload.relay_origin,
                Duration::from_millis(reload.timeout_ms),
                Duration::from_millis(reload.poll_interval_ms),
                &audit_path,
                fingerprint.to_owned(),
                reload.target.clone(),
            )
            .map(|client| Arc::new(client) as Arc<dyn CooperativeActionControl>)
        })
        .transpose()?;
    Ok(CooperativeExtensions {
        control,
        audit_path,
    })
}
