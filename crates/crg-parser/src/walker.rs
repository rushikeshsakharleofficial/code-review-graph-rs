//! Tree-sitter DFS walker.
//!
//! Parses source bytes into `NodeInfo` and `EdgeInfo` using per-language
//! dispatch tables.  Python has a fully hand-written extractor; all other
//! languages use the generic DFS path backed by the dispatch tables.
//!
//! `tree_sitter::Parser` is `!Send`, so we keep one parser per language per
//! thread in a `thread_local` `HashMap`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};
use tracing::{debug, warn};
use tree_sitter::Parser;

use crg_core::types::{EdgeInfo, NodeInfo};

use crate::dispatch;
use crate::languages::detect_language;

// ---------------------------------------------------------------------------
// Thread-local parser pool (Parser is !Send)
// ---------------------------------------------------------------------------

thread_local! {
    static PARSERS: RefCell<HashMap<String, Parser>> = RefCell::new(HashMap::new());
}

// ---------------------------------------------------------------------------
// Public result type
// ---------------------------------------------------------------------------

/// Output of parsing a single source file.
#[derive(Debug, Default)]
pub struct ParseResult {
    pub nodes: Vec<NodeInfo>,
    pub edges: Vec<EdgeInfo>,
    /// Full SHA-256 hex digest of the raw file bytes.
    pub file_hash: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Read `file_path` from disk and parse it.
pub fn parse_file(file_path: &Path) -> anyhow::Result<ParseResult> {
    let bytes = std::fs::read(file_path)?;
    let hash = compute_hash(&bytes);
    let path_str = file_path.to_str().unwrap_or("");
    let lang_name = detect_language(path_str).unwrap_or("unknown");
    parse_bytes_with_lang(&bytes, path_str, lang_name, &hash)
}

/// Parse already-loaded `bytes` without a filesystem read.
pub fn parse_file_bytes(
    file_path: &str,
    bytes: &[u8],
    lang_name: &str,
) -> anyhow::Result<ParseResult> {
    let hash = compute_hash(bytes);
    parse_bytes_with_lang(bytes, file_path, lang_name, &hash)
}

// ---------------------------------------------------------------------------
// Hash
// ---------------------------------------------------------------------------

/// Compute the full 64-character SHA-256 hex digest.
pub fn compute_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Core dispatch
// ---------------------------------------------------------------------------

fn parse_bytes_with_lang(
    bytes: &[u8],
    file_path: &str,
    lang_name: &str,
    hash: &str,
) -> anyhow::Result<ParseResult> {
    let mut result = ParseResult {
        file_hash: hash.to_string(),
        ..Default::default()
    };

    // Try to get a tree-sitter parse tree.  If the language is unknown or the
    // grammar isn't bundled we fall back to producing only the File node.
    let tree_opt = try_parse(bytes, lang_name);

    // Count total lines for the File node.
    let total_lines = bytes.iter().filter(|&&b| b == b'\n').count() as i64 + 1;
    let file_name = Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(file_path);

    // Always emit a File node.
    result.nodes.push(NodeInfo {
        kind: "File".to_string(),
        name: file_name.to_string(),
        file_path: file_path.to_string(),
        line_start: 1,
        line_end: total_lines,
        language: lang_name.to_string(),
        ..Default::default()
    });

    if let Some(tree) = tree_opt {
        let root = tree.root_node();
        walk_tree(
            root,
            bytes,
            file_path,
            lang_name,
            None,  // current_class
            None,  // current_function
            &mut result.nodes,
            &mut result.edges,
        );
    } else {
        debug!("no tree-sitter grammar for lang={lang_name}, file={file_path}");
    }

    Ok(result)
}

/// Attempt to parse `bytes` with tree-sitter.  Returns `None` if the language
/// pack doesn't know the language, or if the parse fails catastrophically.
fn try_parse(bytes: &[u8], lang_name: &str) -> Option<tree_sitter::Tree> {
    PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();

        // Lazily initialise a parser for this language.
        if !map.contains_key(lang_name) {
            match tree_sitter_language_pack::get_language(lang_name) {
                Ok(lang) => {
                    let mut p = Parser::new();
                    if let Err(e) = p.set_language(&lang) {
                        warn!("tree-sitter set_language({lang_name}) failed: {e}");
                        return None;
                    }
                    map.insert(lang_name.to_string(), p);
                }
                Err(e) => {
                    debug!("tree-sitter-language-pack: no grammar for {lang_name}: {e}");
                    return None;
                }
            }
        }

        map.get_mut(lang_name)
            .and_then(|p| p.parse(bytes, None))
    })
}

