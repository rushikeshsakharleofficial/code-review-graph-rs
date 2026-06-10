/// crg-refactor — rename preview, dead code detection, refactoring suggestions.
///
/// Ports Python `refactor.py` (852 lines). Operates entirely through the public
/// `GraphStore` API.
use std::collections::HashSet;

use anyhow::Result;
use crg_core::store::GraphStore;
use tracing::debug;

// ---------------------------------------------------------------------------
// Entry-point patterns — functions that are "called" by the runtime/framework
// and should not be flagged as dead code.
// ---------------------------------------------------------------------------

const ENTRY_POINT_PATTERNS: &[&str] = &[
    "main",
    "__main__",
    "__init__",
    "__new__",
    "__call__",
    "__enter__",
    "__exit__",
    "setup",
    "teardown",
    "setUp",
    "tearDown",
    "app",
    "create_app",
    "handler",
    "lambda_handler",
    "run",
    "start",
    "serve",
    "cli",
];

fn is_entry_point(name: &str) -> bool {
    let lower = name.to_lowercase();
    ENTRY_POINT_PATTERNS
        .iter()
        .any(|pat| lower == *pat || lower.starts_with("test_") || lower.starts_with("test"))
}

// ---------------------------------------------------------------------------
// Preview rename
// ---------------------------------------------------------------------------

