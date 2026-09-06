#!/usr/bin/env bash
# Build (or serve) the MkDocs static site using a temporary Docker container.
# No local Python/MkDocs install required.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

IMAGE="${MKDOCS_IMAGE:-squidfunk/mkdocs-material:9.5}"
SITE_DIR="${SITE_DIR:-site}"
CMD="${1:-build}"

if ! command -v docker >/dev/null 2>&1; then
  echo "error: docker is required to build the docs site" >&2
  exit 1
fi

if [[ ! -f "$ROOT/mkdocs.yml" ]]; then
  echo "error: mkdocs.yml not found at $ROOT/mkdocs.yml" >&2
  exit 1
fi

# Run as current user so generated files are not root-owned.
DOCKER_USER="$(id -u):$(id -g)"

run_mkdocs() {
  docker run --rm \
    --user "$DOCKER_USER" \
    -e HOME=/tmp \
    -e XDG_CACHE_HOME=/tmp/.cache \
    -v "$ROOT:/docs:rw" \
    -w /docs \
    "$@"
}

case "$CMD" in
  build)
    echo "==> building docs with $IMAGE → $SITE_DIR/"
    # Ensure output dir exists and is writable by the container user.
    mkdir -p "$ROOT/$SITE_DIR"
    run_mkdocs "$IMAGE" build --clean -d "/docs/$SITE_DIR"
    echo "==> done: $ROOT/$SITE_DIR/index.html"
    ;;
  serve)
    PORT="${PORT:-8000}"
    echo "==> serving docs with $IMAGE on http://127.0.0.1:${PORT}"
    echo "    (Ctrl+C to stop)"
    run_mkdocs \
      -p "${PORT}:8000" \
      "$IMAGE" serve --dev-addr=0.0.0.0:8000
    ;;
  pull)
    echo "==> pulling $IMAGE"
    docker pull "$IMAGE"
    ;;
  -h|--help|help)
    cat <<EOF
Usage: $(basename "$0") [build|serve|pull|help]

  build   Generate a static site into ./${SITE_DIR}/ (default)
  serve   Live-reload preview on PORT (default 8000) via Docker
  pull    Pre-pull the MkDocs Material image
  help    Show this message

Environment:
  MKDOCS_IMAGE   Image to use (default: ${IMAGE})
  SITE_DIR       Output directory under the repo (default: site)
  PORT           Host port for serve (default: 8000)
EOF
    ;;
  *)
    echo "error: unknown command '$CMD' (try: build|serve|pull|help)" >&2
    exit 1
    ;;
esac
