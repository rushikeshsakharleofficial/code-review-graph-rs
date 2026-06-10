/// SQLite-backed knowledge graph storage and query engine.
///
/// Mirrors `GraphStore` from `graph.py` in the Python codebase.
///
/// Thread safety: `GraphStore` wraps a `rusqlite::Connection` in a
/// `std::sync::Mutex`.  The `Connection` is opened with the default flags
/// (single-thread mode), and the `Mutex` serialises access so only one thread
/// executes SQL at a time.  This matches the Python model which protects the
/// connection with a `threading.Lock`.
///
/// # Deadlock avoidance
///
/// `std::sync::Mutex` is **not reentrant**.  To prevent self-deadlock all
/// public methods follow a strict discipline:
///   1. Acquire the mutex guard **once** at the start of the method.
///   2. Pass `&rusqlite::Connection` (obtained from the guard) to private
///      inner functions (`*_conn`) that contain the real SQL logic.
///   3. Transactional methods (BEGIN / COMMIT) open the transaction after
///      acquiring the guard, then pass the same connection reference to the
///      inner helpers — never re-acquiring the lock.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension};
use tracing::{debug, warn};

use crate::migrations::{get_schema_version, run_migrations};
use crate::schema::BASE_SCHEMA_SQL;
use crate::types::{EdgeInfo, GraphEdge, GraphNode, GraphStats, NodeInfo, node_info_qualified};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn json_to_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string())
        }
        serde_json::Value::Null => "{}".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn text_to_json(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
}

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<GraphNode> {
    let extra_text: Option<String> = row.get("extra")?;
    let extra = extra_text
        .as_deref()
        .map(text_to_json)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    let is_test_int: i64 = row.get("is_test")?;

    // line_start / line_end are nullable in the schema; coerce NULL → 0.
    let line_start: Option<i64> = row.get("line_start")?;
    let line_end: Option<i64> = row.get("line_end")?;
    let language: Option<String> = row.get("language")?;

    // community_id may not exist in very old schemas — use get() with fallback.
    let community_id: Option<i64> = row.get("community_id").unwrap_or(None);

    Ok(GraphNode {
        id: row.get("id")?,
        kind: row.get("kind")?,
        name: row.get("name")?,
        qualified_name: row.get("qualified_name")?,
        file_path: row.get("file_path")?,
        line_start: line_start.unwrap_or(0),
        line_end: line_end.unwrap_or(0),
        language: language.unwrap_or_default(),
        parent_name: row.get("parent_name")?,
        params: row.get("params")?,
        return_type: row.get("return_type")?,
        is_test: is_test_int != 0,
        file_hash: row.get("file_hash")?,
        extra,
        community_id,
    })
}

fn row_to_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<GraphEdge> {
    let extra_text: Option<String> = row.get("extra")?;
    let extra = extra_text
        .as_deref()
        .map(text_to_json)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    // confidence / confidence_tier added in v9; fall back gracefully.
    let confidence: f64 = row.get("confidence").unwrap_or(1.0);
    let confidence_tier: String = row
        .get::<_, String>("confidence_tier")
        .unwrap_or_else(|_| "EXTRACTED".to_string());

    Ok(GraphEdge {
        id: row.get("id")?,
        kind: row.get("kind")?,
        source_qualified: row.get("source_qualified")?,
        target_qualified: row.get("target_qualified")?,
        file_path: row.get("file_path")?,
        line: row.get("line")?,
        extra,
        confidence,
        confidence_tier,
    })
}

// ---------------------------------------------------------------------------
// Inner (lock-free) helpers — take &Connection, never lock the mutex
// ---------------------------------------------------------------------------

