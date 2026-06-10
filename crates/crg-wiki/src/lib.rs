/// crg-wiki — Markdown wiki generator from the community structure.
///
/// Ports Python `wiki.py` (305 lines). All data is read from `GraphStore`
/// directly — crg-communities is listed as a dependency but is currently an
/// empty placeholder, so we do not import from it.
use std::path::Path;

use anyhow::{Context, Result};
use crg_core::store::GraphStore;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Index page
// ---------------------------------------------------------------------------

/// Return a Markdown index of all communities with links to their pages.
pub fn generate_wiki_index(store: &GraphStore) -> Result<String> {
    let communities = store.get_communities_list()?;
    let mut md = String::new();

    md.push_str("# Code Graph Wiki\n\n");
    md.push_str("Auto-generated from the knowledge graph.\n\n");
    md.push_str("## Communities\n\n");

    if communities.is_empty() {
        md.push_str("*No communities detected yet. Run `crg build` first.*\n");
        return Ok(md);
    }

    md.push_str("| Community | Size | Language | Description |\n");
    md.push_str("|-----------|------|----------|-------------|\n");

    for comm in &communities {
        let id = comm["id"].as_i64().unwrap_or(0);
        let name = comm["name"].as_str().unwrap_or("(unnamed)");
        let size = comm["size"].as_i64().unwrap_or(0);
        let lang = comm["dominant_language"]
            .as_str()
            .unwrap_or("—");
        let desc = comm["description"]
            .as_str()
            .unwrap_or("TODO")
            .replace('\n', " ");
        let slug = community_slug(name, id);
        md.push_str(&format!(
            "| [{}]({}.md) | {} | {} | {} |\n",
            name, slug, size, lang, desc
        ));
    }

    Ok(md)
}

// ---------------------------------------------------------------------------
// Community page
// ---------------------------------------------------------------------------

/// Return a Markdown page for a single community.
pub fn generate_community_page(store: &GraphStore, community_id: i64) -> Result<String> {
    let communities = store.get_communities_list()?;
    let comm = communities
        .iter()
        .find(|c| c["id"].as_i64() == Some(community_id))
        .ok_or_else(|| anyhow::anyhow!("Community {} not found", community_id))?;

    let name = comm["name"].as_str().unwrap_or("(unnamed)");
    let size = comm["size"].as_i64().unwrap_or(0);
    let lang = comm["dominant_language"].as_str().unwrap_or("unknown");
    let description = comm["description"].as_str().unwrap_or("TODO");

    let mut md = String::new();
    md.push_str(&format!("# {}\n\n", name));
    md.push_str(&format!("**Size**: {} functions/classes  \n", size));
    md.push_str(&format!("**Language**: {}  \n", lang));
    md.push_str(&format!("**Purpose**: {}\n\n", description));

    // Key symbols.
    let members = store.get_nodes_by_community_id(community_id)?;
    if !members.is_empty() {
        md.push_str("## Key Symbols\n\n");
        // Show Functions and Classes first, then other kinds.
        let mut fns: Vec<_> = members
            .iter()
            .filter(|n| n.kind == "Function" || n.kind == "Class")
            .collect();
        fns.sort_by(|a, b| a.name.cmp(&b.name));
        for n in fns.iter().take(20) {
            md.push_str(&format!(
                "- `{}` ({}) in `{}`\n",
                n.name, n.kind, n.file_path
            ));
        }
    }

    // Dependencies: top 10 outgoing edge targets outside this community.
    md.push_str("\n## Dependencies (top 10 by frequency)\n\n");
    let community_qns = store.get_community_member_qns(community_id)?;
    let community_set: std::collections::HashSet<String> =
        community_qns.into_iter().collect();

    let mut dep_count: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for member in &members {
        let outgoing = store.get_outgoing_targets(&member.qualified_name)?;
        for (target, kind) in outgoing {
            if (kind == "CALLS" || kind == "IMPORTS_FROM" || kind == "DEPENDS_ON")
                && !community_set.contains(&target)
            {
                *dep_count.entry(target).or_default() += 1;
            }
        }
    }

    let mut dep_list: Vec<(String, usize)> = dep_count.into_iter().collect();
    dep_list.sort_by(|a, b| b.1.cmp(&a.1));

    if dep_list.is_empty() {
        md.push_str("*No external dependencies detected.*\n");
    } else {
        for (dep, count) in dep_list.iter().take(10) {
            md.push_str(&format!("- `{}` (referenced {} time(s))\n", dep, count));
        }
    }

    Ok(md)
}

// ---------------------------------------------------------------------------
// Full wiki
// ---------------------------------------------------------------------------

