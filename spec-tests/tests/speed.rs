//! Speed samples, EMA and ETA through the CLI (ENGINE-SPEC §14).

use spec_tests::{Sandbox, TestServer, assert_blob, named};

#[test]
fn progress_reports_speed_and_eta_and_history_keeps_samples() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let path = format!("/blob/1000000?seed=2&throttle=250000&{}", named("s.bin"));
    let out = sb.add(&server, &path, &["-c", "1"]);
    out.assert_success();
    assert_blob(&sb.dl().join("s.bin"), 2, 1_000_000);
    let id = out.id();

    let progress: Vec<_> = out
        .events()
        .into_iter()
        .filter(|e| e["event"] == "progress")
        .collect();
    assert!(
        progress.iter().all(|p| p["speed_bps"].is_u64()),
        "{progress:#?}"
    );
    let with_eta: Vec<_> = progress.iter().filter(|p| p["eta_secs"].is_u64()).collect();
    assert!(!with_eta.is_empty(), "{progress:#?}");
    let first = with_eta[0];
    let speed = first["speed_bps"].as_u64().unwrap();
    assert!((190_000..310_000).contains(&speed), "{first}");
    // Not before three samples (3 s at 250 kB/s, less the probe second).
    assert!(first["downloaded"].as_u64().unwrap() >= 450_000, "{first}");

    let history = sb.dhow(["history", &id]);
    history.assert_success();
    let samples = history.events();
    assert!((3..=6).contains(&samples.len()), "{samples:#?}");
    assert!(
        samples
            .iter()
            .all(|s| s["period_secs"] == 1 && s["id"] == id.as_str())
    );
    let global = sb.dhow(["history"]).events();
    let total = |v: &[spec_tests::Value]| -> u64 {
        v.iter().map(|s| s["bytes_per_sec"].as_u64().unwrap()).sum()
    };
    assert_eq!(total(&global), total(&samples));
    assert!(global.iter().all(|s| s["id"].is_null()));
}