fn upsert_node_conn(conn: &Connection, node: &NodeInfo, file_hash: &str) -> anyhow::Result<i64> {
    let now = now_secs();
    let qualified = node_info_qualified(node);
    let extra = json_to_text(&node.extra);

    conn.execute(
        "INSERT INTO nodes
           (kind, name, qualified_name, file_path, line_start, line_end,
            language, parent_name, params, return_type, modifiers, is_test,
            file_hash, extra, updated_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
           ON CONFLICT(qualified_name) DO UPDATE SET
             kind=excluded.kind, name=excluded.name,
             file_path=excluded.file_path, line_start=excluded.line_start,
             line_end=excluded.line_end, language=excluded.language,
             parent_name=excluded.parent_name, params=excluded.params,
             return_type=excluded.return_type, modifiers=excluded.modifiers,
             is_test=excluded.is_test, file_hash=excluded.file_hash,
             extra=excluded.extra, updated_at=excluded.updated_at",
        rusqlite::params![
            node.kind,
            node.name,
            qualified,
            node.file_path,
            node.line_start,
            node.line_end,
            node.language,
            node.parent_name,
            node.params,
            node.return_type,
            node.modifiers,
            i64::from(node.is_test),
            file_hash,
            extra,
            now,
        ],
    )?;

    let id: i64 = conn.query_row(
        "SELECT id FROM nodes WHERE qualified_name = ?1",
        rusqlite::params![qualified],
        |row| row.get(0),
    )?;
    Ok(id)
}

fn upsert_edge_conn(conn: &Connection, edge: &EdgeInfo) -> anyhow::Result<i64> {
    let now = now_secs();

    // Extract confidence / confidence_tier from edge.extra if present.
    let extra_obj = edge.extra.as_object();
    let confidence: f64 = extra_obj
        .and_then(|m| m.get("confidence"))
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);
    let confidence_tier: String = extra_obj
        .and_then(|m| m.get("confidence_tier"))
        .and_then(|v| v.as_str())
        .unwrap_or("EXTRACTED")
        .to_string();
    let extra = json_to_text(&edge.extra);

    // Check for an existing edge matching (kind, source, target, file_path, line).
    let existing_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM edges
             WHERE kind=?1 AND source_qualified=?2 AND target_qualified=?3
                   AND file_path=?4 AND line=?5",
            rusqlite::params![edge.kind, edge.source, edge.target, edge.file_path, edge.line],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(id) = existing_id {
        conn.execute(
            "UPDATE edges SET line=?1, extra=?2, confidence=?3, confidence_tier=?4,
             updated_at=?5 WHERE id=?6",
            rusqlite::params![edge.line, extra, confidence, confidence_tier, now, id],
        )?;
        return Ok(id);
    }

    conn.execute(
        "INSERT INTO edges
           (kind, source_qualified, target_qualified, file_path, line, extra,
            confidence, confidence_tier, updated_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            edge.kind,
            edge.source,
            edge.target,
            edge.file_path,
            edge.line,
            extra,
            confidence,
            confidence_tier,
            now,
        ],
    )?;
    let id: i64 = conn.last_insert_rowid();
    Ok(id)
}