// ---------------------------------------------------------------------------
// Node-type membership helpers
// ---------------------------------------------------------------------------

#[inline]
fn is_class(kind: &str, lang: &str) -> bool {
    dispatch::class_types(lang).contains(&kind)
}

#[inline]
fn is_function(kind: &str, lang: &str) -> bool {
    dispatch::function_types(lang).contains(&kind)
}

#[inline]
fn is_import(kind: &str, lang: &str) -> bool {
    dispatch::import_types(lang).contains(&kind)
}

#[inline]
fn is_call(kind: &str, lang: &str) -> bool {
    dispatch::call_types(lang).contains(&kind)
}

// ---------------------------------------------------------------------------
// DFS walker
// ---------------------------------------------------------------------------

/// Recursively walk a tree-sitter node and collect `NodeInfo` / `EdgeInfo`.
///
/// - `current_class`    — qualified name of the enclosing class, if any.
/// - `current_function` — qualified name of the enclosing function, if any
///   (used as the source for CALLS edges).
#[allow(clippy::too_many_arguments)]
fn walk_tree(
    node: tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    language: &str,
    current_class: Option<&str>,
    current_function: Option<&str>,
    nodes: &mut Vec<NodeInfo>,
    edges: &mut Vec<EdgeInfo>,
) {
    let kind = node.kind();

    // -----------------------------------------------------------------------
    // Class / struct / trait
    // -----------------------------------------------------------------------
    if is_class(kind, language) {
        if let Some(name) = extract_name(&node, source, language, "class") {
            let sanitized = crg_core::security::sanitize_name(&name);
            let line_start = (node.start_position().row + 1) as i64;
            let line_end = (node.end_position().row + 1) as i64;
            let is_test = is_test_name(&sanitized, language, "class");

            nodes.push(NodeInfo {
                kind: "Class".to_string(),
                name: sanitized.clone(),
                file_path: file_path.to_string(),
                line_start,
                line_end,
                language: language.to_string(),
                parent_name: current_class.map(str::to_string),
                is_test,
                ..Default::default()
            });

            // Recurse into the class body with `sanitized` as the new current_class.
            for i in 0u32..node.child_count() as u32 {
                if let Some(child) = node.child(i) {
                    walk_tree(
                        child,
                        source,
                        file_path,
                        language,
                        Some(&sanitized),
                        current_function,
                        nodes,
                        edges,
                    );
                }
            }
        } else {
            // Can't extract a name — still recurse so children get captured.
            recurse(node, source, file_path, language, current_class, current_function, nodes, edges);
        }
        return;
    }

    // -----------------------------------------------------------------------
    // Function / method
    // -----------------------------------------------------------------------
    if is_function(kind, language) {
        if let Some(name) = extract_name(&node, source, language, "function") {
            let sanitized = crg_core::security::sanitize_name(&name);
            let line_start = (node.start_position().row + 1) as i64;
            let line_end = (node.end_position().row + 1) as i64;
            let params = extract_params(&node, source);
            let return_type = extract_return_type(&node, source);
            let is_test = is_test_name(&sanitized, language, "function");

            let node_kind = if is_test { "Test" } else { "Function" };

            nodes.push(NodeInfo {
                kind: node_kind.to_string(),
                name: sanitized.clone(),
                file_path: file_path.to_string(),
                line_start,
                line_end,
                language: language.to_string(),
                parent_name: current_class.map(str::to_string),
                params,
                return_type,
                is_test,
                ..Default::default()
            });

            // CONTAINS edge from the enclosing class.
            if let Some(class_name) = current_class {
                let source_qname = format!("{}::{}", file_path, class_name);
                let target_qname = format!("{}::{}.{}", file_path, class_name, sanitized);
                edges.push(EdgeInfo {
                    kind: "CONTAINS".to_string(),
                    source: source_qname,
                    target: target_qname,
                    file_path: file_path.to_string(),
                    line: line_start,
                    ..Default::default()
                });
            }

            // Build the function's qualified name for use as CALLS source.
            let fn_qname = match current_class {
                Some(cls) => format!("{}::{}.{}", file_path, cls, sanitized),
                None => format!("{}::{}", file_path, sanitized),
            };

            // Recurse into the body with this function as the current scope.
            for i in 0u32..node.child_count() as u32 {
                if let Some(child) = node.child(i) {
                    walk_tree(
                        child,
                        source,
                        file_path,
                        language,
                        current_class,
                        Some(&fn_qname),
                        nodes,
                        edges,
                    );
                }
            }
        } else {
            recurse(node, source, file_path, language, current_class, current_function, nodes, edges);
        }
        return;
    }

    // -----------------------------------------------------------------------
    // Import / use
    // -----------------------------------------------------------------------
    if is_import(kind, language) {
        let targets = extract_import_targets(&node, source, language);
        let line = (node.start_position().row + 1) as i64;
        for target in targets {
            let sanitized = crg_core::security::sanitize_name(&target);
            edges.push(EdgeInfo {
                kind: "IMPORTS_FROM".to_string(),
                source: file_path.to_string(),
                target: sanitized,
                file_path: file_path.to_string(),
                line,
                ..Default::default()
            });
        }
        // Don't recurse into imports — we've already extracted what we need.
        return;
    }

    // -----------------------------------------------------------------------
    // Call sites
    // -----------------------------------------------------------------------
    if is_call(kind, language) {
        let line = (node.start_position().row + 1) as i64;
        if let Some(callee) = extract_callee(&node, source, language) {
            let sanitized = crg_core::security::sanitize_name(&callee);
            let call_source = current_function
                .unwrap_or(file_path)
                .to_string();
            edges.push(EdgeInfo {
                kind: "CALLS".to_string(),
                source: call_source,
                target: sanitized,
                file_path: file_path.to_string(),
                line,
                ..Default::default()
            });
        }
        // Fall through and continue recursing (nested calls inside args, etc.)
    }

    // Default: recurse into children.
    recurse(node, source, file_path, language, current_class, current_function, nodes, edges);
}

