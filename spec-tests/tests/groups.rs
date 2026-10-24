//! Automatic grouping and its manual override through the CLI
//! (ENGINE-SPEC §13.2).

use spec_tests::{Sandbox, TestServer};

/// T-13.
#[test]
fn parts_group_and_the_user_overrides() {
    let server = TestServer::start().unwrap();
    let sb = Sandbox::new();
    let a1 = sb.add(&server, "/blob/1000", &["-o", "A.part1.rar"]);
    a1.assert_success();
    let a2 = sb.add(&server, "/blob/2000", &["-o", "A.part2.rar"]);
    a2.assert_success();
    let b = sb.add(&server, "/blob/300", &["-o", "B.rar"]);
    b.assert_success();
    let (a1, b) = (a1.id(), b.id());

    let group = |id: &str| sb.ls().into_iter().find(|r| r["id"] == id).unwrap()["group"].clone();
    let out = sb.dhow(["group", "ls"]);
    out.assert_success();
    let groups = out.events();
    assert_eq!(groups.len(), 1, "{groups:?}");
    let g = &groups[0];
    assert_eq!(g["event"], "group");
    assert_eq!(g["name"], "A");
    assert_eq!(g["auto"], true);
    assert_eq!(g["members"], serde_json::json!([a1, a2.id()]));
    assert_eq!(
        (g["downloaded"].as_u64(), g["total"].as_u64()),
        (Some(3000), Some(3000))
    );
    assert_eq!(group(&a1), g["id"]);
    assert_eq!(group(&b), serde_json::Value::Null, "B ungrouped");
    // Adding the second part told the first.
    assert!(a2.events().iter().any(|e| e["event"] == "group_changed"
        && e["id"] == a1.as_str()
        && e["group"]["id"] == g["id"]));

    // By hand: B joins, by group name; the group is manual now.
    let out = sb.dhow(["group", "set", &b, "A"]);
    out.assert_success();
    assert!(
        out.events()
            .iter()
            .any(|e| e["event"] == "group_changed" && e["id"] == b.as_str())
    );
    let out = sb.dhow(["group", "rename", g["id"].as_str().unwrap(), "Archive"]);
    out.assert_success();
    let renamed = out.events().pop().unwrap();
    assert_eq!(
        (renamed["name"].as_str(), renamed["auto"].as_bool()),
        (Some("Archive"), Some(false))
    );
    assert_eq!(renamed["members"].as_array().unwrap().len(), 3);

    sb.dhow(["group", "set", &b, "none"]).assert_success();
    assert_eq!(group(&b), serde_json::Value::Null);
    assert!(!sb.dhow(["group", "set", &b, "nope"]).status.success());
}
