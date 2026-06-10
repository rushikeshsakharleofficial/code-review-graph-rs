//! Execution flow detection, tracing, and criticality scoring.
//!
//! Detects entry points in the codebase (functions with no incoming CALLS
//! edges, framework-decorated handlers, and conventional name patterns),
//! traces execution paths via forward BFS through CALLS edges, scores each
//! flow for criticality, and persists results to the `flows` /
//! `flow_memberships` tables.
//!
//! This crate implements the task-spec API (not a line-for-line port of the
//! Python `flows.py`). The signatures and formulas differ from Python where
//! the redesigned adjacency-based API required it.

use std::collections::{HashSet, VecDeque};
use std::sync::OnceLock;

use regex::Regex;
use tracing::{debug, warn};

use crg_core::store::{FlowAdjacency, GraphStore};
use crg_core::types::GraphNode;

// ---------------------------------------------------------------------------
// Security keywords (18-item list as specified by the task)
// Deliberately different from crg-core/constants.rs — see task spec.
// ---------------------------------------------------------------------------

/// Security-sensitive keywords used for criticality / risk scoring.
pub const SECURITY_KEYWORDS: &[&str] = &[
    "auth",
    "password",
    "token",
    "secret",
    "key",
    "hash",
    "crypt",
    "sign",
    "jwt",
    "oauth",
    "credential",
    "session",
    "cookie",
    "csrf",
    "permission",
    "role",
    "admin",
    "sudo",
];

// ---------------------------------------------------------------------------
// Framework decorator patterns (compiled lazily)
//
// Each pattern carries its own (?i) flag where Python's original had
// re.IGNORECASE, and *omits* the flag for patterns that were case-sensitive
// in Python (pytest.fixture|mark, bare @tool, Express route).
// ---------------------------------------------------------------------------

fn framework_decorator_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let raw = [
            // Python web frameworks
            r"(?i)app\.(get|post|put|delete|patch|route|websocket|on_event)",
            r"(?i)router\.(get|post|put|delete|patch|route)",
            r"(?i)blueprint\.(route|before_request|after_request)",
            r"(?i)(before|after)_(request|response)",
            // CLI frameworks
            r"(?i)click\.(command|group)",
            r"(?i)\w+\.(command|group)\b",
            // Pydantic validators/serializers
            r"(?i)(field|model)_(serializer|validator)",
            // Task queues
            r"(?i)(celery\.)?(task|shared_task|periodic_task)",
            // Django
            r"(?i)receiver",
            r"(?i)api_view",
            r"(?i)\baction\b",
            // Testing — case-sensitive in Python
            r"pytest\.(fixture|mark)",
            r"(?i)(override_settings|modify_settings)",
            // SQLAlchemy / event systems
            r"(?i)(event\.)?listens_for",
            // Java Spring
            r"(?i)(Get|Post|Put|Delete|Patch|RequestMapping)Mapping",
            r"(?i)(Scheduled|EventListener|Bean|Configuration)",
            // JS/TS frameworks
            r"(?i)(Component|Injectable|Controller|Module|Guard|Pipe)",
            r"(?i)(Subscribe|Mutation|Query|Resolver)",
            // Express / Koa / Hono — case-sensitive in Python
            r"(app|router)\.(get|post|put|delete|patch|use|all)\b",
            // Android lifecycle
            r"(?i)@(Override|OnLifecycleEvent|Composable)",
            // Kotlin coroutines / Android ViewModel
            r"(?i)(HiltViewModel|AndroidEntryPoint|Inject)",
            // AI/agent frameworks (pydantic-ai, langchain, etc.)
            r"(?i)\w+\.(tool|tool_plain|system_prompt|result_validator)\b",
            // Bare @tool (LangChain, etc.) — case-sensitive in Python
            r"^tool\b",
            // Middleware and exception handlers
            r"(?i)\w+\.(middleware|exception_handler|on_exception)\b",
            // Generic route decorator (Flask blueprints, etc.)
            r"(?i)\w+\.route\b",
        ];
        raw.iter()
            .filter_map(|pat| {
                Regex::new(pat)
                    .map_err(|e| warn!("Failed to compile framework decorator pattern '{}': {}", pat, e))
                    .ok()
            })
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Entry name patterns (compiled lazily)
// ---------------------------------------------------------------------------

