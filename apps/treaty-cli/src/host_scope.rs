//! The HOST side of the compiler's per-file inputs (TREATY-ISSUES T1 and T3/T9).
//!
//! `treaty_ivy` never reads a file. For each source file the host supplies what the compile needs from disk:
//!
//! * the file's LIBRARY scope: the directives, components, pipes and NgModules its imports bring in from
//!   linked npm packages, read from their published partial declarations ([`treaty_ivy::library_scope`]).
//!   The [`FsModuleLoader`] implemented here resolves and reads those packages with the same resolver the
//!   module graph uses, so the scope and the emitted imports agree on every file;
//! * the external content of its components: each `templateUrl` (read relative to the source file), and each
//!   stylesheet of `styleUrls` / `styleUrl`, preprocessed by the packager's stylesheet resolver (SCSS and
//!   Sass through `grass`, plain CSS as is), keyed by class name ([`treaty_ivy::source_compile::ResolvedContentMap`]).
//!
//! A missing `templateUrl` or stylesheet is an error, as `ng build` reports it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use treaty_ivy::library_scope::{
    library_scope_for_source, LibraryIndex, LibraryScope, LoadedModule, ModuleLoader,
};
use treaty_ivy::source_compile::{external_resources, ResolvedComponentContent, ResolvedContentMap};

/// Reads the packages a source file imports from disk, resolving specifiers the way the module graph does.
/// Results are cached per `(importing file, specifier)`, so every file of one build reads each module once.
pub struct FsModuleLoader {
    resolver: crate::resolve::ModuleResolver,
    cache: RefCell<HashMap<(String, String), Option<LoadedModule>>>,
}

impl FsModuleLoader {
    pub fn new() -> Self {
        FsModuleLoader {
            resolver: crate::resolve::ModuleResolver::new(),
            cache: RefCell::new(HashMap::new()),
        }
    }
}

impl Default for FsModuleLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleLoader for FsModuleLoader {
    fn load(&self, from: &str, specifier: &str) -> Option<LoadedModule> {
        let key = (from.to_string(), specifier.to_string());
        if let Some(hit) = self.cache.borrow().get(&key) {
            return hit.clone();
        }
        let loaded = Path::new(from).parent().and_then(|dir| {
            let path = self.resolver.resolve(dir, specifier)?;
            let code = std::fs::read_to_string(&path).ok()?;
            Some(LoadedModule {
                path: path.to_string_lossy().replace('\\', "/"),
                code,
            })
        });
        self.cache.borrow_mut().insert(key, loaded.clone());
        loaded
    }
}

/// Everything the host resolves for one source file.
#[derive(Debug, Default)]
pub struct FileHost {
    pub library: LibraryScope,
    pub resolved: ResolvedContentMap,
    /// Non-fatal notes from the stylesheet resolver (for example a Less file passed through as text).
    pub notes: Vec<String>,
    /// The external files whose contents the compile used (templates and stylesheets), absolute. A dev server
    /// rebuilds the module when any of them changes.
    pub inputs: Vec<PathBuf>,
}

/// Resolve the host inputs of `source`, the file at `file`. `index` and `loader` are shared by every file of a
/// build (they cache parsed packages and resolved specifiers).
pub fn host_for_file(
    source: &str,
    file: &Path,
    index: &LibraryIndex,
    loader: &FsModuleLoader,
) -> Result<FileHost, String> {
    let library = library_scope_for_source(source, &file.to_string_lossy().replace('\\', "/"), index, loader);
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    let (resolved, notes, inputs) = external_content(source, dir)?;
    Ok(FileHost { library, resolved, notes, inputs })
}

/// The external template and stylesheet contents of the components in `source`, read relative to `dir`, and
/// the paths they were read from.
pub fn external_content(
    source: &str,
    dir: &Path,
) -> Result<(ResolvedContentMap, Vec<String>, Vec<PathBuf>), String> {
    let (mut map, notes) = treaty_packagr::stylesheet::resolve_component_styles(source, dir);
    let mut inputs: Vec<PathBuf> = Vec::new();
    for component in external_resources(source) {
        if let Some(url) = &component.template_url {
            let path = dir.join(url);
            let text = std::fs::read_to_string(&path).map_err(|e| {
                format!(
                    "{}: templateUrl '{url}' of {}: cannot read {}: {e}",
                    dir.display(),
                    component.class_name,
                    path.display()
                )
            })?;
            inputs.push(path);
            map.entry(component.class_name.clone())
                .or_insert_with(ResolvedComponentContent::default)
                .template = Some(text);
        }
        for url in &component.style_urls {
            let path = dir.join(url);
            if !path.exists() {
                return Err(format!(
                    "{}: styleUrl '{url}' of {}: no such file {}",
                    dir.display(),
                    component.class_name,
                    path.display()
                ));
            }
            inputs.push(path);
        }
    }
    Ok((map, notes, inputs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("treaty-host-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// T1: a `templateUrl` is read relative to the source file and supplied as the component's template.
    #[test]
    fn reads_template_url_relative_to_the_source_file() {
        let dir = scratch("template");
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::write(dir.join("app/app.html"), "<p>from file</p>").unwrap();
        let source = "@Component({ selector: 'app-root', templateUrl: './app.html' })\nexport class App {}\n";
        let (map, _, inputs) = external_content(source, &dir.join("app")).expect("template read");
        assert_eq!(inputs, vec![dir.join("app").join("./app.html")]);
        assert_eq!(map.get("App").and_then(|c| c.template.as_deref()), Some("<p>from file</p>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T1: a missing template is a build error that names the component and the path (not a silent empty).
    #[test]
    fn missing_template_is_an_error_naming_the_component() {
        let dir = scratch("missing");
        let source = "@Component({ selector: 'app-root', templateUrl: './nope.html' })\nexport class App {}\n";
        let err = external_content(source, &dir).expect_err("missing template must fail");
        assert!(err.contains("templateUrl './nope.html' of App"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T1: `styleUrls` are preprocessed (SCSS compiles) and attached to the component by class name.
    #[test]
    fn style_urls_are_preprocessed_and_keyed_by_class() {
        let dir = scratch("styles");
        std::fs::write(dir.join("a.scss"), ".a { .b { color: red; } }").unwrap();
        let source = "@Component({ selector: 'x', template: '', styleUrls: ['./a.scss'] })\nexport class X {}\n";
        let (map, _, _) = external_content(source, &dir).expect("styles read");
        let styles = &map.get("X").expect("entry for X").styles;
        assert_eq!(styles.len(), 1, "styles: {styles:?}");
        // The packager compiles the SCSS nesting and compacts the output (`.a .b{color:red}`).
        assert!(styles[0].contains(".a .b{color:red}"), "got: {}", styles[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T1: a missing stylesheet is reported, not skipped.
    #[test]
    fn missing_style_url_is_an_error() {
        let dir = scratch("missing-style");
        let source = "@Component({ selector: 'x', template: '', styleUrl: './gone.css' })\nexport class X {}\n";
        let err = external_content(source, &dir).expect_err("missing style must fail");
        assert!(err.contains("styleUrl './gone.css' of X"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
