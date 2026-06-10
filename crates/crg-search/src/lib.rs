//! crg-search — FTS5 hybrid search with Reciprocal Rank Fusion (RRF).
//!
//! Mirrors `search.py` from the Python codebase.  The entry-point for callers
//! is [`hybrid_search`].  Supporting functions are exposed publicly so that MCP
//! tools and the CLI can call them individually.

use std::collections::HashMap;

use crg_core::security::sanitize_name;
use crg_core::store::GraphStore;
use regex::Regex;
use serde_json::Value as JsonValue;
use std::sync::OnceLock;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Compiled regular expressions — lazily initialised once per process
// ---------------------------------------------------------------------------

static DOTTED_IDENT_RE: OnceLock<Regex> = OnceLock::new();
static SNAKE_IDENT_RE: OnceLock<Regex> = OnceLock::new();
static PASCAL_IDENT_RE: OnceLock<Regex> = OnceLock::new();

fn dotted_ident_re() -> &'static Regex {
    DOTTED_IDENT_RE.get_or_init(|| {
        Regex::new(r"\b[A-Za-z_][\w]*(?:\.[A-Za-z_][\w]*)+\b").expect("valid regex")
    })
}

fn snake_ident_re() -> &'static Regex {
    SNAKE_IDENT_RE.get_or_init(|| {
        Regex::new(r"\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b").expect("valid regex")
    })
}

fn pascal_ident_re() -> &'static Regex {
    PASCAL_IDENT_RE.get_or_init(|| {
        Regex::new(r"\b[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+\b").expect("valid regex")
    })
}

// ---------------------------------------------------------------------------
// FTS5 index management
// ---------------------------------------------------------------------------

/// Rebuild the FTS5 index for the `nodes_fts` table.
///
/// Calls [`GraphStore::rebuild_fts`] (which issues the FTS5 `'rebuild'`
/// command) and returns the total number of nodes now indexed.
///
/// The `nodes_fts` virtual table always exists because it is created by
/// migration v5.  A full DROP + CREATE is therefore not required here;
/// the FTS5 `rebuild` content-table command repopulates the index from the
/// `nodes` table in a single atomic step.
pub fn rebuild_fts_index(store: &GraphStore) -> anyhow::Result<i64> {
    store.rebuild_fts()?;
    let stats = store.get_stats()?;
    info!("FTS index rebuilt: {} rows indexed", stats.total_nodes);
    Ok(stats.total_nodes)
}

// ---------------------------------------------------------------------------
// Query identifier extraction
// ---------------------------------------------------------------------------

