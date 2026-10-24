//! ENGINE-SPEC §2 against dhow-testserver.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use dhow_core::http::{self, Header};
use dhow_core::probe::{DigestAlgorithm, ProbeError, ProbeInfo, ProbeOptions, Resumable, probe};
use dhow_testserver::{LAST_MODIFIED, TestServer};
use reqwest::Url;

async fn run(server: &TestServer, path: &str) -> Result<ProbeInfo, ProbeError> {
    run_with(server, path, &[], ProbeOptions::default()).await
}

async fn run_with(
    server: &TestServer,
    path: &str,
    headers: &[Header],
    opts: ProbeOptions,
) -> Result<ProbeInfo, ProbeError> {
    let url = Url::parse(&server.url(path)).unwrap();
    probe(&http::client().unwrap(), &url, headers, opts).await
}

fn methods(server: &TestServer) -> Vec<String> {
    server.requests().into_iter().map(|r| r.method).collect()
}

/// `header=` knob value, percent-encoded for the query string.
fn header_knob(name: &str, value: &str) -> String {
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

#[tokio::test]
async fn head_alone_settles_a_well_behaved_server() {
    let server = TestServer::start().unwrap();
    let info = run(&server, "/blob/5000?seed=1").await.unwrap();
    assert_eq!(info.total_size, Some(5000));
    assert_eq!(info.resumable, Resumable::Yes);
    assert_eq!(info.status, 200);
    assert_eq!(info.etag.as_deref(), Some("\"1-1388\""));
    assert_eq!(info.last_modified.as_deref(), Some(LAST_MODIFIED));
    assert_eq!(
        info.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(info.effective_url.as_str(), server.url("/blob/5000?seed=1"));
    assert_eq!(methods(&server), ["HEAD"]);
}

#[tokio::test]
async fn sends_identity_encoding_and_user_headers() {
    let server = TestServer::start().unwrap();
    let headers = [Header {
        name: "Cookie".into(),
        value: "s=1".into(),
    }];
    run_with(&server, "/blob/10", &headers, ProbeOptions::default())
        .await
        .unwrap();
    let seen = &server.requests()[0];
    assert_eq!(seen.header("accept-encoding"), Some("identity"));
    assert_eq!(seen.header("cookie"), Some("s=1"));
}

#[tokio::test]
async fn head_rejected_falls_back_to_range_get() {
    for status in [405, 501, 403] {
        let server = TestServer::start().unwrap();
        let info = run(&server, &format!("/blob/5000?head={status}"))
            .await
            .unwrap();
        assert_eq!(info.total_size, Some(5000), "head={status}");
        assert_eq!(info.resumable, Resumable::Yes);
        assert_eq!(info.status, 206);
        assert_eq!(methods(&server), ["HEAD", "GET"]);
        assert_eq!(server.requests()[1].header("range"), Some("bytes=0-0"));
    }
}

#[tokio::test]
async fn head_without_length_falls_back_to_range_get() {
    let server = TestServer::start().unwrap();
    let info = run(&server, "/blob/5000?head_length=omit").await.unwrap();
    assert_eq!(
        (info.total_size, info.resumable),
        (Some(5000), Resumable::Yes)
    );
    assert_eq!(methods(&server), ["HEAD", "GET"]);
}

#[tokio::test]
async fn silent_accept_ranges_is_resolved_by_range_get() {
    let server = TestServer::start().unwrap();
    let info = run(&server, "/blob/5000?ranges=hidden").await.unwrap();
    assert_eq!(
        (info.total_size, info.resumable),
        (Some(5000), Resumable::Yes)
    );
}

#[tokio::test]
async fn silent_accept_ranges_without_range_get_is_unknown() {
    let server = TestServer::start().unwrap();
    let opts = ProbeOptions { range_get: false };
    let info = run_with(&server, "/blob/5000?ranges=hidden", &[], opts)
        .await
        .unwrap();
    assert_eq!(
        (info.total_size, info.resumable),
        (Some(5000), Resumable::Unknown)
    );
    assert_eq!(methods(&server), ["HEAD"]);
}

#[tokio::test]
async fn range_get_disabled_never_sends_get_even_if_head_fails() {
    let server = TestServer::start().unwrap();
    let opts = ProbeOptions { range_get: false };
    let err = run_with(&server, "/blob/10?head=405", &[], opts)
        .await
        .unwrap_err();
    assert_eq!(err, ProbeError::Status(405));
    assert_eq!(methods(&server), ["HEAD"]);
}

#[tokio::test]
async fn accept_ranges_none_is_not_resumable_without_get() {
    let server = TestServer::start().unwrap();
    let info = run(&server, "/blob/5000?ranges=none").await.unwrap();
    assert_eq!(
        (info.total_size, info.resumable),
        (Some(5000), Resumable::No)
    );
    assert_eq!(methods(&server), ["HEAD"]);
}

#[tokio::test]
async fn ignored_range_is_not_resumable() {
    let server = TestServer::start().unwrap();
    let info = run(&server, "/blob/5000?ranges=ignore").await.unwrap();
    assert_eq!(
        (info.total_size, info.resumable),
        (Some(5000), Resumable::No)
    );
    assert_eq!(info.status, 200);
}

#[tokio::test]
async fn no_length_no_ranges_is_blind() {
    let server = TestServer::start().unwrap();
    let info = run(&server, "/blob/5000?length=omit&ranges=ignore")
        .await
        .unwrap();
    assert_eq!((info.total_size, info.resumable), (None, Resumable::No));
}

#[tokio::test]
async fn empty_file_is_size_zero() {
    let server = TestServer::start().unwrap();
    let info = run(&server, "/blob/0?ranges=hidden").await.unwrap();
    assert_eq!(info.total_size, Some(0));
    assert_eq!(info.status, 416);
}

#[tokio::test]
async fn follows_up_to_ten_redirects() {
    let server = TestServer::start().unwrap();
    let info = run(&server, "/blob/100?redirect=10&seed=2").await.unwrap();
    assert_eq!(
        info.effective_url.as_str(),
        server.url("/blob/100?redirect=0&seed=2")
    );
    assert_eq!(info.total_size, Some(100));
    let err = run(&server, "/blob/100?redirect=11").await.unwrap_err();
    assert_eq!(err, ProbeError::TooManyRedirects);
}

#[tokio::test]
async fn http_error_is_reported_when_both_requests_fail() {
    let server = TestServer::start().unwrap();
    assert_eq!(
        run(&server, "/missing").await.unwrap_err(),
        ProbeError::Status(404)
    );
}

#[tokio::test]
async fn refused_connection_is_connect_error() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = Url::parse(&format!("http://127.0.0.1:{port}/blob/1")).unwrap();
    let err = probe(&http::client().unwrap(), &url, &[], ProbeOptions::default()).await;
    assert_eq!(err.unwrap_err(), ProbeError::Connect);
}

#[tokio::test]
async fn rejects_non_http_schemes_and_bad_headers() {
    let client = http::client().unwrap();
    let ftp = Url::parse("ftp://example.com/f").unwrap();
    let err = probe(&client, &ftp, &[], ProbeOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err, ProbeError::UnsupportedScheme("ftp".into()));

    let server = TestServer::start().unwrap();
    let bad = [Header {
        name: "Bad Name".into(),
        value: "v".into(),
    }];
    let err = run_with(&server, "/blob/1", &bad, ProbeOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err, ProbeError::InvalidHeader("Bad Name".into()));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn reports_content_disposition() {
    let server = TestServer::start().unwrap();
    let knob = header_knob("Content-Disposition", "attachment; filename=\"a b.zip\"");
    let info = run(&server, &format!("/blob/10?{knob}")).await.unwrap();
    assert_eq!(
        info.content_disposition.as_deref(),
        Some("attachment; filename=\"a b.zip\"")
    );
}

#[tokio::test]
async fn digest_from_head() {
    let server = TestServer::start().unwrap();
    let sha = STANDARD.encode([9u8; 32]);
    let knob = header_knob("Content-Digest", &format!("sha-256=:{sha}:"));
    let info = run(&server, &format!("/blob/10?{knob}")).await.unwrap();
    let digest = info.digest.unwrap();
    assert_eq!(digest.algorithm, DigestAlgorithm::Sha256);
    assert_eq!(digest.value, [9u8; 32]);
}

#[tokio::test]
async fn partial_content_digest_is_not_mistaken_for_the_file() {
    let server = TestServer::start().unwrap();
    let sha = STANDARD.encode([9u8; 32]);
    // HEAD is rejected, so only the 206 carries the header, and there it
    // describes one byte.
    let knob = header_knob("Content-Digest", &format!("sha-256=:{sha}:"));
    let info = run(&server, &format!("/blob/10?head=405&{knob}"))
        .await
        .unwrap();
    assert_eq!(info.status, 206);
    assert_eq!(info.digest, None);

    let knob = header_knob("Repr-Digest", &format!("sha-256=:{sha}:"));
    let info = run(&server, &format!("/blob/10?head=405&{knob}"))
        .await
        .unwrap();
    assert_eq!(
        info.digest.map(|d| d.algorithm),
        Some(DigestAlgorithm::Sha256)
    );
}