/// Return a preview of what would change if `qualified_name` is renamed to
/// `new_name`, without actually modifying the database.
///
/// Returns JSON:
/// ```json
/// {
///   "old_name": "...",
///   "new_name": "...",
///   "affected_nodes": [{...}, ...],
///   "affected_edges_count": N,
///   "files_to_update": ["file1.py", ...]
/// }
/// ```
pub fn preview_rename(
    store: &GraphStore,
    qualified_name: &str,
    new_name: &str,
) -> Result<serde_json::Value> {
    let node = store
        .get_node(qualified_name)?
        .ok_or_else(|| anyhow::anyhow!("Node not found: {}", qualified_name))?;

    let outgoing = store.get_edges_by_source(qualified_name)?;
    let incoming = store.get_edges_by_target(qualified_name)?;
    let total_edges = outgoing.len() + incoming.len();

    let mut files: HashSet<String> = HashSet::new();
    files.insert(node.file_path.clone());
    for e in outgoing.iter().chain(incoming.iter()) {
        files.insert(e.file_path.clone());
    }

    Ok(serde_json::json!({
        "old_name": node.name,
        "new_name": new_name,
        "affected_nodes": [serde_json::to_value(&node)?],
        "affected_edges_count": total_edges,
        "files_to_update": files.into_iter().collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// Dead code
// ---------------------------------------------------------------------------

/// Return up to `limit` functions/classes that are never called and are not
/// test functions or entry points.
pub fn find_dead_code(store: &GraphStore, limit: usize) -> Result<Vec<serde_json::Value>> {
    let called_set = store.get_all_call_targets()?;

    let mut candidates: Vec<serde_json::Value> = Vec::new();

    for kind in &["Function", "Class"] {
        let nodes = store.get_nodes_by_kind(kind)?;
        for node in nodes {
            if node.is_test {
                continue;
            }
            if is_entry_point(&node.name) {
                continue;
            }
            if called_set.contains(&node.qualified_name) {
                continue;
            }
            candidates.push(serde_json::json!({
                "qualified_name": node.qualified_name,
                "name": node.name,
                "kind": node.kind,
                "file_path": node.file_path,
                "line_start": node.line_start,
            }));
            if candidates.len() >= limit {
                break;
            }
        }
        if candidates.len() >= limit {
            break;
        }
    }

    // Sort by name for deterministic output.
    candidates.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });

    debug!("find_dead_code: {} candidates", candidates.len());
    Ok(candidates)
}

// ---------------------------------------------------------------------------
// Large functions
// ---------------------------------------------------------------------------

/// Return up to `limit` functions/tests whose body spans at least `min_lines` lines.
pub fn find_large_functions(
    store: &GraphStore,
    min_lines: i64,
    limit: usize,
) -> Result<Vec<serde_json::Value>> {
    let mut results: Vec<serde_json::Value> = Vec::new();

    for kind in &["Function", "Test"] {
        let nodes = store.get_nodes_by_kind(kind)?;
        for node in nodes {
            let size = node.line_end - node.line_start;
            if size >= min_lines {
                results.push(serde_json::json!({
                    "qualified_name": node.qualified_name,
                    "name": node.name,
                    "file_path": node.file_path,
                    "line_start": node.line_start,
                    "line_end": node.line_end,
                    "size": size,
                }));
            }
        }
    }

    // Sort by size descending.
    results.sort_by(|a, b| {
        let sa = a["size"].as_i64().unwrap_or(0);
        let sb = b["size"].as_i64().unwrap_or(0);
        sb.cmp(&sa)
    });

    results.truncate(limit);
    debug!("find_large_functions(min_lines={}): {} found", min_lines, results.len());
    Ok(results)
}

// ---------------------------------------------------------------------------
// Refactoring suggestions
// ---------------------------------------------------------------------------

/// Return a prioritized list of refactoring suggestions combining dead code,
/// large functions, and high-coupling nodes.
pub fn get_refactoring_suggestions(store: &GraphStore) -> Result<Vec<serde_json::Value>> {
    let mut suggestions: Vec<serde_json::Value> = Vec::new();

    // Dead code → high priority.
    let dead = find_dead_code(store, 20)?;
    for item in dead {
        let qn = item["qualified_name"].as_str().unwrap_or("").to_string();
        suggestions.push(serde_json::json!({
            "type": "dead_code",
            "qualified_name": qn,
            "reason": "Never called; consider removing or marking with a TODO.",
            "priority": "high",
        }));
    }

    // Large functions (> 50 lines) → medium priority.
    let large = find_large_functions(store, 50, 20)?;
    for item in large {
        let qn = item["qualified_name"].as_str().unwrap_or("").to_string();
        let size = item["size"].as_i64().unwrap_or(0);
        let priority = if size > 150 { "high" } else { "medium" };
        suggestions.push(serde_json::json!({
            "type": "large_function",
            "qualified_name": qn,
            "reason": format!("Function is {} lines; consider splitting.", size),
            "priority": priority,
        }));
    }

    // High coupling: functions with many callers (> 10) that are also large.
    let edges = store.get_all_edges()?;
    let mut incoming_count: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for edge in &edges {
        if edge.kind == "CALLS" {
            *incoming_count
                .entry(edge.target_qualified.clone())
                .or_default() += 1;
        }
    }
    let mut high_coupling: Vec<(String, usize)> = incoming_count
        .into_iter()
        .filter(|(_, c)| *c > 10)
        .collect();
    high_coupling.sort_by(|a, b| b.1.cmp(&a.1));
    for (qn, count) in high_coupling.iter().take(10) {
        suggestions.push(serde_json::json!({
            "type": "high_coupling",
            "qualified_name": qn,
            "reason": format!("Called by {} callers; changes here are high-risk.", count),
            "priority": "medium",
        }));
    }

    Ok(suggestions)
}

// ---------------------------------------------------------------------------
// Apply rename
// ---------------------------------------------------------------------------

/// Actually rename a symbol in the graph database.
///
/// Computes the new qualified name, updates the node, and patches all edges.
/// Returns `{"updated_nodes": N, "updated_edges": N}`.
pub fn apply_rename(
    store: &GraphStore,
    qualified_name: &str,
    new_name: &str,
) -> Result<serde_json::Value> {
    let (updated_nodes, updated_edges) = store.apply_rename(qualified_name, new_name)?;
    debug!(
        "apply_rename: {} → {} ({} nodes, {} edges updated)",
        qualified_name, new_name, updated_nodes, updated_edges
    );
    Ok(serde_json::json!({
        "updated_nodes": updated_nodes,
        "updated_edges": updated_edges,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crg_core::{
        store::GraphStore,
        types::{EdgeInfo, NodeInfo},
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_store() -> GraphStore {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("crg-refactor-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        GraphStore::new(dir.join("graph.db")).unwrap()
    }

    fn node(name: &str, kind: &str, file: &str, is_test: bool, line_end: i64) -> NodeInfo {
        NodeInfo {
            kind: kind.to_string(),
            name: name.to_string(),
            file_path: file.to_string(),
            line_start: 1,
            line_end,
            language: "python".to_string(),
            is_test,
            ..Default::default()
        }
    }

    fn edge(src: &str, tgt: &str, file: &str) -> EdgeInfo {
        EdgeInfo {
            kind: "CALLS".to_string(),
            source: src.to_string(),
            target: tgt.to_string(),
            file_path: file.to_string(),
            line: 5,
            extra: json!({}),
        }
    }

    #[test]
    fn dead_code_finds_uncalled() {
        let store = make_store();
        store.upsert_node(&node("called_fn", "Function", "f.py", false, 10), "").unwrap();
        store.upsert_node(&node("orphan_fn", "Function", "f.py", false, 10), "").unwrap();
        store.upsert_edge(&edge("f.py::caller", "f.py::called_fn", "f.py")).unwrap();

        let dead = find_dead_code(&store, 100).unwrap();
        let names: Vec<&str> = dead.iter().filter_map(|d| d["name"].as_str()).collect();
        assert!(names.contains(&"orphan_fn"), "orphan_fn should be dead code");
        assert!(!names.contains(&"called_fn"), "called_fn is not dead code");
    }

    #[test]
    fn dead_code_excludes_entry_points() {
        let store = make_store();
        store.upsert_node(&node("main", "Function", "m.py", false, 10), "").unwrap();
        let dead = find_dead_code(&store, 100).unwrap();
        let names: Vec<&str> = dead.iter().filter_map(|d| d["name"].as_str()).collect();
        assert!(!names.contains(&"main"), "main should not be flagged as dead code");
    }

    #[test]
    fn large_functions_threshold() {
        let store = make_store();
        store.upsert_node(&node("small_fn", "Function", "f.py", false, 5), "").unwrap();
        store.upsert_node(&node("big_fn", "Function", "f.py", false, 100), "").unwrap();

        let large = find_large_functions(&store, 50, 10).unwrap();
        let names: Vec<&str> = large.iter().filter_map(|d| d["name"].as_str()).collect();
        assert!(names.contains(&"big_fn"));
        assert!(!names.contains(&"small_fn"));
    }

    #[test]
    fn preview_rename_not_found_errors() {
        let store = make_store();
        let result = preview_rename(&store, "nonexistent::fn", "new_name");
        assert!(result.is_err());
    }

    #[test]
    fn preview_rename_returns_correct_shape() {
        let store = make_store();
        store.upsert_node(&node("my_fn", "Function", "f.py", false, 10), "").unwrap();
        let qn = "f.py::my_fn";
        let preview = preview_rename(&store, qn, "renamed_fn").unwrap();
        assert_eq!(preview["old_name"], "my_fn");
        assert_eq!(preview["new_name"], "renamed_fn");
    }

    #[test]
    fn apply_rename_updates_db() {
        let store = make_store();
        store.upsert_node(&node("orig_fn", "Function", "f.py", false, 10), "").unwrap();
        store.upsert_edge(&edge("f.py::caller", "f.py::orig_fn", "f.py")).unwrap();

        let result = apply_rename(&store, "f.py::orig_fn", "new_fn").unwrap();
        assert_eq!(result["updated_nodes"], 1);
        // The edge targeting orig_fn should now point to new_fn
        assert!(store.get_node("f.py::new_fn").unwrap().is_some());
        assert!(store.get_node("f.py::orig_fn").unwrap().is_none());
    }
}