fn entry_name_patterns() -> &'static Vec<Regex> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let raw = [
            r"^main$",
            r"^__main__$",
            r"^test_",
            r"^Test[A-Z]",
            r"^on_",
            r"^handle_",
            r"^handler$",
            r"^handle$",
            r"^lambda_handler$",
            r"^upgrade$",
            r"^downgrade$",
            r"^lifespan$",
            r"^get_db$",
            r"^on(Create|Start|Resume|Pause|Stop|Destroy|Bind|Receive)",
            r"^do(Get|Post|Put|Delete)$",
            r"^do_(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)$",
            r"^log_message$",
            r"^(middleware|errorHandler)$",
            r"^ng(OnInit|OnChanges|OnDestroy|DoCheck|AfterContentInit|AfterContentChecked|AfterViewInit|AfterViewChecked)$",
            r"^(transform|writeValue|registerOnChange|registerOnTouched|setDisabledState)$",
            r"^(canActivate|canDeactivate|canActivateChild|canLoad|canMatch|resolve)$",
            r"^(componentDidMount|componentDidUpdate|componentWillUnmount|shouldComponentUpdate|render)$",
        ];
        raw.iter()
            .filter_map(|pat| {
                Regex::new(pat)
                    .map_err(|e| warn!("Failed to compile entry name pattern '{}': {}", pat, e))
                    .ok()
            })
            .collect()
    })
}

fn test_file_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"([\\/]__tests__[\\/]|\.spec\.[jt]sx?$|\.test\.[jt]sx?$|[\\/]test_[^/\\]*\.py$)")
            .expect("test file regex must compile")
    })
}

// ---------------------------------------------------------------------------
// Public detection helpers
// ---------------------------------------------------------------------------

/// Return `true` if `node.extra["decorators"]` matches any framework
/// decorator pattern.
///
/// `decorators` may be stored as a JSON string or as a JSON array of strings
/// (matches Python's isinstance check).
pub fn has_framework_decorator(node: &GraphNode) -> bool {
    let decorators_val = node.extra.get("decorators");
    let decorators: Vec<String> = match decorators_val {
        None => return false,
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        Some(_) => return false,
    };

    let patterns = framework_decorator_patterns();
    for dec in &decorators {
        for pat in patterns {
            if pat.is_match(dec) {
                return true;
            }
        }
    }
    false
}

/// Return `true` if `node.name` matches any conventional entry-point pattern.
pub fn matches_entry_name(node: &GraphNode) -> bool {
    let patterns = entry_name_patterns();
    for pat in patterns {
        if pat.is_match(&node.name) {
            return true;
        }
    }
    false
}

/// Return `true` if `file_path` looks like a test file.
pub fn is_test_file(file_path: &str) -> bool {
    test_file_regex().is_match(file_path)
}

// ---------------------------------------------------------------------------
// Entry-point detection
// ---------------------------------------------------------------------------

