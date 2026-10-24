//! `dhow network` reports the environment's proxy (ARCHITECTURE §8) on
//! every platform the CLI runs on.

use spec_tests::Sandbox;

#[test]
fn network_shows_the_environment_proxy() {
    let sb = Sandbox::with_env(&[
        ("https_proxy", "http://proxy.test:3128"),
        ("HTTPS_PROXY", "http://ignored.test:1"),
        ("http_proxy", "plain.test:8080"),
        ("all_proxy", ""),
        ("ALL_PROXY", ""),
        ("no_proxy", "localhost,.lan"),
    ]);
    let out = sb.dhow(["network"]);
    out.assert_success();
    let events = out.events();
    assert_eq!(events.len(), 1, "{events:?}");
    let n = &events[0];
    assert_eq!(n["event"], "network");
    let proxy = &n["system_proxy"];
    assert_eq!(proxy["https"], "http://proxy.test:3128/");
    assert_eq!(proxy["http"], "http://plain.test:8080/");
    assert_eq!(proxy["all"], serde_json::Value::Null);
    assert_eq!(proxy["no_proxy"], serde_json::json!(["localhost", ".lan"]));
    // Until M3.4 the client uses no proxy.
    assert_eq!(n["proxy_in_use"], "none");
    assert!(n["vpn_active"].is_boolean());
    let kinds = ["wifi", "ethernet", "cellular", "vpn", "unknown"];
    assert!(kinds.contains(&n["interface"].as_str().unwrap()), "{n}");
}

#[test]
fn network_hides_proxy_credentials() {
    let sb = Sandbox::with_env(&[
        (
            "https_proxy",
            "http://user:pa55@proxy.test:3128/?key=s3cret",
        ),
        ("http_proxy", "http://tok3n@proxy.test:8080"),
    ]);
    let out = sb.dhow(["network"]);
    out.assert_success();
    let proxy = &out.events()[0]["system_proxy"];
    assert_eq!(proxy["https"], "http://***@proxy.test:3128/");
    assert_eq!(proxy["http"], "http://***@proxy.test:8080/");
}