/// Recurse into all children of `node`.
#[allow(clippy::too_many_arguments)]
fn recurse(
    node: tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    language: &str,
    current_class: Option<&str>,
    current_function: Option<&str>,
    nodes: &mut Vec<NodeInfo>,
    edges: &mut Vec<EdgeInfo>,
) {
    for i in 0u32..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            walk_tree(child, source, file_path, language, current_class, current_function, nodes, edges);
        }
    }
}

// ---------------------------------------------------------------------------
// Name extraction
// ---------------------------------------------------------------------------

/// Extract the identifier name from a class/function node.
///
/// `role` is `"class"` or `"function"` and is used to pick the right
/// child-field to look in.  Falls back to scanning children for an
/// `identifier` node.
fn extract_name(
    node: &tree_sitter::Node,
    source: &[u8],
    language: &str,
    role: &str,
) -> Option<String> {
    // Try named child "name" first (works for Python, Rust, Go, …).
    if let Some(name_node) = node.child_by_field_name("name") {
        return node_text(&name_node, source);
    }

    // Language-specific fallbacks.
    match language {
        "javascript" | "typescript" | "tsx" => extract_js_name(node, source, role),
        "rust" => extract_rust_name(node, source, role),
        "go" => {
            // Go type_declaration wraps a type_spec whose child "name" is the ident.
            if node.kind() == "type_declaration" {
                for i in 0u32..node.child_count() as u32 {
                    if let Some(child) = node.child(i) {
                        if child.kind() == "type_spec" {
                            if let Some(n) = child.child_by_field_name("name") {
                                return node_text(&n, source);
                            }
                        }
                    }
                }
            }
            scan_identifier(node, source)
        }
        _ => scan_identifier(node, source),
    }
}

