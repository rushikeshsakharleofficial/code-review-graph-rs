use std::collections::HashMap;

/// Information about a parsed node, used as input to the graph store.
/// Mirrors the Python `NodeInfo` dataclass from `parser.py`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NodeInfo {
    /// Node kind: "File", "Class", "Function", "Type", "Test"
    pub kind: String,
    pub name: String,
    pub file_path: String,
    pub line_start: i64,
    pub line_end: i64,
    pub language: String,
    /// Enclosing class or module name, if any.
    pub parent_name: Option<String>,
    pub params: Option<String>,
    pub return_type: Option<String>,
    pub modifiers: Option<String>,
    pub is_test: bool,
    /// Arbitrary extra metadata. Serialised as JSON TEXT in the database.
    pub extra: serde_json::Value,
}

impl Default for NodeInfo {
    fn default() -> Self {
        Self {
            kind: String::new(),
            name: String::new(),
            file_path: String::new(),
            line_start: 0,
            line_end: 0,
            language: String::new(),
            parent_name: None,
            params: None,
            return_type: None,
            modifiers: None,
            is_test: false,
            extra: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// Information about a parsed edge, used as input to the graph store.
/// Mirrors the Python `EdgeInfo` dataclass from `parser.py`.
///
/// Valid kinds: "CALLS", "IMPORTS_FROM", "INHERITS", "IMPLEMENTS",
///              "CONTAINS", "TESTED_BY", "DEPENDS_ON", "REFERENCES"
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EdgeInfo {
    pub kind: String,
    /// Qualified name or path of the source node.
    pub source: String,
    /// Qualified name or path of the target node.
    pub target: String,
    pub file_path: String,
    pub line: i64,
    /// Arbitrary extra metadata. Serialised as JSON TEXT in the database.
    pub extra: serde_json::Value,
}

impl Default for EdgeInfo {
    fn default() -> Self {
        Self {
            kind: String::new(),
            source: String::new(),
            target: String::new(),
            file_path: String::new(),
            line: 0,
            extra: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

/// A node record as stored in the database.
/// Mirrors the Python `GraphNode` dataclass from `graph.py`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphNode {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub line_start: i64,
    pub line_end: i64,
    pub language: String,
    pub parent_name: Option<String>,
    pub params: Option<String>,
    pub return_type: Option<String>,
    pub is_test: bool,
    pub file_hash: Option<String>,
    pub extra: serde_json::Value,
    pub community_id: Option<i64>,
}

/// An edge record as stored in the database.
/// Mirrors the Python `GraphEdge` dataclass from `graph.py`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphEdge {
    pub id: i64,
    pub kind: String,
    pub source_qualified: String,
    pub target_qualified: String,
    pub file_path: String,
    pub line: i64,
    pub extra: serde_json::Value,
    pub confidence: f64,
    pub confidence_tier: String,
}

/// Aggregate statistics about the graph.
/// Mirrors the Python `GraphStats` dataclass from `graph.py`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphStats {
    pub total_nodes: i64,
    pub total_edges: i64,
    pub nodes_by_kind: HashMap<String, i64>,
    pub edges_by_kind: HashMap<String, i64>,
    pub languages: Vec<String>,
    pub files_count: i64,
    pub last_updated: Option<String>,
}

/// Build the canonical qualified name for a node.
///
/// - File nodes use `file_path` directly (no `::` suffix).
/// - Nodes with a parent: `file_path::parent_name.name`
/// - Nodes without a parent: `file_path::name`
///
/// Mirrors `GraphStore._make_qualified()` from `graph.py`.
pub fn make_qualified(
    kind: &str,
    name: &str,
    file_path: &str,
    parent_name: Option<&str>,
) -> String {
    if kind == "File" {
        return file_path.to_string();
    }
    match parent_name {
        Some(p) if !p.is_empty() => format!("{}::{}.{}", file_path, p, name),
        _ => format!("{}::{}", file_path, name),
    }
}

/// Convenience wrapper that takes a `&NodeInfo` reference.
pub fn node_info_qualified(node: &NodeInfo) -> String {
    make_qualified(
        &node.kind,
        &node.name,
        &node.file_path,
        node.parent_name.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_qualified_file() {
        assert_eq!(make_qualified("File", "mod.py", "src/mod.py", None), "src/mod.py");
    }

    #[test]
    fn make_qualified_function_no_parent() {
        assert_eq!(
            make_qualified("Function", "foo", "src/mod.py", None),
            "src/mod.py::foo"
        );
    }

    #[test]
    fn make_qualified_method_with_parent() {
        assert_eq!(
            make_qualified("Function", "bar", "src/mod.py", Some("MyClass")),
            "src/mod.py::MyClass.bar"
        );
    }

    #[test]
    fn make_qualified_empty_parent_treated_as_none() {
        assert_eq!(
            make_qualified("Function", "baz", "src/mod.py", Some("")),
            "src/mod.py::baz"
        );
    }
}
