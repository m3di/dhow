//! Conformance test harness (`ENGINE-SPEC §15`).
//!
//! A test starts a [`TestServer`], creates a [`Sandbox`], drives the `dhow`
//! CLI inside it and asserts on the files it leaves behind and the JSON
//! events it prints. The client under test is found via `DHOW_CLI`, or else
//! as the `dhow` binary next to this test executable's target dir (built by
//! `cargo test --workspace`).

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub use dhow_testserver::{TestServer, blob};
pub use serde_json::Value;

pub const CLI_TIMEOUT: Duration = Duration::from_secs(30);

pub fn cli_path() -> PathBuf {
    if let Some(path) = std::env::var_os("DHOW_CLI") {
        return path.into();
    }
    // <target>/<profile>/deps/<test-exe> → <target>/<profile>/dhow
    let exe = std::env::current_exe().expect("current exe");
    let path = exe
        .parent()
        .and_then(Path::parent)
        .expect("test exe lives in <target>/<profile>/deps")
        .join(format!("dhow{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "{} not found: run `cargo test --workspace` or set DHOW_CLI",
        path.display()
    );
    path
}

/// A scratch directory the CLI runs in, with its own database, logs and
/// downloads folder (`dl/`); removed on drop. Retries are fast (1 ms base)
/// and commits frequent (100 ms), so tests run quickly and interrupted runs
/// have committed state to resume from.
pub struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create sandbox");
        std::fs::create_dir(dir.path().join("dl")).expect("create dl/");
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Where the CLI logs during this test, never the user's real log dir.
    pub fn log_dir(&self) -> PathBuf {
        self.path().join("logs")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.path().join("data")
    }

    /// The downloads folder.
    pub fn dl(&self) -> PathBuf {
        self.path().join("dl")
    }

    /// Sorted names in `dl/`.
    pub fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dl())
            .expect("read dl/")
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(cli_path());
        cmd.args(args)
            .current_dir(self.path())
            .env("DHOW_LOG_DIR", self.log_dir())
            .env("DHOW_DATA_DIR", self.data_dir())
            .env("DHOW_RETRY_BASE_MS", "1")
            // Commit often, so an interrupted run has something on disk.
            .env("DHOW_COMMIT_INTERVAL_MS", "100")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Runs `dhow <args>` in the sandbox to completion, killing it after
    /// [`CLI_TIMEOUT`].
    pub fn dhow<I, S>(&self, args: I) -> CliOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.spawn(args).wait()
    }

    /// Starts `dhow <args>` without waiting for it.
    pub fn spawn<I, S>(&self, args: I) -> Running
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut child = self.command(args).spawn().expect("spawn dhow");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut all = String::new();
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                all.push_str(&line);
                all.push('\n');
                let _ = tx.send(line);
            }
            all
        });
        let stderr = drain(child.stderr.take().unwrap());
        Running {
            child,
            lines,
            seen: Vec::new(),
            stdout: Some(reader),
            stderr: Some(stderr),
        }
    }

    /// `dhow add <url> -d dl/ <extra>` against `server`.
    pub fn add(&self, server: &TestServer, path: &str, extra: &[&str]) -> CliOutput {
        self.dhow(add_args(self, server, path, extra))
    }

    pub fn spawn_add(&self, server: &TestServer, path: &str, extra: &[&str]) -> Running {
        self.spawn(add_args(self, server, path, extra))
    }

    /// The `dhow ls` rows.
    pub fn ls(&self) -> Vec<Value> {
        let out = self.dhow(["ls"]);
        out.assert_success();
        out.events()
    }
}

fn add_args(sb: &Sandbox, server: &TestServer, path: &str, extra: &[&str]) -> Vec<String> {
    let mut args = vec![
        "add".to_owned(),
        server.url(path),
        "-d".to_owned(),
        sb.dl().to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|s| s.to_string()));
    args
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

/// A `dhow` process still running.
pub struct Running {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<Value>,
    stdout: Option<thread::JoinHandle<String>>,
    stderr: Option<thread::JoinHandle<String>>,
}

impl Running {
    /// Waits for an event matching `pred` and returns it. Panics after
    /// [`CLI_TIMEOUT`] or if the process ends first.
    #[track_caller]
    pub fn wait_for(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + CLI_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let v: Value = serde_json::from_str(&line)
                        .unwrap_or_else(|e| panic!("not JSON: {line:?}: {e}"));
                    self.seen.push(v.clone());
                    if pred(&v) {
                        return v;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("no matching event; saw {:#?}", self.seen)
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("dhow exited before a matching event; saw {:#?}", self.seen)
                }
            }
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// SIGKILL on Unix, TerminateProcess on Windows: no chance to clean up.
    pub fn kill9(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Waits for exit, killing after [`CLI_TIMEOUT`].
    pub fn wait(mut self) -> CliOutput {
        let deadline = Instant::now() + CLI_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("wait on dhow") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("dhow did not exit within {CLI_TIMEOUT:?}");
            }
            thread::sleep(Duration::from_millis(10));
        };
        CliOutput {
            status,
            stdout: self.stdout.take().unwrap().join().unwrap(),
            stderr: self.stderr.take().unwrap().join().unwrap(),
        }
    }
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut out = String::new();
        let _ = pipe.read_to_string(&mut out);
        out
    })
}

#[derive(Debug)]
pub struct CliOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    #[track_caller]
    pub fn assert_success(&self) -> &Self {
        assert!(self.status.success(), "dhow failed: {self:#?}");
        self
    }

    /// stdout as JSON events.
    #[track_caller]
    pub fn events(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l:?}: {e}")))
            .collect()
    }

    /// The id from the `added` event.
    #[track_caller]
    pub fn id(&self) -> String {
        self.events()
            .iter()
            .find(|e| e["event"] == "added")
            .and_then(|e| e["id"].as_str())
            .unwrap_or_else(|| panic!("no added event: {self:#?}"))
            .to_owned()
    }

    /// The `to` states of every `state` event, in order.
    pub fn states(&self) -> Vec<String> {
        self.events()
            .iter()
            .filter(|e| e["event"] == "state")
            .map(|e| e["to"].as_str().unwrap().to_owned())
            .collect()
    }

    pub fn has(&self, event: &str, key: &str, value: &str) -> bool {
        self.events()
            .iter()
            .any(|e| e["event"] == event && e[key] == value)
    }
}

/// Asserts `path` is bit-exact with blob `(seed, size)` (`ENGINE-SPEC §15`).
#[track_caller]
pub fn assert_blob(path: &Path, seed: u64, size: u64) {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let len = file.metadata().unwrap().len();
    assert_eq!(len, size, "{} has wrong size", path.display());
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(file).unwrap();
    assert_eq!(
        hasher.finalize(),
        blob::digest(seed, size),
        "{} differs from blob seed={seed} size={size}",
        path.display()
    );
}

/// `header=` knob for a response header, percent-encoded.
pub fn header_knob(name: &str, value: &str) -> String {
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

/// A `Content-Disposition: attachment; filename="..."` knob.
pub fn named(filename: &str) -> String {
    header_knob(
        "Content-Disposition",
        &format!("attachment; filename=\"{filename}\""),
    )
}
