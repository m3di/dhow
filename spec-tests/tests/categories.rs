//! Categories and the storage breakdown through the CLI (ENGINE-SPEC §13.1).

use spec_tests::{Sandbox, TestServer, header_knob, named};

#[test]
fn categories_and_storage_breakdown() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let out = sb.dhow(["settings", "--extensions", "data=.QQQ,mkv"]);
    out.assert_success();
    assert_eq!(
        out.events()[0]["extra_extensions"],
        serde_json::json!({ "data": ["mkv", "qqq"] })
    );
    assert!(
        !sb.dhow(["settings", "--extensions", "films=x"])
            .status
            .success()
    );

    let audio = header_knob("Content-Type", "audio/mpeg");
    for (path, extra) in [
        (format!("/blob/1000?{}", named("Clip.MP4")), vec![]),
        (format!("/blob/2000?{}", named("b.mp4")), vec![]),
        // No extension: the type decides.
        (format!("/blob/300?{audio}"), vec!["-o", "song"]),
        // An extra extension beats the built-in table.
        (format!("/blob/40?{}", named("x.mkv")), vec![]),
        (format!("/blob/5?{}", named("y.qqq")), vec![]),
    ] {
        sb.add(&server, &path, &extra).assert_success();
    }
    sb.add(
        &server,
        &format!("/blob/7?{}", named("later.zip")),
        &["--queue"],
    )
    .assert_success();

    let category = |name: &str| {
        sb.ls()
            .into_iter()
            .find(|r| r["path"].as_str().unwrap().ends_with(name))
            .unwrap()["category"]
            .clone()
    };
    assert_eq!(category("Clip.MP4"), "video");
    assert_eq!(category("song"), "audio");
    assert_eq!(category("x.mkv"), "data");
    assert_eq!(category("y.qqq"), "data");
    assert_eq!(category("later.zip"), "archive");

    std::fs::remove_file(sb.dl().join("b.mp4")).unwrap();
    let missing_id = sb
        .ls()
        .into_iter()
        .find(|r| r["path"].as_str().unwrap().ends_with("b.mp4"))
        .unwrap()["id"]
        .clone();
    let out = sb.dhow(["storage"]);
    out.assert_success();
    assert_eq!(
        out.events(),
        [
            serde_json::json!({
                "event": "storage", "category": "video", "bytes": 1000, "files": 1,
                "missing": [missing_id],
            }),
            serde_json::json!({
                "event": "storage", "category": "audio", "bytes": 300, "files": 1, "missing": [],
            }),
            serde_json::json!({
                "event": "storage", "category": "data", "bytes": 45, "files": 2, "missing": [],
            }),
        ],
        "the queued zip is not counted"
    );
}