fn remove_file_data_conn(conn: &Connection, file_path: &str) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM nodes WHERE file_path = ?1",
        rusqlite::params![file_path],
    )?;
    conn.execute(
        "DELETE FROM edges WHERE file_path = ?1",
        rusqlite::params![file_path],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// FlowAdjacency — in-memory adjacency snapshot for flow analysis
// ---------------------------------------------------------------------------

/// Pre-loaded adjacency data for efficient flow traversal.
///
/// Constructed by [`GraphStore::load_flow_adjacency`] and consumed by
/// the `crg-flows` crate.
pub struct FlowAdjacency {
    /// Map of `source_qualified → [target_qualified, ...]` for CALLS edges.
    pub calls_out: HashMap<String, Vec<String>>,
    /// Set of `target_qualified` names that appear in at least one TESTED_BY edge.
    pub has_tested_by: std::collections::HashSet<String>,
    /// All graph nodes indexed by qualified name.
    pub nodes_by_qn: HashMap<String, GraphNode>,
    /// All graph nodes indexed by database row ID.
    pub nodes_by_id: HashMap<i64, GraphNode>,
}

// ---------------------------------------------------------------------------
// GraphStore
// ---------------------------------------------------------------------------

/// SQLite-backed code knowledge graph.
pub struct GraphStore {
    conn: Mutex<Connection>,
    pub db_path: PathBuf,
}

impl GraphStore {
    /// Open (or create) the graph database at `db_path`.
    ///
    /// On success the caller gets a fully initialised store with the base schema
    /// applied and all pending migrations run.
    pub fn new(db_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL")?;
        conn.execute_batch("PRAGMA busy_timeout=5000")?;

        // Apply base schema (idempotent — IF NOT EXISTS guards).
        conn.execute_batch(BASE_SCHEMA_SQL)?;

        // Ensure schema_version is set for fresh databases.
        if get_schema_version(&conn) < 1 {
            conn.execute(
                "INSERT OR IGNORE INTO metadata (key, value) VALUES ('schema_version', '1')",
                [],
            )?;
        }

        // Run pending migrations.
        run_migrations(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
        })
    }

    // -----------------------------------------------------------------------
    // Write operations
    // -----------------------------------------------------------------------

    /// Insert or update a node. Returns the node's database row ID.
    pub fn upsert_node(&self, node: &NodeInfo, file_hash: &str) -> anyhow::Result<i64> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        upsert_node_conn(&conn, node, file_hash)
    }

    /// Insert or update an edge. Returns the edge's database row ID.
    pub fn upsert_edge(&self, edge: &EdgeInfo) -> anyhow::Result<i64> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        upsert_edge_conn(&conn, edge)
    }

    /// Remove all nodes and edges associated with `file_path`.
    pub fn remove_file_data(&self, file_path: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        remove_file_data_conn(&conn, file_path)
    }

    /// Atomically replace all data for a single file inside one transaction.
    pub fn store_file_nodes_edges(
        &self,
        file_path: &str,
        nodes: &[NodeInfo],
        edges: &[EdgeInfo],
        fhash: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match (|| -> anyhow::Result<()> {
            remove_file_data_conn(&conn, file_path)?;
            for node in nodes {
                upsert_node_conn(&conn, node, fhash)?;
            }
            for edge in edges {
                upsert_edge_conn(&conn, edge)?;
            }
            Ok(())
        })() {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                warn!("store_file_nodes_edges: rolling back due to error: {}", e);
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Atomically replace data for a batch of files in one transaction.
    ///
    /// Each element of `batch` is `(file_path, nodes, edges, file_hash)`.
    pub fn store_file_batch(
        &self,
        batch: &[(String, Vec<NodeInfo>, Vec<EdgeInfo>, String)],
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match (|| -> anyhow::Result<()> {
            for (file_path, nodes, edges, fhash) in batch {
                remove_file_data_conn(&conn, file_path)?;
                for node in nodes {
                    upsert_node_conn(&conn, node, fhash)?;
                }
                for edge in edges {
                    upsert_edge_conn(&conn, edge)?;
                }
            }
            Ok(())
        })() {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                warn!("store_file_batch: rolling back due to error: {}", e);
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Persist a metadata key-value pair.
    pub fn set_metadata(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Retrieve a metadata value by key. Returns `None` if the key is absent.
    pub fn get_metadata(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        let result = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                rusqlite::params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Read operations
    // -----------------------------------------------------------------------

    /// Look up a node by its qualified name.
    pub fn get_node(&self, qualified_name: &str) -> anyhow::Result<Option<GraphNode>> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        let result = conn
            .query_row(
                "SELECT * FROM nodes WHERE qualified_name = ?1",
                rusqlite::params![qualified_name],
                row_to_node,
            )
            .optional()?;
        Ok(result)
    }

    /// Return all nodes belonging to `file_path`.
    pub fn get_nodes_by_file(&self, file_path: &str) -> anyhow::Result<Vec<GraphNode>> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        let mut stmt = conn.prepare("SELECT * FROM nodes WHERE file_path = ?1")?;
        let rows = stmt
            .query_map(rusqlite::params![file_path], row_to_node)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Return all nodes, optionally excluding File-kind nodes.
    pub fn get_all_nodes(&self, exclude_files: bool) -> anyhow::Result<Vec<GraphNode>> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        let sql = if exclude_files {
            "SELECT * FROM nodes WHERE kind != 'File'"
        } else {
            "SELECT * FROM nodes"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], row_to_node)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Return all edges whose `source_qualified` matches.
    pub fn get_edges_by_source(&self, qualified_name: &str) -> anyhow::Result<Vec<GraphEdge>> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        let mut stmt =
            conn.prepare("SELECT * FROM edges WHERE source_qualified = ?1")?;
        let rows = stmt
            .query_map(rusqlite::params![qualified_name], row_to_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Return all edges whose `target_qualified` matches.
    pub fn get_edges_by_target(&self, qualified_name: &str) -> anyhow::Result<Vec<GraphEdge>> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        let mut stmt =
            conn.prepare("SELECT * FROM edges WHERE target_qualified = ?1")?;
        let rows = stmt
            .query_map(rusqlite::params![qualified_name], row_to_edge)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Return aggregate statistics about the graph.
    pub fn get_stats(&self) -> anyhow::Result<GraphStats> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");

        let total_nodes: i64 =
            conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?;
        let total_edges: i64 =
            conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))?;

        let mut nodes_by_kind: HashMap<String, i64> = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT kind, COUNT(*) as cnt FROM nodes GROUP BY kind")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let kind: String = row.get(0)?;
                let cnt: i64 = row.get(1)?;
                nodes_by_kind.insert(kind, cnt);
            }
        }

        let mut edges_by_kind: HashMap<String, i64> = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT kind, COUNT(*) as cnt FROM edges GROUP BY kind")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let kind: String = row.get(0)?;
                let cnt: i64 = row.get(1)?;
                edges_by_kind.insert(kind, cnt);
            }
        }

        let mut languages: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT language FROM nodes WHERE language IS NOT NULL AND language != ''",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let lang: String = row.get(0)?;
                languages.push(lang);
            }
        }

        let files_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE kind = 'File'",
            [],
            |row| row.get(0),
        )?;

        // Retrieve last_updated from metadata (re-uses the locked connection directly).
        let last_updated: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'last_updated'",
                [],
                |row| row.get(0),
            )
            .optional()?;

        Ok(GraphStats {
            total_nodes,
            total_edges,
            nodes_by_kind,
            edges_by_kind,
            languages,
            files_count,
            last_updated,
        })
    }

    /// Rebuild the FTS5 index from scratch.
    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        conn.execute_batch("INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild')")?;
        debug!("FTS5 index rebuilt");
        Ok(())
    }

    /// Delete all nodes belonging to `file_path` (does **not** delete edges).
    ///
    /// Provided as a lower-level alternative to [`remove_file_data`] for
    /// callers that manage edge deletion separately.
    pub fn delete_nodes_for_file(&self, file_path: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        conn.execute(
            "DELETE FROM nodes WHERE file_path = ?1",
            rusqlite::params![file_path],
        )?;
        Ok(())
    }

    /// Return the distinct file paths present in the `nodes` table.
    pub fn get_all_file_paths(&self) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        let mut stmt =
            conn.prepare("SELECT DISTINCT file_path FROM nodes WHERE kind = 'File'")?;
        let rows = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// Return a map of `file_path → file_hash` for all File nodes that have a
    /// stored hash.  Used by the incremental update pipeline to skip unchanged
    /// files.
    pub fn get_all_file_hashes(&self) -> anyhow::Result<HashMap<String, String>> {
        let conn = self.conn.lock().expect("GraphStore mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT file_path, file_hash FROM nodes
             WHERE kind = 'File' AND file_hash IS NOT NULL AND file_hash != ''",
        )?;
        let mut map = HashMap::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let hash: String = row.get(1)?;
            map.insert(path, hash);
        }
        Ok(map)
    }

    // ---- Missing methods needed by crg-flows, crg-changes, crg-communities ----

    pub fn count_flow_memberships(&self, node_id: i64) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM flow_memberships WHERE node_id = ?",
            [node_id],
            |r| r.get(0),
        ).unwrap_or(0);
        Ok(count)
    }

    pub fn get_flow_criticalities_for_node(&self, node_id: i64) -> anyhow::Result<Vec<f64>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT f.criticality FROM flows f
             JOIN flow_memberships fm ON f.id = fm.flow_id
             WHERE fm.node_id = ?"
        )?;
        let result: Vec<f64> = stmt.query_map([node_id], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
        Ok(result)
    }

    pub fn get_node_community_id(&self, node_id: i64) -> anyhow::Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT community_id FROM nodes WHERE id = ?",
            [node_id],
            |r| r.get::<_, Option<i64>>(0),
        ).optional()?;
        Ok(result.flatten())
    }

    pub fn get_community_ids_by_qualified_names(&self, qns: &[String]) -> anyhow::Result<HashMap<String, Option<i64>>> {
        use crate::constants::BATCH_SIZE;
        let conn = self.conn.lock().unwrap();
        let mut result = HashMap::new();
        for chunk in qns.chunks(BATCH_SIZE) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("SELECT qualified_name, community_id FROM nodes WHERE qualified_name IN ({})", placeholders);
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
            })?;
            for row in rows.filter_map(|r| r.ok()) {
                result.insert(row.0, row.1);
            }
        }
        Ok(result)
    }

    pub fn get_files_matching(&self, pattern: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let like_pattern = format!("%{}", pattern);
        let mut stmt = conn.prepare(
            "SELECT DISTINCT file_path FROM nodes WHERE file_path LIKE ? LIMIT 100"
        )?;
        let paths: Vec<String> = stmt.query_map([&like_pattern], |r| r.get(0))?
            .filter_map(|r| r.ok()).collect();
        Ok(paths)
    }

    pub fn get_transitive_tests(&self, qualified_name: &str) -> anyhow::Result<Vec<String>> {
        // Direct TESTED_BY edges
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT source_qualified FROM edges WHERE target_qualified = ? AND kind = 'TESTED_BY'"
        )?;
        let direct: Vec<String> = stmt.query_map([qualified_name], |r| r.get(0))?
            .filter_map(|r| r.ok()).collect();
        // Also find indirect: tests that CALL this node
        let mut stmt2 = conn.prepare(
            "SELECT source_qualified FROM edges WHERE target_qualified = ? AND kind = 'CALLS'"
        )?;
        let indirect: Vec<String> = stmt2.query_map([qualified_name], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .filter(|s: &String| {
                let lower = s.to_lowercase();
                lower.contains("test") || lower.contains("spec")
            }).collect();
        let mut all = direct;
        for i in indirect {
            if !all.contains(&i) { all.push(i); }
        }
        Ok(all)
    }

    pub fn get_node_by_id(&self, node_id: i64) -> anyhow::Result<Option<GraphNode>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT * FROM nodes WHERE id = ?",
            [node_id],
            row_to_node,
        ).optional()?;
        Ok(result)
    }

    pub fn get_nodes_by_kind(&self, kind: &str) -> anyhow::Result<Vec<GraphNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM nodes WHERE kind = ?")?;
        let nodes: Vec<GraphNode> = stmt.query_map([kind], row_to_node)?
            .filter_map(|r| r.ok()).collect();
        Ok(nodes)
    }

    pub fn get_all_files(&self) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT DISTINCT file_path FROM nodes WHERE kind = 'File'")?;
        let paths: Vec<String> = stmt.query_map([], |r| r.get(0))?
            .filter_map(|r| r.ok()).collect();
        Ok(paths)
    }

    pub fn get_all_edges(&self) -> anyhow::Result<Vec<GraphEdge>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM edges")?;
        let edges: Vec<GraphEdge> = stmt.query_map([], row_to_edge)?
            .filter_map(|r| r.ok()).collect();
        Ok(edges)
    }

    pub fn get_edges_among(&self, qualified_names: &std::collections::HashSet<String>) -> anyhow::Result<Vec<GraphEdge>> {
        use crate::constants::BATCH_SIZE;
        let conn = self.conn.lock().unwrap();
        let qns: Vec<&String> = qualified_names.iter().collect();
        let mut result = Vec::new();
        for chunk in qns.chunks(BATCH_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT * FROM edges WHERE source_qualified IN ({0}) AND target_qualified IN ({0})",
                placeholders
            );
            // Need params twice (once for source, once for target)
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk.iter()
                .chain(chunk.iter())
                .map(|s| *s as &dyn rusqlite::ToSql)
                .collect();
            let edges: Vec<GraphEdge> = stmt.query_map(params.as_slice(), row_to_edge)?
                .filter_map(|r| r.ok()).collect();
            result.extend(edges);
        }
        Ok(result)
    }

    pub fn get_outgoing_targets(&self, qualified_name: &str) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT target_qualified, kind FROM edges WHERE source_qualified = ?"
        )?;
        let result: Vec<(String, String)> = stmt.query_map([qualified_name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?.filter_map(|r| r.ok()).collect();
        Ok(result)
    }

    pub fn get_incoming_sources(&self, qualified_name: &str) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT source_qualified, kind FROM edges WHERE target_qualified = ?"
        )?;
        let result: Vec<(String, String)> = stmt.query_map([qualified_name], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?.filter_map(|r| r.ok()).collect();
        Ok(result)
    }

    pub fn get_all_call_targets(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT DISTINCT target_qualified FROM edges WHERE kind = 'CALLS'")?;
        let targets: std::collections::HashSet<String> = stmt.query_map([], |r| r.get(0))?
            .filter_map(|r| r.ok()).collect();
        Ok(targets)
    }

    pub fn get_nodes_without_signature(&self) -> anyhow::Result<Vec<(i64, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, qualified_name FROM nodes WHERE (signature IS NULL OR signature = '') AND kind IN ('Function', 'Test')"
        )?;
        let result: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok()).collect();
        Ok(result)
    }

    pub fn update_node_signature(&self, node_id: i64, signature: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE nodes SET signature = ? WHERE id = ?", rusqlite::params![signature, node_id])?;
        Ok(())
    }

    pub fn get_all_community_ids(&self) -> anyhow::Result<HashMap<String, Option<i64>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT qualified_name, community_id FROM nodes")?;
        let result: HashMap<String, Option<i64>> = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
        })?.filter_map(|r| r.ok()).collect();
        Ok(result)
    }

    pub fn get_node_ids_by_files(&self, files: &[String]) -> anyhow::Result<HashMap<String, Vec<i64>>> {
        use crate::constants::BATCH_SIZE;
        let conn = self.conn.lock().unwrap();
        let mut result: HashMap<String, Vec<i64>> = HashMap::new();
        for chunk in files.chunks(BATCH_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("SELECT id, file_path FROM nodes WHERE file_path IN ({})", placeholders);
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let rows: Vec<(i64, String)> = stmt.query_map(params.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok()).collect();
            for (id, fp) in rows {
                result.entry(fp).or_default().push(id);
            }
        }
        Ok(result)
    }

    pub fn get_flow_ids_by_node_ids(&self, node_ids: &[i64]) -> anyhow::Result<HashMap<i64, Vec<i64>>> {
        use crate::constants::BATCH_SIZE;
        let conn = self.conn.lock().unwrap();
        let mut result: HashMap<i64, Vec<i64>> = HashMap::new();
        for chunk in node_ids.chunks(BATCH_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("SELECT node_id, flow_id FROM flow_memberships WHERE node_id IN ({})", placeholders);
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
            let rows: Vec<(i64, i64)> = stmt.query_map(params.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok()).collect();
            for (nid, fid) in rows {
                result.entry(nid).or_default().push(fid);
            }
        }
        Ok(result)
    }

    pub fn get_flow_qualified_names(&self, flow_id: i64) -> anyhow::Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT n.qualified_name FROM nodes n
             JOIN flow_memberships fm ON n.id = fm.node_id
             WHERE fm.flow_id = ?"
        )?;
        let result: std::collections::HashSet<String> = stmt.query_map([flow_id], |r| r.get(0))?
            .filter_map(|r| r.ok()).collect();
        Ok(result)
    }

    pub fn get_node_kind_by_id(&self, node_id: i64) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT kind FROM nodes WHERE id = ?",
            [node_id],
            |r| r.get(0),
        ).optional()?;
        Ok(result)
    }

    pub fn get_subgraph(&self, qualified_names: &[String]) -> anyhow::Result<serde_json::Value> {
        use crate::constants::BATCH_SIZE;
        let conn = self.conn.lock().unwrap();
        let mut nodes = Vec::new();
        for chunk in qualified_names.chunks(BATCH_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("SELECT * FROM nodes WHERE qualified_name IN ({})", placeholders);
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let rows: Vec<GraphNode> = stmt.query_map(params.as_slice(), row_to_node)?
                .filter_map(|r| r.ok()).collect();
            nodes.extend(rows);
        }
        // Get edges between these nodes
        let mut edges = Vec::new();
        for chunk in qualified_names.chunks(BATCH_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT * FROM edges WHERE source_qualified IN ({0}) AND target_qualified IN ({0})",
                placeholders
            );
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk.iter()
                .chain(chunk.iter())
                .map(|s| s as &dyn rusqlite::ToSql)
                .collect();
            let rows: Vec<GraphEdge> = stmt.query_map(params.as_slice(), row_to_edge)?
                .filter_map(|r| r.ok()).collect();
            edges.extend(rows);
        }
        Ok(serde_json::json!({
            "nodes": nodes.iter().map(|n| serde_json::to_value(n).unwrap_or_default()).collect::<Vec<_>>(),
            "edges": edges.iter().map(|e| serde_json::to_value(e).unwrap_or_default()).collect::<Vec<_>>(),
        }))
    }

    // Community methods
    pub fn upsert_community(&self, name: &str, level: i64, parent_id: Option<i64>, cohesion: f64, size: i64, dominant_language: Option<&str>, description: Option<&str>) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO communities (name, level, parent_id, cohesion, size, dominant_language, description) VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![name, level, parent_id, cohesion, size, dominant_language, description],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn set_node_community(&self, node_id: i64, community_id: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("UPDATE nodes SET community_id = ? WHERE id = ?", [community_id, node_id])?;
        Ok(())
    }

    pub fn clear_communities(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM communities", [])?;
        conn.execute("UPDATE nodes SET community_id = NULL", [])?;
        Ok(())
    }

    pub fn get_communities_list(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, level, parent_id, cohesion, size, dominant_language, description FROM communities ORDER BY size DESC"
        )?;
        let result: Vec<serde_json::Value> = stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "level": r.get::<_, i64>(2)?,
                "parent_id": r.get::<_, Option<i64>>(3)?,
                "cohesion": r.get::<_, f64>(4)?,
                "size": r.get::<_, i64>(5)?,
                "dominant_language": r.get::<_, Option<String>>(6)?,
                "description": r.get::<_, Option<String>>(7)?,
            }))
        })?.filter_map(|r| r.ok()).collect();
        Ok(result)
    }

    pub fn get_community_member_qns(&self, community_id: i64) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT qualified_name FROM nodes WHERE community_id = ?"
        )?;
        let result: Vec<String> = stmt.query_map([community_id], |r| r.get(0))?
            .filter_map(|r| r.ok()).collect();
        Ok(result)
    }

    pub fn get_nodes_by_community_id(&self, community_id: i64) -> anyhow::Result<Vec<GraphNode>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM nodes WHERE community_id = ?")?;
        let nodes: Vec<GraphNode> = stmt.query_map([community_id], row_to_node)?
            .filter_map(|r| r.ok()).collect();
        Ok(nodes)
    }

    pub fn search_edges_by_target_name(&self, name: &str, kind: &str) -> anyhow::Result<Vec<GraphEdge>> {
        let conn = self.conn.lock().unwrap();
        let like = format!("%{}", name);
        let mut stmt = conn.prepare(
            "SELECT * FROM edges WHERE kind = ? AND target_qualified LIKE ? LIMIT 1000"
        )?;
        let edges: Vec<GraphEdge> = stmt.query_map(rusqlite::params![kind, like], row_to_edge)?
            .filter_map(|r| r.ok()).collect();
        Ok(edges)
    }

    pub fn set_community_summary(&self, community_id: i64, name: &str, purpose: &str, key_symbols: &[String], risk: &str, size: i64, dominant_language: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let key_symbols_json = serde_json::to_string(key_symbols).unwrap_or_else(|_| "[]".to_string());
        conn.execute(
            "INSERT OR REPLACE INTO community_summaries (community_id, name, purpose, key_symbols, risk, size, dominant_language) VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![community_id, name, purpose, key_symbols_json, risk, size, dominant_language],
        )?;
        Ok(())
    }

    pub fn upsert_flow(&self, name: &str, entry_point_id: i64, depth: i64, node_count: i64, file_count: i64, criticality: f64, path_json: &str) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        // Check if flow with same entry_point_id exists
        let existing: Option<i64> = conn.query_row(
            "SELECT id FROM flows WHERE entry_point_id = ?",
            [entry_point_id],
            |r| r.get(0),
        ).optional()?.flatten();
        if let Some(fid) = existing {
            conn.execute(
                "UPDATE flows SET name=?, depth=?, node_count=?, file_count=?, criticality=?, path_json=?, updated_at=datetime('now') WHERE id=?",
                rusqlite::params![name, depth, node_count, file_count, criticality, path_json, fid],
            )?;
            Ok(fid)
        } else {
            conn.execute(
                "INSERT INTO flows (name, entry_point_id, depth, node_count, file_count, criticality, path_json) VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![name, entry_point_id, depth, node_count, file_count, criticality, path_json],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }

    pub fn upsert_flow_memberships(&self, flow_id: i64, node_ids: &[(i64, i64)]) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("BEGIN", [])?;
        let r: anyhow::Result<()> = (|| {
            for (node_id, position) in node_ids {
                conn.execute(
                    "INSERT OR REPLACE INTO flow_memberships (flow_id, node_id, position) VALUES (?, ?, ?)",
                    [flow_id, *node_id, *position],
                )?;
            }
            Ok(())
        })();
        if r.is_ok() { conn.execute("COMMIT", [])?; } else { conn.execute("ROLLBACK", [])?; r?; }
        Ok(())
    }

    pub fn get_flows_list(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, entry_point_id, depth, node_count, file_count, criticality FROM flows ORDER BY criticality DESC"
        )?;
        let result: Vec<serde_json::Value> = stmt.query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "entry_point_id": r.get::<_, i64>(2)?,
                "depth": r.get::<_, i64>(3)?,
                "node_count": r.get::<_, i64>(4)?,
                "file_count": r.get::<_, i64>(5)?,
                "criticality": r.get::<_, f64>(6)?,
            }))
        })?.filter_map(|r| r.ok()).collect();
        Ok(result)
    }

    pub fn get_flow_by_id(&self, flow_id: i64) -> anyhow::Result<Option<serde_json::Value>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT id, name, entry_point_id, depth, node_count, file_count, criticality, path_json FROM flows WHERE id = ?",
            [flow_id],
            |r| Ok(serde_json::json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "entry_point_id": r.get::<_, i64>(2)?,
                "depth": r.get::<_, i64>(3)?,
                "node_count": r.get::<_, i64>(4)?,
                "file_count": r.get::<_, i64>(5)?,
                "criticality": r.get::<_, f64>(6)?,
                "path_json": r.get::<_, String>(7)?,
            })),
        ).optional()?;
        Ok(result)
    }

    pub fn load_flow_adjacency(&self) -> anyhow::Result<FlowAdjacency> {
        let conn = self.conn.lock().unwrap();
        let mut calls_out: HashMap<String, Vec<String>> = HashMap::new();
        let mut has_tested_by: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Load all CALLS edges
        let mut stmt = conn.prepare("SELECT source_qualified, target_qualified FROM edges WHERE kind = 'CALLS'")?;
        let edges: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok()).collect();
        for (src, tgt) in edges {
            calls_out.entry(src).or_default().push(tgt);
        }
        // Load TESTED_BY targets
        let mut stmt2 = conn.prepare("SELECT DISTINCT target_qualified FROM edges WHERE kind = 'TESTED_BY'")?;
        let tested: Vec<String> = stmt2.query_map([], |r| r.get(0))?
            .filter_map(|r| r.ok()).collect();
        has_tested_by.extend(tested);

        // Load all nodes for adjacency
        let mut stmt3 = conn.prepare("SELECT * FROM nodes")?;
        let nodes_list: Vec<GraphNode> = stmt3.query_map([], row_to_node)?
            .filter_map(|r| r.ok()).collect();
        let mut nodes_by_qn = HashMap::new();
        let mut nodes_by_id = HashMap::new();
        for n in nodes_list {
            nodes_by_id.insert(n.id, n.clone());
            nodes_by_qn.insert(n.qualified_name.clone(), n);
        }

        Ok(FlowAdjacency { calls_out, has_tested_by, nodes_by_qn, nodes_by_id })
    }

    pub fn get_impact_radius_bfs(&self, qualified_name: &str, max_depth: usize, max_nodes: usize) -> anyhow::Result<Vec<(String, usize)>> {
        use std::collections::{VecDeque, HashSet};
        let conn = self.conn.lock().unwrap();
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        let mut result = Vec::new();
        queue.push_back((qualified_name.to_string(), 0));
        visited.insert(qualified_name.to_string());
        while let Some((current, depth)) = queue.pop_front() {
            if depth > max_depth || result.len() >= max_nodes { break; }
            if depth > 0 { result.push((current.clone(), depth)); }
            if depth < max_depth {
                let mut stmt = conn.prepare(
                    "SELECT source_qualified FROM edges WHERE target_qualified = ? AND kind IN ('CALLS', 'IMPORTS_FROM', 'DEPENDS_ON')"
                )?;
                let callers: Vec<String> = stmt.query_map([&current], |r| r.get(0))?
                    .filter_map(|r| r.ok()).collect();
                for caller in callers {
                    if !visited.contains(&caller) {
                        visited.insert(caller.clone());
                        queue.push_back((caller, depth + 1));
                    }
                }
            }
        }
        Ok(result)
    }

    pub fn resolve_bare_call_targets(&self) -> anyhow::Result<i64> {
        // Build a name -> qualified_name index for Functions/Methods
        let conn = self.conn.lock().unwrap();
        let mut name_to_qn: HashMap<String, Vec<String>> = HashMap::new();
        let mut stmt = conn.prepare("SELECT name, qualified_name FROM nodes WHERE kind IN ('Function', 'Test')")?;
        let rows: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok()).collect();
        for (name, qn) in rows {
            name_to_qn.entry(name).or_default().push(qn);
        }
        // Find unresolved edges: target_qualified doesn't exist as a node
        let mut unresolved_stmt = conn.prepare(
            "SELECT id, target_qualified FROM edges WHERE kind = 'CALLS' AND target_qualified NOT IN (SELECT qualified_name FROM nodes)"
        )?;
        let unresolved: Vec<(i64, String)> = unresolved_stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok()).collect();
        let mut resolved = 0i64;
        for (edge_id, bare_target) in &unresolved {
            if let Some(candidates) = name_to_qn.get(bare_target) {
                if candidates.len() == 1 {
                    // Unambiguous: update the edge
                    conn.execute("UPDATE edges SET target_qualified = ? WHERE id = ?",
                        rusqlite::params![candidates[0].as_str(), edge_id])?;
                    resolved += 1;
                }
            }
        }
        Ok(resolved)
    }
}

