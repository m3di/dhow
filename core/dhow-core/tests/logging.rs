//! Own test binary: the global subscriber can be installed once per process.

use dhow_core::logging::{self, LogError};

#[test]
fn writes_json_lines_logs_panics_and_refuses_second_init() {
    let dir = tempfile::tempdir().unwrap();
    logging::init(dir.path(), "info").unwrap();
    tracing::info!(download = 7, "hello");
    tracing::debug!("filtered out");
    std::thread::spawn(|| panic!("boom")).join().unwrap_err();

    let files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("dhow.") && name.ends_with(".log"),
        "{name}"
    );

    let text = std::fs::read_to_string(&files[0]).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert_eq!(lines[0]["level"], "INFO");
    assert_eq!(lines[0]["message"], "hello");
    assert_eq!(lines[0]["download"], 7);
    assert!(lines[0]["timestamp"].is_string());
    assert_eq!(lines[1]["level"], "ERROR");
    assert!(
        lines[1]["panic"].as_str().unwrap().contains("boom"),
        "{text}"
    );

    assert!(matches!(
        logging::init(dir.path(), "info"),
        Err(LogError::AlreadyInitialized)
    ));
}
