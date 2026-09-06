# Testing

## Unit tests

```bash
cargo test --lib --bins
```

Covers config precedence helpers, memory store, cache, S3 meta planning (no network), and binary helpers such as advertise/bind port offset.

Vendored protocol crate:

```bash
cargo test -p iscsi-target --lib
```

## S3 integration test

Requires MinIO (or compatible) on `http://127.0.0.1:9000` with bucket `iscsi` and credentials `minioadmin` / `minioadmin` (or adjust the test):

```bash
docker compose up -d minio createbuckets
ISCSI_S3_INTEGRATION=1 cargo test --test s3_integration -- --nocapture
```

## Smoke client example

Rust initiator that logs in and performs a small R/W against a portal:

```bash
cargo run --release --example smoke_client -- 127.0.0.1:3260
```

Inside Compose you can run a one-shot container if you enable a `smoke` service (see [examples — smoke profile](../examples.md#8-compose-with-smoke-client-profile)); the repo’s `docker-compose.yml` may keep that profile commented.

## Full smoke script

```bash
./scripts/smoke-test.sh
```

Typical flow:

1. `cargo test` (lib/bins)
2. `docker compose up --build` MinIO + iscsi-s3
3. Wait until healthy / listening
4. `ISCSI_S3_INTEGRATION=1` S3 test
5. In-network smoke (when Compose smoke service is available)
6. List chunk objects via `mc`

If smoke fails, collect:

```bash
docker compose ps
docker compose logs --tail=200 iscsi-s3
```

## Manual initiator checks

Use open-iscsi as in [user getting started](../users/getting-started.md). For login debugging, set:

```bash
RUST_LOG=info,iscsi_s3=debug,iscsi_target=debug
```

Watch for login transit, FullFeaturePhase digests, and SCSI probes (INQUIRY, REPORT_LUNS, READ_CAPACITY).
