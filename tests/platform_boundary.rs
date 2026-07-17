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

#[test]
fn shared_control_plane_does_not_import_windows_implementation_details() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "src/adapters/control_http.rs",
        "src/adapters/foreground.rs",
        "src/adapters/runtime_token.rs",
        "src/application/control_api.rs",
        "src/cli.rs",
    ] {
        let path = root.join(relative);
        let source = fs::read_to_string(&path).expect("read shared control-plane source");
        for forbidden in [
            "platform::windows",
            "windows_sys",
            "std::os::windows",
            "BCryptGenRandom",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} imports forbidden Windows detail {forbidden}",
                path.display()
            );
        }
    }
}

#[test]
fn foreground_host_does_not_construct_product_specific_cooperative_adapters() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("src/adapters/foreground.rs");
    let source = fs::read_to_string(&path).expect("read foreground host source");
    for forbidden in ["AkuBridgeReloadClient", "adapters::aku_bridge_reload"] {
        assert!(
            !source.contains(forbidden),
            "{} imports product-specific adapter {forbidden}",
            path.display()
        );
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
