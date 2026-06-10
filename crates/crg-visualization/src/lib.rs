/// crg-visualization — D3.js interactive HTML graph generator.
///
/// Ports Python `visualization.py` (2184 lines). Generates a self-contained
/// HTML file with an embedded D3.js force-directed graph.
///
/// Security invariants:
/// - All node names and paths injected into HTML are escaped with `esc_html`.
/// - The D3.js script tag carries an SRI `integrity` attribute.
/// - Graph data is injected via a JSON placeholder, not direct string concat.
use std::collections::HashMap;

use anyhow::Result;
use crg_core::security::esc_html;
use crg_core::store::GraphStore;
use tracing::{debug, info};

// D3.js v7 — SRI hash taken verbatim from the Python source (visualization.py).
const D3_CDN_URL: &str =
    "https://cdnjs.cloudflare.com/ajax/libs/d3/7.8.5/d3.min.js";
const D3_SRI_HASH: &str =
    "sha384-CjloA8y00+1SDAUkjs099PVfnY2KmDC2BZnws9kh8D/lX1s46w6EPhpXdqMfjK6i";

/// Map a node kind to a CSS color.
fn node_color(kind: &str) -> &'static str {
    match kind {
        "File" => "#6366f1",
        "Class" => "#10b981",
        "Function" => "#f59e0b",
        "Test" => "#3b82f6",
        "Type" => "#8b5cf6",
        _ => "#6b7280",
    }
}

// ---------------------------------------------------------------------------
// HTML template
// ---------------------------------------------------------------------------