/// Write one Markdown file per community plus an `index.md` to `output_dir`.
///
/// Returns the list of written file paths.
pub fn generate_full_wiki(store: &GraphStore, output_dir: &str) -> Result<Vec<String>> {
    let dir = Path::new(output_dir);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create wiki output dir: {output_dir}"))?;

    let communities = store.get_communities_list()?;
    let mut written = Vec::new();

    // Index page.
    let index_content = generate_wiki_index(store)?;
    let index_path = dir.join("index.md");
    std::fs::write(&index_path, index_content.as_bytes())?;
    written.push(index_path.to_string_lossy().into_owned());
    info!("wiki: wrote index.md");

    // One page per community.
    for comm in &communities {
        let id = comm["id"].as_i64().unwrap_or(0);
        let name = comm["name"].as_str().unwrap_or("community");
        let slug = community_slug(name, id);
        let page_content = generate_community_page(store, id)?;
        let page_path = dir.join(format!("{}.md", slug));
        std::fs::write(&page_path, page_content.as_bytes())?;
        written.push(page_path.to_string_lossy().into_owned());
        debug!("wiki: wrote {}.md", slug);
    }

    info!("generate_full_wiki: {} files written", written.len());
    Ok(written)
}

// ---------------------------------------------------------------------------
// Ollama-enriched wiki
// ---------------------------------------------------------------------------

