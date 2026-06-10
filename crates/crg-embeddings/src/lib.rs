//! Vector embedding support for semantic code search.
//!
//! Supports four providers:
//! 1. `Local` — sentence-transformers via Python sidecar
//! 2. `OpenAiCompat` — any endpoint speaking OpenAI `/v1/embeddings`
//! 3. `Google` — Google Gemini `batchEmbedContents`
//! 4. `MiniMax` — MiniMax `embo-01` (1536-dim)
//!
//! Mirrors `embeddings.py` from the Python codebase.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crg_core::store::GraphStore;
use crg_core::types::GraphNode;

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Embedding provider configuration.
#[derive(Debug, Clone)]
pub enum Provider {
    /// Local sentence-transformers model via Python sidecar.
    Local { sidecar_path: String, model: String },
    /// Any OpenAI-compatible `/v1/embeddings` endpoint.
    OpenAiCompat { api_key: String, base_url: String, model: String },
    /// Google Gemini `batchEmbedContents`.
    Google { api_key: String, model: String },
    /// MiniMax `embo-01` cloud embeddings.
    MiniMax { api_key: String, model: String },
}

impl Provider {
    /// Detect provider from environment variables.
    ///
    /// Priority: `CRG_EMBEDDING_PROVIDER` → detect from available API keys.
    /// - `"local"` → `Local`
    /// - `"openai"` or `CRG_OPENAI_API_KEY` set → `OpenAiCompat`
    /// - `"google"` or `GOOGLE_API_KEY` set → `Google`
    /// - `"minimax"` or `MINIMAX_API_KEY` set → `MiniMax`
    /// - fallback → `None` (no provider configured)
    pub fn from_env() -> Option<Self> {
        let provider_env = std::env::var("CRG_EMBEDDING_PROVIDER").ok();
        let provider_str = provider_env.as_deref().unwrap_or("").to_lowercase();

        if provider_str == "local" || provider_str.is_empty() {
            // Try local first if explicitly requested, or if no other keys are set
            if provider_str == "local" {
                let sidecar_path = std::env::var("CRG_EMBEDDINGS_SIDECAR").unwrap_or_else(|_| {
                    dirs_for_home()
                        .map(|h| format!("{}/.code-review-graph/crg_embeddings_sidecar.py", h))
                        .unwrap_or_else(|| "~/.code-review-graph/crg_embeddings_sidecar.py".to_string())
                });
                let model = std::env::var("CRG_EMBEDDING_MODEL")
                    .unwrap_or_else(|_| "all-MiniLM-L6-v2".to_string());
                return Some(Provider::Local { sidecar_path, model });
            }
        }

        if provider_str == "openai" || (provider_str.is_empty() && std::env::var("CRG_OPENAI_API_KEY").is_ok()) {
            if let Ok(api_key) = std::env::var("CRG_OPENAI_API_KEY") {
                let base_url = std::env::var("CRG_OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
                let model = std::env::var("CRG_OPENAI_MODEL")
                    .unwrap_or_else(|_| "text-embedding-3-small".to_string());
                return Some(Provider::OpenAiCompat { api_key, base_url, model });
            }
        }

        if provider_str == "google" || (provider_str.is_empty() && std::env::var("GOOGLE_API_KEY").is_ok()) {
            if let Ok(api_key) = std::env::var("GOOGLE_API_KEY") {
                let model = std::env::var("CRG_EMBEDDING_MODEL")
                    .unwrap_or_else(|_| "gemini-embedding-001".to_string());
                return Some(Provider::Google { api_key, model });
            }
        }

        if provider_str == "minimax" || (provider_str.is_empty() && std::env::var("MINIMAX_API_KEY").is_ok()) {
            if let Ok(api_key) = std::env::var("MINIMAX_API_KEY") {
                let model = "embo-01".to_string();
                return Some(Provider::MiniMax { api_key, model });
            }
        }

        // Default to local if nothing else matched and no explicit provider
        if provider_str.is_empty() {
            let sidecar_path = std::env::var("CRG_EMBEDDINGS_SIDECAR").unwrap_or_else(|_| {
                dirs_for_home()
                    .map(|h| format!("{}/.code-review-graph/crg_embeddings_sidecar.py", h))
                    .unwrap_or_else(|| "~/.code-review-graph/crg_embeddings_sidecar.py".to_string())
            });
            let model = std::env::var("CRG_EMBEDDING_MODEL")
                .unwrap_or_else(|_| "all-MiniLM-L6-v2".to_string());
            return Some(Provider::Local { sidecar_path, model });
        }

        None
    }

    /// Human-readable provider name, used as the `provider` column in the DB.
    ///
    /// Mirrors `EmbeddingProvider.name` in Python.
    pub fn name(&self) -> String {
        match self {
            Provider::Local { model, .. } => format!("local:{}", model),
            Provider::OpenAiCompat { base_url, model, .. } => {
                let host = extract_host_key(base_url);
                format!("openai:{}@{}", model, host)
            }
            Provider::Google { model, .. } => format!("google:{}", model),
            Provider::MiniMax { model, .. } => format!("minimax:{}", model),
        }
    }
}

/// Helper: get the home directory as a string.
fn dirs_for_home() -> Option<String> {
    std::env::var("HOME").ok().or_else(|| std::env::var("USERPROFILE").ok())
}

/// Normalise a base URL into a host key, stripping credentials and default ports.
///
/// Mirrors `OpenAIEmbeddingProvider._make_host_key` in Python.
fn extract_host_key(base_url: &str) -> String {
    // Simple extraction without pulling in a URL-parsing crate beyond what we have.
    // Strip scheme
    let rest = if let Some(s) = base_url.strip_prefix("https://") {
        s
    } else if let Some(s) = base_url.strip_prefix("http://") {
        s
    } else {
        base_url
    };
    // Strip user info
    let rest = if let Some(pos) = rest.find('@') {
        &rest[pos + 1..]
    } else {
        rest
    };
    // Take host+path
    let rest = rest.trim_end_matches('/');
    // Strip trailing /embeddings suffix
    let rest = rest.strip_suffix("/embeddings").unwrap_or(rest).trim_end_matches('/');
    rest.to_lowercase()
}

// ---------------------------------------------------------------------------
// EmbeddingStore
// ---------------------------------------------------------------------------

/// Manages vector embeddings stored in the same SQLite database as the graph.
///
/// Uses a separate `rusqlite::Connection` (WAL mode allows concurrent
/// readers from the same process) rather than sharing the `GraphStore` mutex,
/// which avoids nested-lock deadlocks.
pub struct EmbeddingStore {
    db_path: PathBuf,
    provider: Provider,
    conn: Mutex<rusqlite::Connection>,
}

impl EmbeddingStore {
    /// Open (or create) the embeddings table in `db_path`.
    ///
    /// Creates the table if it does not yet exist:
    /// ```sql
    /// CREATE TABLE IF NOT EXISTS embeddings (
    ///     qualified_name TEXT PRIMARY KEY,
    ///     vector BLOB NOT NULL,
    ///     text_hash TEXT NOT NULL,
    ///     provider TEXT NOT NULL
    /// )
    /// ```
    pub fn new(db_path: &Path, provider: Provider) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open(db_path)
            .with_context(|| format!("EmbeddingStore: open {}", db_path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS embeddings (
                qualified_name TEXT PRIMARY KEY,
                vector BLOB NOT NULL,
                text_hash TEXT NOT NULL,
                provider TEXT NOT NULL DEFAULT 'unknown'
            )",
        )?;

        Ok(Self {
            db_path: db_path.to_path_buf(),
            provider,
            conn: Mutex::new(conn),
        })
    }

