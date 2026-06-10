/// crg-analysis — hub/bridge/gap/surprising-connection analysis.
///
/// Ports Python `analysis.py` (410 lines). All logic is built on the public
/// `GraphStore` API — no raw SQL beyond what the store exposes.
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use crg_core::store::GraphStore;
use tracing::debug;

// ---------------------------------------------------------------------------
// Hub nodes
// ---------------------------------------------------------------------------

/// Return the top `limit` nodes ranked by incoming CALLS edge count.
///
/// A "hub" is a widely-called function or class — a change to it has a large
/// blast radius. Returns JSON objects with fields:
/// `{qualified_name, name, kind, file_path, call_count, community_id}`
pub fn get_hub_nodes(store: &GraphStore, limit: usize) -> Result<Vec<serde_json::Value>> {
    // Count incoming CALLS edges per target.
    let edges = store.get_all_edges()?;
    let mut call_count: HashMap<String, usize> = HashMap::new();
    for edge in &edges {
        if edge.kind == "CALLS" {
            *call_count.entry(edge.target_qualified.clone()).or_default() += 1;
        }
    }

    // Sort by count desc.
    let mut ranked: Vec<(String, usize)> = call_count.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));

    let mut result = Vec::new();
    for (qn, count) in ranked.iter().take(limit) {
        if let Some(node) = store.get_node(qn)? {
            result.push(serde_json::json!({
                "qualified_name": node.qualified_name,
                "name": node.name,
                "kind": node.kind,
                "file_path": node.file_path,
                "call_count": count,
                "community_id": node.community_id,
            }));
        }
    }

    debug!("get_hub_nodes: returning {} hub nodes", result.len());
    Ok(result)
}

// ---------------------------------------------------------------------------
// Bridge nodes
// ---------------------------------------------------------------------------

/// Return the top `limit` nodes that bridge the most community boundaries.
///
/// A bridge node has callers/callees in multiple different communities.
/// Score = cross_community_edges / total_edges (ties broken by absolute count).
pub fn get_bridge_nodes(store: &GraphStore, limit: usize) -> Result<Vec<serde_json::Value>> {
    // Build community lookup: qualified_name → community_id
    let community_map = store.get_all_community_ids()?;

    // Only consider nodes that have a community assigned.
    let nodes = store.get_all_nodes(true)?;
    let nodes_with_community: Vec<_> = nodes
        .iter()
        .filter(|n| n.community_id.is_some())
        .collect();

    let mut scores: Vec<(String, f64, usize)> = Vec::new(); // (qn, score, cross_count)

    for node in &nodes_with_community {
        let my_community = node.community_id.unwrap();

        let outgoing = store.get_outgoing_targets(&node.qualified_name)?;
        let incoming = store.get_incoming_sources(&node.qualified_name)?;

        let all_neighbors: Vec<String> = outgoing
            .into_iter()
            .filter(|(_, k)| k == "CALLS" || k == "IMPORTS_FROM")
            .map(|(qn, _)| qn)
            .chain(
                incoming
                    .into_iter()
                    .filter(|(_, k)| k == "CALLS" || k == "IMPORTS_FROM")
                    .map(|(qn, _)| qn),
            )
            .collect();

        let total = all_neighbors.len();
        if total == 0 {
            continue;
        }

        let cross_count = all_neighbors
            .iter()
            .filter(|qn| {
                community_map
                    .get(*qn)
                    .and_then(|c| *c)
                    .map(|c| c != my_community)
                    .unwrap_or(false)
            })
            .count();

        if cross_count == 0 {
            continue;
        }

        let score = cross_count as f64 / total as f64;
        scores.push((node.qualified_name.clone(), score, cross_count));
    }

    // Sort: primary = score desc, secondary = cross_count desc.
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(b.2.cmp(&a.2)));

    let mut result = Vec::new();
    for (qn, score, cross_count) in scores.iter().take(limit) {
        if let Some(node) = store.get_node(qn)? {
            result.push(serde_json::json!({
                "qualified_name": node.qualified_name,
                "name": node.name,
                "kind": node.kind,
                "file_path": node.file_path,
                "community_id": node.community_id,
                "bridge_score": score,
                "cross_community_edges": cross_count,
            }));
        }
    }

    debug!("get_bridge_nodes: returning {} bridge nodes", result.len());
    Ok(result)
}

// ---------------------------------------------------------------------------
// Knowledge gaps
// ---------------------------------------------------------------------------

