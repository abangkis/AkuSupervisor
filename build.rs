use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=dashboard");

    let repository = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_owned());
    watch_git_identity(&repository);

    let commit = git_output(&repository, &["rev-parse", "--short=7", "HEAD"])
        .filter(|value| is_commit_id(value))
        .unwrap_or_else(|| "unknown".to_owned());
    let dirty = git_output(
        &repository,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .is_some_and(|value| !value.is_empty());
    let provenance = if dirty && commit != "unknown" {
        format!("{commit}-dirty")
    } else {
        commit
    };
    println!("cargo:rustc-env=AKU_SUPERVISOR_BUILD_COMMIT={provenance}");
}

fn watch_git_identity(repository: &str) {
    let mut git_paths = vec!["HEAD".to_owned()];
    if let Some(reference) = git_output(repository, &["symbolic-ref", "-q", "HEAD"]) {
        git_paths.push(reference);
    }
    for git_path in git_paths {
        if let Some(path) = git_output(repository, &["rev-parse", "--git-path", &git_path]) {
            let path = Path::new(repository).join(path);
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn git_output(repository: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn is_commit_id(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
