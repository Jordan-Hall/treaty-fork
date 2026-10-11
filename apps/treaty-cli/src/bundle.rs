//! SINGLE-FILE BUNDLES (TREATY-ISSUES T4): the native module graph concatenated into one script.
//!
//! `treaty build` by default writes one ES module per graph module (`_treaty/*.js`), which a browser loads
//! as a module graph. A host that embeds a JavaScript engine without a module loader needs ONE file. This
//! module produces it by SCOPE HOISTING, the way bundlers do:
//!
//!   * every module is analysed with oxc's semantic model, so a reference is attributed to the binding it
//!     really names (a shadowing local is left alone);
//!   * each module's top-level declaration gets a name that is unique across the bundle (its own name when
//!     free, otherwise `name$N`); a name the bundle reads as a global (`Promise`, `Object`) is never taken;
//!   * `import { a as b } from 'x'` is resolved through the export tables of the graph (re-exports and
//!     `export *` included), so `b` becomes a reference to the final name of the exporting binding. Live
//!     bindings are preserved, because there is one shared scope and no copy is taken;
//!   * `import * as ns` and `import('x')` use a namespace object with getters, so they stay live too;
//!   * import and export statements are removed, and the modules run in dependency order, the entry last.
//!
//! The output is either a classic script (wrapped in an IIFE, so it adds no globals) or a single ES module
//! (the entry's exports are kept). No new dependency: the analysis is oxc, already a CLI dependency.
//! Minification is not done here (no minifier is linked; `--bundler rolldown` remains the minifying path).

use std::collections::{BTreeMap, HashMap, HashSet};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Declaration, ExportDefaultDeclarationKind, Expression, ImportDeclarationSpecifier, ModuleExportName,
    Statement,
};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::{GetSpan, SourceType};

/// The shape of the emitted file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// One ES module per graph module under `_treaty/` (the default; browsers load the graph).
    #[default]
    Modules,
    /// One classic script (an IIFE that adds no globals). For engines without a module loader.
    Script,
    /// One ES module holding every graph module, exporting the entry's exports.
    Esm,
}

impl OutputFormat {
    /// The `--format` spelling of this format.
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Modules => "modules",
            OutputFormat::Script => "script",
            OutputFormat::Esm => "esm",
        }
    }

    /// Parse a `--format` value (`modules`, `script` or `esm`).
    pub fn parse(text: &str) -> Option<OutputFormat> {
        match text {
            "modules" => Some(OutputFormat::Modules),
            "script" | "iife" => Some(OutputFormat::Script),
            "esm" => Some(OutputFormat::Esm),
            _ => None,
        }
    }
}

/// One graph module as the bundler receives it: its ESM text (imports still as written) and how each specifier
/// resolves to another module of the graph.
#[derive(Debug, Clone)]
pub struct BundleModule {
    /// A label for error messages (the module's path).
    pub label: String,
    /// The module's ESM source.
    pub code: String,
    /// Each import/export specifier of `code` mapped to the index of the module it resolves to.
    pub edges: BTreeMap<String, usize>,
}

/// The text of the bundle and the names it exports (for [`OutputFormat::Esm`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub code: String,
}

