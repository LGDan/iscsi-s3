#!/usr/bin/env bash
# Build/run the stack and exercise S3 + in-network iSCSI smoke.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> unit tests"
cargo test --quiet --lib --bins

echo "==> docker compose up --build"
docker compose down -v --remove-orphans >/dev/null 2>&1 || true
docker compose up -d --build minio createbuckets iscsi-s3

echo "==> waiting for minio healthy + iscsi-s3 running"
for i in $(seq 1 180); do
  minio_id=$(docker compose ps -q minio 2>/dev/null || true)
  iscsi_id=$(docker compose ps -q iscsi-s3 2>/dev/null || true)
  minio_h=starting
  iscsi_st=starting
  if [[ -n "$minio_id" ]]; then
    minio_h=$(docker inspect --format='{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$minio_id" 2>/dev/null || echo starting)
  fi
  if [[ -n "$iscsi_id" ]]; then
    iscsi_st=$(docker inspect --format='{{.State.Status}}' "$iscsi_id" 2>/dev/null || echo starting)
  fi
  if [[ "$minio_h" == "healthy" && "$iscsi_st" == "running" ]]; then
    if docker compose logs iscsi-s3 2>/dev/null | grep -q "iSCSI server listening"; then
      echo "minio=$minio_h iscsi-s3=$iscsi_st"
      break
    fi
  fi
  if [[ "$i" -eq 180 ]]; then
    echo "timeout (minio=$minio_h iscsi-s3=$iscsi_st)"
    docker compose ps
    docker compose logs --tail=100
    exit 1
  fi
  sleep 2
done

echo "==> S3 store integration test"
ISCSI_S3_INTEGRATION=1 cargo test --test s3_integration -- --nocapture

echo "==> in-network iSCSI smoke client"
docker compose run --rm --build smoke

echo "==> verify chunk objects"
docker compose run --rm --no-deps --entrypoint /bin/sh createbuckets -c '
  set -e
  mc alias set local http://minio:9000 minioadmin minioadmin >/dev/null
  echo "--- disk0 ---"
  mc ls -r local/iscsi/disks/disk0/
  echo "--- disk1 ---"
  mc ls -r local/iscsi/disks/disk1/
  mc cat local/iscsi/disks/disk0/meta.json
  echo
  test -n "$(mc ls local/iscsi/disks/disk0/chunks/ 2>/dev/null | head -1)"
  test -n "$(mc ls local/iscsi/disks/disk1/chunks/ 2>/dev/null | head -1)"
  echo "S3 objects look good"
'

echo "==> all smoke checks passed"
