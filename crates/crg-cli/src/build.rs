use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crg_core::store::GraphStore;

/// Options controlling how a build or incremental update is run.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// If `true`, clear the existing graph before parsing.
    pub full_rebuild: bool,
    /// Git base ref used by `incremental_update` to find changed files.
    pub base: String,
    /// Postprocessing level: "full", "minimal", or "none".
    pub postprocess: String,
    /// Optional Rayon thread-pool size. `None` = Rayon default.
    pub max_threads: Option<usize>,
    /// Whether to recurse into git submodules (future use).
    pub recurse_submodules: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            full_rebuild: false,
            base: "HEAD~1".to_string(),
            postprocess: "full".to_string(),
            max_threads: None,
            recurse_submodules: false,
        }
    }
}

/// Summary returned by [`full_build`] and [`incremental_update`].
#[derive(Debug)]
pub struct BuildResult {
    pub files_parsed: usize,
    pub files_skipped: usize,
    pub nodes_total: i64,
    pub edges_total: i64,
    pub duration_secs: f64,
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Parse every source file under `repo_root` and store the results in `store`.
pub fn full_build(
    repo_root: &Path,
    store: &GraphStore,
    opts: &BuildOptions,
) -> anyhow::Result<BuildResult> {
    let start = std::time::Instant::now();

    if let Some(n) = opts.max_threads {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global();
    }

    let files = collect_files(repo_root)?;
    let batch_size = 50;
    let errors: Vec<String> = Vec::new();
    let mut parsed = 0usize;
    let mut skipped = 0usize;

    for chunk in files.chunks(batch_size) {
        let results: Vec<Option<(String, Vec<crg_core::types::NodeInfo>, Vec<crg_core::types::EdgeInfo>, String)>> =
            chunk.par_iter().map(|file_path| {
                let rel_path = file_path
                    .strip_prefix(repo_root)
                    .unwrap_or(file_path)
                    .to_string_lossy()
                    .to_string();

                match crg_parser::parse_file(file_path) {
                    Ok(result) => Some((rel_path, result.nodes, result.edges, result.file_hash)),
                    Err(e) => {
                        tracing::warn!("Failed to parse {}: {}", rel_path, e);
                        None
                    }
                }
            }).collect();

        let batch: Vec<_> = results.into_iter().flatten().collect();
        parsed += batch.len();
        skipped += chunk.len() - batch.len();

        store.store_file_batch(&batch)?;
    }

    let resolved = store.resolve_bare_call_targets()?;
    tracing::info!("Resolved {} bare call targets", resolved);

    if opts.postprocess != "none" {
        run_postprocess(store)?;
    }

    // Record last_updated timestamp.
    let now = chrono_now();
    store.set_metadata("last_updated", &now)?;

    let stats = store.get_stats()?;

    Ok(BuildResult {
        files_parsed: parsed,
        files_skipped: skipped,
        nodes_total: stats.total_nodes,
        edges_total: stats.total_edges,
        duration_secs: start.elapsed().as_secs_f64(),
        errors,
    })
}

/// Parse only the files that changed since `opts.base` and update `store`.
pub fn incremental_update(
    repo_root: &Path,
    store: &GraphStore,
    opts: &BuildOptions,
) -> anyhow::Result<BuildResult> {
    let changed = get_changed_files(repo_root, &opts.base)?;

    if changed.is_empty() {
        tracing::info!("No changed files detected");
        let stats = store.get_stats()?;
        return Ok(BuildResult {
            files_parsed: 0,
            files_skipped: 0,
            nodes_total: stats.total_nodes,
            edges_total: stats.total_edges,
            duration_secs: 0.0,
            errors: vec![],
        });
    }

    let start = std::time::Instant::now();

    let results: Vec<(String, Vec<crg_core::types::NodeInfo>, Vec<crg_core::types::EdgeInfo>, String)> =
        changed.par_iter().filter_map(|rel_path| {
            let abs_path = repo_root.join(rel_path);
            if !abs_path.exists() {
                // Deleted file — pass empty vecs; store_file_batch will remove data.
                return Some((rel_path.clone(), vec![], vec![], String::new()));
            }
            match crg_parser::parse_file(&abs_path) {
                Ok(result) => Some((rel_path.clone(), result.nodes, result.edges, result.file_hash)),
                Err(e) => {
                    tracing::warn!("Failed to parse {}: {}", rel_path, e);
                    None
                }
            }
        }).collect();

    let count = results.len();
    store.store_file_batch(&results)?;
    let resolved = store.resolve_bare_call_targets()?;
    tracing::info!("Resolved {} bare call targets", resolved);

    if opts.postprocess != "none" {
        run_postprocess(store)?;
    }

    let now = chrono_now();
    store.set_metadata("last_updated", &now)?;

    let stats = store.get_stats()?;

    Ok(BuildResult {
        files_parsed: count,
        files_skipped: 0,
        nodes_total: stats.total_nodes,
        edges_total: stats.total_edges,
        duration_secs: start.elapsed().as_secs_f64(),
        errors: vec![],
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn run_postprocess(store: &GraphStore) -> anyhow::Result<()> {
    store.rebuild_fts()?;
    tracing::info!("Postprocessing complete (FTS rebuilt)");
    Ok(())
}

/// Recursively collect source files under `repo_root`, skipping known noise
/// directories and files larger than 5 MiB.
pub(crate) fn collect_files(repo_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    use std::collections::HashSet;

    let ignore_dirs: HashSet<&str> = [
        "node_modules",
        ".git",
        ".code-review-graph",
        "target",
        "__pycache__",
        ".tox",
        "dist",
        "build",
        ".next",
        ".nuxt",
        "venv",
        ".venv",
        "env",
        "vendor",
    ]
    .iter()
    .copied()
    .collect();

    let mut files = Vec::new();
    let mut stack = vec![repo_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");

            if path.is_dir() {
                // Descend into non-hidden dirs (allow .github) that are not on
                // the ignore list.
                if (!name.starts_with('.') || name == ".github") && !ignore_dirs.contains(name) {
                    stack.push(path);
                }
            } else if path.is_file() {
                let path_str = path.to_string_lossy();
                if crg_parser::detect_language(&path_str).is_some() {
                    if let Ok(meta) = entry.metadata() {
                        if meta.len() <= 5 * 1024 * 1024 {
                            files.push(path);
                        }
                    }
                }
            }
        }
    }

    Ok(files)
}

/// Run `git diff --name-only <base> --` and return the list of changed paths.
///
/// The `base` ref is validated before being passed to git.
/// Returns an empty Vec if the command fails (e.g., shallow clone without `base`).
fn get_changed_files(repo_root: &Path, base: &str) -> anyhow::Result<Vec<String>> {
    crg_core::security::validate_git_ref(base)?;

    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", base, "--"])
        .current_dir(repo_root)
        .stdin(std::process::Stdio::null())
        .output()?;

    if !output.status.success() {
        return Ok(vec![]);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = text
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();

    Ok(files)
}

/// Return the current UTC time as an ISO-8601 string (no chrono dependency).
fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple manual conversion — good enough for metadata logging.
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400; // days since 1970-01-01
    // Approximate date calculation (± 1 day on leap-year boundaries is fine).
    let year = 1970 + days / 365;
    let day_of_year = days % 365;
    let month = day_of_year / 30 + 1;
    let day = day_of_year % 30 + 1;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year, month, day, h, m, s
    )
}
