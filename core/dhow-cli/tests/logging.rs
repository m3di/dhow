use std::process::Command;

fn dhow() -> Command {
    Command::new(env!("CARGO_BIN_EXE_dhow"))
}

fn log_text(dir: &std::path::Path) -> String {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
        .collect()
}

#[test]
fn logs_to_dir_from_env() {
    let dir = tempfile::tempdir().unwrap();
    let out = dhow().env("DHOW_LOG_DIR", dir.path()).output().unwrap();
    assert!(out.status.success());
    let line: serde_json::Value =
        serde_json::from_str(log_text(dir.path()).lines().next().unwrap()).unwrap();
    assert_eq!(line["message"], "cli started");
    assert_eq!(line["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(line["target"], "dhow");
}

#[test]
fn flag_overrides_env() {
    let (env_dir, flag_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let out = dhow()
        .env("DHOW_LOG_DIR", env_dir.path())
        .arg("--log-dir")
        .arg(flag_dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(log_text(flag_dir.path()).contains("cli started"));
    assert_eq!(std::fs::read_dir(env_dir.path()).unwrap().count(), 0);
}

#[test]
fn bad_filter_warns_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let out = dhow()
        .env("DHOW_LOG_DIR", dir.path())
        .env("DHOW_LOG", "dhow=loud")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("logging disabled"), "{stderr}");
}
