//! macOS adapters for the platform-neutral application ports.
//!
//! A future implementation should use process groups for bounded signalling
//! and an OS-native exit observer such as `kqueue`. It must prove descendant
//! ownership independently rather than assuming Linux `pidfd` or cgroup
//! facilities exist. No macOS lifecycle backend is implemented yet.

/// Atomically replaces a file when source and destination share a filesystem.
pub fn atomic_replace_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}
