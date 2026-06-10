/// Schema migration framework for the code-review-graph SQLite database.
///
/// Mirrors `migrations.py` from the Python codebase.  Each migration is
/// idempotent: it checks for the existence of columns/tables before altering
/// the schema.  Migrations run in individual transactions so a failure in
/// migration N does not corrupt the metadata for migrations 1..N-1.
use rusqlite::Connection;
use tracing::{info, error};

/// Latest known schema version.
pub const LATEST_VERSION: i64 = 9;

/// Tables that are allowed to be introspected by [`has_column`] and
/// [`table_exists`].  Mirrors `_KNOWN_TABLES` in `migrations.py`.
const KNOWN_TABLES: &[&str] = &[
    "nodes",
    "edges",
    "metadata",
    "communities",
    "flows",
    "flow_memberships",
    "nodes_fts",
    "community_summaries",
    "flow_snapshots",
    "risk_index",
];

fn is_known_table(table: &str) -> bool {
    KNOWN_TABLES.contains(&table)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read the current schema version from the `metadata` table.
///
/// Returns:
/// - `0` if the `metadata` table does not yet exist (brand-new DB).
/// - `1` if the table exists but the key is absent (schema was just created).
/// - The stored integer value otherwise.
pub fn get_schema_version(conn: &Connection) -> i64 {
    match conn.query_row(
        "SELECT value FROM metadata WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, String>(0),
    ) {
        Ok(v) => v.parse::<i64>().unwrap_or(1),
        Err(rusqlite::Error::QueryReturnedNoRows) => 1,
        Err(_) => {
            // OperationalError: metadata table doesn't exist yet.
            0
        }
    }
}

/// Persist the schema version in the `metadata` table.
pub fn set_schema_version(conn: &Connection, version: i64) -> anyhow::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', ?1)",
        rusqlite::params![version.to_string()],
    )?;
    Ok(())
}

/// Check whether `column` exists in `table`.
///
/// Uses `PRAGMA table_info` which is safe (no user-controlled input reaches
/// the SQL text because we validate against `KNOWN_TABLES` first).
pub fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    if !is_known_table(table) {
        return false;
    }
    // PRAGMA table_info cannot be parameterised; table is validated above.
    let sql = format!("PRAGMA table_info({})", table);
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return false,
    };
    while let Ok(Some(row)) = rows.next() {
        // Column 1 of PRAGMA table_info is the column name.
        if let Ok(name) = row.get::<_, String>(1) {
            if name == column {
                return true;
            }
        }
    }
    false
}