/// Return up to `limit` functions/classes that have no test coverage.
///
/// "No coverage" = no incoming TESTED_BY edge AND the node's own is_test flag
/// is false.
pub fn get_knowledge_gaps(store: &GraphStore, limit: usize) -> Result<Vec<serde_json::Value>> {
    // Build set of tested qualified names.
    let edges = store.get_all_edges()?;
    let tested: HashSet<String> = edges
        .iter()
        .filter(|e| e.kind == "TESTED_BY")
        .map(|e| e.target_qualified.clone())
        .collect();

    let functions = store.get_nodes_by_kind("Function")?;
    let tests = store.get_nodes_by_kind("Test")?;

    let mut result = Vec::new();
    for node in functions.iter().chain(tests.iter()) {
        if node.is_test {
            continue;
        }
        if tested.contains(&node.qualified_name) {
            continue;
        }
        result.push(serde_json::json!({
            "qualified_name": node.qualified_name,
            "name": node.name,
            "kind": node.kind,
            "file_path": node.file_path,
            "line_start": node.line_start,
        }));
        if result.len() >= limit {
            break;
        }
    }

    debug!("get_knowledge_gaps: {} untested symbols", result.len());
    Ok(result)
}

// ---------------------------------------------------------------------------
// Surprising connections
// ---------------------------------------------------------------------------

/// Return up to `limit` CALLS edges that cross unrelated communities.
///
/// Two communities are "related" if one is the parent/child of the other.
pub fn get_surprising_connections(
    store: &GraphStore,
    limit: usize,
) -> Result<Vec<serde_json::Value>> {
    // Build parent/child relationship from community list.
    let communities = store.get_communities_list()?;
    // parent_child_pairs: (parent_id, child_id) and (child_id, parent_id) — both directions.
    let mut related_pairs: HashSet<(i64, i64)> = HashSet::new();
    for comm in &communities {
        if let (Some(id), Some(parent_id)) = (
            comm.get("id").and_then(|v| v.as_i64()),
            comm.get("parent_id").and_then(|v| v.as_i64()),
        ) {
            related_pairs.insert((id, parent_id));
            related_pairs.insert((parent_id, id));
        }
    }

    let community_map = store.get_all_community_ids()?;

    let edges = store.get_all_edges()?;
    let mut result = Vec::new();

    for edge in &edges {
        if edge.kind != "CALLS" {
            continue;
        }
        let src_comm = community_map
            .get(&edge.source_qualified)
            .and_then(|c| *c);
        let tgt_comm = community_map
            .get(&edge.target_qualified)
            .and_then(|c| *c);

        match (src_comm, tgt_comm) {
            (Some(sc), Some(tc)) if sc != tc => {
                // Suppress if communities are parent/child related.
                if related_pairs.contains(&(sc, tc)) {
                    continue;
                }
                result.push(serde_json::json!({
                    "source": edge.source_qualified,
                    "target": edge.target_qualified,
                    "source_community": sc,
                    "target_community": tc,
                    "kind": edge.kind,
                }));
                if result.len() >= limit {
                    break;
                }
            }
            _ => continue,
        }
    }

    debug!(
        "get_surprising_connections: {} cross-community edges",
        result.len()
    );
    Ok(result)
}

// ---------------------------------------------------------------------------
// Suggested questions
// ---------------------------------------------------------------------------

