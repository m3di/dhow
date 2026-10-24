# Dhow Architecture

## 1. The split

```
┌──────────────────────────────────────────────────────────────┐
│  Native shells (one per OS, zero shared UI code)             │
│  macOS/iOS: Swift   Android: Kotlin   Windows: C#   Linux: Rust│
└───────────────┬──────────────────────────────────────────────┘
                │ commands ↓        events ↑
┌───────────────┴──────────────────────────────────────────────┐
│  dhow-ffi  (UniFFI for Swift/Kotlin, C ABI for C#)           │
│  Linux shell links dhow-core directly; no FFI layer          │
└───────────────┬──────────────────────────────────────────────┘
┌───────────────┴──────────────────────────────────────────────┐
│  dhow-core (Rust)                                            │
│  ┌─────────┐ ┌──────────┐ ┌─────────┐ ┌────────┐ ┌────────┐ │
│  │ engine  │ │ scheduler│ │ storage │ │ naming │ │ probe  │ │
│  │ (xfer)  │ │ (queues, │ │ (SQLite,│ │        │ │ (HEAD, │ │
│  │         │ │ politeness│ │ journal)│ │        │ │ ranges)│ │
│  └─────────┘ └──────────┘ └─────────┘ └────────┘ └────────┘ │
│  Platform adapter trait ← implemented by each shell          │
└──────────────────────────────────────────────────────────────┘
```

The core is a library, not a daemon. Each shell instantiates one `Dhow` object in-process, hands it a platform adapter, and talks to it over the command/event API. There is no IPC, no background service. (A headless daemon mode for servers is a possible later addition and must not be designed in now.)

## 2. Crates

| Crate | Purpose | Dependencies of note |
|---|---|---|
| `dhow-core` | Everything that is not UI or FFI | `tokio` (multi-thread runtime, but see §7), `reqwest` (rustls), `rusqlite` (bundled), `serde`, `thiserror`, `tracing` |
| `dhow-ffi` | Generates Swift/Kotlin bindings via `uniffi`; exposes a flat C ABI for C# via `csbindgen` | `uniffi` |
| `dhow-cli` | `dhow add <url>`, `dhow ls`, `dhow pause/resume`, `dhow watch` (TUI-less, prints events). Reference client for conformance tests. | `clap` |
| `dhow-testserver` | Local HTTP/1.1 server (HTTP/2 later) with knobs: drop connection after N bytes, lie about `Content-Length`, change ETag mid-download, ignore `Range`, return 200 to `If-Range`, throttle, inject garbage into a range response, slow headers. Knobs are added as the tests needing them land. | `tokio`; HTTP/1.1 hand-rolled so it can break the protocol on purpose; HTTP/2 when a test needs it |

The core must build for: `aarch64/x86_64-apple-darwin`, `aarch64-apple-ios` (+sim), `x86_64/aarch64-unknown-linux-gnu`, `x86_64/aarch64-pc-windows-msvc`, `aarch64/armv7/x86_64-linux-android`. Per-PR CI runs tests on Linux only; other targets are built locally and in the (later) release workflow.

## 3. Command / event API

This is the whole surface a shell sees. Keep it small. Everything is a plain data type (serde-serializable, UniFFI-compatible: no lifetimes, no generics, no trait objects across the boundary).

### 3.1 Commands (shell → core)

```rust
pub enum Command {
    // Lifecycle
    Probe { url: Url, headers: Vec<Header> },            // → ProbeResult event; does not start anything
    Add   { request: AddRequest },                        // → DownloadAdded; may start immediately per queue rules
    Pause { id: DownloadId },
    Resume { id: DownloadId },
    Cancel { id: DownloadId, delete_partial: bool },
    Remove { id: DownloadId, delete_file: bool },         // only for Completed/Cancelled/Failed
    Retry  { id: DownloadId },                            // Failed → Queued, keeps journal

    // Organisation
    MoveToQueue { id: DownloadId, queue: QueueId },
    Reorder     { id: DownloadId, before: Option<DownloadId> },
    SetGroup    { id: DownloadId, group: Option<GroupId> }, // manual override of auto-grouping
    RenameGroup { group: GroupId, name: String },

    // Queues
    CreateQueue { spec: QueueSpec },
    UpdateQueue { id: QueueId, spec: QueueSpec },
    DeleteQueue { id: QueueId, move_items_to: QueueId },
    StartQueue  { id: QueueId },                          // manual start regardless of schedule
    StopQueue   { id: QueueId },

    // Settings
    SetSettings { patch: SettingsPatch },

    // Queries (synchronous, not events)
    // get_download(id) -> DownloadSnapshot
    // list_downloads(filter) -> Vec<DownloadSnapshot>
    // list_queues() -> Vec<QueueSnapshot>
    // storage_breakdown() -> Vec<(Category, u64)>
    // speed_history(id, window) -> Vec<SpeedSample>
    // network_info() -> NetworkInfo
    // settings() -> Settings
}
```

