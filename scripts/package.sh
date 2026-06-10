#!/usr/bin/env bash
# Build .deb and .rpm packages for code-review-graph.
#
# Usage:
#   ./scripts/package.sh [--deb-only | --rpm-only]
#
# Requires:
#   cargo-deb:          cargo install cargo-deb
#   cargo-generate-rpm: cargo install cargo-generate-rpm

set -euo pipefail

WORKSPACE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$WORKSPACE"

DEB=true
RPM=true
case "${1:-}" in
    --deb-only) RPM=false ;;
    --rpm-only) DEB=false ;;
esac

mkdir -p target/packages

# ── 1. Release build ──────────────────────────────────────────────────────────

echo "[1/3] Building release binaries..."
cargo build --release -p crg-cli -p crg-mcp

# ── 2. DEB ────────────────────────────────────────────────────────────────────

if $DEB; then
    if ! cargo deb --help &>/dev/null 2>&1; then
        echo "Installing cargo-deb..."
        cargo install cargo-deb
    fi
    echo "[2/3] Building .deb package..."
    cargo deb -p crg-cli --no-build --output target/packages/
fi

# ── 3. RPM ────────────────────────────────────────────────────────────────────

if $RPM; then
    if ! cargo generate-rpm --help &>/dev/null 2>&1; then
        echo "Installing cargo-generate-rpm..."
        cargo install cargo-generate-rpm
    fi
    echo "[3/3] Building .rpm package..."
    cargo generate-rpm -p crg-cli --output target/packages/
fi

# ── Result ────────────────────────────────────────────────────────────────────

echo ""
echo "Packages built:"
ls -lh target/packages/ 2>/dev/null || echo "(none found)"
echo ""
echo "Install:"
if $DEB; then
    DEB_FILE=$(ls target/packages/*.deb 2>/dev/null | head -1 || true)
    [[ -n "$DEB_FILE" ]] && echo "  sudo dpkg -i $DEB_FILE"
fi
if $RPM; then
    RPM_FILE=$(ls target/packages/*.rpm 2>/dev/null | head -1 || true)
    [[ -n "$RPM_FILE" ]] && echo "  sudo rpm -i $RPM_FILE"
fi
