use std::process::Command;

#[test]
fn version_flag_prints_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_dhow"))
        .arg("--version")
        .output()
        .expect("run dhow");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.trim(), format!("dhow {}", env!("CARGO_PKG_VERSION")));
}
