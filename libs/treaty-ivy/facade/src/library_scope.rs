//! The LIBRARY SCOPE of a source file: the directives, components, pipes and NgModules that its
//! imports bring in from linked npm packages (`@angular/common`, `@angular/forms`, `@angular/router`,
//! third-party APF libraries), read from their PUBLISHED PARTIAL DECLARATIONS.
//!
//! Angular's compiler resolves a standalone component's `imports:` through the defining module's
//! metadata: an NgModule contributes its exported directives and pipes (transitively, through nested
//! NgModules), a directive or pipe contributes itself. Treaty's own selector registry only indexes
//! project sources, so for a library entity the compiler otherwise never learns the selector or the
//! pipe name (TREATY-ISSUES T3 and T9). This module supplies that metadata.
//!
//! Division of labour (the same rule as [`crate::source_compile::SelectorRegistry`]): this module does
//! no file I/O. The host implements [`ModuleLoader`] (resolve a specifier from an importing file and
//! read the module), and this module parses what the loader returns. The partial declarations are the
//! `ɵɵngDeclareDirective` / `ɵɵngDeclareComponent` / `ɵɵngDeclarePipe` / `ɵɵngDeclareNgModule` calls a
//! published `@angular/*` FESM or any APF library carries; each is attributed to its class through the
//! declaration object's own `type:` property.
//!
//! Export resolution follows the published module graph: `export { a as b }`, `export { a } from`,
//! `export * from` and an import binding re-exported by name. The entry point a file imports from may
//! be a barrel over several chunks (`@angular/common` re-exports `_common_module-chunk.mjs`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::parse::{NArrayElement, NExpr, NObjectProp, NTopStmt, ParseBackend, ParsingBackend, SourceKind};

pub use crate::decorators::registry::{ExtraImport, LibraryEntity, LibraryKind, LibraryScope, ScopeEntry};

/// A module as the host hands it to the compiler: the absolute path it was found at, and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedModule {
    pub path: String,
    pub code: String,
}

/// The host's module resolution: `specifier` as imported from the file `from` (an absolute path).
/// Returns `None` when the specifier does not resolve or the file cannot be read.
pub trait ModuleLoader {
    fn load(&self, from: &str, specifier: &str) -> Option<LoadedModule>;
}

/// A class in a module: `(absolute module path, class identifier)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Target {
    module: String,
    class: String,
}

/// The partial declaration of one class, reduced to what scope resolution reads.
#[derive(Debug, Clone, PartialEq)]
struct LibDecl {
    kind: LibraryKind,
    selector: Option<String>,
    pipe_name: Option<String>,
    is_standalone: bool,
    /// NgModule `exports`, in order (identifiers and `ns.member` references; nested arrays flattened).
    exports: Vec<ExportRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExportRef {
    Ident(String),
    Member { namespace: String, property: String },
}

/// A module's `import` binding: `import { imported as <local> } from source`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportBinding {
    source: String,
    /// The exported name, `*` for a namespace import, `default` for a default import.
    imported: String,
}

/// One entry of a module's export list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExportEntry {
    /// `export { local as exported }` (no `from`). `local` is a class or an import binding of this module.
    Local { exported: String, local: String },
    /// `export { imported as exported } from 'source'`.
    From { source: String, imported: String, exported: String },
    /// `export * from 'source'`.
    Star { source: String },
}

/// The facts of one published module, parsed once.
#[derive(Debug, Clone, Default)]
struct ModuleFacts {
    path: String,
    decls: HashMap<String, LibDecl>,
    imports: HashMap<String, ImportBinding>,
    exports: Vec<ExportEntry>,
}

/// Cache shared by every source file of one build: parsed modules and resolved specifiers. Create one per
/// build and pass it to every [`library_scope_for_source`] call so each published module is parsed once.
#[derive(Default)]
pub struct LibraryIndex {
    modules: RefCell<HashMap<String, Option<Rc<ModuleFacts>>>>,
    resolved: RefCell<HashMap<(String, String), Option<String>>>,
}

/// The maximum re-export depth followed before giving up (a cycle guard).
const MAX_EXPORT_DEPTH: usize = 16;