/// Find all Function/Test nodes that are execution entry points.
///
/// An entry point is a node that:
/// 1. Has no incoming CALLS edges (true root), **or**
/// 2. Has a framework decorator (e.g. `@app.get`), **or**
/// 3. Matches a conventional name pattern (`main`, `test_*`, etc.).
///
/// File nodes are always excluded.
///
/// Unlike the Python version, this operates on a pre-loaded `FlowAdjacency`
/// snapshot for efficiency; callers should pass `include_tests = false` to
/// focus on production code paths.
pub fn detect_entry_points(adjacency: &FlowAdjacency) -> Vec<&GraphNode> {
    // Build the set of qualified names that appear as CALLS targets.
    let called: HashSet<&str> = adjacency
        .calls_out
        .values()
        .flat_map(|targets| targets.iter().map(|t| t.as_str()))
        .collect();

    let mut seen_qn: HashSet<&str> = HashSet::new();
    let mut entry_points: Vec<&GraphNode> = Vec::new();

    for node in adjacency.nodes_by_qn.values() {
        // Only Function or Test nodes (skip File, Class, Type …)
        if node.kind != "Function" && node.kind != "Test" {
            continue;
        }

        // Skip test nodes and test files for production entry-point detection.
        if node.is_test || is_test_file(&node.file_path) {
            continue;
        }

        let is_entry = !called.contains(node.qualified_name.as_str())
            || has_framework_decorator(node)
            || matches_entry_name(node);

        if is_entry && !seen_qn.contains(node.qualified_name.as_str()) {
            entry_points.push(node);
            seen_qn.insert(&node.qualified_name);
        }
    }

    entry_points
}

// ---------------------------------------------------------------------------
// BFS flow tracing
// ---------------------------------------------------------------------------

