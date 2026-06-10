/// Environment variable name for the database path.
pub const CRG_DB_PATH_ENV: &str = "CRG_DB_PATH";

/// Default database path relative to the project root.
pub const CRG_DB_PATH_DEFAULT: &str = ".code-review-graph/graph.db";

/// Environment variable name for tool filtering.
pub const CRG_TOOLS_ENV: &str = "CRG_TOOLS";

/// BFS engine: kept for Python compatibility, unused in Rust.
/// Python default is "sql"; we keep this as a recognised string constant.
pub const BFS_ENGINE_DEFAULT: &str = "networkx";

/// Maximum BFS depth for impact-radius queries.
pub const MAX_IMPACT_DEPTH: i64 = 5;

/// Maximum number of nodes returned by an impact-radius query.
pub const MAX_IMPACT_NODES: i64 = 200;

/// SQLite IN-clause batch size. Must be safely below SQLite's 999 variable limit.
pub const BATCH_SIZE: usize = 450;

/// Language string used when no language can be determined.
pub const DEFAULT_LANGUAGE: &str = "unknown";

/// Directory name used for storing graph data.
pub const SCHEMA_DIR: &str = ".code-review-graph";
