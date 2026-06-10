//! Community/cluster detection for the code knowledge graph.
//!
//! Detects communities of related code nodes using two backends:
//! 1. **File-based grouping** (always available) — groups nodes by directory structure.
//! 2. **Leiden via Python sidecar** (optional) — uses `crg_leiden_sidecar.py`.
//!
//! Mirrors `communities.py` from the Python codebase.

use std::collections::HashMap;

use anyhow::Context;
use regex::Regex;
use tracing::{info, warn};

use crg_core::store::GraphStore;
use crg_core::types::{GraphEdge, GraphNode};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Edge kind → weight mapping, mirroring Python `EDGE_WEIGHTS`.
pub const EDGE_WEIGHTS: &[(&str, f64)] = &[
    ("CALLS", 1.0),
    ("IMPORTS_FROM", 0.5),
    ("INHERITS", 0.8),
    ("IMPLEMENTS", 0.7),
    ("CONTAINS", 0.3),
    ("TESTED_BY", 0.4),
    ("DEPENDS_ON", 0.6),
];

/// Common words filtered when generating community names.
const COMMON_WORDS: &[&str] = &[
    "get", "set", "self", "init", "new", "create", "update", "delete", "add", "remove", "make",
    "build", "from", "to", "for", "with", "the", "and", "test", "main", "run", "do", "is", "has",
    "on", "of", "in", "at", "by", "my", "this", "that", "all", "none",
];

// ---------------------------------------------------------------------------
// Name generation helpers
// ---------------------------------------------------------------------------

/// Convert a string to a short lowercase slug (≤30 chars).
fn to_slug(s: &str) -> String {
    let lower = s.to_lowercase();
    // Replace non-alphanumeric runs with dashes
    let re = Regex::new(r"[^a-z0-9]+").unwrap();
    let slug = re.replace_all(&lower, "-");
    let trimmed = slug.trim_matches('-');
    trimmed.chars().take(30).collect()
}

/// Split a camelCase or snake_case name into component words.
fn split_name(name: &str) -> Vec<String> {
    // Insert boundary before uppercase after lowercase
    let re = Regex::new(r"([a-z])([A-Z])").unwrap();
    let s = re.replace_all(name, "${1}_${2}");
    let re2 = Regex::new(r"[_\-.\s]+").unwrap();
    re2.split(&s)
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
        .collect()
}

/// Extract the most frequent meaningful keywords from member names.
fn extract_keywords(members: &[&GraphNode]) -> Vec<String> {
    let mut word_counts: HashMap<String, usize> = HashMap::new();
    let valid_kinds = ["Function", "Class", "Test", "Type"];

    for m in members {
        if valid_kinds.contains(&m.kind.as_str()) {
            for word in split_name(&m.name) {
                let wl = word.to_lowercase();
                if !COMMON_WORDS.contains(&wl.as_str()) && wl.len() > 1 {
                    *word_counts.entry(wl).or_insert(0) += 1;
                }
            }
        }
    }

    if word_counts.is_empty() {
        return vec![];
    }

    let mut pairs: Vec<(String, usize)> = word_counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    pairs.into_iter().take(5).map(|(w, _)| w).collect()
}

