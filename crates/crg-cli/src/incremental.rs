use std::path::{Path, PathBuf};

/// Walk upward from the current directory looking for a project root marker.
///
/// Recognises `.git`, `.svn`, and `.code-review-graph` directories.
/// Returns `None` if no marker is found before the filesystem root.
pub fn find_project_root() -> Option<PathBuf> {
    let mut current = std::env::current_dir().ok()?;
    loop {
        if current.join(".git").exists()
            || current.join(".code-review-graph").exists()
            || current.join(".svn").exists()
        {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Return the path to the SQLite database for `repo_root`.
///
/// Respects the `CRG_DB_PATH` environment variable if set; otherwise uses
/// `<repo_root>/.code-review-graph/graph.db`.
pub fn get_db_path(repo_root: &Path) -> PathBuf {
    if let Ok(path) = std::env::var("CRG_DB_PATH") {
        return PathBuf::from(path);
    }
    repo_root.join(".code-review-graph").join("graph.db")
}

/// Create the `.code-review-graph` directory under `repo_root` if it does not
/// already exist.
pub fn ensure_schema_dir(repo_root: &Path) -> anyhow::Result<()> {
    let dir = repo_root.join(".code-review-graph");
    std::fs::create_dir_all(&dir)?;
    Ok(())
}