    /// Encode a batch of texts using the configured provider.
    pub async fn encode_texts(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        match &self.provider {
            Provider::Local { sidecar_path, model } => {
                encode_local(sidecar_path, model, texts).await
            }
            Provider::OpenAiCompat { api_key, base_url, model } => {
                encode_openai_compat(api_key, base_url, model, texts).await
            }
            Provider::Google { api_key, model } => {
                encode_google(api_key, model, texts).await
            }
            Provider::MiniMax { api_key, model } => {
                encode_minimax(api_key, model, texts).await
            }
        }
    }

    /// Store a vector for a qualified name.
    ///
    /// Serialises as little-endian f32 bytes.
    pub fn store_vector(&self, qualified_name: &str, vector: &[f32], text_hash: &str) -> anyhow::Result<()> {
        let bytes: Vec<u8> = vector.iter().flat_map(|f| f.to_le_bytes()).collect();
        let provider_name = self.provider.name();
        let conn = self.conn.lock().expect("EmbeddingStore mutex poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO embeddings (qualified_name, vector, text_hash, provider) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![qualified_name, bytes, text_hash, provider_name],
        )?;
        Ok(())
    }

    /// Retrieve a stored vector for a qualified name.
    ///
    /// Returns `None` if no embedding exists for this qualified name.
    pub fn get_vector(&self, qualified_name: &str) -> anyhow::Result<Option<Vec<f32>>> {
        let conn = self.conn.lock().expect("EmbeddingStore mutex poisoned");
        let result: Option<Vec<u8>> = conn
            .query_row(
                "SELECT vector FROM embeddings WHERE qualified_name = ?1",
                rusqlite::params![qualified_name],
                |row| row.get(0),
            )
            .optional()?;

        Ok(result.map(|bytes| {
            bytes
                .chunks(4)
                .map(|b| f32::from_le_bytes(b.try_into().expect("chunk of 4 bytes")))
                .collect()
        }))
    }

