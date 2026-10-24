//! The proxy setting through the CLI (ARCHITECTURE §8). A second test
//! server stands in for the proxy: it answers absolute-form requests itself.

use spec_tests::{Sandbox, TestServer, assert_blob, named};

fn proxied(proxy: &TestServer) -> usize {
    proxy
        .requests()
        .iter()
        .filter(|r| r.target.starts_with("http://"))
        .count()
}

#[test]
fn a_custom_proxy_carries_downloads_and_none_bypasses_it() {
    let origin = TestServer::start().unwrap();
    let proxy = TestServer::start().unwrap();
    let sb = Sandbox::new();

    let out = sb.dhow(["settings"]);
    out.assert_success();
    assert_eq!(out.events()[0]["proxy"], "system");

    let url = format!("http://u:s3cret@{}", proxy.addr());
    let out = sb.dhow(["settings", "--proxy", &url]);
    out.assert_success();
    let shown = &out.events()[0]["proxy"];
    assert_eq!(shown["all"], format!("http://***@{}/", proxy.addr()));
    assert!(!out.stdout.contains("s3cret"), "{}", out.stdout);

    let path = format!("/blob/100000?seed=1&{}", named("a.bin"));
    sb.add(&origin, &path, &[]).assert_success();
    assert_blob(&sb.dl().join("a.bin"), 1, 100000);
    assert!(origin.requests().is_empty(), "went direct");
    let seen = proxy.requests();
    assert!(proxied(&proxy) > 0, "{seen:?}");
    let target = format!("http://{}/blob/100000", origin.addr());
    assert!(
        seen.iter().all(|r| r.target.starts_with(&target)),
        "{seen:?}"
    );
    // `u:s3cret`, from the URL's userinfo.
    assert!(
        seen.iter()
            .all(|r| r.header("proxy-authorization") == Some("Basic dTpzM2NyZXQ=")),
        "{seen:?}"
    );

    let out = sb.dhow(["settings", "--proxy", "none"]);
    out.assert_success();
    assert_eq!(out.events()[0]["proxy"], "none");
    let before = proxy.requests().len();
    let path = format!("/blob/100000?seed=2&{}", named("b.bin"));
    sb.add(&origin, &path, &[]).assert_success();
    assert_blob(&sb.dl().join("b.bin"), 2, 100000);
    assert!(!origin.requests().is_empty());
    assert_eq!(proxy.requests().len(), before, "went through the proxy");
}

#[test]
fn the_system_proxy_is_used_by_default() {
    let origin = TestServer::start().unwrap();
    let proxy = TestServer::start().unwrap();
    let sb = Sandbox::with_env(&[("http_proxy", &format!("http://{}", proxy.addr()))]);
    let path = format!("/blob/50000?seed=3&{}", named("c.bin"));
    sb.add(&origin, &path, &[]).assert_success();
    assert_blob(&sb.dl().join("c.bin"), 3, 50000);
    assert!(origin.requests().is_empty(), "went direct");
    assert!(proxied(&proxy) > 0);
}

#[test]
fn unusable_proxies_are_rejected() {
    let sb = Sandbox::new();
    for bad in ["socks5://127.0.0.1:1080", "ftp://p.test", "http://"] {
        let out = sb.dhow(["settings", "--proxy", bad]);
        assert!(!out.status.success(), "{bad} accepted");
    }
    assert_eq!(sb.dhow(["settings"]).events()[0]["proxy"], "system");
}
