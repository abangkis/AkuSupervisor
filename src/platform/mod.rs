//! Operating-system integration boundary.

#[cfg(windows)]
pub mod host;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
pub use linux::atomic_replace_file;
#[cfg(target_os = "macos")]
pub use macos::atomic_replace_file;
#[cfg(windows)]
pub use windows::atomic_replace_file;

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn atomic_replace_file(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}