    /// Search for nodes by semantic similarity to `query`.
    ///
    /// Encodes the query, then computes cosine similarity against all stored
    /// vectors for the current provider. Returns the top `limit` results as
    /// `(qualified_name, similarity)` pairs.
    ///
    /// Note: this method encodes the query synchronously by blocking on the
    /// async `encode_texts`. Callers from an existing async context should use
    /// `encode_texts` directly and then call `search_by_vector`.
    pub fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<(String, f64)>> {
        // Determine async execution context
        let texts = vec![query.to_string()];
        let query_vecs = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                // Inside a runtime — use block_in_place (multi-thread scheduler)
                // or fall back to a fresh runtime on panic.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    tokio::task::block_in_place(|| handle.block_on(self.encode_texts(&texts)))
                }));
                match result {
                    Ok(r) => r?,
                    Err(_) => {
                        // Current-thread scheduler — create a fresh one
                        tokio::runtime::Runtime::new()?
                            .block_on(self.encode_texts(&texts))?
                    }
                }
            }
            Err(_) => {
                tokio::runtime::Runtime::new()?
                    .block_on(self.encode_texts(&texts))?
            }
        };

        let query_vec = query_vecs.into_iter().next().unwrap_or_default();
        if query_vec.is_empty() {
            return Ok(vec![]);
        }

        self.search_by_vector(&query_vec, limit)
    }

    /// Search using a pre-computed query vector.
    pub fn search_by_vector(&self, query_vec: &[f32], limit: usize) -> anyhow::Result<Vec<(String, f64)>> {
        let provider_name = self.provider.name();
        let conn = self.conn.lock().expect("EmbeddingStore mutex poisoned");

        let mut stmt = conn.prepare(
            "SELECT qualified_name, vector FROM embeddings WHERE provider = ?1",
        )?;

        let mut scored: Vec<(String, f64)> = Vec::new();
        let mut rows = stmt.query(rusqlite::params![provider_name])?;
        while let Some(row) = rows.next()? {
            let qn: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            let vec: Vec<f32> = blob
                .chunks(4)
                .map(|b| f32::from_le_bytes(b.try_into().expect("chunk of 4 bytes")))
                .collect();
            let sim = cosine_similarity(query_vec, &vec);
            scored.push((qn, sim));
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    /// Return the total count of stored embeddings.
    pub fn count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().expect("EmbeddingStore mutex poisoned");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))?;
        Ok(count)
    }

    /// Return true if a provider is configured and the sidecar/API is reachable.
    ///
    /// For HTTP providers we just return true (a failed call will produce an error
    /// at embed time). For local sidecar we check that the sidecar script file exists.
    pub fn available(&self) -> bool {
        match &self.provider {
            Provider::Local { sidecar_path, .. } => Path::new(sidecar_path).exists(),
            Provider::OpenAiCompat { api_key, .. } => !api_key.is_empty(),
            Provider::Google { api_key, .. } => !api_key.is_empty(),
            Provider::MiniMax { api_key, .. } => !api_key.is_empty(),
        }
    }

    /// The db path for this store.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

