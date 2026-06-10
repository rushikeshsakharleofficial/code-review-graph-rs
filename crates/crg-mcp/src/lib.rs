//! crg-mcp — MCP server exposing 30 code-review-graph tools and 5 prompts.

use std::path::PathBuf;

use rmcp::{
    RoleServer, ServerHandler,
    handler::server::{
        router::{prompt::PromptRouter, tool::ToolRouter},
        wrapper::Parameters,
    },
    model::{
        CallToolResult, Content, GetPromptRequestParams, GetPromptResult, Implementation,
        ListPromptsResult, PaginatedRequestParams, PromptMessage, PromptMessageRole,
        ServerCapabilities, ServerInfo,
    },
    prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router,
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crg_core::{security::sanitize_name, store::GraphStore};

// ---------------------------------------------------------------------------
// Parameter structs — one per tool / prompt that takes arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetStatsParams {
    /// Optional extra detail level ("minimal" | "full").
    #[serde(default)]
    pub detail_level: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetNodeParams {
    /// Fully-qualified name of the node (e.g. `my_module::MyStruct::method`).
    pub qualified_name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetNodesByFileParams {
    /// Relative or absolute file path.
    pub file_path: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetAllNodesParams {
    /// If true, exclude file-level nodes.
    #[serde(default)]
    pub exclude_files: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetEdgesBySourceParams {
    /// Fully-qualified name of the source node.
    pub qualified_name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetEdgesByTargetParams {
    /// Fully-qualified name of the target node.
    pub qualified_name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SemanticSearchParams {
    /// Search query string.
    pub query: String,
    /// Maximum number of results to return (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct KeywordSearchParams {
    /// Keyword or phrase to search for.
    pub query: String,
    /// Maximum number of results to return (default 20).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetImpactRadiusParams {
    /// Fully-qualified name of the changed symbol.
    pub qualified_name: String,
    /// Maximum BFS depth (default 5).
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// Maximum number of nodes to return (default 100).
    #[serde(default)]
    pub max_nodes: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetSubgraphParams {
    /// List of fully-qualified names to include in the subgraph.
    pub qualified_names: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct QueryGraphParams {
    /// Pattern: callers_of | callees_of | imports_of | tests_for | dependents_of.
    pub pattern: String,
    /// Target fully-qualified name.
    pub target: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetFlowByIdParams {
    /// Numeric flow identifier.
    pub flow_id: i64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetCommunityMembersParams {
    /// Numeric community identifier.
    pub community_id: i64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetNodesByCommunityParams {
    /// Numeric community identifier.
    pub community_id: i64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetNodesByKindParams {
    /// Node kind filter (e.g. "function", "class", "module").
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetTransitiveTestsParams {
    /// Fully-qualified name of the symbol under test.
    pub qualified_name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetOutgoingTargetsParams {
    /// Fully-qualified name of the source node.
    pub qualified_name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetIncomingSourcesParams {
    /// Fully-qualified name of the target node.
    pub qualified_name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetFilesMatchingParams {
    /// Glob-style pattern to match file paths.
    pub pattern: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetMinimalContextParams {
    /// Short description of the task for which context is needed.
    pub task: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetReviewContextParams {
    /// List of qualified names whose source context is requested.
    pub qualified_names: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetAffectedFlowsParams {
    /// Qualified names of changed symbols.
    pub qualified_names: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DetectChangesParams {
    /// Base git ref (default "HEAD").
    #[serde(default)]
    pub base_ref: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RefactorToolParams {
    /// Operation: "rename_preview" | "dead_code" | "suggestions".
    pub operation: String,
    /// Source qualified name (for rename_preview).
    #[serde(default)]
    pub source: Option<String>,
    /// New name (for rename_preview).
    #[serde(default)]
    pub new_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GetArchitectureOverviewParams {
    /// Optional detail level.
    #[serde(default)]
    pub detail_level: Option<String>,
}

// ---- Prompt parameter structs ----

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReviewChangesParams {
    /// Base git ref for comparison (default "HEAD").
    #[serde(default)]
    pub base_ref: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DebugIssueParams {
    /// Description of the issue or error to debug.
    pub issue: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct OnboardDeveloperParams {
    /// Focus area for onboarding (e.g. "auth", "payments", or blank for full).
    #[serde(default)]
    pub focus_area: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct PreMergeCheckParams {
    /// Pull-request title or description for context.
    #[serde(default)]
    pub pr_description: Option<String>,
}

// ---------------------------------------------------------------------------
// CrgServer
// ---------------------------------------------------------------------------

/// MCP server backed by a code-review-graph SQLite store.
#[derive(Clone)]
pub struct CrgServer {
    repo_root: String,
    tool_router: ToolRouter<Self>,
    pmt_router: PromptRouter<Self>,
}

impl CrgServer {
    pub fn new(repo_root: impl Into<String>) -> Self {
        Self {
            repo_root: repo_root.into(),
            tool_router: Self::tool_router(),
            pmt_router: Self::prompt_router(),
        }
    }

    /// Open a `GraphStore` for this server's repo root.
    fn open_store(&self) -> anyhow::Result<GraphStore> {
        let db_path = PathBuf::from(&self.repo_root)
            .join(".code-review-graph")
            .join("graph.db");
        GraphStore::new(&db_path)
    }

    /// Resolve repo root from `CRG_REPO` env var or current directory.
    pub fn resolve_repo_root() -> String {
        std::env::var("CRG_REPO")
            .unwrap_or_else(|_| ".".to_string())
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router(router = tool_router)]
impl CrgServer {
    /// Get graph statistics: total nodes, edges, languages, and last-updated timestamp.
    #[tool(name = "get_stats", description = "Return graph statistics (node/edge counts, languages, last updated).")]
    pub async fn get_stats(&self, _p: Parameters<GetStatsParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_stats()) {
            Ok(stats) => {
                let v = serde_json::to_string(&stats).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_stats error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Look up a single node by its fully-qualified name.
    #[tool(name = "get_node", description = "Look up a single graph node by fully-qualified name.")]
    pub async fn get_node(&self, Parameters(p): Parameters<GetNodeParams>) -> CallToolResult {
        let qn = sanitize_name(&p.qualified_name);
        match self.open_store().and_then(|s| s.get_node(&qn)) {
            Ok(Some(node)) => {
                let v = serde_json::to_string(&node).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Ok(None) => CallToolResult::success(vec![Content::text("null")]),
            Err(e) => {
                warn!("get_node error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List all nodes in a given source file.
    #[tool(name = "get_nodes_by_file", description = "List all graph nodes defined in a given source file.")]
    pub async fn get_nodes_by_file(&self, Parameters(p): Parameters<GetNodesByFileParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_nodes_by_file(&p.file_path)) {
            Ok(nodes) => {
                let v = serde_json::to_string(&nodes).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_nodes_by_file error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List all nodes in the graph, optionally excluding file-level nodes.
    #[tool(name = "get_all_nodes", description = "List all nodes in the knowledge graph.")]
    pub async fn get_all_nodes(&self, Parameters(p): Parameters<GetAllNodesParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_all_nodes(p.exclude_files)) {
            Ok(nodes) => {
                let v = serde_json::to_string(&nodes).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_all_nodes error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get all edges originating from a node.
    #[tool(name = "get_edges_by_source", description = "Get all edges originating from a given node.")]
    pub async fn get_edges_by_source(&self, Parameters(p): Parameters<GetEdgesBySourceParams>) -> CallToolResult {
        let qn = sanitize_name(&p.qualified_name);
        match self.open_store().and_then(|s| s.get_edges_by_source(&qn)) {
            Ok(edges) => {
                let v = serde_json::to_string(&edges).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_edges_by_source error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get all edges pointing to a node.
    #[tool(name = "get_edges_by_target", description = "Get all edges pointing to a given node.")]
    pub async fn get_edges_by_target(&self, Parameters(p): Parameters<GetEdgesByTargetParams>) -> CallToolResult {
        let qn = sanitize_name(&p.qualified_name);
        match self.open_store().and_then(|s| s.get_edges_by_target(&qn)) {
            Ok(edges) => {
                let v = serde_json::to_string(&edges).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_edges_by_target error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Full-text semantic search over node names and metadata.
    #[tool(name = "semantic_search_nodes", description = "Full-text / semantic search over node names and code metadata.")]
    pub async fn semantic_search_nodes(&self, Parameters(p): Parameters<SemanticSearchParams>) -> CallToolResult {
        let limit = p.limit.unwrap_or(20);
        match self.open_store().and_then(|s| {
            let hits = s.fts_search(&p.query, limit)?;
            let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
            s.get_nodes_by_ids(&ids)
        }) {
            Ok(nodes) => {
                let v = serde_json::to_string(&nodes).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("semantic_search_nodes error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Keyword search over node names using SQLite FTS.
    #[tool(name = "keyword_search", description = "Keyword search over node names using SQLite FTS5.")]
    pub async fn keyword_search(&self, Parameters(p): Parameters<KeywordSearchParams>) -> CallToolResult {
        let limit = p.limit.unwrap_or(20);
        match self.open_store().and_then(|s| {
            let hits = s.keyword_search(&p.query, limit)?;
            let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
            s.get_nodes_by_ids(&ids)
        }) {
            Ok(nodes) => {
                let v = serde_json::to_string(&nodes).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("keyword_search error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Compute the BFS impact radius of a changed symbol.
    #[tool(name = "get_impact_radius", description = "Compute BFS impact radius (blast radius) of a changed symbol.")]
    pub async fn get_impact_radius(&self, Parameters(p): Parameters<GetImpactRadiusParams>) -> CallToolResult {
        let qn = sanitize_name(&p.qualified_name);
        let max_depth = p.max_depth.unwrap_or(5);
        let max_nodes = p.max_nodes.unwrap_or(100);
        match self.open_store().and_then(|s| s.get_impact_radius_bfs(&qn, max_depth, max_nodes)) {
            Ok(results) => {
                let v = serde_json::to_string(&results).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_impact_radius error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Extract a subgraph containing the given nodes and the edges between them.
    #[tool(name = "get_subgraph", description = "Extract a subgraph for a set of qualified names.")]
    pub async fn get_subgraph(&self, Parameters(p): Parameters<GetSubgraphParams>) -> CallToolResult {
        let qns: Vec<String> = p.qualified_names.iter().map(|s| sanitize_name(s)).collect();
        match self.open_store().and_then(|s| s.get_subgraph(&qns)) {
            Ok(v) => {
                let out = serde_json::to_string(&v).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(out)])
            }
            Err(e) => {
                warn!("get_subgraph error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Query the graph using a named relationship pattern.
    #[tool(name = "query_graph", description = "Query the graph with a named pattern: callers_of | callees_of | imports_of | tests_for | dependents_of.")]
    pub async fn query_graph(&self, Parameters(p): Parameters<QueryGraphParams>) -> CallToolResult {
        let target = sanitize_name(&p.target);
        let result: anyhow::Result<serde_json::Value> = (|| {
            let store = self.open_store()?;
            match p.pattern.as_str() {
                "callers_of" | "dependents_of" => {
                    let sources = store.get_incoming_sources(&target)?;
                    Ok(serde_json::to_value(sources)?)
                }
                "callees_of" | "imports_of" => {
                    let targets = store.get_outgoing_targets(&target)?;
                    Ok(serde_json::to_value(targets)?)
                }
                "tests_for" => {
                    let tests = store.get_transitive_tests(&target)?;
                    Ok(serde_json::to_value(tests)?)
                }
                other => {
                    anyhow::bail!("unknown pattern: {other}")
                }
            }
        })();
        match result {
            Ok(v) => {
                let out = serde_json::to_string(&v).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(out)])
            }
            Err(e) => {
                warn!("query_graph error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List all detected execution flows.
    #[tool(name = "list_flows", description = "List all detected execution flows in the codebase.")]
    pub async fn list_flows(&self, _p: Parameters<GetStatsParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_flows_list()) {
            Ok(flows) => {
                let v = serde_json::to_string(&flows).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("list_flows error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get a single execution flow by its numeric identifier.
    #[tool(name = "get_flow_by_id", description = "Get a single execution flow by its numeric ID.")]
    pub async fn get_flow_by_id(&self, Parameters(p): Parameters<GetFlowByIdParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_flow_by_id(p.flow_id)) {
            Ok(Some(v)) => {
                let out = serde_json::to_string(&v).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(out)])
            }
            Ok(None) => CallToolResult::success(vec![Content::text("null")]),
            Err(e) => {
                warn!("get_flow_by_id error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List all detected architectural communities.
    #[tool(name = "list_communities", description = "List all detected architectural communities (modules/clusters).")]
    pub async fn list_communities(&self, _p: Parameters<GetStatsParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_communities_list()) {
            Ok(communities) => {
                let v = serde_json::to_string(&communities).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("list_communities error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List the qualified names of all members of a community.
    #[tool(name = "get_community_members", description = "List qualified names of all nodes in a community.")]
    pub async fn get_community_members(&self, Parameters(p): Parameters<GetCommunityMembersParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_community_member_qns(p.community_id)) {
            Ok(members) => {
                let v = serde_json::to_string(&members).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_community_members error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List full node records for all members of a community.
    #[tool(name = "get_nodes_by_community", description = "List full node records for all members of a community.")]
    pub async fn get_nodes_by_community(&self, Parameters(p): Parameters<GetNodesByCommunityParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_nodes_by_community_id(p.community_id)) {
            Ok(nodes) => {
                let v = serde_json::to_string(&nodes).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_nodes_by_community error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List all nodes of a given kind (function, class, module, etc.).
    #[tool(name = "get_nodes_by_kind", description = "List all nodes of a given kind (function, class, module, etc.).")]
    pub async fn get_nodes_by_kind(&self, Parameters(p): Parameters<GetNodesByKindParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_nodes_by_kind(&p.kind)) {
            Ok(nodes) => {
                let v = serde_json::to_string(&nodes).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_nodes_by_kind error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List all source file paths tracked in the graph.
    #[tool(name = "list_files", description = "List all source file paths currently tracked in the graph.")]
    pub async fn list_files(&self, _p: Parameters<GetStatsParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_all_files()) {
            Ok(files) => {
                let v = serde_json::to_string(&files).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("list_files error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List files whose paths match a glob-style pattern.
    #[tool(name = "get_files_matching", description = "List files whose paths match a glob-style pattern.")]
    pub async fn get_files_matching(&self, Parameters(p): Parameters<GetFilesMatchingParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_files_matching(&p.pattern)) {
            Ok(files) => {
                let v = serde_json::to_string(&files).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_files_matching error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Find all test nodes that transitively cover a given symbol.
    #[tool(name = "get_transitive_tests", description = "Find all test nodes that transitively cover a given symbol.")]
    pub async fn get_transitive_tests(&self, Parameters(p): Parameters<GetTransitiveTestsParams>) -> CallToolResult {
        let qn = sanitize_name(&p.qualified_name);
        match self.open_store().and_then(|s| s.get_transitive_tests(&qn)) {
            Ok(tests) => {
                let v = serde_json::to_string(&tests).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_transitive_tests error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get the outgoing call/import targets of a node.
    #[tool(name = "get_outgoing_targets", description = "Get outgoing call/import targets for a node.")]
    pub async fn get_outgoing_targets(&self, Parameters(p): Parameters<GetOutgoingTargetsParams>) -> CallToolResult {
        let qn = sanitize_name(&p.qualified_name);
        match self.open_store().and_then(|s| s.get_outgoing_targets(&qn)) {
            Ok(targets) => {
                let v = serde_json::to_string(&targets).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_outgoing_targets error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get the incoming callers/importers of a node.
    #[tool(name = "get_incoming_sources", description = "Get incoming callers/importers of a node.")]
    pub async fn get_incoming_sources(&self, Parameters(p): Parameters<GetIncomingSourcesParams>) -> CallToolResult {
        let qn = sanitize_name(&p.qualified_name);
        match self.open_store().and_then(|s| s.get_incoming_sources(&qn)) {
            Ok(sources) => {
                let v = serde_json::to_string(&sources).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_incoming_sources error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get a minimal context summary for a given task description.
    #[tool(name = "get_minimal_context", description = "Get a minimal, token-efficient context summary for a given task.")]
    pub async fn get_minimal_context(&self, Parameters(p): Parameters<GetMinimalContextParams>) -> CallToolResult {
        // Stub: returns stats + top keyword search results for the task description.
        let result: anyhow::Result<serde_json::Value> = (|| {
            let store = self.open_store()?;
            let stats = store.get_stats()?;
            let hits = store.fts_search(&p.task, 10)?;
            let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
            let nodes = store.get_nodes_by_ids(&ids)?;
            Ok(serde_json::json!({
                "task": p.task,
                "stats": stats,
                "relevant_nodes": nodes,
                "next_tool_suggestions": ["get_impact_radius", "query_graph", "get_subgraph"]
            }))
        })();
        match result {
            Ok(v) => {
                let out = serde_json::to_string(&v).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(out)])
            }
            Err(e) => {
                warn!("get_minimal_context error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get source review context (node details) for a set of qualified names.
    #[tool(name = "get_review_context", description = "Get token-efficient source review context for a set of qualified names.")]
    pub async fn get_review_context(&self, Parameters(p): Parameters<GetReviewContextParams>) -> CallToolResult {
        let qns: Vec<String> = p.qualified_names.iter().map(|s| sanitize_name(s)).collect();
        let result: anyhow::Result<serde_json::Value> = (|| {
            let store = self.open_store()?;
            let mut nodes = Vec::new();
            for qn in &qns {
                if let Some(node) = store.get_node(qn)? {
                    nodes.push(node);
                }
            }
            Ok(serde_json::to_value(nodes)?)
        })();
        match result {
            Ok(v) => {
                let out = serde_json::to_string(&v).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(out)])
            }
            Err(e) => {
                warn!("get_review_context error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Find which execution flows are affected by changes to given symbols.
    #[tool(name = "get_affected_flows", description = "Find execution flows that are affected by changes to given symbols.")]
    pub async fn get_affected_flows(&self, Parameters(p): Parameters<GetAffectedFlowsParams>) -> CallToolResult {
        let qns: Vec<String> = p.qualified_names.iter().map(|s| sanitize_name(s)).collect();
        let result: anyhow::Result<serde_json::Value> = (|| {
            let store = self.open_store()?;
            let all_flows = store.get_flows_list()?;
            // Filter flows that contain any of the given qualified names.
            let affected: Vec<&serde_json::Value> = all_flows.iter().filter(|flow| {
                if let Some(path) = flow.get("path_json").and_then(|v| v.as_str()) {
                    qns.iter().any(|qn| path.contains(qn.as_str()))
                } else {
                    false
                }
            }).collect();
            Ok(serde_json::to_value(affected)?)
        })();
        match result {
            Ok(v) => {
                let out = serde_json::to_string(&v).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(out)])
            }
            Err(e) => {
                warn!("get_affected_flows error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Risk-scored analysis of recent code changes.
    #[tool(name = "detect_changes", description = "Risk-scored analysis of recent code changes (git-diff based).")]
    pub async fn detect_changes(&self, _p: Parameters<DetectChangesParams>) -> CallToolResult {
        // Stub: returns placeholder until crg-changes integration is wired.
        let out = serde_json::json!({
            "status": "stub",
            "message": "detect_changes: full git-diff integration pending crg-changes crate wiring.",
            "repo_root": self.repo_root
        });
        CallToolResult::success(vec![Content::text(out.to_string())])
    }

    /// Refactoring utilities: rename preview, dead code detection, suggestions.
    #[tool(name = "refactor_tool", description = "Refactoring utilities: rename_preview, dead_code detection, suggestions.")]
    pub async fn refactor_tool(&self, Parameters(p): Parameters<RefactorToolParams>) -> CallToolResult {
        // Stub: returns placeholder until crg-refactor integration is wired.
        let out = serde_json::json!({
            "status": "stub",
            "operation": p.operation,
            "source": p.source,
            "new_name": p.new_name,
            "message": "refactor_tool: full implementation pending crg-refactor crate wiring."
        });
        CallToolResult::success(vec![Content::text(out.to_string())])
    }

    /// Get a high-level architectural overview of the codebase.
    #[tool(name = "get_architecture_overview", description = "Get a high-level architectural overview of the codebase.")]
    pub async fn get_architecture_overview(&self, _p: Parameters<GetArchitectureOverviewParams>) -> CallToolResult {
        let result: anyhow::Result<serde_json::Value> = (|| {
            let store = self.open_store()?;
            let stats = store.get_stats()?;
            let communities = store.get_communities_list()?;
            Ok(serde_json::json!({
                "stats": stats,
                "communities": communities,
                "next_tool_suggestions": ["list_communities", "get_community_members", "list_flows"]
            }))
        })();
        match result {
            Ok(v) => {
                let out = serde_json::to_string(&v).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(out)])
            }
            Err(e) => {
                warn!("get_architecture_overview error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// List all edges in the graph.
    #[tool(name = "list_all_edges", description = "List all edges in the knowledge graph.")]
    pub async fn list_all_edges(&self, _p: Parameters<GetStatsParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_all_edges()) {
            Ok(edges) => {
                let v = serde_json::to_string(&edges).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("list_all_edges error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get all file paths tracked in the graph.
    #[tool(name = "get_all_file_paths", description = "Get all file paths currently tracked in the graph.")]
    pub async fn get_all_file_paths(&self, _p: Parameters<GetStatsParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_all_file_paths()) {
            Ok(paths) => {
                let v = serde_json::to_string(&paths).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_all_file_paths error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get a node by its internal numeric ID.
    #[tool(name = "get_node_by_id", description = "Get a node by its internal numeric database ID.")]
    pub async fn get_node_by_id(&self, Parameters(p): Parameters<GetFlowByIdParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_node_by_id(p.flow_id)) {
            Ok(Some(node)) => {
                let v = serde_json::to_string(&node).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Ok(None) => CallToolResult::success(vec![Content::text("null")]),
            Err(e) => {
                warn!("get_node_by_id error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }

    /// Get file/hash inventory for all tracked files.
    #[tool(name = "get_all_file_hashes", description = "Get file/hash inventory for all tracked source files.")]
    pub async fn get_all_file_hashes(&self, _p: Parameters<GetStatsParams>) -> CallToolResult {
        match self.open_store().and_then(|s| s.get_all_file_hashes()) {
            Ok(hashes) => {
                let v = serde_json::to_string(&hashes).unwrap_or_else(|e| format!("{e}"));
                CallToolResult::success(vec![Content::text(v)])
            }
            Err(e) => {
                warn!("get_all_file_hashes error: {e}");
                CallToolResult::error(vec![Content::text(format!("error: {e}"))])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Prompt implementations
// ---------------------------------------------------------------------------

#[prompt_router]
impl CrgServer {
    /// Prompt: review recent code changes in the repo.
    #[prompt(name = "review_changes", description = "Review recent code changes with risk scoring and impact analysis.")]
    pub async fn review_changes(&self, Parameters(p): Parameters<ReviewChangesParams>) -> Vec<PromptMessage> {
        let base = p.base_ref.as_deref().unwrap_or("HEAD");
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Please review the code changes in this repository relative to `{base}`. \
                    Use detect_changes to get risk-scored analysis, then get_impact_radius for \
                    any high-risk symbols, and get_review_context for source snippets. \
                    Summarise findings, highlight risks, and suggest improvements."
                ),
            ),
        ]
    }

    /// Prompt: generate an architecture map of the codebase.
    #[prompt(name = "architecture_map", description = "Generate an architecture map of the codebase using community structure.")]
    pub async fn architecture_map(&self) -> Vec<PromptMessage> {
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                "Please produce an architecture map for this codebase. \
                Use get_architecture_overview then list_communities to enumerate modules, \
                get_community_members for key symbols in each module, and list_flows \
                for critical execution paths. Produce a structured, readable summary.".to_string(),
            ),
        ]
    }

    /// Prompt: debug an issue or error in the codebase.
    #[prompt(name = "debug_issue", description = "Debug an issue or error using the knowledge graph.")]
    pub async fn debug_issue(&self, Parameters(p): Parameters<DebugIssueParams>) -> Vec<PromptMessage> {
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Help debug the following issue: {issue}\n\n\
                    Use semantic_search_nodes to locate relevant code, \
                    get_impact_radius to trace blast radius, \
                    query_graph with callers_of/callees_of to understand call chains, \
                    and get_review_context for source snippets. \
                    Identify root cause and suggest a fix.",
                    issue = p.issue
                ),
            ),
        ]
    }

    /// Prompt: onboard a new developer to the codebase.
    #[prompt(name = "onboard_developer", description = "Onboard a new developer to the codebase with an architectural tour.")]
    pub async fn onboard_developer(&self, Parameters(p): Parameters<OnboardDeveloperParams>) -> Vec<PromptMessage> {
        let focus = p.focus_area
            .as_deref()
            .map(|f| format!(" with a focus on the `{f}` area"))
            .unwrap_or_default();
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Please onboard a new developer to this codebase{focus}. \
                    Use get_architecture_overview and list_communities for a structural tour, \
                    list_flows for critical paths, and get_minimal_context for the key areas. \
                    Produce clear, beginner-friendly documentation."
                ),
            ),
        ]
    }

    /// Prompt: pre-merge check for a pull request.
    #[prompt(name = "pre_merge_check", description = "Run a pre-merge check: risk analysis, test coverage, and impact review.")]
    pub async fn pre_merge_check(&self, Parameters(p): Parameters<PreMergeCheckParams>) -> Vec<PromptMessage> {
        let pr_ctx = p.pr_description.as_deref().unwrap_or("(no PR description provided)");
        vec![
            PromptMessage::new_text(
                PromptMessageRole::User,
                format!(
                    "Run a pre-merge check for the following pull request:\n\n{pr_ctx}\n\n\
                    Steps: 1) detect_changes for risk-scored diff analysis. \
                    2) get_impact_radius for all changed symbols. \
                    3) get_transitive_tests to verify test coverage. \
                    4) get_affected_flows to check execution path impact. \
                    Produce a pass/fail summary with detailed findings."
                ),
            ),
        ]
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.pmt_router)]
impl ServerHandler for CrgServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_server_info(
            Implementation::new("code-review-graph", env!("CARGO_PKG_VERSION"))
                .with_title("Code Review Graph MCP Server")
                .with_description(
                    "Persistent, incrementally updated knowledge graph for token-efficient code review.",
                ),
        )
        .with_instructions(
            "Use get_minimal_context first (costs ~100 tokens). \
            Prefer query_graph over list_* for targeted queries. \
            Use detail_level=\"minimal\" when not needing full node data. \
            Target: ≤5 tool calls, ≤800 total tokens per task."
                .to_string(),
        )
    }
}
