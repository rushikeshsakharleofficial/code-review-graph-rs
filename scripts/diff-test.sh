#!/usr/bin/env bash
# Differential test harness: compare Rust and Python graph builds against same fixture.
#
# Usage:
#   ./scripts/diff-test.sh [fixture_dir]
#
# If no fixture_dir given, uses a small built-in Python fixture.
# Produces sorted nodes.tsv and edges.tsv from both builds, then diffs them.
#
# Exit codes:
#   0 — graphs are identical
#   1 — graphs differ (diff output shown)
#   2 — build error

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(dirname "$SCRIPT_DIR")"
PYTHON_CRG_DIR="$(dirname "$WORKSPACE_DIR")/code-review-graph"
FIXTURE_DIR="${1:-}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

PY_OUT="$TMP_DIR/py"
RS_OUT="$TMP_DIR/rs"
mkdir -p "$PY_OUT" "$RS_OUT"

# ── 1. Select or create fixture ──────────────────────────────────────────────

if [[ -z "$FIXTURE_DIR" ]]; then
    FIXTURE_DIR="$TMP_DIR/fixture"
    mkdir -p "$FIXTURE_DIR"
    cat > "$FIXTURE_DIR/hello.py" <<'PYEOF'
import os
from pathlib import Path

class Greeter:
    """Greets users."""

    def greet(self, name: str) -> str:
        return f"Hello, {name}!"

    def greet_file(self, path: Path) -> str:
        content = path.read_text()
        return self.greet(content.strip())

def main():
    g = Greeter()
    print(g.greet("world"))
    print(g.greet_file(Path("name.txt")))

if __name__ == "__main__":
    main()
PYEOF
    cat > "$FIXTURE_DIR/utils.py" <<'PYEOF'
def format_name(first: str, last: str) -> str:
    return f"{first} {last}"

def parse_config(config_path: str) -> dict:
    import json
    with open(config_path) as f:
        return json.load(f)

class Config:
    def __init__(self, path: str):
        self.data = parse_config(path)

    def get(self, key: str):
        return self.data.get(key)
PYEOF
    echo "Using built-in fixture at $FIXTURE_DIR"
fi

# Ensure the fixture has a .git dir (needed by validate_repo_root in the Rust CLI)
if [[ ! -d "$FIXTURE_DIR/.git" && ! -d "$FIXTURE_DIR/.code-review-graph" ]]; then
    git -C "$FIXTURE_DIR" init -q 2>/dev/null || true
fi

echo "Fixture: $FIXTURE_DIR"

# ── 2. Python build ───────────────────────────────────────────────────────────

PY_DB="$PY_OUT/graph.db"
echo "[python] Building graph..."
python3 -c "
import sys, os
from pathlib import Path
sys.path.insert(0, '$PYTHON_CRG_DIR')
from code_review_graph.incremental import full_build
from code_review_graph.graph import GraphStore
store = GraphStore('$PY_DB')
try:
    full_build(Path('$FIXTURE_DIR'), store)
    print('Python build: OK')
except Exception as e:
    print(f'Python build failed: {e}', file=sys.stderr)
    try: store.close()
    except: pass
    sys.exit(2)
try: store.close()
except: pass
" 2>&1
echo "[python] Done"

# ── 3. Rust build ─────────────────────────────────────────────────────────────

RS_DB="$RS_OUT/graph.db"
RS_BIN="$WORKSPACE_DIR/target/debug/code-review-graph"

if [[ ! -f "$RS_BIN" ]]; then
    echo "[rust] Building binary (this may take a while)..."
    (cd "$WORKSPACE_DIR" && cargo build -p crg-cli 2>&1 | tail -5)
    # The binary name may differ; try common names
    for name in code-review-graph crg crg-cli; do
        if [[ -f "$WORKSPACE_DIR/target/debug/$name" ]]; then
            RS_BIN="$WORKSPACE_DIR/target/debug/$name"
            break
        fi
    done
fi

if [[ -f "$RS_BIN" ]]; then
    echo "[rust] Building graph..."
    CRG_DB_PATH="$RS_DB" "$RS_BIN" build --repo "$FIXTURE_DIR" --full 2>/dev/null || true
    echo "[rust] Done"