/// Same as `generate_full_wiki` but enriches community descriptions using the
/// Ollama sidecar located at `sidecar_path`.
///
/// For each community, a prompt is sent to the sidecar's `"generate"` method.
/// The response `text` field is used as the community description in the page.
///
/// Falls back to the static description if the sidecar call fails.
pub async fn generate_wiki_with_ollama(
    store: &GraphStore,
    output_dir: &str,
    sidecar_path: &str,
    model: &str,
) -> Result<Vec<String>> {
    use crg_sidecar_bridge::SidecarPool;

    let dir = Path::new(output_dir);
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create wiki output dir: {output_dir}"))?;

    let pool = SidecarPool::new(sidecar_path.to_string());
    let communities = store.get_communities_list()?;
    let mut written = Vec::new();

    // Write index — plain (no LLM enrichment needed for the table).
    let index_content = generate_wiki_index(store)?;
    let index_path = dir.join("index.md");
    std::fs::write(&index_path, index_content.as_bytes())?;
    written.push(index_path.to_string_lossy().into_owned());

    for comm in &communities {
        let id = comm["id"].as_i64().unwrap_or(0);
        let name = comm["name"].as_str().unwrap_or("community");
        let size = comm["size"].as_i64().unwrap_or(0);
        let lang = comm["dominant_language"].as_str().unwrap_or("unknown");

        // Gather some member names for context.
        let members = store.get_nodes_by_community_id(id)?;
        let sample_names: Vec<&str> = members
            .iter()
            .take(8)
            .map(|n| n.name.as_str())
            .collect();

        let prompt = format!(
            "Describe the purpose of a software module named \"{name}\" \
             written in {lang}. It contains {size} symbols including: {symbols}. \
             Write 1-2 concise sentences suitable for a developer wiki.",
            name = name,
            lang = lang,
            size = size,
            symbols = sample_names.join(", "),
        );

        let description = match pool
            .call(
                "generate",
                serde_json::json!({"prompt": prompt, "model": model}),
            )
            .await
        {
            Ok(resp) => resp["text"]
                .as_str()
                .unwrap_or("(no description)")
                .trim()
                .to_string(),
            Err(e) => {
                tracing::warn!("ollama call failed for community {id}: {e}");
                comm["description"]
                    .as_str()
                    .unwrap_or("TODO")
                    .to_string()
            }
        };

        // Re-generate the page content but inject the LLM description.
        let page_md = generate_community_page_with_description(store, id, &description)?;
        let slug = community_slug(name, id);
        let page_path = dir.join(format!("{}.md", slug));
        std::fs::write(&page_path, page_md.as_bytes())?;
        written.push(page_path.to_string_lossy().into_owned());
        debug!("wiki(ollama): wrote {}.md", slug);
    }

    info!(
        "generate_wiki_with_ollama: {} files written",
        written.len()
    );
    Ok(written)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Build a filesystem-safe slug for a community.
fn community_slug(name: &str, id: i64) -> String {
    let safe: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    format!("community_{}_{}", id, safe.to_lowercase())
}

/// Like `generate_community_page` but overrides the description field.
fn generate_community_page_with_description(
    store: &GraphStore,
    community_id: i64,
    description: &str,
) -> Result<String> {
    let communities = store.get_communities_list()?;
    let comm = communities
        .iter()
        .find(|c| c["id"].as_i64() == Some(community_id))
        .ok_or_else(|| anyhow::anyhow!("Community {} not found", community_id))?;

    let name = comm["name"].as_str().unwrap_or("(unnamed)");
    let size = comm["size"].as_i64().unwrap_or(0);
    let lang = comm["dominant_language"].as_str().unwrap_or("unknown");

    let mut md = String::new();
    md.push_str(&format!("# {}\n\n", name));
    md.push_str(&format!("**Size**: {} functions/classes  \n", size));
    md.push_str(&format!("**Language**: {}  \n", lang));
    md.push_str(&format!("**Purpose**: {}\n\n", description));

    let members = store.get_nodes_by_community_id(community_id)?;
    if !members.is_empty() {
        md.push_str("## Key Symbols\n\n");
        let mut fns: Vec<_> = members
            .iter()
            .filter(|n| n.kind == "Function" || n.kind == "Class")
            .collect();
        fns.sort_by(|a, b| a.name.cmp(&b.name));
        for n in fns.iter().take(20) {
            md.push_str(&format!(
                "- `{}` ({}) in `{}`\n",
                n.name, n.kind, n.file_path
            ));
        }
    }

    md.push_str("\n## Dependencies (top 10 by frequency)\n\n");
    let community_qns = store.get_community_member_qns(community_id)?;
    let community_set: std::collections::HashSet<String> =
        community_qns.into_iter().collect();

    let mut dep_count: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for member in &members {
        let outgoing = store.get_outgoing_targets(&member.qualified_name)?;
        for (target, kind) in outgoing {
            if (kind == "CALLS" || kind == "IMPORTS_FROM" || kind == "DEPENDS_ON")
                && !community_set.contains(&target)
            {
                *dep_count.entry(target).or_default() += 1;
            }
        }
    }

    let mut dep_list: Vec<(String, usize)> = dep_count.into_iter().collect();
    dep_list.sort_by(|a, b| b.1.cmp(&a.1));

    if dep_list.is_empty() {
        md.push_str("*No external dependencies detected.*\n");
    } else {
        for (dep, count) in dep_list.iter().take(10) {
            md.push_str(&format!("- `{}` (referenced {} time(s))\n", dep, count));
        }
    }

    Ok(md)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crg_core::{store::GraphStore, types::NodeInfo};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_store() -> GraphStore {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("crg-wiki-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        GraphStore::new(dir.join("graph.db")).unwrap()
    }

    fn node(name: &str, kind: &str, file: &str) -> NodeInfo {
        NodeInfo {
            kind: kind.to_string(),
            name: name.to_string(),
            file_path: file.to_string(),
            line_start: 1,
            line_end: 10,
            language: "python".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn index_empty_store() {
        let store = make_store();
        let md = generate_wiki_index(&store).unwrap();
        assert!(md.contains("# Code Graph Wiki"));
        assert!(md.contains("No communities detected"));
    }

    #[test]
    fn index_with_communities() {
        let store = make_store();
        store
            .upsert_community("core", 0, None, 0.8, 5, Some("python"), Some("Core utilities"))
            .unwrap();
        let md = generate_wiki_index(&store).unwrap();
        assert!(md.contains("core"));
    }

    #[test]
    fn community_page_not_found() {
        let store = make_store();
        let result = generate_community_page(&store, 9999);
        assert!(result.is_err());
    }

    #[test]
    fn community_page_structure() {
        let store = make_store();
        let comm_id = store
            .upsert_community("parser", 0, None, 0.9, 3, Some("rust"), Some("Parses things"))
            .unwrap();
        store.upsert_node(&node("parse_file", "Function", "src/parser.rs"), "").unwrap();
        store.set_node_community(1, comm_id).unwrap();

        let md = generate_community_page(&store, comm_id).unwrap();
        assert!(md.starts_with("# parser"));
        assert!(md.contains("**Size**"));
        assert!(md.contains("**Language**: rust"));
        assert!(md.contains("## Key Symbols"));
        assert!(md.contains("## Dependencies"));
    }

    #[test]
    fn full_wiki_writes_files() {
        let store = make_store();
        store
            .upsert_community("utils", 0, None, 0.7, 2, Some("python"), Some("Utility helpers"))
            .unwrap();
        let out_dir = std::env::temp_dir()
            .join(format!("crg-wiki-out-{}", std::process::id()));
        let files = generate_full_wiki(&store, out_dir.to_str().unwrap()).unwrap();
        assert!(files.iter().any(|f| f.ends_with("index.md")));
        assert!(files.iter().any(|f| f.ends_with(".md") && !f.ends_with("index.md")));
    }

    #[test]
    fn community_slug_is_safe() {
        let slug = community_slug("My/Nasty: Module!", 42);
        assert!(!slug.contains('/'));
        assert!(!slug.contains(':'));
        assert!(!slug.contains('!'));
        assert!(slug.contains("42"));
    }
}