/// Find the most common short directory or module name from file paths.
///
/// Takes the parent directory segment of each path, counts them, and
/// returns a slug of the most common one — matching the Python behaviour
/// of `_extract_file_prefix`.
fn extract_file_prefix(file_paths: &[&str]) -> String {
    if file_paths.is_empty() {
        return String::new();
    }

    let mut parts: Vec<String> = Vec::new();
    for fp in file_paths {
        let normalized = fp.replace('\\', "/");
        let segments: Vec<&str> = normalized.split('/').collect();
        if segments.len() >= 2 {
            // Take the parent directory
            parts.push(segments[segments.len() - 2].to_string());
        } else {
            // Take the file stem
            let stem = segments
                .last()
                .map(|s| s.rsplit('.').nth(1).unwrap_or(s))
                .unwrap_or("")
                .to_string();
            parts.push(stem);
        }
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for p in &parts {
        *counts.entry(p.clone()).or_insert(0) += 1;
    }

    let top = counts.into_iter().max_by_key(|(_, c)| *c).map(|(k, _)| k).unwrap_or_default();
    to_slug(&top)
}

/// Generate a meaningful name for a community of nodes.
///
/// Algorithm (mirrors `_generate_community_name` in Python):
/// 1. Find most common file prefix.
/// 2. Check for a dominant class (>40% of members).
/// 3. Fallback: most frequent keyword from function/class names.
/// 4. Format: `{prefix}-{keyword}` or `{prefix}` or `{keyword}`.
fn generate_community_name(members: &[&GraphNode]) -> String {
    if members.is_empty() {
        return "empty".to_string();
    }

    let file_paths: Vec<&str> = members.iter().map(|m| m.file_path.as_str()).collect();
    let prefix = extract_file_prefix(&file_paths);

    // Check for a dominant class (>40% of nodes)
    let class_names: Vec<&str> = members.iter().filter(|m| m.kind == "Class").map(|m| m.name.as_str()).collect();
    if !class_names.is_empty() {
        let mut class_counts: HashMap<&str, usize> = HashMap::new();
        for name in &class_names {
            *class_counts.entry(name).or_insert(0) += 1;
        }
        if let Some((top_class, top_count)) = class_counts.into_iter().max_by_key(|(_, c)| *c) {
            if top_count as f64 > members.len() as f64 * 0.4 {
                let slug = to_slug(top_class);
                if prefix.is_empty() {
                    return slug;
                }
                return format!("{}-{}", prefix, slug);
            }
        }
    }

    // Most frequent keyword
    let keywords = extract_keywords(members);
    let keyword = keywords.first().cloned().unwrap_or_default();

    match (!prefix.is_empty(), !keyword.is_empty()) {
        (true, true) => format!("{}-{}", prefix, keyword),
        (true, false) => prefix,
        (false, true) => keyword,
        (false, false) => "cluster".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Cohesion calculation
// ---------------------------------------------------------------------------

/// Compute cohesion for multiple communities in a single O(edges) pass.
///
/// Cohesion = internal_edges / (internal_edges + external_edges).
/// Mirrors `_compute_cohesion_batch` in Python.
fn compute_cohesion_batch(community_member_qns: &[&std::collections::HashSet<String>], all_edges: &[GraphEdge]) -> Vec<f64> {
    // Build qualified_name -> community index reverse map
    let mut qn_to_idx: HashMap<&str, usize> = HashMap::new();
    for (idx, members) in community_member_qns.iter().enumerate() {
        for qn in members.iter() {
            qn_to_idx.insert(qn.as_str(), idx);
        }
    }

    let n = community_member_qns.len();
    let mut internal = vec![0usize; n];
    let mut external = vec![0usize; n];

    for e in all_edges {
        let sc = qn_to_idx.get(e.source_qualified.as_str()).copied();
        let tc = qn_to_idx.get(e.target_qualified.as_str()).copied();

        match (sc, tc) {
            (None, None) => continue,
            (Some(si), Some(ti)) if si == ti => {
                internal[si] += 1;
            }
            _ => {
                if let Some(si) = sc {
                    external[si] += 1;
                }
                if let Some(ti) = tc {
                    external[ti] += 1;
                }
            }
        }
    }

    (0..n)
        .map(|i| {
            let total = internal[i] + external[i];
            if total > 0 {
                (internal[i] as f64 / total as f64 * 10000.0).round() / 10000.0
            } else {
                0.0
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Community data structure
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CommunityData {
    name: String,
    level: i64,
    size: i64,
    cohesion: f64,
    dominant_language: String,
    description: String,
    members: Vec<String>,
}

// ---------------------------------------------------------------------------
// File-based community detection
// ---------------------------------------------------------------------------

/// Group nodes by directory structure when Leiden is unavailable.
///
/// Mirrors `_detect_file_based` in Python: strips the longest common directory
/// prefix, then adaptively picks a depth that yields 10–200 communities.
fn file_based_communities(nodes: &[GraphNode], edges: &[GraphEdge], min_size: usize) -> Vec<CommunityData> {
    // Collect directory parts for each node (stripped of the filename)
    let all_dir_parts: Vec<Vec<String>> = nodes
        .iter()
        .map(|n| {
            let norm = n.file_path.replace('\\', "/");
            let segs: Vec<&str> = norm.split('/').collect();
            segs[..segs.len().saturating_sub(1)]
                .iter()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
        .collect();

    // Find the longest common prefix among directory parts
    let prefix_len = if all_dir_parts.is_empty() {
        0
    } else {
        let shortest = all_dir_parts.iter().map(|p| p.len()).min().unwrap_or(0);
        let mut plen = 0usize;
        'outer: for i in 0..shortest {
            let seg = &all_dir_parts[0][i];
            for parts in &all_dir_parts[1..] {
                if &parts[i] != seg {
                    break 'outer;
                }
            }
            plen = i + 1;
        }
        plen
    };

    let group_at_depth = |depth: usize| -> HashMap<String, Vec<usize>> {
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, n) in nodes.iter().enumerate() {
            let norm = n.file_path.replace('\\', "/");
            let segs: Vec<&str> = norm.split('/').collect();
            let dir_parts: Vec<&str> = segs[..segs.len().saturating_sub(1)]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect();
            let remainder: Vec<&str> = dir_parts[prefix_len.min(dir_parts.len())..].to_vec();
            let key = if !remainder.is_empty() {
                remainder[..depth.min(remainder.len())].join("/")
            } else {
                // File stem fallback
                segs.last()
                    .map(|s| s.rsplit('.').nth(1).unwrap_or(s).to_string())
                    .unwrap_or_else(|| "root".to_string())
            };
            groups.entry(key).or_default().push(idx);
        }
        groups
    };

    let max_depth = all_dir_parts.iter().map(|p| p.len().saturating_sub(prefix_len)).max().unwrap_or(0);
    let mut best_groups = group_at_depth(1);

    for depth in 1..=max_depth {
        let groups = group_at_depth(depth);
        let qualifying = groups.values().filter(|v| v.len() >= min_size).count();
        best_groups = groups;
        if qualifying >= 10 {
            break;
        }
    }

    // Collect pending communities (those meeting min_size)
    let mut pending: Vec<(String, Vec<&GraphNode>, std::collections::HashSet<String>)> = Vec::new();
    let mut sorted_keys: Vec<String> = best_groups.keys().cloned().collect();
    sorted_keys.sort();

    for dir_path in sorted_keys {
        let node_indices = &best_groups[&dir_path];
        if node_indices.len() < min_size {
            continue;
        }
        let members: Vec<&GraphNode> = node_indices.iter().map(|&i| &nodes[i]).collect();
        let member_qns: std::collections::HashSet<String> =
            members.iter().map(|m| m.qualified_name.clone()).collect();
        pending.push((dir_path, members, member_qns));
    }

    let member_sets: Vec<&std::collections::HashSet<String>> = pending.iter().map(|(_, _, s)| s).collect();
    let cohesions = compute_cohesion_batch(&member_sets, edges);

    pending
        .into_iter()
        .zip(cohesions.into_iter())
        .map(|((dir_path, members, member_qns), cohesion)| {
            let mut lang_counts: HashMap<&str, usize> = HashMap::new();
            for m in &members {
                if !m.language.is_empty() {
                    *lang_counts.entry(m.language.as_str()).or_insert(0) += 1;
                }
            }
            let dominant_language = lang_counts
                .into_iter()
                .max_by_key(|(_, c)| *c)
                .map(|(l, _)| l.to_string())
                .unwrap_or_default();

            let name = generate_community_name(&members);
            let size = members.len() as i64;
            CommunityData {
                name,
                level: 0,
                size,
                cohesion,
                dominant_language,
                description: format!("Directory-based community: {}", dir_path),
                members: member_qns.into_iter().collect(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Store communities helper
// ---------------------------------------------------------------------------

fn store_community_data(store: &GraphStore, communities: &[CommunityData]) -> anyhow::Result<i64> {
    store.clear_communities()?;

    let mut count = 0i64;
    for comm in communities {
        let community_id = store.upsert_community(
            &comm.name,
            comm.level,
            None, // parent_id
            comm.cohesion,
            comm.size,
            if comm.dominant_language.is_empty() {
                None
            } else {
                Some(comm.dominant_language.as_str())
            },
            Some(comm.description.as_str()),
        )?;

        // Generate a summary for this community
        let key_symbols: Vec<String> = comm.members.iter().take(5).cloned().collect();
        store.set_community_summary(
            community_id,
            &comm.name,
            &comm.description,
            &key_symbols,
            "unknown",
            comm.size,
            &comm.dominant_language,
        )?;

        // Assign community_id to each member node
        let conn_guard_workaround = (); // we use the public API
        let _ = conn_guard_workaround;

        // Batch-assign nodes
        for batch in comm.members.chunks(450) {
            for qn in batch {
                // Look up node id to call set_node_community
                if let Some(node) = store.get_node(qn)? {
                    store.set_node_community(node.id, community_id)?;
                }
            }
        }

        count += 1;
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// File-based grouping (public)
// ---------------------------------------------------------------------------

/// Group nodes by their directory — the default, always-available backend.
///
/// Clears all existing communities and populates the communities table with
/// one community per directory group. Returns the number of communities created.
pub fn file_based_grouping(store: &GraphStore) -> anyhow::Result<i64> {
    let nodes = store.get_all_nodes(true)?;
    let edges = store.get_all_edges()?;
    info!("file_based_grouping: {} nodes, {} edges", nodes.len(), edges.len());

    let communities = file_based_communities(&nodes, &edges, 2);
    info!("file_based_grouping: detected {} communities", communities.len());
    store_community_data(store, &communities)
}

// ---------------------------------------------------------------------------
// Leiden grouping via sidecar (public, async)
// ---------------------------------------------------------------------------

/// Detect communities using the Leiden algorithm via a Python sidecar.
///
/// The sidecar receives the list of CALLS edges and returns a
/// `qualified_name → community_id` mapping. On failure, the caller should
/// fall back to `file_based_grouping`.
pub async fn leiden_grouping(
    store: &GraphStore,
    sidecar_path: &str,
    resolution: f64,
    seed: u64,
) -> anyhow::Result<i64> {
    use crg_sidecar_bridge::SidecarPool;

    let edges = store.get_all_edges()?;
    let nodes = store.get_all_nodes(true)?;

    // Build edge list for the sidecar
    let edge_list: Vec<serde_json::Value> = edges
        .iter()
        .filter(|e| e.kind == "CALLS")
        .map(|e| {
            serde_json::json!({
                "source": e.source_qualified,
                "target": e.target_qualified,
            })
        })
        .collect();

    if edge_list.is_empty() {
        info!("leiden_grouping: no CALLS edges, falling back to file-based");
        return file_based_grouping(store);
    }

    let pool = SidecarPool::new(sidecar_path.to_string());
    let params = serde_json::json!({
        "edges": edge_list,
        "resolution": resolution,
        "seed": seed,
    });

    let result = pool
        .call("detect", params)
        .await
        .context("leiden sidecar call failed")?;

    // Parse community assignments: { qn: community_id }
    let assignments = result
        .as_object()
        .context("leiden sidecar: expected object response")?;

    // Group nodes by community id
    let mut community_groups: HashMap<i64, Vec<String>> = HashMap::new();
    for (qn, cid_val) in assignments {
        if let Some(cid) = cid_val.as_i64() {
            community_groups.entry(cid).or_default().push(qn.clone());
        }
    }

    // Build CommunityData for each group
    let all_edges = edges;
    let node_by_qn: HashMap<&str, &GraphNode> =
        nodes.iter().map(|n| (n.qualified_name.as_str(), n)).collect();

    let mut pending: Vec<(std::collections::HashSet<String>, Vec<&GraphNode>)> = Vec::new();
    for (_cid, qns) in &community_groups {
        if qns.len() < 2 {
            continue;
        }
        let members: Vec<&GraphNode> = qns.iter().filter_map(|q| node_by_qn.get(q.as_str()).copied()).collect();
        if members.len() < 2 {
            continue;
        }
        let member_qns: std::collections::HashSet<String> = qns.iter().cloned().collect();
        pending.push((member_qns, members));
    }

    let member_sets: Vec<&std::collections::HashSet<String>> = pending.iter().map(|(s, _)| s).collect();
    let cohesions = compute_cohesion_batch(&member_sets, &all_edges);

    let communities: Vec<CommunityData> = pending
        .into_iter()
        .zip(cohesions.into_iter())
        .map(|((member_qns, members), cohesion)| {
            let mut lang_counts: HashMap<&str, usize> = HashMap::new();
            for m in &members {
                if !m.language.is_empty() {
                    *lang_counts.entry(m.language.as_str()).or_insert(0) += 1;
                }
            }
            let dominant_language = lang_counts
                .into_iter()
                .max_by_key(|(_, c)| *c)
                .map(|(l, _)| l.to_string())
                .unwrap_or_default();
            let name = generate_community_name(&members);
            let size = members.len() as i64;
            CommunityData {
                name,
                level: 0,
                size,
                cohesion,
                dominant_language,
                description: format!("Community of {} nodes", size),
                members: member_qns.into_iter().collect(),
            }
        })
        .collect();

    info!("leiden_grouping: detected {} communities", communities.len());
    store_community_data(store, &communities)
}

// ---------------------------------------------------------------------------
// Public detect_communities entry point
// ---------------------------------------------------------------------------

/// Detect and store communities in the graph.
///
/// If `use_leiden` is true and `sidecar_path` is Some, attempts Leiden detection
/// via the sidecar. If that fails (including when we're inside an async runtime
/// and can't block), falls back to file-based grouping.
///
/// After grouping, architecture overview summaries are generated via
/// `set_community_summary` for each community.
pub fn detect_communities(
    store: &GraphStore,
    use_leiden: bool,
    sidecar_path: Option<&str>,
    resolution: f64,
    seed: u64,
) -> anyhow::Result<i64> {
    if use_leiden {
        if let Some(sp) = sidecar_path {
            // Determine if we're inside an existing tokio runtime.
            // If so, use block_in_place (tokio multi-thread) or fall back.
            // If not, create a fresh runtime.
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    // We're inside a runtime — use block_in_place to avoid
                    // nested-runtime panic. This requires multi-thread scheduler;
                    // if it's single-thread (current_thread), block_in_place will
                    // also panic. Treat any failure here as "fall back to file-based".
                    let sp_owned = sp.to_string();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        tokio::task::block_in_place(|| {
                            handle.block_on(leiden_grouping(store, &sp_owned, resolution, seed))
                        })
                    }));
                    match result {
                        Ok(Ok(count)) => return Ok(count),
                        Ok(Err(e)) => {
                            warn!("leiden_grouping failed ({}), falling back to file-based", e);
                        }
                        Err(_) => {
                            warn!("leiden_grouping panicked, falling back to file-based");
                        }
                    }
                }
                Err(_) => {
                    // No active runtime — create one.
                    let sp_owned = sp.to_string();
                    match tokio::runtime::Runtime::new() {
                        Ok(rt) => match rt.block_on(leiden_grouping(store, &sp_owned, resolution, seed)) {
                            Ok(count) => return Ok(count),
                            Err(e) => {
                                warn!("leiden_grouping failed ({}), falling back to file-based", e);
                            }
                        },
                        Err(e) => {
                            warn!("failed to create tokio runtime ({}), falling back to file-based", e);
                        }
                    }
                }
            }
        }
    }

    file_based_grouping(store)
}

// ---------------------------------------------------------------------------
// Architecture overview
// ---------------------------------------------------------------------------

/// Generate a high-level architecture overview from stored communities.
///
/// Returns:
/// `{"total_communities": N, "communities": [{"id": 1, "name": "...", "size": 10,
///   "dominant_language": "python", "cohesion": 0.8}, ...]}`
///
/// Also counts cross-community edges and generates warnings for high coupling,
/// mirroring `get_architecture_overview` in Python.
pub fn get_architecture_overview(store: &GraphStore) -> anyhow::Result<serde_json::Value> {
    let communities = store.get_communities_list()?;

    // Build node → community_id map
    let mut node_to_community: HashMap<String, i64> = HashMap::new();
    for comm in &communities {
        let cid = comm["id"].as_i64().unwrap_or(0);
        if let Ok(qns) = store.get_community_member_qns(cid) {
            for qn in qns {
                node_to_community.insert(qn, cid);
            }
        }
    }

    // Count cross-community edges (exclude TESTED_BY, as in Python)
    let all_edges = store.get_all_edges()?;
    let mut cross_counts: HashMap<(i64, i64), usize> = HashMap::new();

    for e in &all_edges {
        if e.kind == "TESTED_BY" {
            continue;
        }
        let src_comm = node_to_community.get(&e.source_qualified).copied();
        let tgt_comm = node_to_community.get(&e.target_qualified).copied();
        if let (Some(sc), Some(tc)) = (src_comm, tgt_comm) {
            if sc != tc {
                let pair = (sc.min(tc), sc.max(tc));
                *cross_counts.entry(pair).or_insert(0) += 1;
            }
        }
    }

    // Generate warnings for high coupling (>10 cross edges), skipping test communities
    let test_re = Regex::new(r"(?i)(^test[-/]|[-/]test([:/]|$)|it:should|describe:|spec[-/]|[-/]spec$)").unwrap();
    let comm_name_map: HashMap<i64, String> = communities
        .iter()
        .map(|c| {
            (
                c["id"].as_i64().unwrap_or(0),
                c["name"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();

    let mut sorted_cross: Vec<((i64, i64), usize)> = cross_counts.into_iter().collect();
    sorted_cross.sort_by(|a, b| b.1.cmp(&a.1));

    let mut warnings: Vec<String> = Vec::new();
    for ((c1, c2), count) in &sorted_cross {
        if *count > 10 {
            let name1 = comm_name_map.get(c1).map(String::as_str).unwrap_or("");
            let name2 = comm_name_map.get(c2).map(String::as_str).unwrap_or("");
            if test_re.is_match(name1) || test_re.is_match(name2) {
                continue;
            }
            warnings.push(format!(
                "High coupling ({} edges) between '{}' and '{}'",
                count, name1, name2
            ));
        }
    }

    let total = communities.len();
    Ok(serde_json::json!({
        "total_communities": total,
        "communities": communities,
        "warnings": warnings,
    }))
}

/// Get details for a single community including member qualified_names (first 20).
pub fn get_community_summary(store: &GraphStore, community_id: i64) -> anyhow::Result<serde_json::Value> {
    let communities = store.get_communities_list()?;
    let comm = communities
        .iter()
        .find(|c| c["id"].as_i64() == Some(community_id))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("community {} not found", community_id))?;

    let mut members = store.get_community_member_qns(community_id)?;
    members.truncate(20);

    let mut result = comm;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("members".to_string(), serde_json::json!(members));
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_slug_basic() {
        assert_eq!(to_slug("MyModule"), "mymodule");
        assert_eq!(to_slug("my-module"), "my-module");
        assert_eq!(to_slug("My Module!"), "my-module");
    }

    #[test]
    fn to_slug_length_cap() {
        let long = "a".repeat(40);
        assert_eq!(to_slug(&long).len(), 30);
    }

    #[test]
    fn split_name_camel() {
        let words = split_name("MyClassName");
        assert!(words.iter().any(|w| w.to_lowercase() == "my" || w.to_lowercase() == "class"));
    }

    #[test]
    fn extract_keywords_filters_common() {
        use crg_core::types::GraphNode;
        let node = GraphNode {
            id: 1,
            kind: "Function".to_string(),
            name: "processData".to_string(),
            qualified_name: "mod::processData".to_string(),
            file_path: "src/mod.rs".to_string(),
            line_start: 1,
            line_end: 10,
            language: "rust".to_string(),
            parent_name: None,
            params: None,
            return_type: None,
            is_test: false,
            file_hash: None,
            extra: serde_json::json!({}),
            community_id: None,
        };
        let refs = vec![&node];
        let kws = extract_keywords(&refs);
        // "process" and "data" should appear, "get" etc. should not
        assert!(kws.contains(&"process".to_string()) || kws.contains(&"data".to_string()));
    }

    #[test]
    fn generate_name_empty() {
        let name = generate_community_name(&[]);
        assert_eq!(name, "empty");
    }

    #[test]
    fn cohesion_all_internal() {
        use crg_core::types::GraphEdge;
        let mut members = std::collections::HashSet::new();
        members.insert("a".to_string());
        members.insert("b".to_string());
        let edges = vec![GraphEdge {
            id: 1,
            kind: "CALLS".to_string(),
            source_qualified: "a".to_string(),
            target_qualified: "b".to_string(),
            file_path: "f.rs".to_string(),
            line: 1,
            extra: serde_json::json!({}),
            confidence: 1.0,
            confidence_tier: "EXTRACTED".to_string(),
        }];
        let sets = vec![&members];
        let cohesions = compute_cohesion_batch(&sets, &edges);
        assert!((cohesions[0] - 1.0).abs() < 1e-6, "expected cohesion=1.0, got {}", cohesions[0]);
    }

    #[test]
    fn cohesion_all_external() {
        use crg_core::types::GraphEdge;
        let mut members = std::collections::HashSet::new();
        members.insert("a".to_string());
        let edges = vec![GraphEdge {
            id: 1,
            kind: "CALLS".to_string(),
            source_qualified: "a".to_string(),
            target_qualified: "c".to_string(), // c is not in community
            file_path: "f.rs".to_string(),
            line: 1,
            extra: serde_json::json!({}),
            confidence: 1.0,
            confidence_tier: "EXTRACTED".to_string(),
        }];
        let sets = vec![&members];
        let cohesions = compute_cohesion_batch(&sets, &edges);
        assert!((cohesions[0] - 0.0).abs() < 1e-6);
    }
}