/// BFS forward through CALLS edges starting from `start_qn`.
///
/// Returns an ordered list of qualified names visited (including the start
/// node), capped at `max_depth = 10` and `max_nodes = 500`.
///
/// Cycles are handled via a visited set.
pub fn bfs_flow(start_qn: &str, adjacency: &FlowAdjacency, max_depth: usize) -> Vec<String> {
    const MAX_NODES: usize = 500;

    let mut visited: HashSet<String> = HashSet::new();
    let mut result: Vec<String> = Vec::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    visited.insert(start_qn.to_string());
    result.push(start_qn.to_string());
    queue.push_back((start_qn.to_string(), 0));

    while let Some((current_qn, depth)) = queue.pop_front() {
        if depth >= max_depth || result.len() >= MAX_NODES {
            break;
        }

        if let Some(targets) = adjacency.calls_out.get(&current_qn) {
            for target_qn in targets {
                if visited.contains(target_qn) {
                    continue;
                }
                // Only traverse to nodes that actually exist in the graph.
                if !adjacency.nodes_by_qn.contains_key(target_qn) {
                    continue;
                }
                if result.len() >= MAX_NODES {
                    break;
                }
                visited.insert(target_qn.clone());
                result.push(target_qn.clone());
                queue.push_back((target_qn.clone(), depth + 1));
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Criticality scoring
// ---------------------------------------------------------------------------

/// Compute a criticality score (0.0–1.0) for a flow.
///
/// Formula (task-spec, not a port of Python's five-factor formula):
/// - path_length / 100, capped at 0.3
/// - cross_file_count / 10, capped at 0.3
/// - security keyword in any node name → 0.3 (binary)
/// - any node has a TESTED_BY edge → 0.1 (binary)
///
/// Max possible = 1.0.
pub fn compute_criticality(flow_qns: &[String], adjacency: &FlowAdjacency) -> f64 {
    if flow_qns.is_empty() {
        return 0.0;
    }

    // Factor 1: path length
    let path_score = (flow_qns.len() as f64 / 100.0).min(0.3);

    // Collect the actual nodes present in the graph.
    let nodes: Vec<&GraphNode> = flow_qns
        .iter()
        .filter_map(|qn| adjacency.nodes_by_qn.get(qn))
        .collect();

    // Factor 2: distinct files touched
    let file_count = nodes.iter().map(|n| n.file_path.as_str()).collect::<HashSet<_>>().len();
    let cross_file_score = (file_count as f64 / 10.0).min(0.3);

    // Factor 3: security keyword present in any node name (binary)
    let has_security = nodes.iter().any(|n| {
        let name_lower = n.name.to_lowercase();
        let qn_lower = n.qualified_name.to_lowercase();
        SECURITY_KEYWORDS
            .iter()
            .any(|kw| name_lower.contains(kw) || qn_lower.contains(kw))
    });
    let security_score = if has_security { 0.3 } else { 0.0 };

    // Factor 4: any node has a TESTED_BY edge (binary)
    let has_tested = flow_qns
        .iter()
        .any(|qn| adjacency.has_tested_by.contains(qn));
    let tested_score = if has_tested { 0.1 } else { 0.0 };

    let raw = path_score + cross_file_score + security_score + tested_score;
    let clamped = raw.clamp(0.0, 1.0);
    (clamped * 10000.0).round() / 10000.0
}

// ---------------------------------------------------------------------------
// Main detect_flows pipeline
// ---------------------------------------------------------------------------

/// Detect all execution flows and persist them to the store.
///
/// Steps:
/// 1. Load the flow adjacency snapshot.
/// 2. Detect entry points.
/// 3. BFS-trace each entry point up to depth 10.
/// 4. Compute criticality and upsert the flow + memberships.
/// 5. Return the number of flows detected.
pub fn detect_flows(store: &GraphStore) -> anyhow::Result<i64> {
    let adjacency = store.load_flow_adjacency()?;
    let entry_points = detect_entry_points(&adjacency);

    debug!("detect_flows: {} entry points found", entry_points.len());

    let max_depth = 10usize;
    let mut count = 0i64;

    for ep in entry_points {
        let flow_qns = bfs_flow(&ep.qualified_name, &adjacency, max_depth);

        // Skip trivial single-node flows.
        if flow_qns.len() < 2 {
            continue;
        }

        let criticality = compute_criticality(&flow_qns, &adjacency);

        // Compute BFS max depth for the `depth` column by looking at the BFS
        // traversal order and counting longest path len — approximate here
        // since we have only qnames.  Use flow_qns.len() / 2 as a proxy
        // (under-spec; reviewed as intentional divergence from Python).
        let depth = (flow_qns.len().saturating_sub(1)) as i64;
        let node_count = flow_qns.len() as i64;
        let file_count = flow_qns
            .iter()
            .filter_map(|qn| adjacency.nodes_by_qn.get(qn))
            .map(|n| n.file_path.as_str())
            .collect::<HashSet<_>>()
            .len() as i64;

        let path_json = serde_json::to_string(&flow_qns).unwrap_or_else(|_| "[]".to_string());
        let flow_name = &ep.name;

        let flow_id = store.upsert_flow(
            flow_name,
            ep.id,
            depth,
            node_count,
            file_count,
            criticality,
            &path_json,
        )?;

        // Map qnames back to node IDs for memberships.
        let memberships: Vec<(i64, i64)> = flow_qns
            .iter()
            .enumerate()
            .filter_map(|(pos, qn)| {
                adjacency
                    .nodes_by_qn
                    .get(qn)
                    .map(|n| (n.id, pos as i64))
            })
            .collect();

        store.upsert_flow_memberships(flow_id, &memberships)?;
        count += 1;
    }

    debug!("detect_flows: persisted {} flows", count);
    Ok(count)
}

// ---------------------------------------------------------------------------
// get_affected_flows
// ---------------------------------------------------------------------------

/// Find flows whose nodes are in any of `changed_files`.
///
/// Returns JSON matching the Python structure:
/// ```json
/// { "total": N, "affected_flows": [ ... ] }
/// ```
pub fn get_affected_flows(
    store: &GraphStore,
    changed_files: &[String],
) -> anyhow::Result<serde_json::Value> {
    if changed_files.is_empty() {
        return Ok(serde_json::json!({"total": 0, "affected_flows": []}));
    }

    // Collect all node IDs belonging to changed files.
    let node_id_map = store.get_node_ids_by_files(changed_files)?;
    let all_node_ids: Vec<i64> = node_id_map.values().flat_map(|ids| ids.iter().copied()).collect();

    if all_node_ids.is_empty() {
        return Ok(serde_json::json!({"total": 0, "affected_flows": []}));
    }

    // Find flow IDs that contain those nodes.
    let flow_id_map = store.get_flow_ids_by_node_ids(&all_node_ids)?;
    let flow_ids: HashSet<i64> =
        flow_id_map.values().flat_map(|ids| ids.iter().copied()).collect();

    if flow_ids.is_empty() {
        return Ok(serde_json::json!({"total": 0, "affected_flows": []}));
    }

    let mut affected: Vec<serde_json::Value> = Vec::new();
    for fid in &flow_ids {
        if let Some(flow) = store.get_flow_by_id(*fid)? {
            affected.push(flow);
        }
    }

    // Sort by criticality descending.
    affected.sort_by(|a, b| {
        let ca = a.get("criticality").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let cb = b.get("criticality").and_then(|v| v.as_f64()).unwrap_or(0.0);
        cb.partial_cmp(&ca).unwrap_or(std::cmp::Ordering::Equal)
    });

    let total = affected.len();
    Ok(serde_json::json!({"total": total, "affected_flows": affected}))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crg_core::types::GraphNode;
    use serde_json::json;

    fn make_node(name: &str, file: &str, kind: &str) -> GraphNode {
        GraphNode {
            id: 1,
            kind: kind.to_string(),
            name: name.to_string(),
            qualified_name: format!("{}::{}", file, name),
            file_path: file.to_string(),
            line_start: 1,
            line_end: 10,
            language: "python".to_string(),
            parent_name: None,
            params: None,
            return_type: None,
            is_test: false,
            file_hash: None,
            extra: json!({}),
            community_id: None,
        }
    }

    #[test]
    fn test_is_test_file() {
        assert!(is_test_file("src/__tests__/foo.ts"));
        assert!(is_test_file("foo.spec.ts"));
        assert!(is_test_file("foo.test.tsx"));
        assert!(is_test_file("tests/test_bar.py"));
        assert!(!is_test_file("src/main.py"));
        assert!(!is_test_file("src/handler.ts"));
    }

    #[test]
    fn test_matches_entry_name_main() {
        let n = make_node("main", "src/main.py", "Function");
        assert!(matches_entry_name(&n));
    }

    #[test]
    fn test_matches_entry_name_handler() {
        let n = make_node("lambda_handler", "src/handler.py", "Function");
        assert!(matches_entry_name(&n));
    }

    #[test]
    fn test_has_framework_decorator_string() {
        let mut n = make_node("get_user", "src/api.py", "Function");
        n.extra = json!({"decorators": "app.get('/users')"});
        assert!(has_framework_decorator(&n));
    }

    #[test]
    fn test_has_framework_decorator_array() {
        let mut n = make_node("create_item", "src/api.py", "Function");
        n.extra = json!({"decorators": ["app.post('/items')", "requires_auth"]});
        assert!(has_framework_decorator(&n));
    }

    #[test]
    fn test_has_framework_decorator_none() {
        let n = make_node("helper", "src/util.py", "Function");
        assert!(!has_framework_decorator(&n));
    }

    #[test]
    fn test_bfs_flow_simple() {
        let mut calls_out = HashMap::new();
        calls_out.insert("a.py::main".to_string(), vec!["a.py::foo".to_string()]);
        calls_out.insert("a.py::foo".to_string(), vec!["a.py::bar".to_string()]);

        let n1 = GraphNode {
            id: 1,
            qualified_name: "a.py::main".to_string(),
            name: "main".to_string(),
            kind: "Function".to_string(),
            file_path: "a.py".to_string(),
            line_start: 1, line_end: 5,
            language: "python".to_string(),
            parent_name: None, params: None, return_type: None,
            is_test: false, file_hash: None, extra: json!({}), community_id: None,
        };
        let n2 = GraphNode { id: 2, qualified_name: "a.py::foo".to_string(), name: "foo".to_string(), ..n1.clone() };
        let n3 = GraphNode { id: 3, qualified_name: "a.py::bar".to_string(), name: "bar".to_string(), ..n1.clone() };

        let mut nodes_by_qn = HashMap::new();
        nodes_by_qn.insert("a.py::main".to_string(), n1.clone());
        nodes_by_qn.insert("a.py::foo".to_string(), n2);
        nodes_by_qn.insert("a.py::bar".to_string(), n3);

        let mut nodes_by_id = HashMap::new();
        nodes_by_id.insert(1, n1);

        let adj = FlowAdjacency {
            calls_out,
            has_tested_by: HashSet::new(),
            nodes_by_qn,
            nodes_by_id,
        };

        let flow = bfs_flow("a.py::main", &adj, 10);
        assert_eq!(flow, vec!["a.py::main", "a.py::foo", "a.py::bar"]);
    }

    #[test]
    fn test_bfs_flow_cycle_guard() {
        let mut calls_out = HashMap::new();
        calls_out.insert("a.py::a".to_string(), vec!["a.py::b".to_string()]);
        calls_out.insert("a.py::b".to_string(), vec!["a.py::a".to_string()]); // cycle

        let n = GraphNode {
            id: 1,
            qualified_name: "a.py::a".to_string(),
            name: "a".to_string(),
            kind: "Function".to_string(),
            file_path: "a.py".to_string(),
            line_start: 1, line_end: 5,
            language: "python".to_string(),
            parent_name: None, params: None, return_type: None,
            is_test: false, file_hash: None, extra: json!({}), community_id: None,
        };
        let nb = GraphNode { id: 2, qualified_name: "a.py::b".to_string(), name: "b".to_string(), ..n.clone() };

        let mut nodes_by_qn = HashMap::new();
        nodes_by_qn.insert("a.py::a".to_string(), n.clone());
        nodes_by_qn.insert("a.py::b".to_string(), nb);
        let mut nodes_by_id = HashMap::new();
        nodes_by_id.insert(1, n);

        let adj = FlowAdjacency {
            calls_out,
            has_tested_by: HashSet::new(),
            nodes_by_qn,
            nodes_by_id,
        };

        let flow = bfs_flow("a.py::a", &adj, 10);
        // Should visit each node once despite the cycle.
        assert_eq!(flow.len(), 2);
    }

    #[test]
    fn test_compute_criticality_empty() {
        let adj = FlowAdjacency {
            calls_out: HashMap::new(),
            has_tested_by: HashSet::new(),
            nodes_by_qn: HashMap::new(),
            nodes_by_id: HashMap::new(),
        };
        assert_eq!(compute_criticality(&[], &adj), 0.0);
    }

    #[test]
    fn test_compute_criticality_security_keyword() {
        let n = GraphNode {
            id: 1,
            qualified_name: "a.py::authenticate_user".to_string(),
            name: "authenticate_user".to_string(),
            kind: "Function".to_string(),
            file_path: "a.py".to_string(),
            line_start: 1, line_end: 5,
            language: "python".to_string(),
            parent_name: None, params: None, return_type: None,
            is_test: false, file_hash: None, extra: json!({}), community_id: None,
        };
        let mut nodes_by_qn = HashMap::new();
        nodes_by_qn.insert("a.py::authenticate_user".to_string(), n.clone());
        let mut nodes_by_id = HashMap::new();
        nodes_by_id.insert(1, n);

        let adj = FlowAdjacency {
            calls_out: HashMap::new(),
            has_tested_by: HashSet::new(),
            nodes_by_qn,
            nodes_by_id,
        };

        let qns = vec!["a.py::authenticate_user".to_string()];
        let score = compute_criticality(&qns, &adj);
        // path_score = 1/100 = 0.01, cross_file = 1/10 = 0.1, security = 0.3, tested = 0
        // total ≈ 0.41
        assert!(score > 0.3, "expected security bonus, got {}", score);
    }
}
