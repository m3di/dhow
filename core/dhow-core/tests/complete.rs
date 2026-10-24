//! Download → completion gate end to end (ENGINE-SPEC §8, I1, I7, I8).

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use dhow_core::complete::{CompleteError, Target, finalize};
use dhow_core::download::{DownloadConfig, Request, download};
use dhow_core::http;
use dhow_core::probe::DigestAlgorithm;
use dhow_testserver::{TestServer, blob};
use reqwest::Url;
use sha2::{Digest, Sha256, Sha512};

fn knob(name: &str, value: &str) -> String {
    let raw = format!("{name}: {value}");
    let enc: String = raw
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("header={enc}")
}

struct Outcome {
    dir: tempfile::TempDir,
    result: Result<std::path::PathBuf, CompleteError>,
    names: Vec<String>,
}

async fn fetch_and_finalize(server: &TestServer, path: &str) -> Outcome {
    let dir = tempfile::tempdir().unwrap();
    let req = Request {
        url: Url::parse(&server.url(path)).unwrap(),
        headers: vec![],
        part: dir.path().join("dl.part"),
        sidecar: dir.path().join("dl.dhow"),
        connections: None,
        started_at: None,
        limits: vec![],
    };
    let client = http::client().unwrap();
    let done = tokio::time::timeout(
        Duration::from_secs(30),
        download(
            &req,
            &client,
            &DownloadConfig::default(),
            &dhow_core::download::Shared::default(),
            &|_| {},
            &tokio::sync::watch::channel(false).1,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    // I8 needs the digest the probe found.
    let digest = done.info.digest.clone();
    let name = done.info.suggested_name.clone();
    let (part, sidecar, directory) = (req.part.clone(), req.sidecar.clone(), dir.path().to_owned());
    let result = tokio::task::spawn_blocking(move || {
        let target = Target {
            part: &part,
            sidecar: &sidecar,
            directory: &directory,
            name: &name,
        };
        finalize(&target, digest.as_ref(), |_| Ok(()))
    })
    .await
    .unwrap();
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Outcome { dir, result, names }
}

#[tokio::test(flavor = "multi_thread")]
async fn verified_download_lands_at_its_final_name() {
    let server = TestServer::start().unwrap();
    let data = blob::bytes(5, 0..500_000);
    let sha = STANDARD.encode(Sha256::digest(&data));
    let path = format!(
        "/blob/500000?seed=5&{}&{}",
        knob("Content-Digest", &format!("sha-256=:{sha}:")),
        knob("Content-Disposition", "attachment; filename=\"report.bin\""),
    );
    let out = fetch_and_finalize(&server, &path).await;
    let final_path = out.result.unwrap();
    assert_eq!(final_path, out.dir.path().join("report.bin"));
    assert_eq!(out.names, ["report.bin"], "part and sidecar are gone");
    assert_eq!(std::fs::read(final_path).unwrap(), data);
}

/// T-I8.
#[tokio::test(flavor = "multi_thread")]
async fn digest_mismatch_fails_and_renames_nothing() {
    let server = TestServer::start().unwrap();
    let wrong = STANDARD.encode(Sha256::digest(b"something else"));
    let path = format!(
        "/blob/500000?{}&{}",
        knob("Content-Digest", &format!("sha-256=:{wrong}:")),
        knob("Content-Disposition", "attachment; filename=\"report.bin\""),
    );
    let out = fetch_and_finalize(&server, &path).await;
    assert!(matches!(
        out.result,
        Err(CompleteError::DigestMismatch {
            algorithm: DigestAlgorithm::Sha256,
            ..
        })
    ));
    assert_eq!(
        out.names,
        ["dl.dhow", "dl.part"],
        "no file at the final name (I1)"
    );
}

/// A whole-file digest that only arrives on a 206 (`Repr-Digest`) is used.
#[tokio::test(flavor = "multi_thread")]
async fn repr_digest_from_a_range_response_is_checked() {
    let server = TestServer::start().unwrap();
    let data = blob::bytes(0, 0..200_000);
    let sha = STANDARD.encode(Sha512::digest(&data));
    let path = format!(
        "/blob/200000?head=405&{}",
        knob("Repr-Digest", &format!("sha-512=:{sha}:"))
    );
    let out = fetch_and_finalize(&server, &path).await;
    out.result.unwrap();
    assert_eq!(out.names, ["200000"]);
}
