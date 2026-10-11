//! Single-file bundles (TREATY-ISSUES T4), run for real: a plain TypeScript app (no Angular) is built into ONE
//! script and ONE ES module, and each is executed under Node (skipped when Node is not installed). The app
//! exercises what scope hoisting must preserve: a live `let` binding, a re-export, a namespace import, a default
//! export and a dynamic import.

use std::path::{Path, PathBuf};
use std::process::Command;

use treaty_cli::bundle::OutputFormat;
use treaty_cli::native_build::{build, NativeBuildOptions, BUNDLE_FILE};

/// What the fixture prints (the live binding moves from 1 to 2 between the two lines).
const EXPECTED: &str = "[1,2,\"dflt\"]\n[2,4,\"dflt\"]\n";

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bundle-app")
}

fn node_available() -> bool {
    Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn build_as(tag: &str, format: OutputFormat) -> PathBuf {
    let root = fixture_root();
    let out_dir = std::env::temp_dir().join(format!("treaty-bundle-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    build(&NativeBuildOptions {
        root: root.clone(),
        entry: root.join("src/main.ts"),
        out_dir: out_dir.clone(),
        format,
    })
    .expect("native bundle build");
    out_dir
}

/// Run `file` with Node and return its stdout, asserting a clean exit.
fn run_node(file: &Path) -> String {
    let output = Command::new("node").arg(file).output().expect("run node");
    assert!(
        output.status.success(),
        "node failed on {}:\n{}",
        file.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn script_bundle_is_one_file_that_runs_and_keeps_live_bindings() {
    let dist = build_as("script", OutputFormat::Script);
    let bundle = dist.join(BUNDLE_FILE);
    let code = std::fs::read_to_string(&bundle).expect("bundle.js written");
    assert!(!code.contains("import "), "a static import survived in the script");
    assert!(!dist.join("_treaty").exists(), "a script bundle writes no per-module files");
    let index = std::fs::read_to_string(dist.join("index.html")).unwrap();
    assert!(index.contains("<script src=\"./bundle.js\"></script>"), "classic script tag: {index}");
    if node_available() {
        // The lazy import logs asynchronously, after the two synchronous lines.
        let stdout = run_node(&bundle);
        assert!(stdout.starts_with(EXPECTED), "stdout: {stdout}");
        assert!(stdout.contains("lazy lazy-loaded"), "dynamic import did not resolve: {stdout}");
    }
    let _ = std::fs::remove_dir_all(&dist);
}

#[test]
fn esm_bundle_is_one_module_that_runs() {
    let dist = build_as("esm", OutputFormat::Esm);
    let bundle = dist.join(BUNDLE_FILE);
    let code = std::fs::read_to_string(&bundle).expect("bundle.js written");
    let export_line = code.lines().rev().find(|l| l.starts_with("export {")).expect("an export line");
    assert!(export_line.contains("label") && export_line.contains("report"), "the entry's exports are kept: {code}");
    if node_available() {
        let module = dist.join("bundle.mjs");
        std::fs::copy(&bundle, &module).unwrap();
        let stdout = run_node(&module);
        assert!(stdout.starts_with(EXPECTED), "stdout: {stdout}");
    }
    let _ = std::fs::remove_dir_all(&dist);
}

#[test]
fn modules_format_still_writes_one_file_per_graph_module() {
    let dist = build_as("modules", OutputFormat::Modules);
    assert!(dist.join("_treaty").is_dir(), "the default output is the module graph");
    assert!(!dist.join(BUNDLE_FILE).exists(), "no bundle file in the modules format");
    let _ = std::fs::remove_dir_all(&dist);
}
