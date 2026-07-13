use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn domain_and_application_do_not_import_operating_system_adapters() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for relative in ["src/domain", "src/application"] {
        inspect_rust_files(&root.join(relative));
    }
}

#[test]
fn each_planned_operating_system_has_an_explicit_adapter_boundary() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "src/platform/windows/mod.rs",
        "src/platform/linux/mod.rs",
        "src/platform/macos/mod.rs",
    ] {
        assert!(root.join(relative).is_file(), "missing {relative}");
    }
}

fn inspect_rust_files(directory: &Path) {
    for entry in fs::read_dir(directory).expect("read architecture layer") {
        let path = entry.expect("read architecture entry").path();
        if path.is_dir() {
            inspect_rust_files(&path);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path).expect("read Rust source");
            for forbidden in ["platform::windows", "windows_sys", "std::os::windows"] {
                assert!(
                    !source.contains(forbidden),
                    "{} imports forbidden OS detail {forbidden}",
                    path.display()
                );
            }
        }
    }
}
