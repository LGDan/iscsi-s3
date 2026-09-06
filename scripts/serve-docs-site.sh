#!/usr/bin/env bash
# Live-reload docs preview for local development (MkDocs via temporary Docker).
# Usage:
#   ./scripts/serve-docs-site.sh          # http://127.0.0.1:8000
#   ./scripts/serve-docs-site.sh 8080     # custom host port
#   PORT=9000 ./scripts/serve-docs-site.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || "${1:-}" == "help" ]]; then
  cat <<EOF
Usage: $(basename "$0") [PORT]

  Start a live-reload MkDocs Material preview in Docker.
  Edits under docs/ and mkdocs.yml reload automatically.

  PORT   Host port (default: 8000). Also accepts PORT= env.

Environment:
  MKDOCS_IMAGE   Image to use (default: squidfunk/mkdocs-material:9.5)
  PORT           Host port if not passed as an argument
EOF
  exit 0
fi

if [[ -n "${1:-}" ]]; then
  export PORT="$1"
fi

exec "$ROOT/scripts/build-docs-site.sh" serve