`AddRequest`:
```rust
pub struct AddRequest {
    pub url: Url,
    pub headers: Vec<Header>,            // cookies, auth, referer — opaque to core
    pub filename: Option<String>,        // user override; else from Probe/naming
    pub directory: PathBuf,              // shell resolves platform defaults
    pub queue: Option<QueueId>,          // default queue if None
    pub start: StartMode,                // Now | Queued | Paused
    pub connections: Option<u8>,         // None = auto
    pub speed_limit: Option<u64>,        // bytes/s, None = unlimited (global limit still applies)
}
```

### 3.2 Events (core → shell)

Delivered on a single ordered channel. Shells subscribe once. The core never blocks on a slow subscriber; events are coalesced per `§3.3`.

```rust
pub enum Event {
    ProbeResult { url: Url, result: Result<ProbeInfo, ProbeError> },
    DownloadAdded { snapshot: DownloadSnapshot },
    StateChanged { id: DownloadId, from: State, to: State, reason: Option<FailReason> },
    Progress { id: DownloadId, downloaded: u64, total: Option<u64>, speed_bps: u64, eta_secs: Option<u64>, active_connections: u8 },
    RangeMap { id: DownloadId, ranges: Vec<RangeStatus> },   // for the segment visualizer; throttled
    Renamed { id: DownloadId, path: PathBuf },               // final rename done, or file moved (see §6)
    GroupChanged { id: DownloadId, group: Option<GroupSnapshot> },
    QueueChanged { snapshot: QueueSnapshot },
    NetworkChanged { info: NetworkInfo },
    SettingsChanged { settings: Settings },
    Warning { id: Option<DownloadId>, kind: WarningKind },   // e.g. NotResumable, ServerIgnoredRange, ProxyUnreachable
}
```

`State`: `Probing | Queued | Waiting(WaitReason) | Active | Paused | Completing | Completed | Failed(FailReason) | Cancelled`.
`WaitReason`: `Schedule | QueueConcurrency | ConnectionBudget | Retry { attempt: u8, at: Instant }`.

### 3.3 Event rate

- `Progress` at most every 250 ms per download, and immediately on state change.
- `RangeMap` at most every 1 s per download, only while a subscriber has opted in (`Command::WatchRanges { id, on: bool }`).
- Everything else: as it happens.
- With no active downloads, **no events are emitted** and no timers exist. (`CLAUDE.md` non-negotiable.)

## 4. Platform adapter