/// Bundle `modules` (graph index order is irrelevant; `entry` is the index of the entry module).
pub fn bundle(modules: &[BundleModule], entry: usize, format: OutputFormat) -> Result<Bundle, String> {
    if format == OutputFormat::Modules {
        return Err("bundle: the modules format is not a bundle".to_string());
    }
    let analyses = modules
        .iter()
        .map(|m| analyze(&m.code).map_err(|e| format!("{}: {e}", m.label)))
        .collect::<Result<Vec<_>, _>>()?;
    let order = dependency_order(modules, &analyses, entry)?;

    // Names every module reads as globals must never be taken by a hoisted top-level name.
    let mut reserved: HashSet<String> = HashSet::new();
    for analysis in &analyses {
        reserved.extend(analysis.globals.iter().cloned());
    }

    // 1. Final names of every module's top-level declarations, in dependency order.
    let mut taken: HashSet<String> = reserved.clone();
    let mut finals: Vec<HashMap<String, String>> = vec![HashMap::new(); modules.len()];
    for &m in &order {
        for name in &analyses[m].declared {
            let mut candidate = name.clone();
            let mut k = 1;
            while taken.contains(&candidate) {
                candidate = format!("{name}${k}");
                k += 1;
            }
            taken.insert(candidate.clone());
            finals[m].insert(name.clone(), candidate);
        }
    }

    // 2. Namespace objects: one per module, a getter per export (live, and lazy so cycles are safe).
    let ns_name = |m: usize| format!("$treaty_ns{m}");
    let mut header = String::new();
    for m in 0..modules.len() {
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        collect_export_names(modules, &analyses, m, &mut names, &mut seen, 0);
        let mut getters = Vec::new();
        for name in &names {
            let target = resolve_export(modules, &analyses, &finals, m, name, 0)?;
            let key = if name == "default" { "default".to_string() } else { name.clone() };
            getters.push(format!("get {key}() {{ return {target}; }}"));
        }
        header.push_str(&format!(
            "const {} = Object.freeze({{ __proto__: null, {} }});\n",
            ns_name(m),
            getters.join(", ")
        ));
    }

    // 3. Each module's text, rewritten.
    let mut body = String::new();
    for &m in &order {
        let analysis = &analyses[m];
        let module = &modules[m];
        // Local bindings of this module that stand for another module's binding.
        let mut import_map: HashMap<String, String> = HashMap::new();
        for import in &analysis.imports {
            let target = *module.edges.get(&import.source).ok_or_else(|| {
                format!("{}: unresolved import '{}' (not in the graph)", module.label, import.source)
            })?;
            for spec in &import.specifiers {
                let replacement = match spec.imported.as_str() {
                    "*" => ns_name(target),
                    name => resolve_export(modules, &analyses, &finals, target, name, 0)
                        .map_err(|e| format!("{}: {e}", module.label))?,
                };
                import_map.insert(spec.local.clone(), replacement);
            }
        }
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        for (start, end, text) in &analysis.removals {
            edits.push((*start, *end, text.clone()));
        }
        for (start, end, name) in &analysis.decl_names {
            let final_name = finals[m]
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone());
            edits.push((*start, *end, final_name));
        }
        // `export default <expression>`: a synthesized top-level binding carries the value.
        for default in &analysis.default_exports {
            match &default.local {
                // A named declaration keeps its name: only the keyword goes.
                Some(_) => edits.push((default.start, default.end, String::new())),
                // An expression (or anonymous declaration) is bound to the module's `default$`.
                None => {
                    let final_name = finals[m].get("default$").cloned().unwrap_or_else(|| "default$".to_string());
                    edits.push((default.start, default.end, format!("const {final_name} = ")));
                }
            }
        }
        for (start, end, spec) in &analysis.dynamic_imports {
            let target = *module.edges.get(spec).ok_or_else(|| {
                format!("{}: unresolved dynamic import '{spec}' (not in the graph)", module.label)
            })?;
            edits.push((*start, *end, format!("Promise.resolve({})", ns_name(target))));
        }
        for (start, end, local) in &analysis.refs {
            // A reference inside a removed statement (an `export { a }` specifier) is dropped with it.
            if analysis.removals.iter().any(|(rs, re, _)| *start >= *rs && *end <= *re) {
                continue;
            }
            let replacement = import_map
                .get(local)
                .cloned()
                .or_else(|| finals[m].get(local).cloned());
            if let Some(replacement) = replacement {
                if analysis.shorthand_refs.contains(&(*start, *end)) {
                    edits.push((*start, *end, format!("{local}: {replacement}")));
                } else {
                    edits.push((*start, *end, replacement));
                }
            }
        }
        body.push_str(&apply_edits(&module.code, edits).map_err(|e| format!("{}: {e}", module.label))?);
        body.push('\n');
    }

    // The entry's exports are the bundle's exports for the ESM format.
    let entry_exports: Vec<(String, String)> = {
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        collect_export_names(modules, &analyses, entry, &mut names, &mut seen, 0);
        let mut out = Vec::new();
        for name in names {
            let target = resolve_export(modules, &analyses, &finals, entry, &name, 0)?;
            out.push((target, name));
        }
        out
    };

    let code = match format {
        OutputFormat::Script => format!("(function () {{\n\"use strict\";\n{header}{body}}})();\n"),
        OutputFormat::Esm => {
            let exports = entry_exports
                .iter()
                .map(|(local, exported)| {
                    if local == exported {
                        local.clone()
                    } else {
                        format!("{local} as {exported}")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{header}{body}export {{ {exports} }};\n")
        }
        OutputFormat::Modules => unreachable!("rejected above"),
    };
    Ok(Bundle { code })
}

// ---------------------------------------------------------------------------
// Analysis of one module (owned results only; the oxc arena is dropped here).
// ---------------------------------------------------------------------------

/// One import declaration of a module.
#[derive(Debug, Clone)]
struct ImportDecl {
    source: String,
    specifiers: Vec<ImportSpec>,
}

/// One import binding: `imported` is the exported name (`*` for a namespace, `default` for a default import).
#[derive(Debug, Clone)]
struct ImportSpec {
    imported: String,
    local: String,
}

/// `export default <expression|declaration>`. `local` is the declared name when the declaration has one.
#[derive(Debug, Clone)]
struct DefaultExport {
    start: usize,
    end: usize,
    local: Option<String>,
}

/// What a module exports, by exported name.
#[derive(Debug, Clone)]
enum ExportKind {
    /// A top-level declared name of this module.
    Local(String),
    /// A binding this module imported (`import { a } from 'x'; export { a }`): resolved through the import.
    Imported { imported: String },
    /// `export { a as b } from 'x'` / `export * as ns from 'x'` (namespace = `*`).
    ReExport { source: String, imported: String },
    /// `export * from 'x'`: every name of `x` except `default`.
    Star(String),
}

#[derive(Debug, Clone, Default)]
struct Analysis {
    /// Top-level names this module declares (excluding its imports), in declaration order.
    declared: Vec<String>,
    /// Import declarations, in source order.
    imports: Vec<ImportDecl>,
    /// Exported name -> what it is.
    exports: Vec<(String, ExportKind)>,
    /// `export default` statements.
    default_exports: Vec<DefaultExport>,
    /// Byte spans of declared identifiers, with their names (renamed to the final name).
    decl_names: Vec<(usize, usize, String)>,
    /// Byte spans of references to top-level bindings (declared or imported), with the local name.
    refs: Vec<(usize, usize, String)>,
    /// The subset of `refs` that sit in an object-literal shorthand (`{ a }`), which must keep the key.
    shorthand_refs: HashSet<(usize, usize)>,
    /// Dynamic `import('x')`: the span and the specifier.
    dynamic_imports: Vec<(usize, usize, String)>,
    /// Spans deleted from the output, with their replacement text (empty for a deletion).
    removals: Vec<(usize, usize, String)>,
    /// Names the module reads without declaring them (globals).
    globals: HashSet<String>,
    /// Source-order specifiers of imports and re-exports (the dependency order).
    source_order: Vec<String>,
}

fn analyze(code: &str) -> Result<Analysis, String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, SourceType::mjs()).parse();
    if !parsed.errors.is_empty() {
        let messages: Vec<String> = parsed.errors.iter().map(|e| e.to_string()).collect();
        return Err(format!("parse error: {}", messages.join("; ")));
    }
    let program = parsed.program;
    let semantic = SemanticBuilder::new().build(&program).semantic;
    let scoping = semantic.scoping();
    let nodes = semantic.nodes();
    let root = scoping.root_scope_id();

    let mut analysis = Analysis::default();

    // Pass 1: import declarations (their local names classify the export statements below).
    let mut import_locals: HashSet<String> = HashSet::new();
    for stmt in &program.body {
        let Statement::ImportDeclaration(decl) = stmt else { continue };
        let source = decl.source.value.to_string();
        analysis.source_order.push(source.clone());
        let mut specifiers = Vec::new();
        for spec in decl.specifiers.iter().flatten() {
            let (local, imported) = match spec {
                ImportDeclarationSpecifier::ImportSpecifier(s) => {
                    (s.local.name.to_string(), module_export_name(&s.imported))
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                    (s.local.name.to_string(), "default".to_string())
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                    (s.local.name.to_string(), "*".to_string())
                }
            };
            import_locals.insert(local.clone());
            specifiers.push(ImportSpec { imported, local });
        }
        analysis.imports.push(ImportDecl { source, specifiers });
        analysis.removals.push((decl.span.start as usize, decl.span.end as usize, String::new()));
    }

    // Pass 2: export statements and default exports.
    let mut needs_default_binding = false;
    for stmt in &program.body {
        match stmt {
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(source) = &decl.source {
                    let source = source.value.to_string();
                    analysis.source_order.push(source.clone());
                    for spec in &decl.specifiers {
                        let exported = module_export_name(&spec.exported);
                        let imported = module_export_name(&spec.local);
                        analysis
                            .exports
                            .push((exported, ExportKind::ReExport { source: source.clone(), imported }));
                    }
                    analysis.removals.push((decl.span.start as usize, decl.span.end as usize, String::new()));
                } else if let Some(declaration) = &decl.declaration {
                    // `export const/function/class X`: keep the declaration, drop the `export` keyword.
                    for name in declaration_names(declaration) {
                        analysis.exports.push((name.clone(), ExportKind::Local(name)));
                    }
                    let keyword_end = declaration.span().start as usize;
                    analysis.removals.push((decl.span.start as usize, keyword_end, String::new()));
                } else {
                    // `export { a as b }`: a local declaration of this module, or a binding it imported.
                    for spec in &decl.specifiers {
                        let exported = module_export_name(&spec.exported);
                        let local = module_export_name(&spec.local);
                        let kind = if import_locals.contains(&local) {
                            ExportKind::Imported { imported: local.clone() }
                        } else {
                            ExportKind::Local(local.clone())
                        };
                        analysis.exports.push((exported, kind));
                    }
                    analysis.removals.push((decl.span.start as usize, decl.span.end as usize, String::new()));
                }
            }
            Statement::ExportAllDeclaration(decl) => {
                let source = decl.source.value.to_string();
                analysis.source_order.push(source.clone());
                match &decl.exported {
                    Some(name) => analysis.exports.push((
                        module_export_name(name),
                        ExportKind::ReExport { source: source.clone(), imported: "*".to_string() },
                    )),
                    None => analysis.exports.push((String::new(), ExportKind::Star(source.clone()))),
                }
                analysis.removals.push((decl.span.start as usize, decl.span.end as usize, String::new()));
            }
            Statement::ExportDefaultDeclaration(decl) => {
                // The `export default` keyword(s) are dropped; a named declaration keeps its name, and an
                // expression (or an anonymous declaration) is bound to the synthesized `default$`.
                let start = decl.span.start as usize;
                let (end, local) = match &decl.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        (f.span.start as usize, f.id.as_ref().map(|id| id.name.to_string()))
                    }
                    ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                        (c.span.start as usize, c.id.as_ref().map(|id| id.name.to_string()))
                    }
                    other => (other.span().start as usize, None),
                };
                if local.is_none() {
                    needs_default_binding = true;
                }
                let exported_local = local.clone().unwrap_or_else(|| "default$".to_string());
                analysis.exports.push(("default".to_string(), ExportKind::Local(exported_local)));
                analysis.default_exports.push(DefaultExport { start, end, local });
            }
            _ => {}
        }
    }
    if needs_default_binding {
        analysis.declared.push("default$".to_string());
    }

    // Declared top-level names (imports excluded), with their declaration spans.
    for symbol in scoping.iter_bindings_in(root) {
        let name = scoping.symbol_name(symbol).to_string();
        if import_locals.contains(&name) {
            continue;
        }
        let span = scoping.symbol_span(symbol);
        analysis.declared.push(name.clone());
        analysis.decl_names.push((span.start as usize, span.end as usize, name));
    }

    // References to every top-level binding (declared and imported), attributed by semantic resolution, so a
    // shadowing local of the same name is not one of them.
    for symbol in scoping.iter_bindings_in(root) {
        let name = scoping.symbol_name(symbol).to_string();
        for reference in scoping.get_resolved_references(symbol) {
            let span = nodes.get_node(reference.node_id()).span();
            let (start, end) = (span.start as usize, span.end as usize);
            analysis.refs.push((start, end, name.clone()));
            if let oxc_ast::AstKind::ObjectProperty(prop) = nodes.parent_kind(reference.node_id()) {
                if prop.shorthand {
                    analysis.shorthand_refs.insert((start, end));
                }
            }
        }
    }

    // Names read without a declaration: globals the bundle must not shadow.
    for name in scoping.root_unresolved_references().keys() {
        analysis.globals.insert(name.to_string());
    }

    // Dynamic `import('x')` anywhere in the module, with a literal specifier.
    for node in nodes.iter() {
        if let oxc_ast::AstKind::ImportExpression(import) = node.kind() {
            let Expression::StringLiteral(literal) = &import.source else {
                return Err("dynamic import with a non-literal specifier is not supported".to_string());
            };
            let span = node.span();
            analysis
                .dynamic_imports
                .push((span.start as usize, span.end as usize, literal.value.to_string()));
        }
    }
    analysis.dynamic_imports.sort();
    Ok(analysis)
}

