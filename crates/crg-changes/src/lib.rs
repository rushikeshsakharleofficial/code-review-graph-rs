//! Change impact analysis for code review.
//!
//! Maps git/svn diffs to affected functions, flows, communities, and test
//! coverage gaps. Produces risk-scored, priority-ordered review guidance.
//!
//! # Security invariants
//! - NEVER uses `shell=true` (no `Command::new("sh")` / `Command::new("bash")`).
//! - Git refs and SVN revisions are validated against safe-char regexes before
//!   any subprocess is spawned.
//! - All subprocess calls set `.stdin(Stdio::null())`.

use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use tracing::warn;

use crg_core::store::GraphStore;
use crg_core::types::GraphNode;
use crg_flows::{get_affected_flows, SECURITY_KEYWORDS};

// ---------------------------------------------------------------------------
// Security: safe-ref validation regexes
// ---------------------------------------------------------------------------

/// Validates a git ref string — only safe characters allowed.
fn safe_git_ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9_.~^/@{}\-]+$").expect("safe git ref regex must compile")
    })
}

/// Validates an SVN revision range — only safe revision syntax allowed.
fn safe_svn_rev_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^r?\d+(:r?\d+|:HEAD|:BASE|:COMMITTED)?$")
            .expect("safe svn rev regex must compile")
    })
}

/// Read `CRG_GIT_TIMEOUT` from environment, default 30 seconds.
fn git_timeout() -> Duration {
    let secs: u64 = env::var("CRG_GIT_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    Duration::from_secs(secs)
}

// ---------------------------------------------------------------------------
// 1. parse_git_diff_ranges
// ---------------------------------------------------------------------------

/// Run `git diff --unified=0 <base> --` and extract changed line ranges per file.
///
/// Returns a map of file paths to lists of `(start_line, end_line)` tuples.
/// Returns an empty map on error.
///
/// # Security
/// - `base` is validated against [`safe_git_ref_re`] before the subprocess is spawned.
/// - Uses `Command::new("git")` — never a shell.
/// - `.stdin(Stdio::null())` is set.
pub fn parse_git_diff_ranges(
    repo_root: &str,
    base: &str,
) -> HashMap<String, Vec<(i64, i64)>> {
    if !safe_git_ref_re().is_match(base) {
        warn!("Invalid git ref rejected: {}", base);
        return HashMap::new();
    }

    let timeout = git_timeout();

    match run_with_timeout(
        std::process::Command::new("git")
            .args(["diff", "--unified=0", base, "--"])
            .current_dir(repo_root)
            .stdin(std::process::Stdio::null()),
        timeout,
    ) {
        Ok(stdout) => parse_unified_diff(&stdout),
        Err(e) => {
            warn!("git diff error: {}", e);
            HashMap::new()
        }
    }
}

/// Run `svn diff --non-interactive [−r rev_range]` and extract changed line ranges.
///
/// # Security
/// - `rev_range` (when provided) is validated against [`safe_svn_rev_re`].
/// - Uses `Command::new("svn")` — never a shell.
/// - `.stdin(Stdio::null())` is set.
pub fn parse_svn_diff_ranges(
    repo_root: &str,
    rev_range: Option<&str>,
) -> HashMap<String, Vec<(i64, i64)>> {
    let mut cmd = std::process::Command::new("svn");
    cmd.args(["diff", "--non-interactive"])
        .current_dir(repo_root)
        .stdin(std::process::Stdio::null());

    if let Some(rev) = rev_range {
        if !safe_svn_rev_re().is_match(rev) {
            warn!("Invalid SVN revision range rejected: {}", rev);
            return HashMap::new();
        }
        cmd.args(["-r", rev]);
    }

    let timeout = git_timeout();
    match run_with_timeout(&mut cmd, timeout) {
        Ok(stdout) => parse_unified_diff(&stdout),
        Err(e) => {
            warn!("svn diff error: {}", e);
            HashMap::new()
        }
    }
}

/// Auto-detect VCS (Git vs SVN) and return changed line ranges per file.
///
/// - Dispatches to `parse_svn_diff_ranges` when `.svn` directory is present.
/// - Otherwise dispatches to `parse_git_diff_ranges`.
pub fn parse_diff_ranges(
    repo_root: &str,
    base: &str,
) -> HashMap<String, Vec<(i64, i64)>> {
    if Path::new(repo_root).join(".svn").exists() {
        let rev_range = if safe_svn_rev_re().is_match(base) {
            Some(base)
        } else {
            None
        };
        return parse_svn_diff_ranges(repo_root, rev_range);
    }
    parse_git_diff_ranges(repo_root, base)
}

// ---------------------------------------------------------------------------
// Internal: spawn subprocess with a timeout
// ---------------------------------------------------------------------------

/// Spawn a `Command`, wait up to `timeout`, kill the child if it exceeds that
/// limit, and return stdout on success or an error string on failure.
///
/// `std::process::Command` has no built-in timeout; we implement one by
/// spawning the child and using a thread + channel to enforce the deadline.
fn run_with_timeout(
    cmd: &mut std::process::Command,
    timeout: Duration,
) -> Result<String, String> {
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {}", e))?;

    // Communicate via a channel from a dedicated thread.
    let (tx, rx) = std::sync::mpsc::channel::<Result<(Vec<u8>, Vec<u8>), String>>();

    // Take ownership of stdout/stderr before moving `child` into the thread.
    use std::io::Read;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(ref mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut out);
        }
        if let Some(ref mut p) = stderr_pipe {
            let _ = p.read_to_end(&mut err);
        }
        let _ = tx.send(Ok((out, err)));
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok((stdout_bytes, stderr_bytes))) => {
            let status = child.wait().map_err(|e| format!("wait failed: {}", e))?;
            if status.success() {
                let text = String::from_utf8_lossy(&stdout_bytes).into_owned();
                Ok(text)
            } else {
                let msg = String::from_utf8_lossy(&stderr_bytes).into_owned();
                Err(format!("process exited with {:?}: {}", status.code(), &msg[..msg.len().min(200)]))
            }
        }
        Ok(Err(e)) => {
            let _ = child.kill();
            Err(e)
        }
        Err(_timeout) => {
            let _ = child.kill();
            Err(format!("subprocess timed out after {:?}", timeout))
        }
    }
}