impl LibraryIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// The parsed module that `specifier` resolves to from `from`, or `None`.
    fn module_for(&self, loader: &dyn ModuleLoader, from: &str, specifier: &str) -> Option<Rc<ModuleFacts>> {
        let key = (from.to_string(), specifier.to_string());
        let cached = self.resolved.borrow().get(&key).cloned();
        let path = match cached {
            Some(path) => path,
            None => {
                let path = loader.load(from, specifier).map(|m| {
                    self.remember_code(&m.path, m.code);
                    m.path
                });
                self.resolved.borrow_mut().insert(key, path.clone());
                path
            }
        }?;
        self.module_at(&path)
    }

    /// Stash a loaded module's text so `module_at` can parse it without a second read.
    fn remember_code(&self, path: &str, code: String) {
        let mut modules = self.modules.borrow_mut();
        if !modules.contains_key(path) {
            let facts = read_module(path, &code);
            modules.insert(path.to_string(), Some(Rc::new(facts)));
        }
    }

    /// The parsed module at an absolute path (already loaded through `remember_code`).
    fn module_at(&self, path: &str) -> Option<Rc<ModuleFacts>> {
        self.modules.borrow().get(path).cloned().flatten()
    }

    /// The declaration of a class, if its module declares it as a library entity.
    fn decl(&self, target: &Target) -> Option<LibDecl> {
        self.module_at(&target.module)?.decls.get(&target.class).cloned()
    }

    /// The class a module exports under `name`, following re-exports.
    fn find_export(&self, loader: &dyn ModuleLoader, module: &ModuleFacts, name: &str, depth: usize) -> Option<Target> {
        if depth > MAX_EXPORT_DEPTH {
            return None;
        }
        for entry in &module.exports {
            match entry {
                ExportEntry::Local { exported, local } if exported == name => {
                    return self.find_local(loader, module, local, depth + 1);
                }
                ExportEntry::From { source, imported, exported } if exported == name => {
                    let next = self.module_for(loader, &module.path, source)?;
                    return self.find_export(loader, &next, imported, depth + 1);
                }
                _ => {}
            }
        }
        for entry in &module.exports {
            if let ExportEntry::Star { source } = entry {
                if let Some(next) = self.module_for(loader, &module.path, source) {
                    if let Some(found) = self.find_export(loader, &next, name, depth + 1) {
                        return Some(found);
                    }
                }
            }
        }
        None
    }

    /// The class a module-local identifier refers to: a class declared in the module, or an import binding
    /// followed to its exporting module.
    fn find_local(&self, loader: &dyn ModuleLoader, module: &ModuleFacts, ident: &str, depth: usize) -> Option<Target> {
        if depth > MAX_EXPORT_DEPTH {
            return None;
        }
        if module.decls.contains_key(ident) {
            return Some(Target { module: module.path.clone(), class: ident.to_string() });
        }
        let Some(binding) = module.imports.get(ident) else {
            // A class (or other value) the module declares itself, without a library declaration: a plain
            // class such as `FormControl`. It resolves, so the file's bindings know it is not a directive.
            return Some(Target { module: module.path.clone(), class: ident.to_string() });
        };
        if binding.imported == "*" {
            return None;
        }
        let next = self.module_for(loader, &module.path, &binding.source)?;
        self.find_export(loader, &next, &binding.imported, depth + 1)
    }

    /// The expansion of an NgModule target into the directives and pipes it exports, transitively.
    fn expand(&self, loader: &dyn ModuleLoader, target: &Target, visited: &mut Vec<Target>, out: &mut Vec<Target>) {
        if visited.contains(target) {
            return;
        }
        visited.push(target.clone());
        let Some(decl) = self.decl(target) else { return };
        if decl.kind != LibraryKind::NgModule {
            if !out.contains(target) {
                out.push(target.clone());
            }
            return;
        }
        let Some(module) = self.module_at(&target.module) else { return };
        for export in &decl.exports {
            let member = match export {
                ExportRef::Ident(ident) => self.find_local(loader, &module, ident, 0),
                ExportRef::Member { namespace, property } => match module.imports.get(namespace) {
                    Some(binding) if binding.imported == "*" => self
                        .module_for(loader, &module.path, &binding.source)
                        .and_then(|next| self.find_export(loader, &next, property, 0)),
                    _ => None,
                },
            };
            if let Some(member) = member {
                self.expand(loader, &member, visited, out);
            }
        }
    }

    /// The name under which the module `entry` (from `from`) exports `target`, or `None` when it does not.
    fn export_name_in(&self, loader: &dyn ModuleLoader, from: &str, specifier: &str, target: &Target) -> Option<String> {
        let entry = self.module_for(loader, from, specifier)?;
        let mut names: Vec<String> = vec![target.class.clone()];
        for export in &entry.exports {
            match export {
                ExportEntry::Local { exported, .. } | ExportEntry::From { exported, .. } => {
                    if !names.contains(exported) {
                        names.push(exported.clone());
                    }
                }
                ExportEntry::Star { .. } => {}
            }
        }
        names
            .into_iter()
            .find(|name| self.find_export(loader, &entry, name, 0).as_ref() == Some(target))
    }
}

