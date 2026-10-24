//! Conformance test harness (`ENGINE-SPEC §15`).
//!
//! A test starts a [`TestServer`], creates a [`Sandbox`], drives the `dhow`
//! CLI inside it and asserts on the files it leaves behind. The client under
//! test is found via `DHOW_CLI`, or else as the `dhow` binary next to this test
//! executable's target dir (built by `cargo test --workspace`).

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub use dhow_testserver::{TestServer, blob};

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

/// A scratch directory the CLI runs in; removed on drop.
pub struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("create sandbox"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Where the CLI logs during this test, never the user's real log dir.
    pub fn log_dir(&self) -> PathBuf {
        self.path().join("logs")
    }

    /// Runs `dhow <args>` in the sandbox to completion, killing it after
    /// [`CLI_TIMEOUT`].
    pub fn dhow<I, S>(&self, args: I) -> CliOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut child = Command::new(cli_path())
            .args(args)
            .current_dir(self.path())
            .env("DHOW_LOG_DIR", self.log_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dhow");
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());
        let deadline = Instant::now() + CLI_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait on dhow") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("dhow did not exit within {CLI_TIMEOUT:?}");
            }
            thread::sleep(Duration::from_millis(10));
        };
        CliOutput {
            status,
            stdout: stdout.join().unwrap(),
            stderr: stderr.join().unwrap(),
        }
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
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