Shells implement this trait (via UniFFI callback interface, or C function table for C#). Every method has a default that does nothing / returns `Unsupported`, so a shell can be minimal.

```rust
pub trait PlatformAdapter: Send + Sync {
    // File identity tracking — see §6
    fn file_token_create(&self, path: &Path) -> Result<Option<FileToken>, AdapterError>;
    fn file_token_resolve(&self, token: &FileToken) -> Result<Option<PathBuf>, AdapterError>;

    // Network — see §8
    fn system_proxy(&self, for_url: &Url) -> Result<ProxyConfig, AdapterError>;
    fn network_info(&self) -> Result<NetworkInfo, AdapterError>;
    fn watch_network(&self, cb: Box<dyn Fn(NetworkInfo) + Send>) -> Result<(), AdapterError>;

    // Power / lifecycle hints
    fn keep_awake(&self, on: bool);          // prevent sleep while transfers are active
    fn background_capability(&self) -> BackgroundCapability; // Unrestricted | TimeLimited | None (iOS)

    // Notifications are the shell's job; core only emits events.
}
```

## 5. Storage

### 5.1 SQLite (`dhow.sqlite` in the platform data dir)

One database per user. WAL mode, `synchronous=NORMAL`, foreign keys on. Schema is versioned with embedded migrations.

Tables (abridged, see `dhow-core/src/storage/schema.sql` for truth):

- `downloads` — id (UUIDv7), url, headers (json), final_path, part_path, state, fail_reason, queue_id, group_id, category, created_at, started_at, completed_at, total_size, etag, last_modified, resumable, connections, speed_limit, file_token (blob)
- `ranges` — download_id, start, end, state (`pending|active|committed`) — **mirror** of the sidecar journal, for queries only; the sidecar is authoritative (§5.2)
- `queues` — id, name, max_concurrent, schedule (json), speed_limit, is_default
- `groups` — id, name, auto (bool)
- `speed_samples` — download_id, at, bytes_per_sec — ring-buffered, 10-minute retention at 1 s, hourly rollups beyond
- `settings` — key, value (json)

### 5.2 Sidecar journal (`<name>.ext.dhow`)

Lives next to the `.part` file. Authoritative record of what has been committed to disk for that download. Format and semantics are in `ENGINE-SPEC.md §5`. A download is fully recoverable from `.part` + `.dhow` alone, even if the database is lost. On completion the sidecar is deleted after the final rename.

Why both: the DB answers "what do I have" cheaply across thousands of downloads; the sidecar makes each download self-describing and keeps the fsync discipline local to the data it protects.

### 5.3 Logs

`tracing` events, written by `dhow_core::logging::init(dir, filter)` as JSON lines to `dhow.<date>.log`, rotated daily, 7 kept. Library code never installs a subscriber; the embedding process (CLI, Linux shell, FFI) calls `init` once. Writes are synchronous so the lines before a crash are on disk.

Default dirs: macOS `~/Library/Logs/Dhow`, Linux `$XDG_STATE_HOME/dhow/logs`, Windows `%LOCALAPPDATA%\Dhow\logs`. iOS/Android shells pass a sandbox path. CLI: `--log-dir`/`DHOW_LOG_DIR`, `--log`/`DHOW_LOG` (EnvFilter syntax, default `warn,dhow=info`).

Request headers (cookies, auth) and URL query strings may carry credentials and MUST NOT be logged.

## 6. Moved-file tracking

Optional per platform via `PlatformAdapter::file_token_*`.

| Platform | Mechanism | Cost |
|---|---|---|
| macOS, iOS | `URL.bookmarkData()` (non-security-scoped on macOS outside sandbox, security-scoped in sandbox) | free |
| Windows | `FILE_ID_INFO` (volume serial + 128-bit file id), re-resolved with `OpenFileById` | free |
| Linux | none in v1 (inode is stable but not resolvable without a scan) | — |
| Android | none (scoped storage) | — |

On `get_download` / `list_downloads`, if `final_path` no longer exists and a token is present, the core asks the adapter to resolve it; on success it updates `final_path` and emits `Renamed`. Resolution happens lazily on query, never on a timer.

## 7. Concurrency & idle model

- One `tokio` multi-thread runtime per `Dhow` instance, worker threads = min(4, cores). Threads park when idle; parked threads cost nothing measurable.
- Each active download is a task tree: one coordinator task + N connection tasks. Pausing cancels the tree; the journal makes that safe at any instant.
- No `interval` timers survive the last active download. Schedule wake-ups use a single `sleep_until(next_schedule_edge)` that is dropped and recreated only when schedules change. If no schedule exists, no sleep exists.
- Speed sampling runs inside the coordinator task; it dies with the download.
- The idle test: after all downloads complete, `tokio::runtime::Handle::metrics().num_alive_tasks()` must be 0 (modulo the event channel) and RSS must not grow over 60 s.

## 8. Network awareness

`NetworkInfo { interface_kind: Wifi|Ethernet|Cellular|Vpn|Unknown, vpn_active: bool, system_proxy: Option<ProxyConfig>, proxy_in_use: ProxyMode }`.

Shell adapters:
- Apple: `NWPathMonitor` (`usesInterfaceType`, `.other` + `utun` for VPN), `CFNetworkCopySystemProxySettings`.
- Windows: `WinHttpGetIEProxyConfigForCurrentUser`, `GetAdaptersAddresses` (type `IF_TYPE_PPP`/`TUNNEL` for VPN heuristics), `NetworkListManager` for connectivity.
- Linux: env (`https_proxy` etc.), GNOME `gsettings org.gnome.system.proxy`, KDE `kioslaverc`; VPN via `tun*`/`wg*` interface presence from `/sys/class/net`.
- Android: `ConnectivityManager` + `NetworkCapabilities.TRANSPORT_VPN`, `ProxyInfo`.

Core settings: `proxy_mode: System | None | Custom(ProxyConfig)`. `reqwest` is rebuilt when the mode or the system proxy changes. Informational only: Dhow never changes OS settings.

## 9. FFI notes

- UniFFI proc-macro style (`#[uniffi::export]`), not UDL. Async methods exposed as async; events via a callback interface (`EventListener`), one listener per `Dhow`.
- C ABI for C#: `dhow_new`, `dhow_command(json) -> json`, `dhow_poll_event(timeout_ms) -> json`, `dhow_free`. JSON on the C boundary is deliberate: WinUI shell is the last to be built and this keeps the surface stable without regenerating bindings. Revisit if profiling shows it matters (it won't for a download manager).
- Linux GTK shell depends on `dhow-core` as a normal crate. It is also where core API ergonomics get tested first.

## 10. What is deliberately not here

- No plugin system, no scripting. (Maybe later; the command API is the extension point.)
- No browser extension in core; when it comes (`ROADMAP M6`) it is a thin native-messaging host that sends `Add` commands.
- No torrent, FTP, SFTP, or LAN transfer in v1. The `engine` module is organized around a `Source` trait so these can come later without touching the scheduler or storage.