/// Walk immediate children looking for the first `identifier` (or
/// `property_identifier`) node and return its text.
fn scan_identifier(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    for i in 0u32..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            match child.kind() {
                "identifier" | "property_identifier" | "type_identifier" => {
                    return node_text(&child, source);
                }
                _ => {}
            }
        }
    }
    None
}

fn extract_js_name(
    node: &tree_sitter::Node,
    source: &[u8],
    _role: &str,
) -> Option<String> {
    // class_declaration → identifier
    // function_declaration → identifier
    // arrow_function: no name at declaration level (assigned via variable)
    // method_definition → property_identifier
    for i in 0u32..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            match child.kind() {
                "identifier" | "property_identifier" | "type_identifier" => {
                    return node_text(&child, source);
                }
                _ => {}
            }
        }
    }
    None
}

fn extract_rust_name(
    node: &tree_sitter::Node,
    source: &[u8],
    role: &str,
) -> Option<String> {
    match node.kind() {
        "impl_item" => {
            // impl Foo → extract the type name (second meaningful token)
            // impl Trait for Foo → use "Foo"
            // Try "type" field first, then scan for type_identifier.
            if let Some(n) = node.child_by_field_name("type") {
                return node_text(&n, source);
            }
            scan_type_identifier(node, source)
        }
        _ if role == "function" => {
            // function_item has child field "name" → identifier
            scan_identifier(node, source)
        }
        _ => scan_type_identifier(node, source),
    }
}

