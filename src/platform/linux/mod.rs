//! Linux adapters for the platform-neutral application ports.
//!
//! This boundary is intentionally separate from macOS. A future implementation
//! should evaluate process groups plus `pidfd`, subreaper behavior, or a scoped
//! cgroup before claiming the same ownership guarantees as the Windows Job
//! Object adapter. No Linux lifecycle backend is implemented yet.

/// Atomically replaces a file when source and destination share a filesystem.
pub fn atomic_replace_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}