/// Generate 5–10 investigation questions based on the graph's current state.
///
/// If `context_qn` is supplied, questions will be biased toward that node's
/// neighbourhood.
pub fn get_suggested_questions(
    store: &GraphStore,
    context_qn: Option<&str>,
) -> Result<Vec<String>> {
    let mut questions: Vec<String> = Vec::new();

    // Q1 family: untested files.
    let gaps = get_knowledge_gaps(store, 3)?;
    for gap in &gaps {
        if let Some(fp) = gap.get("file_path").and_then(|v| v.as_str()) {
            questions.push(format!(
                "Which functions in `{}` lack test coverage?",
                fp
            ));
            if questions.len() >= 3 {
                break;
            }
        }
    }

    // Q2 family: hub stability.
    let hubs = get_hub_nodes(store, 3)?;
    for hub in &hubs {
        if let Some(name) = hub.get("name").and_then(|v| v.as_str()) {
            questions.push(format!(
                "What calls `{}` and could affect system stability if it changes?",
                name
            ));
            if questions.len() >= 6 {
                break;
            }
        }
    }

    // Q3 family: bridge coupling.
    let bridges = get_bridge_nodes(store, 3)?;
    let communities = store.get_communities_list()?;
    let comm_name: HashMap<i64, String> = communities
        .iter()
        .filter_map(|c| {
            let id = c.get("id")?.as_i64()?;
            let name = c.get("name")?.as_str()?.to_string();
            Some((id, name))
        })
        .collect();

    for bridge in &bridges {
        if let (Some(name), Some(sc), Some(tc)) = (
            bridge.get("name").and_then(|v| v.as_str()),
            bridge.get("community_id").and_then(|v| v.as_i64()),
            // get the "other" community from the cross-community data — approximate
            bridge
                .get("cross_community_edges")
                .and_then(|v| v.as_i64()),
        ) {
            let comm_a = comm_name.get(&sc).cloned().unwrap_or_else(|| format!("community {}", sc));
            questions.push(format!(
                "How does `{}` bridge `{}` to other parts of the codebase ({} cross-community connections)?",
                name, comm_a, tc
            ));
            if questions.len() >= 9 {
                break;
            }
        }
    }

    // Q4: context-specific question.
    if let Some(qn) = context_qn {
        if let Some(node) = store.get_node(qn)? {
            questions.push(format!(
                "What is the impact radius of changes to `{}`?",
                node.name
            ));
        }
    }

    // Ensure at least a generic fallback.
    if questions.is_empty() {
        questions.push("Which functions are most heavily called and pose the highest risk?".to_string());
        questions.push("Are there any isolated components with no dependencies?".to_string());
    }

    Ok(questions)
}

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
            .join(format!("crg-analysis-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        GraphStore::new(dir.join("graph.db")).unwrap()
    }

    fn node(name: &str, kind: &str, file: &str, is_test: bool) -> NodeInfo {
        NodeInfo {
            kind: kind.to_string(),
            name: name.to_string(),
            file_path: file.to_string(),
            line_start: 1,
            line_end: 10,
            language: "python".to_string(),
            is_test,
            ..Default::default()
        }
    }

    fn edge(src: &str, tgt: &str, kind: &str, file: &str) -> EdgeInfo {
        EdgeInfo {
            kind: kind.to_string(),
            source: src.to_string(),
            target: tgt.to_string(),
            file_path: file.to_string(),
            line: 5,
            extra: json!({}),
        }
    }

    #[test]
    fn hub_nodes_empty_graph() {
        let store = make_store();
        let hubs = get_hub_nodes(&store, 10).unwrap();
        assert!(hubs.is_empty());
    }

    #[test]
    fn hub_nodes_returns_top_callee() {
        let store = make_store();
        store.upsert_node(&node("a", "Function", "f.py", false), "").unwrap();
        store.upsert_node(&node("b", "Function", "f.py", false), "").unwrap();
        store.upsert_node(&node("c", "Function", "f.py", false), "").unwrap();
        store.upsert_edge(&edge("f.py::a", "f.py::c", "CALLS", "f.py")).unwrap();
        store.upsert_edge(&edge("f.py::b", "f.py::c", "CALLS", "f.py")).unwrap();

        let hubs = get_hub_nodes(&store, 1).unwrap();
        assert_eq!(hubs.len(), 1);
        assert_eq!(hubs[0]["name"], "c");
        assert_eq!(hubs[0]["call_count"], 2);
    }

    #[test]
    fn knowledge_gaps_excludes_tested() {
        let store = make_store();
        store.upsert_node(&node("fn_tested", "Function", "f.py", false), "").unwrap();
        store.upsert_node(&node("fn_untested", "Function", "f.py", false), "").unwrap();
        store.upsert_node(&node("test_it", "Test", "test_f.py", true), "").unwrap();
        // fn_tested is covered
        store.upsert_edge(&edge("f.py::fn_tested", "f.py::fn_tested", "TESTED_BY", "test_f.py")).unwrap();

        let gaps = get_knowledge_gaps(&store, 100).unwrap();
        let names: Vec<&str> = gaps.iter().filter_map(|g| g["name"].as_str()).collect();
        assert!(!names.contains(&"fn_tested"), "tested fn should not appear in gaps");
        assert!(names.contains(&"fn_untested"), "untested fn should appear in gaps");
        // test_it has is_test=true, so excluded
        assert!(!names.contains(&"test_it"));
    }

    #[test]
    fn get_suggested_questions_nonempty() {
        let store = make_store();
        let qs = get_suggested_questions(&store, None).unwrap();
        assert!(!qs.is_empty());
    }
}