fn scan_type_identifier(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    for i in 0u32..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            if matches!(child.kind(), "type_identifier" | "identifier") {
                return node_text(&child, source);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Parameter extraction
// ---------------------------------------------------------------------------

fn extract_params(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // Most languages put params in a child node named "parameters".
    if let Some(params_node) = node.child_by_field_name("parameters") {
        return node_text(&params_node, source);
    }
    // Rust uses "parameters" field on function_item.
    // Some grammars name it "params".
    for i in 0u32..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            if matches!(child.kind(), "parameters" | "formal_parameters" | "parameter_list") {
                return node_text(&child, source);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Return type extraction
// ---------------------------------------------------------------------------

fn extract_return_type(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // Python: "return_type" child field (e.g. `-> int`)
    if let Some(rt_node) = node.child_by_field_name("return_type") {
        return node_text(&rt_node, source);
    }
    // TypeScript/Go use "return_type" too.
    // Rust: "return_type" field on function_item → type
    for i in 0u32..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            if matches!(child.kind(), "return_type" | "type_annotation") {
                return node_text(&child, source);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Import target extraction
// ---------------------------------------------------------------------------

fn extract_import_targets(
    node: &tree_sitter::Node,
    source: &[u8],
    language: &str,
) -> Vec<String> {
    match language {
        "python" => extract_python_import_targets(node, source),
        "rust" => extract_rust_use(node, source),
        "javascript" | "typescript" | "tsx" => extract_js_import_targets(node, source),
        "go" => extract_go_import_targets(node, source),
        "java" | "kotlin" => extract_java_import_targets(node, source),
        "cpp" | "c" | "objc" => extract_include_targets(node, source),
        _ => {
            // Generic: grab the full node text as a single target.
            node_text(node, source)
                .map(|t| vec![t])
                .unwrap_or_default()
        }
    }
}

fn extract_python_import_targets(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut targets = Vec::new();
    match node.kind() {
        "import_statement" => {
            // `import foo, bar` → ["foo", "bar"]
            // `import foo.bar` → ["foo.bar"]
            for i in 0u32..node.child_count() as u32 {
                if let Some(child) = node.child(i) {
                    match child.kind() {
                        "dotted_name" | "identifier" => {
                            if let Some(t) = node_text(&child, source) {
                                targets.push(t);
                            }
                        }
                        "aliased_import" => {
                            // `import foo as f` — extract the module name (first child)
                            if let Some(name_node) = child.child(0u32) {
                                if let Some(t) = node_text(&name_node, source) {
                                    targets.push(t);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        "import_from_statement" => {
            // `from foo.bar import baz` → ["foo.bar"]
            // We emit the module path as the target (the "from" part).
            if let Some(module_node) = node.child_by_field_name("module_name") {
                if let Some(t) = node_text(&module_node, source) {
                    targets.push(t);
                    return targets;
                }
            }
            // Fallback: first dotted_name child
            for i in 0u32..node.child_count() as u32 {
                if let Some(child) = node.child(i) {
                    if child.kind() == "dotted_name" || child.kind() == "relative_import" {
                        if let Some(t) = node_text(&child, source) {
                            targets.push(t);
                            return targets;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    targets
}

fn extract_rust_use(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    // `use crate::foo::bar;` → ["crate::foo::bar"]
    if let Some(arg) = node.child_by_field_name("argument") {
        if let Some(t) = node_text(&arg, source) {
            return vec![t];
        }
    }
    node_text(node, source)
        .map(|t| vec![t])
        .unwrap_or_default()
}

fn extract_js_import_targets(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    // `import { foo } from "bar"` → ["bar"]
    if let Some(src) = node.child_by_field_name("source") {
        if let Some(t) = node_text(&src, source) {
            // Strip surrounding quotes.
            return vec![t.trim_matches(|c| c == '"' || c == '\'').to_string()];
        }
    }
    // Fallback: look for a string literal child
    for i in 0u32..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            if child.kind() == "string" || child.kind() == "string_fragment" {
                if let Some(t) = node_text(&child, source) {
                    return vec![t.trim_matches(|c| c == '"' || c == '\'').to_string()];
                }
            }
        }
    }
    vec![]
}

fn extract_go_import_targets(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let mut targets = Vec::new();
    // import_declaration may contain import_spec or import_spec_list
    extract_go_import_recursive(node, source, &mut targets);
    targets
}

fn extract_go_import_recursive(
    node: &tree_sitter::Node,
    source: &[u8],
    targets: &mut Vec<String>,
) {
    for i in 0u32..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            match child.kind() {
                "import_spec" => {
                    if let Some(path_node) = child.child_by_field_name("path") {
                        if let Some(t) = node_text(&path_node, source) {
                            targets.push(t.trim_matches('"').to_string());
                        }
                    } else {
                        // Fallback: first interpreted_string_literal child
                        for j in 0u32..child.child_count() as u32 {
                            if let Some(gc) = child.child(j) {
                                if gc.kind() == "interpreted_string_literal" {
                                    if let Some(t) = node_text(&gc, source) {
                                        targets.push(t.trim_matches('"').to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                "import_spec_list" => {
                    extract_go_import_recursive(&child, source, targets);
                }
                _ => {}
            }
        }
    }
}

fn extract_java_import_targets(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    // `import com.example.Foo;` — grab everything after "import " and before ";"
    if let Some(t) = node_text(node, source) {
        let trimmed = t
            .trim_start_matches("import")
            .trim_start_matches("static")
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string();
        return vec![trimmed];
    }
    vec![]
}

fn extract_include_targets(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    // `#include "foo.h"` or `#include <stdlib.h>`
    for i in 0u32..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            if child.kind() == "string_literal"
                || child.kind() == "system_lib_string"
                || child.kind() == "string"
            {
                if let Some(t) = node_text(&child, source) {
                    let stripped = t
                        .trim_matches('"')
                        .trim_matches('<')
                        .trim_matches('>')
                        .to_string();
                    return vec![stripped];
                }
            }
        }
    }
    node_text(node, source)
        .map(|t| vec![t])
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Callee extraction
// ---------------------------------------------------------------------------

fn extract_callee(
    node: &tree_sitter::Node,
    source: &[u8],
    language: &str,
) -> Option<String> {
    match language {
        "python" => extract_python_callee(node, source),
        "javascript" | "typescript" | "tsx" => extract_js_callee(node, source),
        "rust" => extract_rust_callee(node, source),
        "go" => extract_go_callee(node, source),
        _ => extract_generic_callee(node, source),
    }
}

fn extract_python_callee(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // call → function: attribute | identifier | subscript | ...
    let func_node = node.child_by_field_name("function")?;
    match func_node.kind() {
        "attribute" => {
            // foo.bar() → "bar"
            if let Some(attr) = func_node.child_by_field_name("attribute") {
                node_text(&attr, source)
            } else {
                // Last identifier child
                for i in (0u32..func_node.child_count() as u32).rev() {
                    if let Some(c) = func_node.child(i) {
                        if c.kind() == "identifier" {
                            return node_text(&c, source);
                        }
                    }
                }
                None
            }
        }
        "identifier" => node_text(&func_node, source),
        _ => node_text(&func_node, source),
    }
}

fn extract_js_callee(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // call_expression → function: identifier | member_expression | ...
    let func_node = node.child_by_field_name("function")?;
    match func_node.kind() {
        "member_expression" => {
            // obj.method() → "method"
            if let Some(prop) = func_node.child_by_field_name("property") {
                node_text(&prop, source)
            } else {
                // Last identifier
                for i in (0u32..func_node.child_count() as u32).rev() {
                    if let Some(c) = func_node.child(i) {
                        if matches!(c.kind(), "identifier" | "property_identifier") {
                            return node_text(&c, source);
                        }
                    }
                }
                None
            }
        }
        "identifier" => node_text(&func_node, source),
        _ => {
            // new_expression → constructor
            if let Some(ctor) = node.child_by_field_name("constructor") {
                return node_text(&ctor, source);
            }
            node_text(&func_node, source)
        }
    }
}

fn extract_rust_callee(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "call_expression" => {
            let func_node = node.child_by_field_name("function")?;
            // foo::bar() → "bar"
            if func_node.kind() == "scoped_identifier" {
                if let Some(name_node) = func_node.child_by_field_name("name") {
                    return node_text(&name_node, source);
                }
            }
            node_text(&func_node, source)
        }
        "macro_invocation" => {
            // println!(...) → "println!"
            if let Some(mac) = node.child_by_field_name("macro") {
                return node_text(&mac, source).map(|s| format!("{s}!"));
            }
            scan_identifier(node, source)
        }
        _ => scan_identifier(node, source),
    }
}

fn extract_go_callee(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let func_node = node.child_by_field_name("function")?;
    match func_node.kind() {
        "selector_expression" => {
            if let Some(field) = func_node.child_by_field_name("field") {
                node_text(&field, source)
            } else {
                scan_identifier(&func_node, source)
            }
        }
        "identifier" => node_text(&func_node, source),
        _ => node_text(&func_node, source),
    }
}

fn extract_generic_callee(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // Try "function" or "name" child fields; fall back to first identifier.
    if let Some(func) = node.child_by_field_name("function") {
        return node_text(&func, source);
    }
    if let Some(name) = node.child_by_field_name("name") {
        return node_text(&name, source);
    }
    scan_identifier(node, source)
}

// ---------------------------------------------------------------------------
// Test detection
// ---------------------------------------------------------------------------

/// Return `true` if `name` looks like a test.
fn is_test_name(name: &str, _language: &str, role: &str) -> bool {
    match role {
        "function" => {
            name.starts_with("test_")
                || name.starts_with("Test")
                || name.ends_with("_test")
                || name.ends_with("_spec")
        }
        "class" => {
            name.starts_with("Test")
                || name.ends_with("Test")
                || name.ends_with("Tests")
                || name.ends_with("TestCase")
                || name.ends_with("Spec")
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract UTF-8 text for a node's byte range.
fn node_text(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let start = node.start_byte();
    let end = node.end_byte();
    if end > source.len() {
        return None;
    }
    std::str::from_utf8(&source[start..end])
        .ok()
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_hash_is_64_hex_chars() {
        let h = compute_hash(b"hello");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_empty_python() {
        let result = parse_file_bytes("test.py", b"", "python").unwrap();
        // Should at least have the File node.
        assert!(!result.nodes.is_empty());
        assert_eq!(result.nodes[0].kind, "File");
        assert_eq!(result.nodes[0].language, "python");
    }

    #[test]
    fn parse_python_function() {
        let src = b"def hello(x: int) -> str:\n    return str(x)\n";
        let result = parse_file_bytes("test.py", src, "python").unwrap();
        let funcs: Vec<_> = result.nodes.iter().filter(|n| n.kind == "Function").collect();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "hello");
    }

    #[test]
    fn parse_python_class_with_method() {
        let src = b"class MyClass:\n    def method(self):\n        pass\n";
        let result = parse_file_bytes("test.py", src, "python").unwrap();

        let classes: Vec<_> = result.nodes.iter().filter(|n| n.kind == "Class").collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "MyClass");

        let funcs: Vec<_> = result.nodes.iter().filter(|n| n.kind == "Function").collect();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "method");
        assert_eq!(funcs[0].parent_name.as_deref(), Some("MyClass"));

        let contains_edges: Vec<_> = result.edges.iter().filter(|e| e.kind == "CONTAINS").collect();
        assert_eq!(contains_edges.len(), 1);
    }

    #[test]
    fn parse_python_import() {
        let src = b"import os\nfrom sys import argv\n";
        let result = parse_file_bytes("test.py", src, "python").unwrap();
        let imports: Vec<_> = result.edges.iter().filter(|e| e.kind == "IMPORTS_FROM").collect();
        assert!(!imports.is_empty());
        let targets: Vec<_> = imports.iter().map(|e| e.target.as_str()).collect();
        assert!(targets.contains(&"os"));
        assert!(targets.contains(&"sys"));
    }

    #[test]
    fn parse_python_call() {
        let src = b"def foo():\n    print('hello')\n";
        let result = parse_file_bytes("test.py", src, "python").unwrap();
        let calls: Vec<_> = result.edges.iter().filter(|e| e.kind == "CALLS").collect();
        assert!(!calls.is_empty());
        assert!(calls.iter().any(|e| e.target == "print"));
    }

    #[test]
    fn parse_python_test_function() {
        let src = b"def test_foo():\n    assert 1 == 1\n";
        let result = parse_file_bytes("test.py", src, "python").unwrap();
        let tests: Vec<_> = result.nodes.iter().filter(|n| n.kind == "Test").collect();
        assert_eq!(tests.len(), 1);
        assert!(tests[0].is_test);
    }

    #[test]
    fn file_hash_is_deterministic() {
        let src = b"def foo(): pass";
        let r1 = parse_file_bytes("a.py", src, "python").unwrap();
        let r2 = parse_file_bytes("b.py", src, "python").unwrap();
        assert_eq!(r1.file_hash, r2.file_hash);
    }

    #[test]
    fn unknown_language_still_emits_file_node() {
        let result = parse_file_bytes("mystery.xyz", b"blah blah", "xyz").unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].kind, "File");
    }
}