fn module_export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::IdentifierName(n) => n.name.to_string(),
        ModuleExportName::IdentifierReference(n) => n.name.to_string(),
        ModuleExportName::StringLiteral(s) => s.value.to_string(),
    }
}

fn declaration_names(declaration: &Declaration) -> Vec<String> {
    match declaration {
        Declaration::VariableDeclaration(v) => v
            .declarations
            .iter()
            .flat_map(|d| binding_names(&d.id))
            .collect(),
        Declaration::FunctionDeclaration(f) => f.id.iter().map(|id| id.name.to_string()).collect(),
        Declaration::ClassDeclaration(c) => c.id.iter().map(|id| id.name.to_string()).collect(),
        _ => Vec::new(),
    }
}

fn binding_names(pattern: &oxc_ast::ast::BindingPattern) -> Vec<String> {
    let mut out = Vec::new();
    collect_binding_names(pattern, &mut out);
    out
}

fn collect_binding_names(pattern: &oxc_ast::ast::BindingPattern, out: &mut Vec<String>) {
    use oxc_ast::ast::BindingPattern;
    match pattern {
        BindingPattern::BindingIdentifier(id) => out.push(id.name.to_string()),
        BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_binding_names(&prop.value, out);
            }
            if let Some(rest) = &obj.rest {
                collect_binding_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(arr) => {
            for element in arr.elements.iter().flatten() {
                collect_binding_names(element, out);
            }
            if let Some(rest) = &arr.rest {
                collect_binding_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(assign) => collect_binding_names(&assign.left, out),
    }
}


/// The dependency order: depth-first post-order from the entry, following each module's import and re-export
/// specifiers in source order, then its dynamic-import targets. A module is emitted after everything it depends
/// on; a cycle is cut where it closes (ESM evaluation order for the acyclic graphs the native build produces).
///
/// Dynamic-import targets are hoisted into the bundle's scope like static ones: their top-level code runs at
/// bundle start, not on the first `import()` (a lazy initialiser would need each module wrapped in a function,
/// which the shared scope does not allow). This is documented in the report for T4.
fn dependency_order(modules: &[BundleModule], analyses: &[Analysis], entry: usize) -> Result<Vec<usize>, String> {
    let mut order = Vec::new();
    let mut state = vec![0u8; modules.len()];
    fn visit(
        m: usize,
        modules: &[BundleModule],
        analyses: &[Analysis],
        state: &mut [u8],
        order: &mut Vec<usize>,
    ) -> Result<(), String> {
        if state[m] != 0 {
            return Ok(());
        }
        state[m] = 1;
        // Static dependencies first, then dynamic-import targets: they are part of the bundle too.
        let dynamic = analyses[m].dynamic_imports.iter().map(|(_, _, spec)| spec);
        for spec in analyses[m].source_order.iter().chain(dynamic) {
            let target = *modules[m].edges.get(spec).ok_or_else(|| {
                format!("{}: unresolved import '{spec}' (not in the graph)", modules[m].label)
            })?;
            visit(target, modules, analyses, state, order)?;
        }
        state[m] = 2;
        order.push(m);
        Ok(())
    }
    visit(entry, modules, analyses, &mut state, &mut order)?;
    Ok(order)
}

/// Collect the export names of module `m` (its own exports, then `export *` names, without `default`).
fn collect_export_names(
    modules: &[BundleModule],
    analyses: &[Analysis],
    m: usize,
    out: &mut Vec<String>,
    seen: &mut HashSet<usize>,
    depth: usize,
) {
    if depth > 32 || !seen.insert(m) {
        return;
    }
    for (name, kind) in &analyses[m].exports {
        match kind {
            ExportKind::Star(source) => {
                if let Some(&target) = modules[m].edges.get(source) {
                    let mut inner = Vec::new();
                    let mut inner_seen = seen.clone();
                    collect_export_names(modules, analyses, target, &mut inner, &mut inner_seen, depth + 1);
                    for n in inner {
                        if n != "default" && !out.contains(&n) {
                            out.push(n);
                        }
                    }
                }
            }
            _ => {
                if !name.is_empty() && !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
    }
}

/// The final expression for export `name` of module `m`: a hoisted name, or a chain through imports and
/// re-exports resolved to the binding that owns it.
fn resolve_export(
    modules: &[BundleModule],
    analyses: &[Analysis],
    finals: &[HashMap<String, String>],
    m: usize,
    name: &str,
    depth: usize,
) -> Result<String, String> {
    if depth > 32 {
        return Err(format!("{}: export cycle resolving '{name}'", modules[m].label));
    }
    // Own export first.
    for (exported, kind) in &analyses[m].exports {
        if exported != name {
            continue;
        }
        return match kind {
            ExportKind::Local(local) => finals[m]
                .get(local)
                .cloned()
                .ok_or_else(|| format!("{}: export '{name}' has no declaration", modules[m].label)),
            ExportKind::Imported { imported, .. } => {
                let import = analyses[m]
                    .imports
                    .iter()
                    .find_map(|d| d.specifiers.iter().find(|s| &s.local == imported).map(|s| (d, s.clone())))
                    .ok_or_else(|| format!("{}: '{name}' is not an import", modules[m].label))?;
                let (decl, spec) = import;
                let target = *modules[m]
                    .edges
                    .get(&decl.source)
                    .ok_or_else(|| format!("{}: unresolved import '{}'", modules[m].label, decl.source))?;
                if spec.imported == "*" {
                    Ok(format!("$treaty_ns{target}"))
                } else {
                    resolve_export(modules, analyses, finals, target, &spec.imported, depth + 1)
                }
            }
            ExportKind::ReExport { source, imported } => {
                let target = *modules[m]
                    .edges
                    .get(source)
                    .ok_or_else(|| format!("{}: unresolved re-export '{source}'", modules[m].label))?;
                if imported == "*" {
                    Ok(format!("$treaty_ns{target}"))
                } else {
                    resolve_export(modules, analyses, finals, target, imported, depth + 1)
                }
            }
            ExportKind::Star(_) => continue,
        };
    }
    // Then `export *` sources, in order.
    for (_, kind) in &analyses[m].exports {
        if let ExportKind::Star(source) = kind {
            let Some(&target) = modules[m].edges.get(source) else { continue };
            if name == "default" {
                continue;
            }
            if let Ok(found) = resolve_export(modules, analyses, finals, target, name, depth + 1) {
                return Ok(found);
            }
        }
    }
    Err(format!("{}: no export named '{name}'", modules[m].label))
}

/// Apply non-overlapping `(start, end, text)` edits to `code`.
fn apply_edits(code: &str, mut edits: Vec<(usize, usize, String)>) -> Result<String, String> {
    edits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut out = String::with_capacity(code.len());
    let mut cursor = 0usize;
    for (start, end, text) in edits {
        if start < cursor {
            return Err(format!("overlapping edits at {start}..{end}"));
        }
        if end > code.len() || start > end {
            return Err(format!("edit out of range {start}..{end}"));
        }
        out.push_str(&code[cursor..start]);
        out.push_str(&text);
        cursor = end;
    }
    out.push_str(&code[cursor..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(label: &str, code: &str, edges: &[(&str, usize)]) -> BundleModule {
        BundleModule {
            label: label.to_string(),
            code: code.to_string(),
            edges: edges.iter().map(|(s, i)| (s.to_string(), *i)).collect(),
        }
    }

    #[test]
    fn format_parses_its_spellings() {
        assert_eq!(OutputFormat::parse("script"), Some(OutputFormat::Script));
        assert_eq!(OutputFormat::parse("iife"), Some(OutputFormat::Script));
        assert_eq!(OutputFormat::parse("esm"), Some(OutputFormat::Esm));
        assert_eq!(OutputFormat::parse("modules"), Some(OutputFormat::Modules));
        assert_eq!(OutputFormat::parse("bogus"), None);
    }

    /// The entry reads a binding of its dependency through an import: the bundle has one scope, so the
    /// import becomes a reference to the dependency's hoisted name and the import statement is gone.
    #[test]
    fn hoists_an_import_into_one_scope() {
        let dep = module("dep.ts", "export const dep = 41;", &[]);
        let entry = module(
            "main.ts",
            "import { dep } from './dep';\nexport const answer = dep + 1;",
            &[("./dep", 0)],
        );
        let out = bundle(&[dep, entry], 1, OutputFormat::Esm).unwrap().code;
        assert!(!out.contains("import "), "import survived:\n{out}");
        assert!(out.contains("const dep = 41;"), "dep not hoisted:\n{out}");
        assert!(out.contains("const answer = dep + 1;"), "reference not resolved:\n{out}");
        assert!(out.contains("export { answer };"), "entry export lost:\n{out}");
    }

    /// Two modules declare the same top-level name: the second gets a unique name, and the references in each
    /// module follow their own binding.
    #[test]
    fn same_named_top_level_bindings_stay_distinct() {
        let a = module("a.ts", "export const value = 'a';", &[]);
        let b = module("b.ts", "export const value = 'b';", &[]);
        let entry = module(
            "main.ts",
            "import { value as va } from './a';\nimport { value as vb } from './b';\nexport const both = va + vb;",
            &[("./a", 0), ("./b", 1)],
        );
        let out = bundle(&[a, b, entry], 2, OutputFormat::Script).unwrap().code;
        assert!(out.contains("const value = 'a';"), "{out}");
        assert!(out.contains("const value$1 = 'b';"), "second binding not renamed:\n{out}");
        assert!(out.contains("const both = value + value$1;"), "{out}");
    }

    /// A name the bundle reads as a global is never taken by a hoisted top-level declaration.
    #[test]
    fn a_global_the_bundle_reads_is_not_shadowed() {
        let lib = module("lib.ts", "export const Promise = 1; export const r = Promise;", &[]);
        let entry = module(
            "main.ts",
            "import { r } from './lib';\nexport const p = Promise.resolve(r);",
            &[("./lib", 0)],
        );
        let out = bundle(&[lib, entry], 1, OutputFormat::Script).unwrap().code;
        assert!(out.contains("const Promise$1 = 1;"), "the global Promise was shadowed:\n{out}");
        assert!(out.contains("Promise.resolve(r)"), "{out}");
    }

    /// `export *` and a re-export chain resolve to the owning binding.
    #[test]
    fn resolves_export_star_and_reexports() {
        let inner = module("inner.ts", "export function make() { return 'made'; }", &[]);
        let barrel = module("barrel.ts", "export * from './inner';", &[("./inner", 0)]);
        let entry = module(
            "main.ts",
            "import { make } from './barrel';\nexport const out = make();",
            &[("./barrel", 1)],
        );
        let out = bundle(&[inner, barrel, entry], 2, OutputFormat::Script).unwrap().code;
        assert!(out.contains("function make()"), "{out}");
        assert!(out.contains("const out = make();"), "{out}");
    }

    /// A namespace import is a live object whose getters read the hoisted bindings.
    #[test]
    fn namespace_import_is_a_live_object() {
        let dep = module("dep.ts", "export let count = 1; export function bump() { count++; }", &[]);
        let entry = module(
            "main.ts",
            "import * as ns from './dep';\nexport const read = () => ns.count;",
            &[("./dep", 0)],
        );
        let out = bundle(&[dep, entry], 1, OutputFormat::Script).unwrap().code;
        assert!(out.contains("get count() { return count; }"), "namespace getter missing:\n{out}");
        // The namespace binding is rewritten to the dependency's namespace object, so the member read stays live.
        assert!(out.contains("$treaty_ns0.count"), "member access not rewritten:\n{out}");
    }

    /// Shorthand properties keep their key when the value is renamed.
    #[test]
    fn shorthand_property_keeps_its_key() {
        let dep = module("dep.ts", "export const value = 3;", &[]);
        let entry = module(
            "main.ts",
            "import { value } from './dep';\nexport const box = { value };",
            &[("./dep", 0)],
        );
        let out = bundle(&[dep, entry], 1, OutputFormat::Script).unwrap().code;
        assert!(out.contains("const box = { value: value };") || out.contains("{ value: value }"), "{out}");
    }

    /// `export default <expression>` becomes a hoisted binding the importer can read.
    #[test]
    fn default_export_expression_is_bound() {
        let dep = module("dep.ts", "export default 7 * 6;", &[]);
        let entry = module(
            "main.ts",
            "import answer from './dep';\nexport const out = answer;",
            &[("./dep", 0)],
        );
        let out = bundle(&[dep, entry], 1, OutputFormat::Script).unwrap().code;
        assert!(out.contains("const default$ = 7 * 6;"), "{out}");
        assert!(out.contains("const out = default$;"), "{out}");
    }

    /// A dynamic import is inlined as a promise of the target's namespace.
    #[test]
    fn dynamic_import_is_inlined_as_a_namespace_promise() {
        let lazy = module("lazy.ts", "export const name = 'lazy';", &[]);
        let entry = module(
            "main.ts",
            "export const load = () => import('./lazy');",
            &[("./lazy", 0)],
        );
        let out = bundle(&[lazy, entry], 1, OutputFormat::Script).unwrap().code;
        assert!(!out.contains("import("), "dynamic import survived:\n{out}");
        assert!(out.contains("Promise.resolve($treaty_ns0)"), "{out}");
    }

    /// A shadowing local is not the top-level binding, so it is left alone.
    #[test]
    fn a_shadowing_local_is_not_renamed() {
        let dep = module("dep.ts", "export const x = 1;", &[]);
        let entry = module(
            "main.ts",
            "import { x as outer } from './dep';\nexport function f(outer2) { const x = 2; return x + outer; }",
            &[("./dep", 0)],
        );
        let out = bundle(&[dep, entry], 1, OutputFormat::Script).unwrap().code;
        assert!(out.contains("const x = 2;"), "the local was renamed:\n{out}");
        assert!(out.contains("return x + x"), "the outer reference did not resolve to the dep binding:\n{out}");
    }

    /// The script format adds no globals: the bundle is an IIFE in strict mode.
    #[test]
    fn script_format_is_a_strict_iife() {
        let entry = module("main.ts", "export const a = 1;", &[]);
        let out = bundle(&[entry], 0, OutputFormat::Script).unwrap().code;
        assert!(out.starts_with("(function () {"), "{out}");
        assert!(out.contains("\"use strict\";"), "{out}");
        assert!(out.trim_end().ends_with("})();"), "{out}");
    }

    /// A bundle must be valid JavaScript: the generated output parses.
    #[test]
    fn output_parses_as_a_module() {
        let dep = module("dep.ts", "export class Base { m() { return 1; } }", &[]);
        let entry = module(
            "main.ts",
            "import { Base } from './dep';\nexport class Sub extends Base { n() { return this.m() + 1; } }",
            &[("./dep", 0)],
        );
        let out = bundle(&[dep, entry], 1, OutputFormat::Esm).unwrap().code;
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &out, SourceType::mjs()).parse();
        assert!(parsed.errors.is_empty(), "bundle does not parse: {:?}\n{out}", parsed.errors);
    }
}