/// Pull out identifier-shaped tokens from anywhere in a query string.
///
/// Catches dotted forms (`Context.Next`), snake_case (`get_dependant`), and
/// PascalCase (`APIRoute`) even when embedded in a natural-language sentence.
///
/// Mirrors `extract_query_identifiers` in `search.py`.
pub fn extract_query_identifiers(query: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for re in [dotted_ident_re(), snake_ident_re(), pascal_ident_re()] {
        for mat in re.find_iter(query) {
            let lo = mat.as_str().to_lowercase();
            if !seen.contains(&lo) && lo.len() >= 3 {
                seen.insert(lo.clone());
                found.push(lo);
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Query kind boost detection
// ---------------------------------------------------------------------------

/// Detect query patterns and return per-node boost multipliers.
///
/// Returned map keys are either node kind strings (`"Class"`, `"Function"`,
/// …) mapped to `f64` multipliers, or the special keys `"_qualified"` (f64)
/// and `"_qualified_identifiers"` (Vec<String>).
///
/// Mirrors `detect_query_kind_boost` in `search.py`.
pub fn detect_query_kind_boost(query: &str) -> HashMap<String, JsonValue> {
    let mut boosts: HashMap<String, JsonValue> = HashMap::new();
    let q = query.trim();
    if q.is_empty() {
        return boosts;
    }

    // PascalCase: starts with uppercase followed by at least one lowercase.
    let pascal_start = Regex::new(r"^[A-Z][a-z]").expect("valid regex");
    if pascal_start.is_match(q) && !q.chars().all(|c| c.is_uppercase() || c == '_') {
        boosts.insert("Class".to_string(), JsonValue::from(1.5_f64));
        boosts.insert("Type".to_string(), JsonValue::from(1.5_f64));
    }

    // snake_case: contains an underscore with at least one letter.
    if q.contains('_') && q.chars().any(|c| c.is_alphabetic()) {
        boosts.insert("Function".to_string(), JsonValue::from(1.5_f64));
    }

    // Dotted path: boost qualified-name matches.
    if q.contains('.') {
        boosts.insert("_qualified".to_string(), JsonValue::from(2.0_f64));
    }

    // Identifier tokens anywhere in the query.
    let idents = extract_query_identifiers(q);
    if !idents.is_empty() {
        boosts.insert(
            "_qualified_identifiers".to_string(),
            JsonValue::Array(idents.into_iter().map(JsonValue::String).collect()),
        );
    }

    boosts
}

// ---------------------------------------------------------------------------
// Reciprocal Rank Fusion
// ---------------------------------------------------------------------------

/// Merge multiple ranked result lists using Reciprocal Rank Fusion (RRF).
///
/// Each list contains `(id, score)` tuples ordered by score descending.
/// The RRF score for each item is `sum(1 / (k + rank + 1))` across all lists.
///
/// Returns a Vec of `(id, rrf_score)` tuples sorted by score descending.
///
/// Mirrors `rrf_merge` in `search.py`.
pub fn rrf_merge(result_lists: &[Vec<(i64, f64)>], k: f64) -> Vec<(i64, f64)> {
    let mut scores: HashMap<i64, f64> = HashMap::new();

    for result_list in result_lists {
        for (rank, (item_id, _score)) in result_list.iter().enumerate() {
            let entry = scores.entry(*item_id).or_insert(0.0);
            *entry += 1.0 / (k + rank as f64 + 1.0);
        }
    }

    let mut merged: Vec<(i64, f64)> = scores.into_iter().collect();
    // Stable sort for consistent tie-breaking.
    merged.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    merged
}

// ---------------------------------------------------------------------------
// Hybrid search — main entry point
// ---------------------------------------------------------------------------

/// Hybrid search combining FTS5 BM25 results via RRF.
///
/// Attempts FTS5 search first, falling back to keyword LIKE matching if FTS5
/// returns nothing or errors.  Applies query-aware kind boosting and optional
/// context-file boosting, then returns the top `limit` results.
///
/// Mirrors `hybrid_search` in `search.py` (without the embedding path, which
/// lives in `crg-embeddings`).
///
/// # Parameters
/// - `store`: the graph store to search.
/// - `query`: search query string.
/// - `kind`: optional node-kind filter (e.g. `"Function"`, `"Class"`).
/// - `limit`: maximum results to return (default 20).
/// - `context_files`: nodes in these files receive a 1.5× score boost.
pub fn hybrid_search(
    store: &GraphStore,
    query: &str,
    kind: Option<&str>,
    limit: usize,
    context_files: Option<&[String]>,
) -> anyhow::Result<Vec<JsonValue>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(vec![]);
    }

    let fetch_limit = limit * 3; // Fetch extra to allow for filtering and boosting.

    // ------ Phase 1: Gather ranked lists ------

    let fts_results: Vec<(i64, f64)> = store.fts_search(q, fetch_limit).unwrap_or_else(|e| {
        warn!("FTS5 unavailable, will use fallback: {}", e);
        vec![]
    });

    // ------ Phase 2: Merge via RRF or fall back to keyword LIKE ------

    let merged: Vec<(i64, f64)> = if !fts_results.is_empty() {
        // Single-list RRF is just a rank-to-score transformation (identical to
        // iterating the list directly, but consistent with the Python path).
        rrf_merge(&[fts_results], 60.0)
    } else {
        // Fallback: keyword LIKE matching.
        let keyword_results = store.keyword_search(q, fetch_limit).unwrap_or_else(|e| {
            warn!("Keyword search failed: {}", e);
            vec![]
        });
        if keyword_results.is_empty() {
            return Ok(vec![]);
        }
        keyword_results
    };

    // ------ Phase 3: Batch-fetch candidate nodes ------

    let kind_boosts = detect_query_kind_boost(q);
    let context_set: std::collections::HashSet<&str> = context_files
        .unwrap_or(&[])
        .iter()
        .map(String::as_str)
        .collect();

    let candidate_ids: Vec<i64> = merged.iter().map(|(id, _)| *id).collect();
    let nodes_vec = store.get_nodes_by_ids(&candidate_ids)?;
    let node_map: HashMap<i64, crg_core::types::GraphNode> =
        nodes_vec.into_iter().map(|n| (n.id, n)).collect();

    // ------ Phase 4: Apply boosting ------

    let mut boosted: Vec<(i64, f64)> = Vec::with_capacity(merged.len());
    for (node_id, score) in &merged {
        let node = match node_map.get(node_id) {
            Some(n) => n,
            None => continue,
        };

        let node_kind = &node.kind;
        let file_path = &node.file_path;
        let qualified_name = &node.qualified_name;

        let mut boost = 1.0_f64;

        // Kind-based boost (e.g. 1.5× for Class/Function).
        if let Some(v) = kind_boosts.get(node_kind.as_str()) {
            if let Some(f) = v.as_f64() {
                boost *= f;
            }
        }

        // Qualified-name boost for dotted queries.
        if kind_boosts.contains_key("_qualified") && q.contains('.') {
            if qualified_name.to_lowercase().contains(&q.to_lowercase()) {
                if let Some(v) = kind_boosts.get("_qualified") {
                    if let Some(f) = v.as_f64() {
                        boost *= f;
                    }
                }
            }
        }

        // Identifier token boost.
        if let Some(JsonValue::Array(idents)) = kind_boosts.get("_qualified_identifiers") {
            let qn_lo = qualified_name.to_lowercase();
            let any_match = idents.iter().any(|iv| {
                iv.as_str()
                    .map(|s| qn_lo.contains(s))
                    .unwrap_or(false)
            });
            if any_match {
                boost *= 2.0;
            }
        }

        // Context-file boost.
        if !context_set.is_empty() && context_set.contains(file_path.as_str()) {
            boost *= 1.5;
        }

        boosted.push((*node_id, score * boost));
    }

    // Sort descending by boosted score (stable for consistent tie-breaking).
    boosted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // ------ Phase 5: Build result dicts ------

    let mut results: Vec<JsonValue> = Vec::with_capacity(limit);
    for (node_id, final_score) in boosted {
        if results.len() >= limit {
            break;
        }

        let node = match node_map.get(&node_id) {
            Some(n) => n,
            None => continue,
        };

        // Apply kind filter *after* limit check, matching Python order.
        if let Some(k) = kind {
            if node.kind != k {
                continue;
            }
        }

        let score_rounded = (final_score * 1_000_000.0).round() / 1_000_000.0;

        results.push(serde_json::json!({
            "name": sanitize_name(&node.name),
            "qualified_name": sanitize_name(&node.qualified_name),
            "kind": node.kind,
            "file_path": node.file_path,
            "line_start": node.line_start,
            "line_end": node.line_end,
            "language": node.language,
            "params": node.params,
            "return_type": node.return_type,
            // `signature` is not in GraphNode (no SELECT column); emit null.
            "signature": JsonValue::Null,
            "score": score_rounded,
        }));
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crg_core::store::GraphStore;
    use crg_core::types::NodeInfo;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_store() -> GraphStore {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("crg-search-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        GraphStore::new(dir.join("graph.db")).unwrap()
    }

    fn insert_node(store: &GraphStore, name: &str, kind: &str, file: &str, language: &str) {
        let node = NodeInfo {
            kind: kind.to_string(),
            name: name.to_string(),
            file_path: file.to_string(),
            line_start: 1,
            line_end: 10,
            language: language.to_string(),
            parent_name: None,
            params: None,
            return_type: None,
            modifiers: None,
            is_test: false,
            extra: json!({}),
        };
        store.upsert_node(&node, "hash").unwrap();
    }

    // ---- extract_query_identifiers ----

    #[test]
    fn test_extract_dotted() {
        let ids = extract_query_identifiers("Who calls Context.Next in gin");
        assert!(ids.iter().any(|s| s.contains("context.next")), "{ids:?}");
    }

    #[test]
    fn test_extract_snake() {
        let ids = extract_query_identifiers("find get_users function");
        assert!(ids.iter().any(|s| s == "get_users"), "{ids:?}");
    }

    #[test]
    fn test_extract_pascal() {
        let ids = extract_query_identifiers("show me MyClass usage");
        assert!(ids.iter().any(|s| s == "myclass"), "{ids:?}");
    }

    #[test]
    fn test_extract_min_length() {
        // "a.b" is only 3 chars including the dot — the identifiers "a" and "b" are 1 char each
        // so the regex won't match them as standalone 3-char identifiers.
        // Tokens below 3 chars must be excluded.
        let ids = extract_query_identifiers("ab");
        assert!(ids.is_empty(), "short tokens should be excluded: {ids:?}");
    }

    // ---- detect_query_kind_boost ----

    #[test]
    fn test_boost_pascal_case() {
        let boosts = detect_query_kind_boost("MyClass");
        assert!(boosts.contains_key("Class"), "{boosts:?}");
        assert!(boosts.contains_key("Type"), "{boosts:?}");
    }

    #[test]
    fn test_boost_snake_case() {
        let boosts = detect_query_kind_boost("get_users");
        assert!(boosts.contains_key("Function"), "{boosts:?}");
    }

    #[test]
    fn test_boost_dotted() {
        let boosts = detect_query_kind_boost("os.path");
        assert!(boosts.contains_key("_qualified"), "{boosts:?}");
    }

    #[test]
    fn test_boost_empty_query() {
        let boosts = detect_query_kind_boost("");
        assert!(boosts.is_empty());
    }

    // ---- rrf_merge ----

    #[test]
    fn test_rrf_single_list() {
        let list = vec![(1i64, 5.0f64), (2i64, 3.0f64), (3i64, 1.0f64)];
        let merged = rrf_merge(&[list], 60.0);
        // rank 0 → 1/(60+0+1) = 1/61
        let first_score = merged[0].1;
        assert!(
            (first_score - 1.0 / 61.0).abs() < 1e-9,
            "first score should be 1/61, got {first_score}"
        );
        // Order should be preserved.
        assert_eq!(merged[0].0, 1);
        assert_eq!(merged[1].0, 2);
        assert_eq!(merged[2].0, 3);
    }

    #[test]
    fn test_rrf_two_lists() {
        // id=1 appears in both lists at rank 0 and rank 1.
        let list_a = vec![(1i64, 10.0f64), (2i64, 5.0f64)];
        let list_b = vec![(3i64, 8.0f64), (1i64, 4.0f64)];
        let merged = rrf_merge(&[list_a, list_b], 60.0);
        // id=1 should have score 1/61 + 1/62
        let id1_score = merged.iter().find(|(id, _)| *id == 1).map(|(_, s)| *s).unwrap();
        let expected = 1.0 / 61.0 + 1.0 / 62.0;
        assert!((id1_score - expected).abs() < 1e-9, "got {id1_score}");
    }

    #[test]
    fn test_rrf_empty() {
        let merged = rrf_merge(&[], 60.0);
        assert!(merged.is_empty());
    }

    // ---- rebuild_fts_index ----

    #[test]
    fn test_rebuild_fts_empty_store() {
        let store = make_store();
        let count = rebuild_fts_index(&store).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_rebuild_fts_with_nodes() {
        let store = make_store();
        insert_node(&store, "foo", "Function", "src/a.py", "python");
        insert_node(&store, "bar", "Function", "src/a.py", "python");
        // Rebuild so FTS index is populated.
        let count = rebuild_fts_index(&store).unwrap();
        assert_eq!(count, 2);
    }

    // ---- fts_search (via store) ----

    #[test]
    fn test_fts_search_finds_match() {
        let store = make_store();
        insert_node(&store, "calculate_total", "Function", "src/math.py", "python");
        rebuild_fts_index(&store).unwrap();

        let results = store.fts_search("calculate_total", 10).unwrap();
        assert!(!results.is_empty(), "FTS should find 'calculate_total'");
    }

    #[test]
    fn test_fts_search_no_match() {
        let store = make_store();
        insert_node(&store, "foo", "Function", "src/a.py", "python");
        rebuild_fts_index(&store).unwrap();

        let results = store.fts_search("zzz_nonexistent_zzz", 10).unwrap();
        assert!(results.is_empty());
    }

    // ---- keyword_search (via store) ----

    #[test]
    fn test_keyword_search_finds_match() {
        let store = make_store();
        insert_node(&store, "get_user_by_id", "Function", "src/db.py", "python");

        let results = store.keyword_search("user", 10).unwrap();
        assert!(!results.is_empty());
    }

    #[test]
    fn test_keyword_search_exact_score_3() {
        let store = make_store();
        insert_node(&store, "search", "Function", "src/a.py", "python");

        let results = store.keyword_search("search", 10).unwrap();
        let (_, score) = results.iter().find(|(_, _)| true).unwrap();
        assert!(
            (*score - 3.0).abs() < 1e-9,
            "exact match should score 3.0, got {score}"
        );
    }

    // ---- hybrid_search ----

    #[test]
    fn test_hybrid_search_empty_query() {
        let store = make_store();
        let results = hybrid_search(&store, "", None, 20, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_hybrid_search_returns_results() {
        let store = make_store();
        insert_node(&store, "parse_config", "Function", "src/config.py", "python");
        rebuild_fts_index(&store).unwrap();

        let results = hybrid_search(&store, "parse_config", None, 10, None).unwrap();
        assert!(!results.is_empty());
        let first = &results[0];
        assert_eq!(first["name"].as_str().unwrap(), "parse_config");
        assert_eq!(first["kind"].as_str().unwrap(), "Function");
        assert!(first["score"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn test_hybrid_search_kind_filter() {
        let store = make_store();
        insert_node(&store, "MyHandler", "Class", "src/handlers.py", "python");
        insert_node(&store, "MyHandler", "Function", "src/handlers.py", "python");
        rebuild_fts_index(&store).unwrap();

        let results =
            hybrid_search(&store, "MyHandler", Some("Class"), 10, None).unwrap();
        for r in &results {
            assert_eq!(r["kind"].as_str().unwrap(), "Class");
        }
    }

    #[test]
    fn test_hybrid_search_result_shape() {
        let store = make_store();
        insert_node(&store, "do_work", "Function", "src/worker.py", "python");
        rebuild_fts_index(&store).unwrap();

        let results = hybrid_search(&store, "do_work", None, 5, None).unwrap();
        assert!(!results.is_empty());
        let r = &results[0];
        assert!(r.get("name").is_some());
        assert!(r.get("qualified_name").is_some());
        assert!(r.get("kind").is_some());
        assert!(r.get("file_path").is_some());
        assert!(r.get("line_start").is_some());
        assert!(r.get("line_end").is_some());
        assert!(r.get("language").is_some());
        assert!(r.get("params").is_some());
        assert!(r.get("return_type").is_some());
        assert!(r.get("signature").is_some());
        assert!(r.get("score").is_some());
    }

    #[test]
    fn test_hybrid_search_context_file_boost() {
        let store = make_store();
        insert_node(&store, "helper", "Function", "src/hot.py", "python");
        insert_node(&store, "helper", "Function", "src/cold.py", "python");
        rebuild_fts_index(&store).unwrap();

        let context = vec!["src/hot.py".to_string()];
        let results =
            hybrid_search(&store, "helper", None, 10, Some(&context)).unwrap();
        // The node in src/hot.py should appear first (higher boosted score).
        if results.len() >= 2 {
            assert_eq!(
                results[0]["file_path"].as_str().unwrap(),
                "src/hot.py",
                "context-file boosted node should rank first"
            );
        }
    }
}
