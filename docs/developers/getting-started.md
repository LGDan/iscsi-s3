# Developer getting started

## Prerequisites

- Rust stable (edition 2021; recent stable recommended)
- Docker (for MinIO / Compose smoke)
- Optional: `open-iscsi` on a Linux host for end-to-end initiator tests

## Clone and build

```bash
git clone <repo-url> iscsi-http
cd iscsi-http
cargo build --release --bin iscsi-s3
cargo build --release --example smoke_client
```

Run locally against Compose MinIO:

```bash
docker compose up -d minio createbuckets
cp config.example.toml config.toml
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin
cargo run --release -- --config config.toml --log info,iscsi_s3=debug,iscsi_target=debug
```

## Project layout

```text
src/
  main.rs          # binary: shared-portal IscsiServer, volume open
  lib.rs           # crate root
  config.rs        # figment + clap
  volume.rs        # open volume, S3 client, meta grow rules
  device.rs        # ScsiBlockDevice adapter over BlockStore
  cache.rs         # whole-chunk LRU
  store/
    mod.rs         # BlockStore trait
    memory.rs      # in-memory backend (tests)
    s3.rs          # S3 chunk objects + meta.json
vendor/iscsi-target/   # vendored protocol stack (see VENDOR.md)
examples/smoke_client.rs
tests/s3_integration.rs
scripts/smoke-test.sh
docs/              # this documentation
```

## Vendored `iscsi-target`

Upstream crates.io `iscsi-target` 1.0.0 is vendored under `vendor/iscsi-target` with local fixes documented in `vendor/iscsi-target/VENDOR.md`, including:

- `advertise_addr` for SendTargets behind Docker/NAT
- Login Response ISID on the wire
- `AuthMethod=None` transit into operational negotiation
- `MaxConnections` negotiation / richer logging

Depend via path in root `Cargo.toml`:

```toml
iscsi-target = { path = "vendor/iscsi-target" }
```

When changing protocol behavior, prefer patches in `vendor/` and note them in `VENDOR.md`.

## Logging while developing

```bash
export RUST_LOG=info,iscsi_s3=debug,iscsi_target=debug
```

Or CLI `--log` (overridden if `RUST_LOG` is set). Login and SCSI probe commands log at info; bulk R/W at debug.

## Next

- [Architecture](architecture.md)
- [Testing](testing.md)
- [Examples](../examples.md)