// ---------------------------------------------------------------------------
// Cosine similarity
// ---------------------------------------------------------------------------

/// Compute cosine similarity between two f32 vectors.
///
/// Returns 0.0 if either vector is all-zero.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        (dot / (norm_a * norm_b)) as f64
    }
}

// ---------------------------------------------------------------------------
// Provider-specific encode functions
// ---------------------------------------------------------------------------

/// Encode texts via a local sentence-transformers Python sidecar.
async fn encode_local(
    sidecar_path: &str,
    model: &str,
    texts: &[String],
) -> anyhow::Result<Vec<Vec<f32>>> {
    use crg_sidecar_bridge::SidecarPool;

    let pool = SidecarPool::new(sidecar_path.to_string());
    let params = serde_json::json!({
        "texts": texts,
        "model": model,
    });

    let result = pool.call("encode", params).await.context("local sidecar encode call failed")?;

    let vectors = result["vectors"]
        .as_array()
        .context("local sidecar: expected 'vectors' array")?;

    let mut out = Vec::with_capacity(vectors.len());
    for v in vectors {
        let arr = v.as_array().context("local sidecar: expected vector array")?;
        let floats: Vec<f32> = arr
            .iter()
            .map(|x| x.as_f64().unwrap_or(0.0) as f32)
            .collect();
        out.push(floats);
    }
    Ok(out)
}

/// Encode texts via an OpenAI-compatible `/v1/embeddings` endpoint.
async fn encode_openai_compat(
    api_key: &str,
    base_url: &str,
    model: &str,
    texts: &[String],
) -> anyhow::Result<Vec<Vec<f32>>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build reqwest client")?;

    let base = base_url.trim_end_matches('/');
    let url = format!("{}/embeddings", base);

    // Process in batches of 100, as Python does
    let mut all_vecs: Vec<Vec<f32>> = Vec::with_capacity(texts.len());

    for batch in texts.chunks(100) {
        let body = serde_json::json!({
            "model": model,
            "input": batch,
        });

        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .context("OpenAI-compat: send request")?;

        let status = resp.status();
        let text = resp.text().await.context("OpenAI-compat: read response body")?;

        if !status.is_success() {
            anyhow::bail!("OpenAI-compat API HTTP {}: {}", status, text);
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&text).context("OpenAI-compat: parse JSON response")?;

        if let Some(err) = parsed.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            anyhow::bail!("OpenAI-compat API error: {}", msg);
        }

        let data = parsed["data"]
            .as_array()
            .context("OpenAI-compat: expected 'data' array")?;

        // Respect the 'index' field if present (sort by it to handle re-ordered responses)
        let any_has_index = data.iter().any(|item| item.get("index").is_some());
        let all_int_index = data.iter().all(|item| item.get("index").and_then(|v| v.as_i64()).is_some());

        let sorted_data: Vec<&serde_json::Value> = if all_int_index {
            let mut d: Vec<&serde_json::Value> = data.iter().collect();
            d.sort_by_key(|item| item["index"].as_i64().unwrap_or(0));
            d
        } else if !any_has_index {
            data.iter().collect()
        } else {
            anyhow::bail!("OpenAI-compat API returned mixed indexed/unindexed data");
        };

        for item in sorted_data {
            let embedding = item["embedding"]
                .as_array()
                .context("OpenAI-compat: expected 'embedding' array")?;
            let floats: Vec<f32> = embedding
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect();
            all_vecs.push(floats);
        }
    }

    Ok(all_vecs)
}