/// Parse one published module's text into its facts: partial declarations, imports and export entries.
fn read_module(path: &str, code: &str) -> ModuleFacts {
    let backend = ParsingBackend::default();
    backend.parse_module(code, SourceKind::ByFilename(path), |module| {
        let summary = module.summary();
        let mut facts = ModuleFacts { path: path.to_string(), ..ModuleFacts::default() };
        if !summary.errors.is_empty() {
            return facts;
        }
        for call in &summary.ng_declare_calls {
            let Some(kind) = declare_kind(&call.kind) else { continue };
            let props = &call.object.nprops;
            let Some(class) = find_prop(props, "type").and_then(identifier_of) else { continue };
            let selector = find_prop(props, "selector").and_then(string_of);
            let pipe_name = find_prop(props, "name").and_then(string_of);
            let is_standalone = match find_prop(props, "isStandalone") {
                Some(NExpr::Boolean(b)) => *b,
                _ => true,
            };
            let mut exports = Vec::new();
            if let Some(list) = find_prop(props, "exports") {
                collect_export_refs(list, &mut exports);
            }
            // A class may carry several declarations (a component's `ɵcmp` and `ɵfac`); the scope reads the
            // directive/component/pipe/module one, so the first recognised kind wins.
            facts.decls.entry(class).or_insert(LibDecl { kind, selector, pipe_name, is_standalone, exports });
        }
        for import in &summary.imports {
            // Type-only bindings never reach the runtime and are not scope references.
            if import.type_only {
                continue;
            }
            facts.imports.insert(import.local_name.clone(), ImportBinding {
                source: import.source.clone(),
                imported: import.imported.clone(),
            });
        }
        for stmt in &summary.top_level {
            let NTopStmt::Other(span) = stmt else { continue };
            let text = backend.span_text(code, *span).trim_start();
            if let Some(rest) = text.strip_prefix("export") {
                if rest.starts_with(|c: char| c.is_whitespace() || c == '{' || c == '*') {
                    parse_export_text(rest, &mut facts.exports);
                }
            }
        }
        facts
    })
}

/// The top-level `const NAME = [a, b, …]` declarations of a module, with the identifiers each array lists (nested
/// arrays and `forwardRef(() => X)` are read as their target). Other initialisers are not tuples.
fn local_array_constants(top_level: &[NTopStmt]) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for stmt in top_level {
        let NTopStmt::VarDecl { decls, .. } = stmt else { continue };
        for decl in decls {
            let (Some(name), Some(NExpr::Array(elements))) = (&decl.name, &decl.init) else { continue };
            let mut members = Vec::new();
            collect_tuple_members(elements, &mut members);
            out.push((name.clone(), members));
        }
    }
    out
}