// `GraphStore` is `Send + Sync` automatically:
//   - `rusqlite::Connection` is `Send` (not `Sync`).
//   - `Mutex<Connection>` is `Send + Sync` whenever `T: Send` (std guarantee).
//   - `PathBuf` is `Send + Sync`.
// No manual impls needed; relying on auto-trait derivation lets the compiler
// catch any future addition of a non-Send/non-Sync field.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EdgeInfo, NodeInfo};
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Global counter — each call returns a unique integer so parallel tests
    /// never share the same DB file.
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_store() -> GraphStore {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("crg-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        GraphStore::new(dir.join("graph.db")).unwrap()
    }

    fn sample_node(name: &str, kind: &str, file: &str) -> NodeInfo {
        NodeInfo {
            kind: kind.to_string(),
            name: name.to_string(),
            file_path: file.to_string(),
            line_start: 1,
            line_end: 10,
            language: "python".to_string(),
            parent_name: None,
            params: None,
            return_type: None,
            modifiers: None,
            is_test: false,
            extra: json!({}),
        }
    }

    fn sample_edge(source: &str, target: &str, file: &str) -> EdgeInfo {
        EdgeInfo {
            kind: "CALLS".to_string(),
            source: source.to_string(),
            target: target.to_string(),
            file_path: file.to_string(),
            line: 5,
            extra: json!({}),
        }
    }

    #[test]
    fn open_and_schema() {
        let _store = make_store();
    }

    #[test]
    fn upsert_and_get_node() {
        let store = make_store();
        let node = sample_node("my_func", "Function", "src/lib.py");
        let id = store.upsert_node(&node, "abc123").unwrap();
        assert!(id > 0);

        let qn = "src/lib.py::my_func";
        let got = store.get_node(qn).unwrap().expect("node should exist");
        assert_eq!(got.name, "my_func");
        assert_eq!(got.kind, "Function");
    }

    #[test]
    fn upsert_is_idempotent() {
        let store = make_store();
        let node = sample_node("my_func", "Function", "src/lib.py");
        let id1 = store.upsert_node(&node, "abc123").unwrap();
        let id2 = store.upsert_node(&node, "abc123").unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn remove_file_data() {
        let store = make_store();
        let node = sample_node("fn_a", "Function", "src/a.py");
        store.upsert_node(&node, "").unwrap();
        assert!(store.get_node("src/a.py::fn_a").unwrap().is_some());
        store.remove_file_data("src/a.py").unwrap();
        assert!(store.get_node("src/a.py::fn_a").unwrap().is_none());
    }

    #[test]
    fn store_file_nodes_edges_atomicity() {
        let store = make_store();
        let nodes = vec![
            sample_node("fn_a", "Function", "src/b.py"),
            sample_node("fn_b", "Function", "src/b.py"),
        ];
        let edges = vec![sample_edge("src/b.py::fn_a", "src/b.py::fn_b", "src/b.py")];
        store
            .store_file_nodes_edges("src/b.py", &nodes, &edges, "hash1")
            .unwrap();

        let got = store.get_nodes_by_file("src/b.py").unwrap();
        assert_eq!(got.len(), 2);

        let src_edges = store.get_edges_by_source("src/b.py::fn_a").unwrap();
        assert_eq!(src_edges.len(), 1);
    }

    #[test]
    fn get_stats_returns_counts() {
        let store = make_store();
        store
            .upsert_node(&sample_node("fn_x", "Function", "x.py"), "")
            .unwrap();
        store
            .upsert_node(&sample_node("fn_y", "Function", "y.py"), "")
            .unwrap();
        let stats = store.get_stats().unwrap();
        assert!(stats.total_nodes >= 2);
    }

    #[test]
    fn metadata_roundtrip() {
        let store = make_store();
        store.set_metadata("last_updated", "2024-01-01").unwrap();
        let v = store.get_metadata("last_updated").unwrap();
        assert_eq!(v, Some("2024-01-01".to_string()));
    }

    #[test]
    fn get_all_file_hashes() {
        let store = make_store();
        let file_node = sample_node("src/x.py", "File", "src/x.py");
        store.upsert_node(&file_node, "sha256abc").unwrap();
        let hashes = store.get_all_file_hashes().unwrap();
        assert_eq!(hashes.get("src/x.py"), Some(&"sha256abc".to_string()));
    }
}