/// Encode texts via Google Gemini `batchEmbedContents`.
async fn encode_google(
    api_key: &str,
    model: &str,
    texts: &[String],
) -> anyhow::Result<Vec<Vec<f32>>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build reqwest client")?;

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:batchEmbedContents?key={}",
        model, api_key
    );

    let mut all_vecs: Vec<Vec<f32>> = Vec::with_capacity(texts.len());

    // Process in batches of 100
    for batch in texts.chunks(100) {
        let requests: Vec<serde_json::Value> = batch
            .iter()
            .map(|t| {
                serde_json::json!({
                    "model": format!("models/{}", model),
                    "content": {
                        "parts": [{ "text": t }]
                    }
                })
            })
            .collect();

        let body = serde_json::json!({ "requests": requests });

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Google Gemini: send request")?;

        let status = resp.status();
        let text = resp.text().await.context("Google Gemini: read response body")?;

        if !status.is_success() {
            anyhow::bail!("Google Gemini API HTTP {}: {}", status, text);
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&text).context("Google Gemini: parse JSON response")?;

        let embeddings = parsed["embeddings"]
            .as_array()
            .context("Google Gemini: expected 'embeddings' array")?;

        for emb in embeddings {
            let values = emb["values"]
                .as_array()
                .context("Google Gemini: expected 'values' array")?;
            let floats: Vec<f32> = values
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect();
            all_vecs.push(floats);
        }
    }

    Ok(all_vecs)
}

/// Encode texts via the MiniMax embeddings API.
async fn encode_minimax(
    api_key: &str,
    model: &str,
    texts: &[String],
) -> anyhow::Result<Vec<Vec<f32>>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build reqwest client")?;

    let url = "https://api.minimax.chat/v1/embeddings";
    let mut all_vecs: Vec<Vec<f32>> = Vec::with_capacity(texts.len());

    for batch in texts.chunks(100) {
        let body = serde_json::json!({
            "model": model,
            "texts": batch,
            "type": "query",
        });

        let resp = client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("MiniMax: send request")?;

        let status = resp.status();
        let text = resp.text().await.context("MiniMax: read response body")?;

        if !status.is_success() {
            anyhow::bail!("MiniMax API HTTP {}: {}", status, text);
        }

        let parsed: serde_json::Value =
            serde_json::from_str(&text).context("MiniMax: parse JSON response")?;

        // Check for API-level error in base_resp
        if let Some(base_resp) = parsed.get("base_resp") {
            let status_code = base_resp.get("status_code").and_then(|v| v.as_i64()).unwrap_or(0);
            if status_code != 0 {
                let msg = base_resp
                    .get("status_msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                anyhow::bail!("MiniMax API error: {}", msg);
            }
        }

        let vectors = parsed["vectors"]
            .as_array()
            .context("MiniMax: expected 'vectors' array")?;

        for v in vectors {
            let arr = v.as_array().context("MiniMax: expected vector array")?;
            let floats: Vec<f32> = arr
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect();
            all_vecs.push(floats);
        }
    }

    Ok(all_vecs)
}

// ---------------------------------------------------------------------------
// Node text conversion
// ---------------------------------------------------------------------------

/// Convert a graph node to a searchable text representation.
///
/// Mirrors `_node_to_text` in Python's `embeddings.py`. Produces a rich
/// representation for better semantic search quality.
fn node_to_text(node: &GraphNode) -> String {
    let mut parts: Vec<String> = Vec::new();

    // 1. Dotted form (strongest lexical signal for "method in class")
    if let Some(ref parent) = node.parent_name {
        if node.kind != "File" {
            parts.push(format!("{}.{}", parent, node.name));
        }
    }

    // 2. Bare name
    parts.push(node.name.clone());

    // 3. Split-words form of the name
    let name_split = split_identifier(&node.name);
    if !name_split.is_empty() && name_split.to_lowercase() != node.name.to_lowercase() {
        parts.push(name_split.clone());
    }

    // 4. Kind
    if node.kind != "File" {
        parts.push(node.kind.to_lowercase());
    }

    // 5. Parent context
    if let Some(ref parent) = node.parent_name {
        parts.push(format!("in {}", parent));
        let parent_split = split_identifier(parent);
        if !parent_split.is_empty() && parent_split.to_lowercase() != parent.to_lowercase() {
            parts.push(parent_split);
        }
    }

    // 6. Signature bits
    if let Some(ref params) = node.params {
        if !params.is_empty() {
            parts.push(params.clone());
        }
    }
    if let Some(ref ret) = node.return_type {
        if !ret.is_empty() {
            parts.push(format!("returns {}", ret));
        }
    }

    // 7. Module/directory context
    let fp = std::path::Path::new(&node.file_path);
    if let Some(parent_dir) = fp.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
        if !parent_dir.is_empty() && !matches!(parent_dir, "." | "src" | "lib") {
            parts.push(parent_dir.to_string());
        }
    }

    // 8. Language
    if !node.language.is_empty() {
        parts.push(node.language.clone());
    }

    parts.join(" ")
}