// ---------------------------------------------------------------------------
// Unified diff parser
// ---------------------------------------------------------------------------

/// Parse unified diff output into `file → [(start, end)]` mappings.
///
/// Handles `+++ b/path` headers and `@@ … +start,count @@` hunk headers.
pub fn parse_unified_diff(diff_text: &str) -> HashMap<String, Vec<(i64, i64)>> {
    static FILE_RE: OnceLock<Regex> = OnceLock::new();
    static HUNK_RE: OnceLock<Regex> = OnceLock::new();

    let file_pat = FILE_RE
        .get_or_init(|| Regex::new(r"^\+\+\+ b/(.+)$").expect("file pattern must compile"));
    let hunk_pat = HUNK_RE.get_or_init(|| {
        Regex::new(r"^@@ .+? \+(\d+)(?:,(\d+))? @@").expect("hunk pattern must compile")
    });

    let mut ranges: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
    let mut current_file: Option<String> = None;

    for line in diff_text.lines() {
        if let Some(caps) = file_pat.captures(line) {
            current_file = Some(caps[1].to_string());
            continue;
        }
        if let (Some(caps), Some(ref file)) = (hunk_pat.captures(line), &current_file) {
            let start: i64 = caps[1].parse().unwrap_or(1);
            let count: i64 = caps.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
            let end = if count == 0 { start } else { start + count - 1 };
            ranges.entry(file.clone()).or_default().push((start, end));
        }
    }

    ranges
}

// ---------------------------------------------------------------------------
// 2. map_changes_to_nodes
// ---------------------------------------------------------------------------

