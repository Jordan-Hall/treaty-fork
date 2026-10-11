//! Native build of a fixture that imports LIBRARY declarations and uses an external template (TREATY-ISSUES
//! T1, T2, T3, T9).
//!
//! The fixture (`tests/fixtures/library-scope`) is a standalone component that imports pipes, NgModules and
//! directives from `@angular/common` and `@angular/forms`, plus a project-local component with `templateUrl`
//! and `styleUrl`. The `@angular/*` packages in the fixture are tiny stand-ins whose chunks carry the published
//! partial-declaration shapes (`ɵɵngDeclarePipe`, `ɵɵngDeclareDirective`, `ɵɵngDeclareNgModule`) and the
//! re-export layout of a real FESM entry point. No network and no real Angular install is needed: the native
//! build resolves and links these packages, it never runs them.

use std::path::{Path, PathBuf};

use treaty_cli::native_build::{build, NativeBuildOptions};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/library-scope")
}

/// Build the fixture into a fresh directory and return it.
fn build_fixture(tag: &str) -> (PathBuf, Vec<String>) {
    let root = fixture_root();
    let out_dir = std::env::temp_dir().join(format!("treaty-libscope-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    let out = build(&NativeBuildOptions {
        root: root.clone(),
        entry: root.join("src/main.ts"),
        out_dir: out_dir.clone(),
        format: treaty_cli::bundle::OutputFormat::Modules,
    })
    .expect("native build of the library-scope fixture");
    (out_dir, out.notes)
}

/// The emitted module whose file name starts with `stem-` (the native build names files `<stem>-<hash>.js`).
fn module_source(dist: &Path, stem: &str) -> String {
    let modules = dist.join("_treaty");
    let file = std::fs::read_dir(&modules)
        .expect("dist/_treaty exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with(&format!("{stem}-")))
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no emitted module for {stem} in {}", modules.display()));
    std::fs::read_to_string(file).unwrap()
}

/// The names of the `dependencies: [...]` array of the component the module defines, in emitted order.
fn dependencies(code: &str) -> Vec<String> {
    let start = code
        .find("dependencies: [")
        .unwrap_or_else(|| panic!("no dependencies array in:\n{code}"));
    let rest = &code[start + "dependencies: [".len()..];
    let end = rest.find(']').expect("closing bracket");
    rest[..end]
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// T3/T9: the library directives, NgModule expansions, pipes and project-local directives a standalone component
/// uses are listed in `dependencies`; a plain class with the same name as a directive's attribute is not.
#[test]
fn library_imports_resolve_to_the_dependencies_angular_lists() {
    let (dist, _) = build_fixture("deps");
    let app = module_source(&dist, "app");
    let deps = dependencies(&app);

    let mut sorted = deps.clone();
    sorted.sort();
    let mut expected = vec![
        "AsyncPipe",
        "DecimalPipe",
        "DefaultValueAccessor",
        "FormControlDirective",
        "Hello",
        "NgModel",
        "UpperCasePipe",
    ];
    expected.sort();
    assert_eq!(sorted, expected, "dependencies of App:\n{app}");

    // Angular lists the directives before the pipes.
    let first_pipe = deps.iter().position(|d| d.ends_with("Pipe")).expect("a pipe");
    assert!(deps[..first_pipe].iter().all(|d| !d.ends_with("Pipe")), "directives first: {deps:?}");

    // The NgModule-expanded entities are not named in the source, so the module imports them itself.
    assert!(
        app.contains("DefaultValueAccessor") && app.lines().any(|l| l.starts_with("import ") && l.contains("DefaultValueAccessor")),
        "DefaultValueAccessor needs an import line:\n{app}"
    );
    let _ = std::fs::remove_dir_all(&dist);
}

/// T1: a `templateUrl` is read relative to the component's file and its `styleUrl` is compiled (SCSS) into the
/// component's styles.
#[test]
fn external_template_and_style_are_read_from_disk() {
    let (dist, _) = build_fixture("external");
    let hello = module_source(&dist, "hello");
    assert!(hello.contains("hello from file"), "templateUrl content missing:\n{hello}");
    // The SCSS nesting is compiled (`.hello .inner`) and scoped by the Emulated encapsulation Angular applies.
    assert!(
        hello.contains(".inner[_ngcontent-%COMP%]{color:red}"),
        "styleUrl (SCSS) not compiled and scoped into the component's styles:\n{hello}"
    );
    let _ = std::fs::remove_dir_all(&dist);
}

/// T2: the AOT-health counter reports no residual partial CALLS for a build whose library partials are linked.
#[test]
fn aot_health_counts_no_residual_partial_calls() {
    let (dist, notes) = build_fixture("health");
    let health = notes
        .iter()
        .find(|n| n.starts_with("AOT health"))
        .unwrap_or_else(|| panic!("no AOT health note: {notes:?}"));
    assert!(health.contains("residualNgDeclare=0"), "unexpected: {health}");
    let _ = std::fs::remove_dir_all(&dist);
}