/// Split a camelCase/snake_case/PascalCase identifier into space-separated words.
///
/// Mirrors `_split_identifier` in Python.
fn split_identifier(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    // Insert space between lowercase→uppercase transitions
    let mut spaced = String::with_capacity(name.len() * 2);
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() && chars[i - 1].is_lowercase() {
            spaced.push(' ');
        }
        spaced.push(c);
    }
    // Replace underscores, dots, slashes, hyphens with spaces
    let spaced = spaced.replace(|c: char| matches!(c, '_' | '.' | '/' | '-'), " ");
    spaced.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// Compute SHA-256 hash of a string, returning the first 16 hex chars.
fn text_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)[..16].to_string()
}

// ---------------------------------------------------------------------------
// Public top-level: embed_graph_nodes
// ---------------------------------------------------------------------------

/// Embed all non-File graph nodes and store their vectors.
///
/// For each non-File node:
/// - Build a rich text representation via `node_to_text`.
/// - Hash the text; skip if an embedding with the same hash already exists.
/// - Encode in batches of `batch_size`.
/// - Store vectors in the embeddings table.
///
/// Returns the count of nodes actually embedded (skipped = already up-to-date).
pub async fn embed_graph_nodes(
    store: &GraphStore,
    provider: Provider,
    batch_size: usize,
) -> anyhow::Result<i64> {
    let emb_store = EmbeddingStore::new(&store.db_path, provider)?;
    let provider_name = emb_store.provider.name();

    let nodes = store.get_all_nodes(true)?; // exclude File nodes
    info!("embed_graph_nodes: {} candidate nodes", nodes.len());

    // Determine which nodes need (re-)embedding
    let mut to_embed: Vec<(&GraphNode, String, String)> = Vec::new();

    for node in &nodes {
        let text = node_to_text(node);
        let hash = text_hash(&text);

        // Check if embedding already exists with same hash and provider
        let existing = {
            let conn = emb_store.conn.lock().expect("EmbeddingStore mutex poisoned");
            conn.query_row(
                "SELECT text_hash, provider FROM embeddings WHERE qualified_name = ?1",
                rusqlite::params![node.qualified_name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .unwrap_or(None)
        };

        match existing {
            Some((existing_hash, existing_provider))
                if existing_hash == hash && existing_provider == provider_name =>
            {
                // Up to date, skip
            }
            _ => {
                to_embed.push((node, text, hash));
            }
        }
    }

    if to_embed.is_empty() {
        info!("embed_graph_nodes: all {} nodes up-to-date", nodes.len());
        return Ok(0);
    }

    info!("embed_graph_nodes: embedding {} nodes in batches of {}", to_embed.len(), batch_size);

    let mut embedded = 0i64;

    for chunk in to_embed.chunks(batch_size) {
        let texts: Vec<String> = chunk.iter().map(|(_, t, _)| t.clone()).collect();

        let vectors = emb_store.encode_texts(&texts).await.with_context(|| {
            format!("embed_graph_nodes: encode batch of {} texts", texts.len())
        })?;

        if vectors.len() != chunk.len() {
            warn!(
                "embed_graph_nodes: provider returned {} vectors for {} texts",
                vectors.len(),
                chunk.len()
            );
        }

        for ((node, _text, hash), vec) in chunk.iter().zip(vectors.iter()) {
            emb_store.store_vector(&node.qualified_name, vec, hash)?;
            embedded += 1;
        }
    }

    info!("embed_graph_nodes: stored {} new embeddings", embedded);
    Ok(embedded)
}

// ---------------------------------------------------------------------------
// rusqlite OptionalExtension re-export
// ---------------------------------------------------------------------------

use rusqlite::OptionalExtension;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_same_vector() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_zero_vector() {
        let a = vec![0.0f32, 0.0];
        let b = vec![1.0f32, 0.0];
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_mismatched_lengths() {
        let a = vec![1.0f32, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn text_hash_is_16_chars() {
        let h = text_hash("hello world");
        assert_eq!(h.len(), 16);
    }

    #[test]
    fn text_hash_is_deterministic() {
        assert_eq!(text_hash("foo"), text_hash("foo"));
        assert_ne!(text_hash("foo"), text_hash("bar"));
    }

    #[test]
    fn split_identifier_camel() {
        let r = split_identifier("processData");
        assert!(r.contains(' '));
        assert!(r.to_lowercase().contains("process"));
    }

    #[test]
    fn split_identifier_snake() {
        let r = split_identifier("get_route_handler");
        assert!(r.contains(' '));
    }

    #[test]
    fn split_identifier_empty() {
        assert_eq!(split_identifier(""), "");
    }

    fn make_test_db_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("crg-emb-test-{}-{}-{}.db", std::process::id(), n, name))
    }

    #[test]
    fn store_and_retrieve_vector() {
        let db_path = make_test_db_path("store_retrieve");
        let provider = Provider::Local {
            sidecar_path: "/nonexistent/sidecar.py".to_string(),
            model: "test-model".to_string(),
        };
        let store = EmbeddingStore::new(&db_path, provider).unwrap();

        let vec = vec![1.0f32, 0.5, 0.0];
        store.store_vector("src/foo.py::bar", &vec, "abc123").unwrap();

        let retrieved = store.get_vector("src/foo.py::bar").unwrap().unwrap();
        assert_eq!(retrieved.len(), 3);
        assert!((retrieved[0] - 1.0).abs() < 1e-6);
        assert!((retrieved[1] - 0.5).abs() < 1e-6);
        assert!((retrieved[2] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn count_embeddings() {
        let db_path = make_test_db_path("count");
        let provider = Provider::Local {
            sidecar_path: "/nonexistent/sidecar.py".to_string(),
            model: "test-model".to_string(),
        };
        let store = EmbeddingStore::new(&db_path, provider).unwrap();

        assert_eq!(store.count().unwrap(), 0);
        store.store_vector("qn1", &[1.0, 0.0], "h1").unwrap();
        store.store_vector("qn2", &[0.0, 1.0], "h2").unwrap();
        assert_eq!(store.count().unwrap(), 2);
    }

    #[test]
    fn provider_name_local() {
        let p = Provider::Local {
            sidecar_path: "/foo".to_string(),
            model: "all-MiniLM-L6-v2".to_string(),
        };
        assert_eq!(p.name(), "local:all-MiniLM-L6-v2");
    }

    #[test]
    fn provider_name_openai() {
        let p = Provider::OpenAiCompat {
            api_key: "key".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "text-embedding-3-small".to_string(),
        };
        assert!(p.name().starts_with("openai:text-embedding-3-small@"));
    }

    #[test]
    fn extract_host_key_strips_scheme() {
        assert_eq!(extract_host_key("https://api.openai.com/v1"), "api.openai.com/v1");
        assert_eq!(extract_host_key("https://api.openai.com/v1/embeddings"), "api.openai.com/v1");
    }

    #[test]
    fn node_to_text_basic() {
        let node = GraphNode {
            id: 1,
            kind: "Function".to_string(),
            name: "processData".to_string(),
            qualified_name: "src/mod.rs::processData".to_string(),
            file_path: "src/handlers/mod.rs".to_string(),
            line_start: 1,
            line_end: 10,
            language: "rust".to_string(),
            parent_name: Some("MyStruct".to_string()),
            params: Some("data: &str".to_string()),
            return_type: Some("String".to_string()),
            is_test: false,
            file_hash: None,
            extra: serde_json::json!({}),
            community_id: None,
        };
        let text = node_to_text(&node);
        assert!(text.contains("processData"));
        assert!(text.contains("function"));
        assert!(text.contains("MyStruct"));
        assert!(text.contains("rust"));
    }
}