/// Find graph nodes whose line ranges overlap the changed lines.
///
/// Returns a deduplicated list of overlapping nodes.
pub fn map_changes_to_nodes(
    store: &GraphStore,
    changed_ranges: &HashMap<String, Vec<(i64, i64)>>,
) -> anyhow::Result<Vec<GraphNode>> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result: Vec<GraphNode> = Vec::new();

    for (file_path, ranges) in changed_ranges {
        let mut nodes = store.get_nodes_by_file(file_path)?;

        // Fallback: the graph may store absolute paths; try suffix matching.
        if nodes.is_empty() {
            let matched_paths = store.get_files_matching(file_path)?;
            for mp in matched_paths {
                nodes.extend(store.get_nodes_by_file(&mp)?);
            }
        }

        for node in nodes {
            if seen.contains(&node.qualified_name) {
                continue;
            }
            // Check line-range overlap: node overlaps range if node.start <= end && node.end >= start
            for (start, end) in ranges {
                if node.line_start <= *end && node.line_end >= *start {
                    seen.insert(node.qualified_name.clone());
                    result.push(node);
                    break;
                }
            }
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// 3. compute_risk_score
// ---------------------------------------------------------------------------

/// Compute a risk score (0.0–1.0) for a single node.
///
/// Scoring factors (ported faithfully from Python `changes.py`):
/// - Flow participation: `sum(criticalities)` capped at 0.25
/// - Community crossing: `count_cross_community * 0.05` capped at 0.15
/// - Test coverage: `0.30 - (min(test_count/5.0, 1.0) * 0.25)` (0.05–0.30)
/// - Security sensitivity: +0.20 if name contains a security keyword
/// - Caller count: `callers/20.0` capped at 0.10
pub fn compute_risk_score(store: &GraphStore, node: &GraphNode) -> anyhow::Result<f64> {
    let mut score = 0.0f64;

    // --- Flow participation (cap 0.25), weighted by criticality ---
    let flow_criticalities = store.get_flow_criticalities_for_node(node.id)?;
    if !flow_criticalities.is_empty() {
        score += flow_criticalities.iter().copied().sum::<f64>().min(0.25);
    } else {
        let flow_count = store.count_flow_memberships(node.id)?;
        score += (flow_count as f64 * 0.05).min(0.25);
    }

    // --- Community crossing (cap 0.15) ---
    let callers = store.get_edges_by_target(&node.qualified_name)?;
    let caller_edges: Vec<_> = callers.iter().filter(|e| e.kind == "CALLS").collect();

    let node_cid = store.get_node_community_id(node.id)?;
    let cross_community = if node_cid.is_some() && !caller_edges.is_empty() {
        let caller_qns: Vec<String> = caller_edges
            .iter()
            .map(|e| e.source_qualified.clone())
            .collect();
        let cid_map = store.get_community_ids_by_qualified_names(&caller_qns)?;
        cid_map
            .values()
            .filter(|cid| cid.is_some() && **cid != node_cid)
            .count() as f64
    } else {
        0.0
    };
    score += (cross_community * 0.05).min(0.15);

    // --- Test coverage (direct + transitive) ---
    let transitive_tests = store.get_transitive_tests(&node.qualified_name)?;
    let test_count = transitive_tests.len() as f64;
    score += 0.30 - (test_count / 5.0).min(1.0) * 0.25;

    // --- Security sensitivity ---
    let name_lower = node.name.to_lowercase();
    let qn_lower = node.qualified_name.to_lowercase();
    if SECURITY_KEYWORDS
        .iter()
        .any(|kw| name_lower.contains(kw) || qn_lower.contains(kw))
    {
        score += 0.20;
    }

    // --- Caller count (cap 0.10) ---
    let caller_count = caller_edges.len() as f64;
    score += (caller_count / 20.0).min(0.10);

    let clamped = score.clamp(0.0, 1.0);
    Ok((clamped * 10000.0).round() / 10000.0)
}

// ---------------------------------------------------------------------------
// 4. analyze_changes
// ---------------------------------------------------------------------------

/// Maximum number of changed functions to score (prevent O(N×M) explosion).
fn max_changed_funcs() -> usize {
    env::var("CRG_MAX_CHANGED_FUNCS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500)
}

/// Analyze changes and produce risk-scored review guidance.
///
/// Returns JSON matching the Python `analyze_changes` output:
/// ```json
/// {
///   "summary": "...",
///   "risk_score": 0.5,
///   "changed_functions": [...],
///   "affected_flows": [...],
///   "test_gaps": [...],
///   "review_priorities": [...],
///   "functions_truncated": false
/// }
/// ```
pub fn analyze_changes(
    store: &GraphStore,
    changed_files: &[String],
    changed_ranges: Option<&HashMap<String, Vec<(i64, i64)>>>,
    repo_root: Option<&str>,
    base: &str,
) -> anyhow::Result<serde_json::Value> {
    // Compute changed ranges if not supplied.
    let computed_ranges: Option<HashMap<String, Vec<(i64, i64)>>> = if changed_ranges.is_none() {
        repo_root.map(|root| parse_diff_ranges(root, base))
    } else {
        None
    };
    let resolved_ranges: Option<&HashMap<String, Vec<(i64, i64)>>> =
        changed_ranges.or(computed_ranges.as_ref());

    // Map changes to nodes.
    let changed_nodes: Vec<GraphNode> = if let Some(ranges) = resolved_ranges {
        map_changes_to_nodes(store, ranges)?
    } else {
        // Fallback: all nodes in changed files.
        let mut nodes = Vec::new();
        for fp in changed_files {
            nodes.extend(store.get_nodes_by_file(fp)?);
        }
        nodes
    };

    // Filter to scoreable kinds (Function, Test, Class).
    let mut changed_funcs: Vec<GraphNode> = changed_nodes
        .into_iter()
        .filter(|n| matches!(n.kind.as_str(), "Function" | "Test" | "Class"))
        .collect();

    let max_funcs = max_changed_funcs();
    let funcs_truncated = changed_funcs.len() > max_funcs;
    if funcs_truncated {
        changed_funcs.truncate(max_funcs);
    }

    // Compute per-node risk scores.
    let mut node_risks: Vec<serde_json::Value> = Vec::new();
    for node in &changed_funcs {
        let risk = compute_risk_score(store, node)?;
        node_risks.push(serde_json::json!({
            "name": node.name,
            "qualified_name": node.qualified_name,
            "kind": node.kind,
            "file": node.file_path,
            "line_start": node.line_start,
            "line_end": node.line_end,
            "risk_score": risk,
        }));
    }

    // Overall risk = max of individual risks, or 0.
    let overall_risk = node_risks
        .iter()
        .filter_map(|nr| nr.get("risk_score").and_then(|v| v.as_f64()))
        .fold(0.0f64, f64::max);

    // Affected flows.
    let affected = get_affected_flows(store, changed_files)?;
    let affected_flows = affected
        .get("affected_flows")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let affected_total = affected
        .get("total")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // Test gaps: changed functions without any TESTED_BY edges.
    let mut test_gaps: Vec<serde_json::Value> = Vec::new();
    for node in &changed_funcs {
        if node.is_test {
            continue;
        }
        let tested = store.get_edges_by_target(&node.qualified_name)?;
        if !tested.iter().any(|e| e.kind == "TESTED_BY") {
            test_gaps.push(serde_json::json!({
                "name": node.name,
                "qualified_name": node.qualified_name,
                "file": node.file_path,
                "line_start": node.line_start,
                "line_end": node.line_end,
            }));
        }
    }

    // Review priorities: top 10 by risk score descending.
    let mut review_priorities = node_risks.clone();
    review_priorities.sort_by(|a, b| {
        let ra = a.get("risk_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let rb = b.get("risk_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
    });
    review_priorities.truncate(10);

    // Build summary string (mirrors Python output format).
    let mut summary_parts = vec![
        format!("Analyzed {} changed file(s):", changed_files.len()),
        format!("  - {} changed function(s)/class(es)", changed_funcs.len()),
        format!("  - {} affected flow(s)", affected_total),
        format!("  - {} test gap(s)", test_gaps.len()),
        format!("  - Overall risk score: {:.2}", overall_risk),
    ];

    if !test_gaps.is_empty() {
        // Dedup by bare name in the human summary (defensive against duplicate
        // qualified_names from path normalisation issues).
        let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut gap_names: Vec<String> = Vec::new();
        for g in &test_gaps {
            if let Some(n) = g.get("name").and_then(|v| v.as_str()) {
                if seen_names.insert(n.to_string()) {
                    gap_names.push(n.to_string());
                    if gap_names.len() >= 5 {
                        break;
                    }
                }
            }
        }
        summary_parts.push(format!("  - Untested: {}", gap_names.join(", ")));
    }

    if funcs_truncated {
        summary_parts.push(format!(
            "  - Warning: analysis capped at {} functions (set CRG_MAX_CHANGED_FUNCS to adjust)",
            max_funcs
        ));
    }

    Ok(serde_json::json!({
        "summary": summary_parts.join("\n"),
        "risk_score": overall_risk,
        "changed_functions": node_risks,
        "affected_flows": affected_flows,
        "test_gaps": test_gaps,
        "review_priorities": review_priorities,
        "functions_truncated": funcs_truncated,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_git_ref_valid() {
        assert!(safe_git_ref_re().is_match("HEAD~1"));
        assert!(safe_git_ref_re().is_match("main"));
        assert!(safe_git_ref_re().is_match("origin/main"));
        assert!(safe_git_ref_re().is_match("v1.2.3"));
        assert!(safe_git_ref_re().is_match("feature/my-branch"));
        assert!(safe_git_ref_re().is_match("abc123def"));
    }

    #[test]
    fn test_safe_git_ref_invalid() {
        assert!(!safe_git_ref_re().is_match("main; rm -rf /"));
        assert!(!safe_git_ref_re().is_match("$(whoami)"));
        assert!(!safe_git_ref_re().is_match("main`id`"));
        assert!(!safe_git_ref_re().is_match("branch\nnewline"));
        assert!(!safe_git_ref_re().is_match(""));
    }

    #[test]
    fn test_safe_svn_rev_valid() {
        assert!(safe_svn_rev_re().is_match("r123"));
        assert!(safe_svn_rev_re().is_match("123"));
        assert!(safe_svn_rev_re().is_match("r100:HEAD"));
        assert!(safe_svn_rev_re().is_match("100:r200"));
        assert!(safe_svn_rev_re().is_match("r42:BASE"));
        assert!(safe_svn_rev_re().is_match("10:COMMITTED"));
    }

    #[test]
    fn test_safe_svn_rev_invalid() {
        assert!(!safe_svn_rev_re().is_match("r123; rm -rf /"));
        assert!(!safe_svn_rev_re().is_match("$(rev)"));
        assert!(!safe_svn_rev_re().is_match("HEAD"));
        assert!(!safe_svn_rev_re().is_match(""));
    }

    #[test]
    fn test_parse_unified_diff_basic() {
        let diff = "\
diff --git a/src/main.py b/src/main.py
--- a/src/main.py
+++ b/src/main.py
@@ -10,3 +10,4 @@ def foo():
+    new_line = True
";
        let ranges = parse_unified_diff(diff);
        let r = ranges.get("src/main.py").expect("should have main.py");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0], (10, 13)); // start=10, count=4 => end=13
    }

    #[test]
    fn test_parse_unified_diff_deletion_hunk() {
        let diff = "\
+++ b/src/foo.rs
@@ -5,0 +5,0 @@ fn bar() {
";
        let ranges = parse_unified_diff(diff);
        let r = ranges.get("src/foo.rs").expect("should have foo.rs");
        // count=0 → end = start (pure deletion handled)
        assert_eq!(r[0], (5, 5));
    }

    #[test]
    fn test_parse_unified_diff_multiple_files() {
        let diff = "\
+++ b/a.py
@@ -1,2 +1,3 @@
+added
+++ b/b.py
@@ -10,1 +10,2 @@
+line
";
        let ranges = parse_unified_diff(diff);
        assert!(ranges.contains_key("a.py"));
        assert!(ranges.contains_key("b.py"));
    }

    #[test]
    fn test_invalid_git_ref_returns_empty() {
        // Should return empty without panicking (no subprocess spawned)
        let result = parse_git_diff_ranges("/tmp", "main; rm -rf /");
        assert!(result.is_empty());
    }

    #[test]
    fn test_invalid_svn_rev_returns_empty() {
        let result = parse_svn_diff_ranges("/tmp", Some("$(rm -rf /)"));
        assert!(result.is_empty());
    }
}
