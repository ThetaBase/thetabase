//! Bring-your-own object storage, against a real S3-compatible store.
//!
//! Runs the *same* contract `backend_contract.rs` runs against a directory
//! backend. That is the point of the contract being shared: "we support S3 too"
//! must mean exactly what it means for every other backend, and the way it
//! comes to mean something weaker is a backend that only ever runs tests
//! written for it.
//!
//! MinIO rather than AWS, for the reason `make kms` uses LocalStack: what is
//! under test is our use of the API, and that needs neither Amazon's bill nor
//! Amazon's uptime. It is also the more honest target — MinIO is
//! *S3-compatible*, so passing against it is evidence for the claim actually
//! being made, where passing against AWS alone would only be evidence for AWS.
//!
//! What this cannot tell us is whether a particular provider's quirks are
//! handled — R2's multipart behaviour, B2's checksum support — which is a
//! per-provider question no single test answers. `S3Archive::integrity`
//! reporting `Unknown` rather than guessing is the design's answer to that.
//!
//! ```text
//! docker run -d --name thetabase-s3 -p 9000:9000 \
//!   -e MINIO_ROOT_USER=thetabase -e MINIO_ROOT_PASSWORD=thetabasesecret \
//!   minio/minio server /data
//! ```
//!
//! `THETA_REQUIRE_LIVE=1` turns a skip into a failure, because a suite that
//! skipped for want of a bucket reports the same green as one that passed.

#![cfg(feature = "s3")]

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::Client;
use theta_archive::s3::{Integrity, S3Archive};
use theta_archive::{ArchiveBackend, StoredRef};

mod contract;
use contract::contract;

const BUCKET: &str = "thetabase-archive-test";

fn endpoint() -> String {
    std::env::var("THETA_ARCHIVE_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:9000".into())
}

fn require_live() -> bool {
    std::env::var("THETA_REQUIRE_LIVE").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// A client pointed at the local store, or `None` to skip.
async fn client_or_skip() -> Option<Client> {
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(endpoint())
        // Path style: MinIO and most self-hosted stores do not do
        // virtual-hosted buckets, which is the first thing that breaks when a
        // customer points this at something that is not AWS.
        .force_path_style(true)
        .credentials_provider(Credentials::new(
            std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "thetabase".into()),
            std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "thetabasesecret".into()),
            None,
            None,
            "thetabase-test",
        ))
        .build();
    let client = Client::from_conf(config);

    match client.create_bucket().bucket(BUCKET).send().await {
        Ok(_) => Some(client),
        // Already there from an earlier run.
        Err(e) if format!("{e:?}").contains("BucketAlreadyOwnedByYou") => Some(client),
        Err(e) if require_live() => {
            panic!(
                "THETA_REQUIRE_LIVE is set and no S3-compatible store at {}: {e}",
                endpoint()
            )
        }
        Err(e) => {
            eprintln!(
                "skipping: no S3-compatible store at {} ({e}).\n  \
                 docker run -d --name thetabase-s3 -p 9000:9000 \\\n    \
                 -e MINIO_ROOT_USER=thetabase -e MINIO_ROOT_PASSWORD=thetabasesecret \\\n    \
                 minio/minio server /data",
                endpoint()
            );
            None
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_s3_backend_satisfies_the_same_contract_as_every_other() {
    // Multi-thread on purpose: `ArchiveBackend` is synchronous and the SDK is
    // not, so the backend bridges with `block_in_place`, which panics on a
    // current-thread runtime. A test that used one would be testing a
    // configuration no deployment runs.
    let Some(client) = client_or_skip().await else {
        return;
    };

    // Each case gets a fresh prefix, so a rerun does not read a previous run's
    // objects and call it a pass.
    let mut run = 0;
    contract("s3", move || {
        run += 1;
        S3Archive::new(client.clone(), BUCKET, format!("contract-{run}"))
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn compression_actually_buys_something_on_log_shaped_data() {
    // Reported rather than asserted against a fixed ratio: what zstd achieves
    // depends on the data, and pinning a number here would pin a claim to
    // whatever this fixture happens to look like.
    //
    // What *is* asserted is the direction. A backend that stored more than it
    // was given would be charging a customer for the privilege.
    let Some(client) = client_or_skip().await else {
        return;
    };
    let mut archive = S3Archive::new(client, BUCKET, "ratio");

    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("000000000000.seg");
    let bytes: Vec<u8> = (0..8_000)
        .flat_map(|n| {
            format!(
                "{{\"commitId\":{n},\"branchId\":0,\"op\":{{\"put\":{{\"key\":\"users:{n}\",\
                 \"value\":\"a fairly ordinary row value\"}}}}}}\n"
            )
            .into_bytes()
        })
        .collect();
    std::fs::write(&source, &bytes).expect("write");

    let stored = archive.store("segments/ratio", &source).expect("store");
    let ratio = stored.stored_bytes as f64 / bytes.len() as f64;
    eprintln!(
        "s3 + zstd: {} bytes to {} ({:.3}x), against AT-1's 0.019 on comparable data",
        bytes.len(),
        stored.stored_bytes,
        ratio
    );

    assert!(
        stored.stored_bytes < bytes.len() as u64,
        "the backend stored more than it was given"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn integrity_says_unknown_rather_than_guessing_when_it_has_nothing_to_compare() {
    // The failure this design is shaped around. An ETag is an MD5 for a
    // single-part upload and not a content hash for a multipart one, so a
    // backend that compared against it would report "intact" for an object
    // nothing had checked.
    let Some(client) = client_or_skip().await else {
        return;
    };
    let archive = S3Archive::new(client, BUCKET, "unknown");

    let stored = StoredRef {
        key: "segments/never-written".into(),
        stored_bytes: 0,
        container_checksum: None,
    };
    assert_eq!(
        archive
            .integrity(&stored)
            .expect("no call is made without a checksum"),
        Integrity::Unknown,
        "an object with no recorded checksum must not read as verified"
    );

    // And `check_integrity` maps Unknown to "no reason to think it is broken",
    // because a check that reports a healthy archive as corrupt is one somebody
    // switches off, taking the real detection with it.
    assert!(archive.check_integrity(&stored).expect("check"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_object_reports_intact_against_the_stores_own_checksum() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let mut archive = S3Archive::new(client, BUCKET, "intact");

    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("000000000000.seg");
    std::fs::write(&source, b"{\"commitId\":1}\n").expect("write");

    let stored = archive.store("segments/intact", &source).expect("store");
    match archive.integrity(&stored).expect("integrity") {
        Integrity::Intact => {}
        // Acceptable: a store that computes no SHA-256 cannot answer, and
        // saying so is the correct behaviour rather than a failure.
        Integrity::Unknown => {
            eprintln!("note: this store returned no SHA-256, so integrity is Unknown")
        }
        Integrity::Corrupt => panic!("a freshly written object reported corrupt"),
    }
}