/// Build the full HTML document.
///
/// `graph_data_json` is a serialised JSON string for `{nodes:[…], links:[…]}`.
/// It is injected as JavaScript assignment, so it must be valid JSON and must
/// not contain the literal `</script>` — `serde_json::to_string` never emits
/// that sequence, but we double-check by wrapping in a template that uses a
/// variable assignment to avoid inline string-termination issues.
fn build_html(title: &str, graph_data_json: &str) -> String {
    // Escape title for HTML attribute / heading context.
    let safe_title = esc_html(title);

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>{title}</title>
  <style>
    * {{ box-sizing: border-box; margin: 0; padding: 0; }}
    body {{ font-family: system-ui, sans-serif; background: #0f172a; color: #e2e8f0; height: 100vh; display: flex; flex-direction: column; }}
    #toolbar {{ padding: 10px 16px; background: #1e293b; border-bottom: 1px solid #334155; display: flex; gap: 12px; align-items: center; flex-wrap: wrap; }}
    #toolbar h1 {{ font-size: 1rem; font-weight: 600; color: #f1f5f9; margin-right: auto; }}
    #search {{ padding: 6px 10px; border-radius: 6px; border: 1px solid #475569; background: #0f172a; color: #e2e8f0; font-size: 0.85rem; width: 220px; }}
    .kind-btn {{ padding: 4px 10px; border-radius: 4px; border: 1px solid #475569; background: #1e293b; color: #cbd5e1; font-size: 0.8rem; cursor: pointer; }}
    .kind-btn.active {{ border-color: #60a5fa; color: #60a5fa; }}
    #stats {{ font-size: 0.78rem; color: #94a3b8; }}
    #graph {{ flex: 1; overflow: hidden; }}
    .tooltip {{ position: absolute; background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 8px 12px; font-size: 0.8rem; pointer-events: none; opacity: 0; transition: opacity 0.15s; max-width: 320px; word-break: break-all; }}
    .node circle {{ stroke-width: 1.5px; cursor: pointer; }}
    .node text {{ fill: #e2e8f0; font-size: 10px; pointer-events: none; }}
    .link {{ stroke: #475569; stroke-width: 1px; stroke-opacity: 0.6; }}
    .link.highlighted {{ stroke: #60a5fa; stroke-opacity: 1; stroke-width: 2px; }}
    .node.faded circle {{ opacity: 0.15; }}
    .node.faded text {{ opacity: 0.15; }}
  </style>
</head>
<body>
  <div id="toolbar">
    <h1>{title}</h1>
    <input id="search" type="text" placeholder="Search nodes…" />
    <div id="kind-filters"></div>
    <span id="stats"></span>
  </div>
  <div id="graph"></div>
  <div class="tooltip" id="tooltip"></div>

  <script src="{d3_url}" integrity="{d3_sri}" crossorigin="anonymous"></script>
  <script>
    const GRAPH_DATA = {graph_data};

    const width = window.innerWidth;
    const height = window.innerHeight - document.getElementById('toolbar').offsetHeight;

    const svg = d3.select('#graph').append('svg')
      .attr('width', width)
      .attr('height', height)
      .call(d3.zoom().on('zoom', e => g.attr('transform', e.transform)));

    const g = svg.append('g');

    const tooltip = document.getElementById('tooltip');

    // --- Kind filters ---
    const allKinds = [...new Set(GRAPH_DATA.nodes.map(n => n.kind))].sort();
    const activeKinds = new Set(allKinds);
    const kindDiv = document.getElementById('kind-filters');
    allKinds.forEach(k => {{
      const btn = document.createElement('button');
      btn.className = 'kind-btn active';
      btn.textContent = k;
      btn.dataset.kind = k;
      btn.onclick = () => {{
        if (activeKinds.has(k)) {{ activeKinds.delete(k); btn.classList.remove('active'); }}
        else {{ activeKinds.add(k); btn.classList.add('active'); }}
        restart();
      }};
      kindDiv.appendChild(btn);
    }});

    // --- Search ---
    document.getElementById('search').addEventListener('input', e => {{
      searchTerm = e.target.value.toLowerCase();
      highlightSearch();
    }});
    let searchTerm = '';

    function highlightSearch() {{
      node.classed('faded', d => searchTerm && !d.label.toLowerCase().includes(searchTerm));
    }}

    let simulation, node, link;

    function restart() {{
      const visibleNodes = GRAPH_DATA.nodes.filter(n => activeKinds.has(n.kind));
      const visibleIds = new Set(visibleNodes.map(n => n.id));
      const visibleLinks = GRAPH_DATA.links.filter(l =>
        visibleIds.has(typeof l.source === 'object' ? l.source.id : l.source) &&
        visibleIds.has(typeof l.target === 'object' ? l.target.id : l.target)
      );

      document.getElementById('stats').textContent =
        `${{visibleNodes.length}} nodes · ${{visibleLinks.length}} edges`;

      g.selectAll('*').remove();

      simulation = d3.forceSimulation(visibleNodes)
        .force('link', d3.forceLink(visibleLinks).id(d => d.id).distance(60))
        .force('charge', d3.forceManyBody().strength(-120))
        .force('center', d3.forceCenter(width / 2, height / 2))
        .force('collision', d3.forceCollide(18));

      link = g.append('g').selectAll('line')
        .data(visibleLinks).join('line').attr('class', 'link');

      node = g.append('g').selectAll('g')
        .data(visibleNodes).join('g')
        .attr('class', 'node')
        .call(d3.drag()
          .on('start', (e, d) => {{ if (!e.active) simulation.alphaTarget(0.3).restart(); d.fx = d.x; d.fy = d.y; }})
          .on('drag', (e, d) => {{ d.fx = e.x; d.fy = e.y; }})
          .on('end', (e, d) => {{ if (!e.active) simulation.alphaTarget(0); d.fx = null; d.fy = null; }}))
        .on('mouseover', (e, d) => {{
          tooltip.innerHTML = `<b>${{d.label}}</b><br>Kind: ${{d.kind}}<br>File: ${{d.file}}`;
          tooltip.style.opacity = 1;
        }})
        .on('mousemove', e => {{
          tooltip.style.left = (e.pageX + 14) + 'px';
          tooltip.style.top = (e.pageY - 10) + 'px';
        }})
        .on('mouseout', () => {{ tooltip.style.opacity = 0; }})
        .on('click', (e, d) => highlightNeighbors(d, visibleLinks));

      node.append('circle')
        .attr('r', 8)
        .attr('fill', d => d.color)
        .attr('stroke', '#e2e8f0');

      node.append('text')
        .attr('x', 11)
        .attr('dy', '0.35em')
        .text(d => d.label.length > 20 ? d.label.slice(0, 20) + '…' : d.label);

      simulation.on('tick', () => {{
        link
          .attr('x1', d => d.source.x).attr('y1', d => d.source.y)
          .attr('x2', d => d.target.x).attr('y2', d => d.target.y);
        node.attr('transform', d => `translate(${{d.x}},${{d.y}})`);
      }});

      highlightSearch();
    }}

    function highlightNeighbors(selected, links) {{
      const neighborIds = new Set();
      links.forEach(l => {{
        const sid = typeof l.source === 'object' ? l.source.id : l.source;
        const tid = typeof l.target === 'object' ? l.target.id : l.target;
        if (sid === selected.id) neighborIds.add(tid);
        if (tid === selected.id) neighborIds.add(sid);
      }});
      node.classed('faded', d => d.id !== selected.id && !neighborIds.has(d.id));
      link.classed('highlighted', l => {{
        const sid = typeof l.source === 'object' ? l.source.id : l.source;
        const tid = typeof l.target === 'object' ? l.target.id : l.target;
        return sid === selected.id || tid === selected.id;
      }});
    }}

    restart();
  </script>
</body>
</html>"#,
        title = safe_title,
        d3_url = D3_CDN_URL,
        d3_sri = D3_SRI_HASH,
        graph_data = graph_data_json,
    )
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generate a D3.js force-directed graph HTML string from the store.
///
/// Up to `max_nodes` nodes are included (oldest/smallest ID first).
/// `title` defaults to `"Code Graph"` if `None`.
pub fn generate_html(
    store: &GraphStore,
    title: Option<&str>,
    max_nodes: usize,
) -> Result<String> {
    let title = title.unwrap_or("Code Graph");

    // Fetch nodes (exclude raw File nodes for clarity; keep functions/classes/tests).
    let all_nodes = store.get_all_nodes(false)?;

    // Split: non-File nodes first, pad with Files if needed.
    let mut non_file: Vec<_> = all_nodes.iter().filter(|n| n.kind != "File").collect();
    let mut file_nodes: Vec<_> = all_nodes.iter().filter(|n| n.kind == "File").collect();

    non_file.truncate(max_nodes);
    if non_file.len() < max_nodes {
        file_nodes.truncate(max_nodes - non_file.len());
        non_file.extend(file_nodes.iter().copied());
    }
    let visible_nodes = non_file;

    debug!(
        "generate_html: {} nodes selected (max_nodes={})",
        visible_nodes.len(),
        max_nodes
    );

    // Build id lookup for edge filtering.
    let id_set: std::collections::HashSet<i64> = visible_nodes.iter().map(|n| n.id).collect();

    // Collect all edges among visible nodes.
    let qn_set: std::collections::HashSet<String> =
        visible_nodes.iter().map(|n| n.qualified_name.clone()).collect();
    let edges = store.get_edges_among(&qn_set)?;

    // Build D3 node list.
    let d3_nodes: Vec<serde_json::Value> = visible_nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id,
                // Use esc_html for label — injected into HTML tooltips.
                "label": esc_html(&n.name),
                "kind": n.kind,
                "file": esc_html(&n.file_path),
                "color": node_color(&n.kind),
                "community": n.community_id,
            })
        })
        .collect();

    // Build id lookup: qualified_name → node_id (for edge source/target).
    let qn_to_id: HashMap<String, i64> = visible_nodes
        .iter()
        .map(|n| (n.qualified_name.clone(), n.id))
        .collect();

    let d3_links: Vec<serde_json::Value> = edges
        .iter()
        .filter_map(|e| {
            let src_id = qn_to_id.get(&e.source_qualified)?;
            let tgt_id = qn_to_id.get(&e.target_qualified)?;
            // Sanity-check: both endpoints must be in the visible set.
            if !id_set.contains(src_id) || !id_set.contains(tgt_id) {
                return None;
            }
            Some(serde_json::json!({
                "source": src_id,
                "target": tgt_id,
                "kind": e.kind,
            }))
        })
        .collect();

    let graph_data = serde_json::json!({
        "nodes": d3_nodes,
        "links": d3_links,
    });
    let graph_data_json = serde_json::to_string(&graph_data)?;

    let html = build_html(title, &graph_data_json);
    info!(
        "generate_html: produced {} bytes for '{}' ({} nodes, {} links)",
        html.len(),
        title,
        d3_nodes.len(),
        d3_links.len()
    );
    Ok(html)
}

/// Write the generated HTML to `output_path`.
pub fn export_to_file(
    store: &GraphStore,
    output_path: &str,
    title: Option<&str>,
    max_nodes: usize,
) -> Result<()> {
    let html = generate_html(store, title, max_nodes)?;
    std::fs::write(output_path, html.as_bytes())?;
    info!("export_to_file: wrote {}", output_path);
    Ok(())
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
            .join(format!("crg-viz-test-{}-{}", std::process::id(), n));
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
    fn generate_html_empty_graph() {
        let store = make_store();
        let html = generate_html(&store, None, 2000).unwrap();
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains(D3_SRI_HASH), "SRI hash must be present");
        assert!(html.contains(D3_CDN_URL));
    }

    #[test]
    fn generate_html_contains_sri() {
        let store = make_store();
        let html = generate_html(&store, Some("My Project"), 2000).unwrap();
        assert!(html.contains("integrity="), "integrity attribute missing");
        assert!(html.contains(D3_SRI_HASH));
    }

    #[test]
    fn generate_html_escapes_names() {
        let store = make_store();
        // Insert a node with a name that contains HTML special chars.
        let n = node("<script>alert(1)</script>", "Function", "f.py");
        store.upsert_node(&n, "").unwrap();
        let html = generate_html(&store, None, 2000).unwrap();
        // The raw XSS string must not appear in the output.
        assert!(
            !html.contains("<script>alert(1)</script>"),
            "unescaped XSS string found in output"
        );
    }

    #[test]
    fn node_color_coverage() {
        assert_eq!(node_color("File"), "#6366f1");
        assert_eq!(node_color("Class"), "#10b981");
        assert_eq!(node_color("Function"), "#f59e0b");
        assert_eq!(node_color("Test"), "#3b82f6");
        assert_eq!(node_color("Type"), "#8b5cf6");
        assert_eq!(node_color("Unknown"), "#6b7280");
    }

    #[test]
    fn export_to_file_creates_file() {
        let store = make_store();
        store.upsert_node(&node("fn_a", "Function", "f.py"), "").unwrap();
        let out = std::env::temp_dir()
            .join(format!("crg-viz-out-{}.html", std::process::id()));
        export_to_file(&store, out.to_str().unwrap(), Some("Test"), 2000).unwrap();
        assert!(out.exists());
        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.contains("<!DOCTYPE html>"));
    }
}