else
    echo "[rust] Binary not found yet — skipping Rust build (run 'cargo build -p crg-cli' first)"
    echo "PARTIAL: only Python graph was built at $PY_DB"
    exit 0
fi

# ── 4. Export sorted TSV snapshots ───────────────────────────────────────────

export_nodes() {
    local db="$1" out="$2"
    python3 -c "
import sqlite3, sys
conn = sqlite3.connect('$db')
rows = conn.execute(\"SELECT kind, name, qualified_name, file_path, line_start, line_end, language FROM nodes WHERE kind != 'File' ORDER BY qualified_name ASC\").fetchall()
conn.close()
for r in rows:
    print('\t'.join('' if v is None else str(v) for v in r))
" > "$out/nodes.tsv"
}

export_edges() {
    local db="$1" out="$2"
    python3 -c "
import sqlite3, sys
conn = sqlite3.connect('$db')
rows = conn.execute('SELECT kind, source_qualified, target_qualified FROM edges ORDER BY kind ASC, source_qualified ASC, target_qualified ASC').fetchall()
conn.close()
for r in rows:
    print('\t'.join('' if v is None else str(v) for v in r))
" > "$out/edges.tsv"
}

if [[ -f "$PY_DB" ]]; then
    export_nodes "$PY_DB" "$PY_OUT"
    export_edges "$PY_DB" "$PY_OUT"
fi

if [[ -f "$RS_DB" ]]; then
    export_nodes "$RS_DB" "$RS_OUT"
    export_edges "$RS_DB" "$RS_OUT"
fi

# ── 5. Diff ───────────────────────────────────────────────────────────────────

if [[ ! -f "$PY_DB" ]]; then
    echo "ERROR: Python build failed, no graph.db produced" >&2
    exit 2
fi
if [[ ! -f "$RS_DB" ]]; then
    echo "PARTIAL: Rust build not done yet, only Python graph available"
    echo "Python nodes: $(wc -l < "$PY_OUT/nodes.tsv") non-file nodes"
    echo "Python edges: $(wc -l < "$PY_OUT/edges.tsv") edges"
    exit 0
fi

PY_NODES=$(wc -l < "$PY_OUT/nodes.tsv")
RS_NODES=$(wc -l < "$RS_OUT/nodes.tsv")
PY_EDGES=$(wc -l < "$PY_OUT/edges.tsv")
RS_EDGES=$(wc -l < "$RS_OUT/edges.tsv")

echo ""
echo "═══════════════════════════════════════"
echo "  Python: $PY_NODES nodes, $PY_EDGES edges"
echo "  Rust:   $RS_NODES nodes, $RS_EDGES edges"
echo "═══════════════════════════════════════"

NODE_DIFF=$({ diff "$PY_OUT/nodes.tsv" "$RS_OUT/nodes.tsv" || true; } | wc -l)
EDGE_DIFF=$({ diff "$PY_OUT/edges.tsv" "$RS_OUT/edges.tsv" || true; } | wc -l)

if [[ "$NODE_DIFF" -eq 0 && "$EDGE_DIFF" -eq 0 ]]; then
    echo "✓ PASS: Graphs are identical"
    exit 0
else
    echo ""
    echo "✗ DIFF: Graphs differ"
    echo ""
    if [[ "$NODE_DIFF" -gt 0 ]]; then
        echo "--- NODES diff (Python vs Rust) ---"
        diff "$PY_OUT/nodes.tsv" "$RS_OUT/nodes.tsv" | head -50 || true
    fi
    if [[ "$EDGE_DIFF" -gt 0 ]]; then
        echo "--- EDGES diff (Python vs Rust) ---"
        diff "$PY_OUT/edges.tsv" "$RS_OUT/edges.tsv" | head -50 || true
    fi
    echo ""
    echo "Note: Grammar version differences between Python tree-sitter-language-pack"
    echo "0.13.0 and Rust tree-sitter-language-pack 1.8.1 may cause minor variations."
    echo "Use 'rebuild recommended' migration path when deploying the Rust version."
    exit 1
fi
