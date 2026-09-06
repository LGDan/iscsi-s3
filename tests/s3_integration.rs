//! Integration tests against a live MinIO (docker compose).
//! Run with: `ISCSI_S3_INTEGRATION=1 cargo test --test s3_integration -- --nocapture`

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::Client;
use iscsi_s3::metrics::{Metrics, VolumeLabels};
use iscsi_s3::store::{plan_capacity, BlockStore, S3ChunkStore, S3StoreConfig, StoreError, VolumeMeta};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

fn endpoint() -> String {
    std::env::var("ISCSI_S3_TEST_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:9000".into())
}

fn unique_prefix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("itest/{nanos}")
}

async fn make_client() -> Client {
    let conf = aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            "minioadmin",
            "minioadmin",
            None,
            None,
            "itest",
        ))
        .endpoint_url(endpoint())
        .load()
        .await;
    let s3 = aws_sdk_s3::config::Builder::from(&conf)
        .force_path_style(true)
        .build();
    Client::from_conf(s3)
}

#[test]
fn s3_chunk_round_trip_and_grow() {
    if std::env::var("ISCSI_S3_INTEGRATION").is_err() {
        eprintln!("skipping: set ISCSI_S3_INTEGRATION=1");
        return;
    }

    // Dedicated runtime: store methods call Handle::block_on and must not run
    // on a worker thread of that same runtime.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle().clone();
    let client = rt.block_on(make_client());
    let prefix = unique_prefix();

    let metrics = Metrics::new().unwrap();
    let labels = VolumeLabels::new("itest", "iqn.test:itest");

    let store = S3ChunkStore::open(
        client.clone(),
        handle.clone(),
        S3StoreConfig {
            bucket: "iscsi".into(),
            prefix: prefix.clone(),
            capacity: 2 * 1024 * 1024,
            block_size: 512,
            chunk_size: 1024 * 1024,
            compression: iscsi_s3::compression::Compression::None,
            labels: labels.clone(),
            metrics: Arc::clone(&metrics),
        },
    )
    .expect("open");

    let pattern: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    store.write_at(0, &pattern).expect("write");
    let mut buf = vec![0u8; 4096];
    store.read_at(0, &mut buf).expect("read");
    assert_eq!(buf, pattern);

    store.set_capacity(4 * 1024 * 1024).expect("grow");
    assert_eq!(store.capacity(), 4 * 1024 * 1024);

    let err = store.set_capacity(1024 * 1024).unwrap_err();
    assert!(matches!(err, StoreError::ShrinkRefused { .. }));

    let reopened = S3ChunkStore::open(
        client,
        handle,
        S3StoreConfig {
            bucket: "iscsi".into(),
            prefix,
            capacity: 4 * 1024 * 1024,
            block_size: 512,
            chunk_size: 1024 * 1024,
            compression: iscsi_s3::compression::Compression::None,
            labels,
            metrics,
        },
    )
    .expect("reopen");
    assert_eq!(reopened.capacity(), 4 * 1024 * 1024);
    let mut buf2 = vec![0u8; 4096];
    reopened.read_at(0, &mut buf2).expect("reread");
    assert_eq!(buf2, pattern);
}

#[test]
fn plan_capacity_unit() {
    let meta = VolumeMeta {
        version: 1,
        capacity_bytes: 1024,
        chunk_size: 4096,
        block_size: 512,
        compression: iscsi_s3::compression::Compression::None,
    };
    let (cap, write) = plan_capacity(
        Some(&meta),
        2048,
        4096,
        512,
        iscsi_s3::compression::Compression::None,
    )
    .unwrap();
    assert_eq!(cap, 2048);
    assert!(write.is_some());
}