/// Check whether a table (or virtual table) exists in the database.
pub fn table_exists(conn: &Connection, table: &str) -> bool {
    if !is_known_table(table) {
        return false;
    }
    match conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
        rusqlite::params![table],
        |row| row.get::<_, i64>(0),
    ) {
        Ok(n) => n > 0,
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Individual migration functions
// ---------------------------------------------------------------------------

/// v1: No-op.  The base schema (applied by [`crate::store::GraphStore::new`]) is v1.
fn migrate_v1(_conn: &Connection) -> anyhow::Result<()> {
    Ok(())
}

/// v2: Add `signature TEXT` column to `nodes`.
fn migrate_v2(conn: &Connection) -> anyhow::Result<()> {
    if !has_column(conn, "nodes", "signature") {
        conn.execute_batch("ALTER TABLE nodes ADD COLUMN signature TEXT")?;
        info!("Migration v2: added 'signature' column to nodes");
    }
    Ok(())
}

/// v3: Create `flows` and `flow_memberships` tables with indexes.
fn migrate_v3(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS flows (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            entry_point_id INTEGER NOT NULL,
            depth INTEGER NOT NULL,
            node_count INTEGER NOT NULL,
            file_count INTEGER NOT NULL,
            criticality REAL NOT NULL DEFAULT 0.0,
            path_json TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS flow_memberships (
            flow_id INTEGER NOT NULL,
            node_id INTEGER NOT NULL,
            position INTEGER NOT NULL,
            PRIMARY KEY (flow_id, node_id)
        );
        CREATE INDEX IF NOT EXISTS idx_flows_criticality ON flows(criticality DESC);
        CREATE INDEX IF NOT EXISTS idx_flows_entry ON flows(entry_point_id);
        CREATE INDEX IF NOT EXISTS idx_flow_memberships_node ON flow_memberships(node_id);
    ")?;
    info!("Migration v3: created flows and flow_memberships tables");
    Ok(())
}

/// v4: Create `communities` table and add `community_id` column to `nodes`.
fn migrate_v4(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS communities (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            level INTEGER NOT NULL DEFAULT 0,
            parent_id INTEGER,
            cohesion REAL NOT NULL DEFAULT 0.0,
            size INTEGER NOT NULL DEFAULT 0,
            dominant_language TEXT,
            description TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
    ")?;
    if !has_column(conn, "nodes", "community_id") {
        conn.execute_batch("ALTER TABLE nodes ADD COLUMN community_id INTEGER")?;
        info!("Migration v4: added 'community_id' column to nodes");
    }
    conn.execute_batch("
        CREATE INDEX IF NOT EXISTS idx_nodes_community ON nodes(community_id);
        CREATE INDEX IF NOT EXISTS idx_communities_parent ON communities(parent_id);
        CREATE INDEX IF NOT EXISTS idx_communities_cohesion ON communities(cohesion DESC);
    ")?;
    info!("Migration v4: created communities table");
    Ok(())
}

/// v5: Create the FTS5 virtual table `nodes_fts`.
fn migrate_v5(conn: &Connection) -> anyhow::Result<()> {
    if !table_exists(conn, "nodes_fts") {
        conn.execute_batch("
            CREATE VIRTUAL TABLE nodes_fts USING fts5(
                name, qualified_name, file_path, signature,
                content='nodes', content_rowid='rowid',
                tokenize='porter unicode61'
            )
        ")?;
        info!("Migration v5: created nodes_fts FTS5 virtual table");
    }
    Ok(())
}

/// v6: Add pre-computed summary tables for token-efficient queries.
fn migrate_v6(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS community_summaries (
            community_id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            purpose TEXT DEFAULT '',
            key_symbols TEXT DEFAULT '[]',
            risk TEXT DEFAULT 'unknown',
            size INTEGER DEFAULT 0,
            dominant_language TEXT DEFAULT '',
            FOREIGN KEY (community_id) REFERENCES communities(id)
        );
        CREATE TABLE IF NOT EXISTS flow_snapshots (
            flow_id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            entry_point TEXT NOT NULL,
            critical_path TEXT DEFAULT '[]',
            criticality REAL DEFAULT 0.0,
            node_count INTEGER DEFAULT 0,
            file_count INTEGER DEFAULT 0,
            FOREIGN KEY (flow_id) REFERENCES flows(id)
        );
        CREATE TABLE IF NOT EXISTS risk_index (
            node_id INTEGER PRIMARY KEY,
            qualified_name TEXT NOT NULL,
            risk_score REAL DEFAULT 0.0,
            caller_count INTEGER DEFAULT 0,
            test_coverage TEXT DEFAULT 'unknown',
            security_relevant INTEGER DEFAULT 0,
            last_computed TEXT DEFAULT '',
            FOREIGN KEY (node_id) REFERENCES nodes(id)
        );
        CREATE INDEX IF NOT EXISTS idx_risk_index_score ON risk_index(risk_score DESC);
    ")?;
    info!("Migration v6: created summary tables (community_summaries, flow_snapshots, risk_index)");
    Ok(())
}

/// v7: Add compound edge indexes for summary and risk queries.
fn migrate_v7(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch("
        CREATE INDEX IF NOT EXISTS idx_edges_target_kind ON edges(target_qualified, kind);
        CREATE INDEX IF NOT EXISTS idx_edges_source_kind ON edges(source_qualified, kind);
    ")?;
    info!("Migration v7: added compound edge indexes");
    Ok(())
}

/// v8: Add composite index on edges for upsert performance.
fn migrate_v8(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch("
        CREATE INDEX IF NOT EXISTS idx_edges_composite
        ON edges(kind, source_qualified, target_qualified, file_path, line)
    ")?;
    info!("Migration v8: created composite edge index");
    Ok(())
}

/// v9: Add `confidence` and `confidence_tier` columns to edges.
///
/// On a fresh database these columns already exist (base schema), so this
/// migration is a no-op.  On older databases that were created before these
/// columns were added to the base schema, this adds them.
fn migrate_v9(conn: &Connection) -> anyhow::Result<()> {
    if !has_column(conn, "edges", "confidence") {
        conn.execute_batch("ALTER TABLE edges ADD COLUMN confidence REAL DEFAULT 1.0")?;
    }
    if !has_column(conn, "edges", "confidence_tier") {
        conn.execute_batch("ALTER TABLE edges ADD COLUMN confidence_tier TEXT DEFAULT 'EXTRACTED'")?;
    }
    info!("Migration v9: added edge confidence columns");
    Ok(())
}

// ---------------------------------------------------------------------------
// Migration registry
// ---------------------------------------------------------------------------

type MigrationFn = fn(&Connection) -> anyhow::Result<()>;

static MIGRATIONS: &[(i64, MigrationFn)] = &[
    (1, migrate_v1),
    (2, migrate_v2),
    (3, migrate_v3),
    (4, migrate_v4),
    (5, migrate_v5),
    (6, migrate_v6),
    (7, migrate_v7),
    (8, migrate_v8),
    (9, migrate_v9),
];

/// Run all pending migrations in ascending version order.
///
/// Each migration executes inside its own `BEGIN IMMEDIATE` / `COMMIT`
/// transaction.  On failure the transaction is rolled back and the error is
/// propagated; previously completed migrations are not undone.
pub fn run_migrations(conn: &Connection) -> anyhow::Result<()> {
    let current = get_schema_version(conn);
    if current >= LATEST_VERSION {
        return Ok(());
    }

    info!(
        "Schema version {} -> {}: running migrations",
        current, LATEST_VERSION
    );

    for &(version, migrate_fn) in MIGRATIONS {
        if version <= current {
            continue;
        }
        info!("Running migration v{}", version);

        // Each migration runs in its own transaction.
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match (|| -> anyhow::Result<()> {
            migrate_fn(conn)?;
            set_schema_version(conn, version)?;
            Ok(())
        })() {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
            }
            Err(e) => {
                error!("Migration v{} failed, rolling back: {}", version, e);
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }

    info!("Migrations complete, now at schema version {}", LATEST_VERSION);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use crate::schema::BASE_SCHEMA_SQL;

    fn open_mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;").unwrap();
        conn.execute_batch(BASE_SCHEMA_SQL).unwrap();
        conn
    }

    #[test]
    fn fresh_db_version_is_1() {
        let conn = open_mem_db();
        // metadata table exists but key is absent → returns 1
        assert_eq!(get_schema_version(&conn), 1);
    }

    #[test]
    fn set_and_get_version() {
        let conn = open_mem_db();
        set_schema_version(&conn, 5).unwrap();
        assert_eq!(get_schema_version(&conn), 5);
    }

    #[test]
    fn has_column_detects_existing() {
        let conn = open_mem_db();
        assert!(has_column(&conn, "nodes", "id"));
        assert!(has_column(&conn, "nodes", "kind"));
        assert!(!has_column(&conn, "nodes", "nonexistent_column"));
    }

    #[test]
    fn table_exists_works() {
        let conn = open_mem_db();
        assert!(table_exists(&conn, "nodes"));
        assert!(table_exists(&conn, "edges"));
        assert!(!table_exists(&conn, "flows")); // not yet created
    }

    #[test]
    fn run_all_migrations_succeeds() {
        let conn = open_mem_db();
        // Set version to 1 (as a fresh DB would have after base schema)
        set_schema_version(&conn, 1).unwrap();
        run_migrations(&conn).unwrap();
        assert_eq!(get_schema_version(&conn), LATEST_VERSION);
        // Post-migration: these tables must exist
        assert!(table_exists(&conn, "flows"));
        assert!(table_exists(&conn, "communities"));
        assert!(table_exists(&conn, "nodes_fts"));
    }

    #[test]
    fn migrations_are_idempotent() {
        let conn = open_mem_db();
        set_schema_version(&conn, 1).unwrap();
        run_migrations(&conn).unwrap();
        // Running again must not error.
        run_migrations(&conn).unwrap();
        assert_eq!(get_schema_version(&conn), LATEST_VERSION);
    }
}
