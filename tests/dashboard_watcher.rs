#![cfg(windows)]

use std::fs;
use std::process::Command;

#[test]
fn embedded_asset_edits_change_the_watcher_fingerprint_without_starting_services() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("dashboard-watcher-fixture")
        .join(std::process::id().to_string());
    fs::create_dir_all(root.join("dashboard")).expect("create isolated fixture");
    fs::create_dir_all(root.join("src")).expect("create source fixture");
    for asset in [
        "index.html",
        "app.css",
        "app.js",
        "model.js",
        "view.js",
        "api.js",
    ] {
        fs::write(root.join("dashboard").join(asset), "initial").expect("write fixture asset");
    }
    fs::write(
        root.join("dev-source.ps1"),
        include_str!("../scripts/dev.ps1"),
    )
    .expect("write watcher source as data");
    fs::write(root.join("harness.ps1"), HARNESS).expect("write fingerprint-only harness");
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(root.join("harness.ps1"))
        .env("TEMP", &root)
        .env("TMP", &root)
        .output()
        .expect("run isolated fingerprint harness");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("All embedded asset edits detected"));
}

const HARNESS: &str = r"
$ErrorActionPreference = 'Stop'
$repository = $PSScriptRoot
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile((Join-Path $repository 'dev-source.ps1'), [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'Watcher source did not parse' }
$function = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Get-WatchFingerprint' }, $true)
if ($null -eq $function) { throw 'Missing fingerprint function' }
# Execute only this pure file-inspection function. Never dot-source the watcher entrypoint.
. ([scriptblock]::Create($function.Extent.Text))
$script:gitIdentityPaths = @()
foreach ($asset in @('index.html', 'app.css', 'app.js', 'model.js', 'view.js', 'api.js')) {
    $before = Get-WatchFingerprint
    if ($before -notlike ('*dashboard\' + $asset + '*')) { throw ('Asset absent from fingerprint: ' + $asset) }
    Add-Content -LiteralPath (Join-Path $repository ('dashboard\' + $asset)) -Value 'edited asset'
    $after = Get-WatchFingerprint
    if ($before -ceq $after) { throw ('Embedded asset edit was ignored: ' + $asset) }
}
'All embedded asset edits detected'
";