fn collect_tuple_members(elements: &[NArrayElement], out: &mut Vec<String>) {
    for element in elements {
        let NArrayElement::Expr(expr) = element else { continue };
        match expr {
            NExpr::Identifier(name) => out.push(name.clone()),
            NExpr::Array(inner) => collect_tuple_members(inner, out),
            NExpr::Call { callee, args } if matches!(callee.as_ref(), NExpr::Identifier(f) if f == "forwardRef") => {
                if let Some(NExpr::Arrow { body, .. }) = args.first().map(|a| a.expr()) {
                    if let crate::parse::NArrowBody::Expr(returned) = body.as_ref() {
                        if let NExpr::Identifier(name) = returned.as_ref() {
                            out.push(name.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Bind each tuple name to the entries of its members, repeating until nothing changes so a tuple of tuples
/// resolves. A member that is not in scope contributes nothing (it is not a library entity of this file).
fn add_tuple_entries(scope: &mut LibraryScope, tuples: &[(String, Vec<String>)]) {
    for _ in 0..tuples.len().max(1) {
        let mut changed = false;
        for (name, members) in tuples {
            let mut entries: Vec<ScopeEntry> = scope.by_name.get(name).cloned().unwrap_or_default();
            for member in members {
                for entry in scope.by_name.get(member).cloned().unwrap_or_default() {
                    if !entries.iter().any(|e| e.local == entry.local) {
                        entries.push(entry);
                    }
                }
            }
            if !entries.is_empty() && scope.by_name.get(name) != Some(&entries) {
                scope.by_name.insert(name.clone(), entries);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn declare_kind(suffix: &str) -> Option<LibraryKind> {
    match suffix {
        "Directive" => Some(LibraryKind::Directive),
        "Component" => Some(LibraryKind::Component),
        "Pipe" => Some(LibraryKind::Pipe),
        "NgModule" => Some(LibraryKind::NgModule),
        _ => None,
    }
}

fn find_prop<'a>(props: &'a [NObjectProp], name: &str) -> Option<&'a NExpr> {
    props.iter().find_map(|p| match p {
        NObjectProp::KeyValue { key, value, .. } if key == name => Some(value),
        _ => None,
    })
}

fn identifier_of(expr: &NExpr) -> Option<String> {
    match expr {
        NExpr::Identifier(name) => Some(name.clone()),
        NExpr::Parenthesized(inner) => identifier_of(inner),
        _ => None,
    }
}

fn string_of(expr: &NExpr) -> Option<String> {
    match expr {
        NExpr::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Flatten an NgModule `exports` value into identifier and `ns.member` references, in source order.
fn collect_export_refs(expr: &NExpr, out: &mut Vec<ExportRef>) {
    match expr {
        NExpr::Array(elements) => {
            for element in elements {
                if let NArrayElement::Expr(inner) = element {
                    collect_export_refs(inner, out);
                }
            }
        }
        NExpr::Parenthesized(inner) => collect_export_refs(inner, out),
        NExpr::Identifier(name) => out.push(ExportRef::Ident(name.clone())),
        NExpr::Member { object, property } => {
            if let NExpr::Identifier(namespace) = object.as_ref() {
                out.push(ExportRef::Member { namespace: namespace.clone(), property: property.clone() });
            }
        }
        _ => {}
    }
}

/// Parse the text of an `export` statement (the part after the `export` keyword) into export entries:
/// `{ a, b as c }`, `{ a as b } from 'x'`, `* from 'x'`, `* as ns from 'x'` (the namespace form is not a
/// class export and is ignored). Unknown forms are skipped, never guessed.
fn parse_export_text(rest: &str, out: &mut Vec<ExportEntry>) {
    let body = rest.trim().trim_end_matches(';').trim();
    let (names_part, source) = match body.rfind(" from ") {
        Some(idx) => (&body[..idx], unquote(&body[idx + " from ".len()..])),
        None => (body, None),
    };
    let names_part = names_part.trim();
    if let Some(star) = names_part.strip_prefix('*') {
        if star.trim().is_empty() {
            if let Some(source) = source {
                out.push(ExportEntry::Star { source });
            }
        }
        return;
    }
    let Some(inner) = names_part.strip_prefix('{').and_then(|s| s.trim_end().strip_suffix('}')) else {
        return;
    };
    for part in inner.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let part = part.strip_prefix("type ").map(str::trim).unwrap_or(part);
        let (local, exported) = match part.split_once(" as ") {
            Some((l, e)) => (l.trim().to_string(), e.trim().to_string()),
            None => (part.to_string(), part.to_string()),
        };
        match &source {
            Some(source) => out.push(ExportEntry::From { source: source.clone(), imported: local, exported }),
            None => out.push(ExportEntry::Local { exported, local }),
        }
    }
}

/// The string inside a quoted module specifier, or `None` when the text is not a quoted string.
fn unquote(text: &str) -> Option<String> {
    let text = text.trim().trim_end_matches(';').trim();
    let first = text.chars().next()?;
    if (first == '\'' || first == '"') && text.len() >= 2 && text.ends_with(first) {
        Some(text[1..text.len() - 1].to_string())
    } else {
        None
    }
}

/// Build the library scope of one source file.
///
/// `file_path` is the absolute path of the file (imports resolve relative to it). `source` is its TypeScript
/// text. Every named import binding that resolves to a library declaration contributes an entry; an NgModule
/// contributes its expansion. Entities an NgModule exports that the file does not import get an
/// [`ExtraImport`] under a local identifier that does not clash with the file's own bindings.
pub fn library_scope_for_source(
    source: &str,
    file_path: &str,
    index: &LibraryIndex,
    loader: &dyn ModuleLoader,
) -> LibraryScope {
    let backend = ParsingBackend::default();
    let (imports, tuples): (Vec<crate::parse::ImportInfo>, Vec<(String, Vec<String>)>) = backend.parse_module(
        source,
        SourceKind::TypeScriptModule,
        |module| {
            let summary = module.summary();
            (summary.imports.clone(), local_array_constants(&summary.top_level))
        },
    );
    let mut scope = LibraryScope::default();
    if imports.is_empty() {
        return scope;
    }
    let file_bindings: Vec<String> = imports.iter().map(|i| i.local_name.clone()).collect();

    // Resolve each named, value import of the file to the class it names.
    let mut direct: Vec<(String, Target)> = Vec::new();
    for import in &imports {
        if import.type_only || import.imported == "*" || import.imported == "default" {
            continue;
        }
        let Some(module) = index.module_for(loader, file_path, &import.source) else { continue };
        if let Some(target) = index.find_export(loader, &module, &import.imported, 0) {
            if !direct.iter().any(|(local, _)| local == &import.local_name) {
                direct.push((import.local_name.clone(), target));
            }
        }
    }

    scope.resolved_bindings = direct.iter().map(|(local, _)| local.clone()).collect();

    let mut extra_by_target: HashMap<Target, String> = HashMap::new();
    let mut used_locals: Vec<String> = file_bindings.clone();
    for (local, target) in &direct {
        let Some(decl) = index.decl(target) else { continue };
        let entity_of = |t: &Target, d: &LibDecl| LibraryEntity {
            kind: d.kind,
            class_name: t.class.clone(),
            module_path: t.module.clone(),
            selector: d.selector.clone(),
            pipe_name: d.pipe_name.clone(),
            is_standalone: d.is_standalone,
        };
        if decl.kind != LibraryKind::NgModule {
            scope.by_name.insert(local.clone(), vec![ScopeEntry { local: local.clone(), entity: entity_of(target, &decl) }]);
            continue;
        }
        let import_source = imports
            .iter()
            .find(|i| &i.local_name == local)
            .map(|i| i.source.clone())
            .unwrap_or_default();
        let mut expansion: Vec<Target> = Vec::new();
        index.expand(loader, target, &mut Vec::new(), &mut expansion);
        let mut entries: Vec<ScopeEntry> = Vec::new();
        for member in &expansion {
            let Some(member_decl) = index.decl(member) else { continue };
            // A binding the file already has for this exact entity is reused (no duplicate import).
            let existing = direct
                .iter()
                .find(|(_, t)| t == member)
                .map(|(l, _)| l.clone());
            let local_name = if let Some(existing) = existing {
                existing
            } else if let Some(local_name) = extra_by_target.get(member) {
                local_name.clone()
            } else {
                let class = member.class.clone();
                let local_name = if used_locals.contains(&class) {
                    format!("__ng_{class}")
                } else {
                    class.clone()
                };
                let specifier = match index.export_name_in(loader, file_path, &import_source, member) {
                    Some(exported) => Some((exported, import_source.clone())),
                    None => index
                        .module_at(&member.module)
                        .filter(|m| index.find_export(loader, m, &class, 0).as_ref() == Some(member))
                        .map(|_| (class.clone(), member.module.clone())),
                };
                match specifier {
                    Some((imported, specifier)) => {
                        used_locals.push(local_name.clone());
                        scope.extra_imports.push(ExtraImport {
                            local: local_name.clone(),
                            imported,
                            specifier,
                        });
                        extra_by_target.insert(member.clone(), local_name.clone());
                        local_name
                    }
                    None => continue,
                }
            };
            entries.push(ScopeEntry { local: local_name, entity: entity_of(member, &member_decl) });
        }
        scope.by_name.insert(local.clone(), entries);
    }
    // A local constant tuple (`const SHARED = [CommonModule, FormsModule]`) is one of the names an `imports:`
    // array can use: it brings in what its elements bring in, in order, and so does a tuple of tuples.
    add_tuple_entries(&mut scope, &tuples);
    scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    /// An in-memory package: `(from-file-or-empty, specifier) -> (absolute path, code)`. Resolution is by
    /// exact specifier, so every test spells the specifiers it uses.
    struct FakePackage {
        modules: Map<String, LoadedModule>,
        specs: Map<String, String>,
    }

    impl FakePackage {
        fn new(modules: &[(&str, &str)], specs: &[(&str, &str)]) -> Self {
            FakePackage {
                modules: modules
                    .iter()
                    .map(|(path, code)| {
                        (path.to_string(), LoadedModule { path: path.to_string(), code: code.to_string() })
                    })
                    .collect(),
                specs: specs.iter().map(|(s, p)| (s.to_string(), p.to_string())).collect(),
            }
        }
    }

    impl ModuleLoader for FakePackage {
        fn load(&self, from: &str, specifier: &str) -> Option<LoadedModule> {
            let path = if specifier.starts_with('/') {
                specifier.to_string()
            } else if let Some(relative) = specifier.strip_prefix("./") {
                let dir = from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                format!("{dir}/{relative}")
            } else {
                self.specs.get(specifier)?.clone()
            };
            self.modules.get(&path).cloned()
        }
    }

    /// A partial-declaration pipe in the published shape.
    fn pipe_decl(class: &str, name: &str) -> String {
        format!(
            "class {class} {{\n  static \u{0275}pipe = i0.\u{0275}\u{0275}ngDeclarePipe({{\n    minVersion: \"14.0.0\",\n    version: \"22.0.0\",\n    ngImport: i0,\n    type: {class},\n    isStandalone: true,\n    name: \"{name}\",\n    pure: true\n  }});\n}}\n"
        )
    }

    /// A partial-declaration directive in the published shape.
    fn directive_decl(class: &str, selector: &str, standalone: bool) -> String {
        format!(
            "class {class} {{\n  static \u{0275}dir = i0.\u{0275}\u{0275}ngDeclareDirective({{\n    minVersion: \"14.0.0\",\n    version: \"22.0.0\",\n    type: {class},\n    isStandalone: {standalone},\n    selector: \"{selector}\"\n  }});\n}}\n"
        )
    }

    /// A partial-declaration NgModule whose `exports` list is `exports`.
    fn module_decl(class: &str, exports: &str) -> String {
        format!(
            "class {class} {{\n  static \u{0275}mod = i0.\u{0275}\u{0275}ngDeclareNgModule({{\n    minVersion: \"14.0.0\",\n    version: \"22.0.0\",\n    ngImport: i0,\n    type: {class},\n    exports: [{exports}]\n  }});\n}}\n"
        )
    }

    const CHUNK_PATH: &str = "/pkg/fesm2022/_common-chunk.mjs";

    fn common_chunk() -> String {
        format!(
            "import * as i0 from '@angular/core';\n{}{}{}{}export {{ AsyncPipe, DecimalPipe, UpperCasePipe, CommonModule, NgIf }};\n",
            pipe_decl("AsyncPipe", "async"),
            pipe_decl("DecimalPipe", "number"),
            pipe_decl("UpperCasePipe", "uppercase"),
            directive_decl("NgIf", "[ngIf]", true),
        ) + module_decl("CommonModule", "NgIf, AsyncPipe, UpperCasePipe, DecimalPipe").as_str()
    }

    /// `@angular/common` as published: an entry module that re-exports the chunk.
    fn common_entry() -> String {
        "export { AsyncPipe, CommonModule, DecimalPipe, NgIf, UpperCasePipe } from './_common-chunk.mjs';\n".to_string()
    }

    fn package() -> FakePackage {
        FakePackage::new(
            &[(CHUNK_PATH, &common_chunk()), ("/pkg/fesm2022/common.mjs", &common_entry())],
            &[("@angular/common", "/pkg/fesm2022/common.mjs")],
        )
    }

    #[test]
    fn reads_pipe_name_and_selector_from_partial_declarations() {
        let index = LibraryIndex::new();
        let loader = package();
        let app = "import { Component } from '@angular/core';\nimport { DecimalPipe } from '@angular/common';\n\
                   @Component({ imports: [DecimalPipe], template: '{{ 1 | number }}' }) export class App {}\n";
        let scope = library_scope_for_source(app, "/app/app.ts", &index, &loader);
        let entries = scope.entries_for(&["DecimalPipe".to_string()]);
        assert_eq!(entries.len(), 1, "scope: {scope:?}");
        assert_eq!(entries[0].local, "DecimalPipe");
        assert_eq!(entries[0].entity.kind, LibraryKind::Pipe);
        assert_eq!(entries[0].entity.pipe_name.as_deref(), Some("number"));
        assert_eq!(entries[0].entity.module_path, CHUNK_PATH, "the defining chunk, not the entry");
    }

    #[test]
    fn ngmodule_expands_to_its_exports_in_order() {
        let index = LibraryIndex::new();
        let loader = package();
        let app = "import { CommonModule } from '@angular/common';\n";
        let scope = library_scope_for_source(app, "/app/app.ts", &index, &loader);
        let names: Vec<String> = scope
            .entries_for(&["CommonModule".to_string()])
            .iter()
            .map(|e| e.entity.class_name.clone())
            .collect();
        assert_eq!(names, vec!["NgIf", "AsyncPipe", "UpperCasePipe", "DecimalPipe"]);
        // Every expansion the file does not import by name gets an import line.
        assert!(scope.extra_imports.iter().any(|i| i.local == "NgIf" && i.specifier == "@angular/common"),
            "extra imports: {:?}", scope.extra_imports);
    }

    #[test]
    fn directly_imported_names_are_reused_not_imported_twice() {
        let index = LibraryIndex::new();
        let loader = package();
        let app = "import { CommonModule, NgIf } from '@angular/common';\n";
        let scope = library_scope_for_source(app, "/app/app.ts", &index, &loader);
        let ng_if = scope.entries_for(&["CommonModule".to_string()]).into_iter().find(|e| e.entity.class_name == "NgIf");
        assert_eq!(ng_if.map(|e| e.local), Some("NgIf".to_string()));
        assert!(scope.extra_imports.iter().all(|i| i.local != "NgIf"), "NgIf is already imported");
    }

    #[test]
    fn expansion_local_names_avoid_file_bindings() {
        let index = LibraryIndex::new();
        let loader = package();
        // The file already binds `NgIf` to something else: the expansion must use an alias.
        let app = "import { CommonModule } from '@angular/common';\nimport { NgIf } from './local';\n";
        let scope = library_scope_for_source(app, "/app/app.ts", &index, &loader);
        let ng_if = scope.entries_for(&["CommonModule".to_string()]).into_iter().find(|e| e.entity.class_name == "NgIf").unwrap();
        assert_eq!(ng_if.local, "__ng_NgIf");
        assert!(scope.extra_imports.iter().any(|i| i.local == "__ng_NgIf" && i.imported == "NgIf"));
    }

    #[test]
    fn nested_ngmodule_exports_are_expanded_transitively_and_deduplicated() {
        let chunk = format!(
            "{}{}{}export {{ Dir, Inner, Outer }};\n",
            directive_decl("Dir", "[dir]", true),
            module_decl("Inner", "Dir"),
            module_decl("Outer", "Inner, Dir"),
        );
        let loader = FakePackage::new(
            &[("/pkg/outer.mjs", &chunk)],
            &[("pkg-outer", "/pkg/outer.mjs")],
        );
        let index = LibraryIndex::new();
        let scope = library_scope_for_source("import { Outer } from 'pkg-outer';\n", "/app/a.ts", &index, &loader);
        let names: Vec<String> = scope
            .entries_for(&["Outer".to_string()])
            .iter()
            .map(|e| e.entity.class_name.clone())
            .collect();
        assert_eq!(names, vec!["Dir"], "nested module expanded and duplicate dropped");
    }

    #[test]
    fn an_export_the_file_cannot_import_is_left_out_of_scope() {
        // `Dir` is declared and exported by an NgModule but the module does not export it by name, so no
        // import line can name it. It is left out rather than emitted with a dangling identifier.
        let chunk = format!(
            "{}{}export {{ Outer }};\n",
            directive_decl("Dir", "[dir]", true),
            module_decl("Outer", "Dir"),
        );
        let loader = FakePackage::new(&[("/pkg/outer.mjs", &chunk)], &[("pkg-outer", "/pkg/outer.mjs")]);
        let index = LibraryIndex::new();
        let scope = library_scope_for_source("import { Outer } from 'pkg-outer';\n", "/app/a.ts", &index, &loader);
        assert!(scope.entries_for(&["Outer".to_string()]).is_empty(), "scope: {scope:?}");
        assert!(scope.extra_imports.is_empty());
    }

    #[test]
    fn follows_reexports_across_barrels() {
        let index = LibraryIndex::new();
        let loader = package();
        // A barrel that re-exports the entry under another name.
        let loader = FakePackage {
            modules: loader
                .modules
                .into_iter()
                .chain(std::iter::once((
                    "/pkg/barrel.mjs".to_string(),
                    LoadedModule {
                        path: "/pkg/barrel.mjs".to_string(),
                        code: "export * from './fesm2022/common.mjs';\n".to_string(),
                    },
                )))
                .collect(),
            specs: [("@angular/common".to_string(), "/pkg/barrel.mjs".to_string())].into_iter().collect(),
        };
        let scope = library_scope_for_source("import { AsyncPipe } from '@angular/common';\n", "/app/a.ts", &index, &loader);
        let entries = scope.entries_for(&["AsyncPipe".to_string()]);
        assert_eq!(entries.first().map(|e| e.entity.pipe_name.clone()), Some(Some("async".to_string())));
    }

    #[test]
    fn type_only_imports_and_non_declarations_are_ignored() {
        let index = LibraryIndex::new();
        let loader = package();
        let app = "import type { DecimalPipe } from '@angular/common';\nimport { Injectable } from '@angular/core';\n";
        let scope = library_scope_for_source(app, "/app/a.ts", &index, &loader);
        assert!(scope.is_empty(), "scope: {scope:?}");
    }

    #[test]
    fn a_local_constant_tuple_brings_in_its_members_entries() {
        let index = LibraryIndex::new();
        let loader = package();
        let app = "import { CommonModule, DecimalPipe } from '@angular/common';\n\
                   const SHARED = [CommonModule, [DecimalPipe]];\n\
                   const ALL = [SHARED];\n";
        let scope = library_scope_for_source(app, "/app/app.ts", &index, &loader);
        let names: Vec<String> = scope
            .entries_for(&["SHARED".to_string()])
            .iter()
            .map(|e| e.entity.class_name.clone())
            .collect();
        assert_eq!(names, vec!["NgIf", "AsyncPipe", "UpperCasePipe", "DecimalPipe"], "SHARED expands CommonModule");
        let outer: Vec<String> = scope.entries_for(&["ALL".to_string()]).iter().map(|e| e.entity.class_name.clone()).collect();
        assert_eq!(outer, names, "a tuple of tuples resolves the same way");
    }

    #[test]
    fn export_statement_parser_handles_aliases_and_sources() {
        let mut out = Vec::new();
        parse_export_text(" { A, B as C } from './x.mjs';", &mut out);
        assert_eq!(out, vec![
            ExportEntry::From { source: "./x.mjs".into(), imported: "A".into(), exported: "A".into() },
            ExportEntry::From { source: "./x.mjs".into(), imported: "B".into(), exported: "C".into() },
        ]);
        let mut local = Vec::new();
        parse_export_text(" { Foo as default };", &mut local);
        assert_eq!(local, vec![ExportEntry::Local { exported: "default".into(), local: "Foo".into() }]);
        let mut star = Vec::new();
        parse_export_text(" * from \"./all.mjs\";", &mut star);
        assert_eq!(star, vec![ExportEntry::Star { source: "./all.mjs".into() }]);
    }
}
