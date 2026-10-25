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
| `dhow-cli` | `dhow add <url>`, `dhow ls`, `dhow pause/resume/cancel`, `dhow watch` (TUI-less, prints events as JSON lines). Reference client for conformance tests. No daemon: the process that runs a download is the engine for it (see below). | `clap` |
| `dhow-testserver` | Local HTTP/1.1 server (HTTP/2 later) with knobs: drop connection after N bytes, lie about `Content-Length`, change ETag mid-download, ignore `Range`, return 200 to `If-Range`, throttle, inject garbage into a range response, slow headers. Knobs are added as the tests needing them land. | `tokio`; HTTP/1.1 hand-rolled so it can break the protocol on purpose; HTTP/2 when a test needs it |

The core must build for: `aarch64/x86_64-apple-darwin`, `aarch64-apple-ios` (+sim), `x86_64/aarch64-unknown-linux-gnu`, `x86_64/aarch64-pc-windows-msvc`, `aarch64/armv7/x86_64-linux-android`. Per-PR CI runs tests on Linux only; other targets are built locally and in the (later) release workflow.

**CLI process model.** `dhow add` probes, records the download in the database and runs it in the foreground; `--queue`/`--paused` only record it. `dhow resume <id>` runs one in the foreground, `dhow watch` runs queued downloads concurrently, as many per queue as its `max_concurrent` allows (re-checked whenever one finishes), and exits when none are left. Downloads `watch` runs share one connection budget (ENGINE-SPEC §10). `dhow queue add|ls|set|rm`, `dhow move` and `dhow reorder` manage queues; `dhow network` prints `NetworkInfo`. A process running a download holds an exclusive lock on `<data>/locks/<id>.lock` holding its PID, so two processes never run the same download and a `kill -9` never leaves a stale lock. `dhow pause`/`cancel` on a running download record the request in the database and send the process running it `SIGUSR1` ("re-check"); that process stops just that download (others it runs continue). Runners also register in `<data>/runners/`, and commands that change queues (`queue add|set|rm|start|stop`, `add --queue`, `move`, `reorder`) or limits (`limit`, `settings`, `queue add|set --limit`) poke them the same way, so an idle `watch` reacts at once instead of at its next schedule edge and a running download gets its new speed limit at once (a runner re-reads the stored limits on every wake). Every runner installs the `SIGUSR1` handler before taking any lock. `watch` stays alive while queued work waits for a future schedule edge, and exits otherwise. Ctrl-C/`SIGTERM` stop a runner: `add`/`resume` leave their download Paused, `watch` puts its downloads back in their queues. Everything printed to stdout is one JSON object per line. The CLI uses `dhow_core::engine`, the M1 slice of the `Dhow` object: database-backed state, probe → download → completion gate, no scheduler yet.

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
    SetSpeedLimit { id: DownloadId, limit: Option<u64> }, // bytes/s; Some(0) = paused by limit (ENGINE-SPEC §12)

    // Organisation
    MoveToQueue { id: DownloadId, queue: QueueId },
    Reorder     { id: DownloadId, before: Option<DownloadId> },
    SetGroup    { id: DownloadId, group: Option<GroupId> }, // manual override of auto-grouping (ENGINE-SPEC §13.2)
    RenameGroup { group: GroupId, name: String },

    // Queues
    CreateQueue { spec: QueueSpec },
    UpdateQueue { id: QueueId, spec: QueueSpec },
    DeleteQueue { id: QueueId, move_items_to: QueueId },
    StartQueue  { id: QueueId },                          // manual start regardless of schedule
    StopQueue   { id: QueueId },

    // Settings
    SetSettings { patch: SettingsPatch },                 // all fields or none; → SettingsChanged

    // Queries (synchronous, not events)
    // get_download(id) -> DownloadSnapshot
    // list_downloads(filter) -> Vec<DownloadSnapshot>
    // list_queues() -> Vec<QueueSnapshot>
    // list_groups() -> Vec<GroupSnapshot>                    // { id, name, auto, members, downloaded, total, speed_bps, eta_secs }
    // storage_breakdown() -> Vec<CategoryUsage>              // { category, bytes, files, missing: Vec<DownloadId> }
    // speed_history(id, window) -> Vec<SpeedSample>
    // network_info() -> NetworkInfo
    // settings() -> Settings
}
```

`Settings` holds every user setting as in effect (the keys of §5.1, with defaults); `SettingsPatch` has each one optional, `None` leaving it as it is. The one nullable setting, `global_speed_limit`, is patched with `LimitPatch { Unlimited, Rate(u64) }`, not `Option<Option<u64>>`, which Kotlin bindings flatten (`T??` is `T?`). `extra_extensions` is patched per category (`Vec<CategoryExtensions { category, extensions }>`, each replacing that category's extras and taking them out of others). The whole patch is validated first (each field's range, and `default_connections` ≤ `max_connections_per_download` after the patch) and stored in one transaction; once `SetSettings` succeeds the patch is stored. Speed limits apply to running downloads at once, connections per download (default and maximum), timeouts, retries and the commit interval from the next download run, the naming toggles, `auto_grouping` and `probe_range_get` from the next `Add` (also `Renamed` for grouping), `proxy_mode` from the next lease (§8), the connection budget (`global_connections`, `per_host_connections`, `min_connections_for_new`) when an engine opens. Another process sees a change at its next read: CLI runners re-read limits when poked (§2). Other `settings.*` knobs of ENGINE-SPEC (ramp-up window and gain, steal gain, saturation TTL, minimum chunk) are engine configuration (`DownloadConfig`), not user settings.

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
    fn file_token_resolve(&self, token: &FileToken) -> Result<Option<Resolved>, AdapterError>;

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

In `dhow_core::platform`. `Engine::open_with(data_dir, config, Arc<dyn PlatformAdapter>)` takes it (`Engine::open` uses `NoopAdapter`). The engine calls `keep_awake(true)` when its first transfer starts and `keep_awake(false)` when its last ends (a guard, so also when a `run` future is dropped or panics), with a lock held (an adapter must not call back into the engine). `FileToken` is opaque bytes; `Resolved` is `{ path, token: Option<FileToken> }`, where `token` replaces the stored one when given (macOS re-creates a stale bookmark this way); `AdapterError` is `Unsupported | Failed(String)`; `BackgroundCapability` defaults to `Unrestricted`.

`dhow_core::platform::linux::LinuxAdapter` is the Linux network half (§8), shared by the CLI and the GTK shell; its inputs (environment, `gsettings` output, `kioslaverc`, `/sys/class/net`, `/proc/net/route`) are fields so tests can fake them. Elsewhere the CLI uses `LinuxAdapter::env_only()` until the platform's own adapter exists.

## 5. Storage

### 5.1 SQLite (`dhow.sqlite` in the platform data dir)

One database per user. WAL mode, `synchronous=NORMAL`, foreign keys on, `STRICT` tables. Schema version is `PRAGMA user_version`; embedded migrations in `dhow-core/src/storage/migrations/` are append-only, and each runs in one transaction with its version bump. A database newer than the running Dhow is refused, never downgraded.

Tables (abridged, the migrations are the truth):

- `downloads` — id (UUIDv7), url, headers (json), final_path, part_path, state, fail_reason, queue_id, position, group_id, group_pinned, category, created_at, started_at, completed_at, total_size, etag, last_modified, digest, resumable, size_unverified, identity_weak, connections, speed_limit, file_token (blob)
- `ranges` — download_id, start, end, state (`pending|active|committed`) — **mirror** of the sidecar journal, for queries only; the sidecar is authoritative (§5.2)
- `queues` — id, name, max_concurrent, schedule (json), speed_limit, on_complete, is_default. The default queue is created by migration 1 with a fixed id and a NULL name (the shell localizes it); a trigger prevents deleting it.
- `groups` — id, name, auto (bool)
- `speed_samples` — download_id (NULL = global aggregate), period_secs, at, bytes_per_sec — ring-buffered, 10-minute retention at 1 s (until the hour ends), hourly rollups beyond (ENGINE-SPEC §14)
- `settings` — key, value (json), one row per setting; no row means the default, an unreadable value is the default with a warning, and keys a newer version wrote are left alone. Keys: `global_speed_limit` (bytes/s or null), `headroom_percent`, `global_connections`, `per_host_connections`, `min_connections_for_new`, `default_connections`, `max_connections_per_download`, `first_byte_timeout_ms`, `idle_timeout_ms`, `max_retries`, `commit_interval_ms`, `probe_range_get`, `use_content_disposition`, `append_type_extension`, `auto_grouping`, `extra_extensions` (category → extensions), `proxy_mode`.

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

The token is created at completion (ENGINE-SPEC §8 step 8, also when completion is finished after a crash, off the async workers in both cases) and stored in `downloads.file_token`. An adapter without tokens (`Unsupported`, `Ok(None)`) stores none; a failure is logged and stores none. Only `Completed` downloads are resolved. The resolve contract:

- `Ok(Some(Resolved { path, token }))`: the file is at `path`. If that exists and differs, it becomes `final_path` (another directory is fine), a refreshed `token` replaces the stored one, the category is derived again if the name changed (ENGINE-SPEC §13.1), the download is regrouped (§13.2), and `Renamed` is emitted.
- `Ok(None)`: gone for good. The core drops the token and never asks again. A file moved to the Trash / Recycle Bin MUST be reported this way (shells implement).
- `Err(Failed)`: can't tell now (e.g. an unmounted volume). Logged at warn, the file counts as missing, and it is asked again on the next query.

The update is conditional on the old `final_path`, so two concurrent queries emit `Renamed` once. The core's `get`, `list` and `storage_breakdown` take the same `on_event` callback as commands (until the event channel exists) for the `Renamed` and `GroupChanged` they may cause.

Queries stat files and may call the adapter synchronously, on the caller's thread and with no engine lock held. Shells must call them off the main thread, and adapters must not hop synchronously to the main thread.

## 7. Concurrency & idle model

- One `tokio` multi-thread runtime per `Dhow` instance, worker threads = min(4, cores). Threads park when idle; parked threads cost nothing measurable.
- Each active download is a task tree: one coordinator task + N connection tasks. Pausing cancels the tree; the journal makes that safe at any instant.
- Each active download also has one writer thread (ENGINE-SPEC I9) fed by a bounded channel: `pwrite`/`fsync` block, and on macOS `F_FULLFSYNC` can take tens of milliseconds, which must not stall runtime workers. The thread waits with a deadline only while uncommitted bytes exist and exits when the download's handles drop.
- No `interval` timers survive the last active download. Schedule wake-ups use a single `sleep_until(next_schedule_edge)` (`Engine::next_edge`) that is dropped and recreated only when schedules change. If no schedule exists, no sleep exists. Everything else that can change what runs (a download finishing, a queue edited, a pause requested) wakes the runner directly; nothing polls.
- Speed sampling runs inside the coordinator task; it dies with the download.
- The HTTP client exists only while something uses it (a probe or a running download) and is dropped with its connection pool when the last user finishes: pooled keep-alive connections and the pool's idle sweep are tasks. Connections are reused while anything runs; the next download after an idle period opens new ones.
- The idle test (T-idle, `dhow-core/tests/idle.rs`): after all downloads complete, `tokio::runtime::Handle::metrics().num_alive_tasks()` must be 0 (modulo the event channel), no event is emitted and RSS must not grow over 60 s.

## 8. Network awareness

`NetworkInfo { interface_kind: Wifi|Ethernet|Cellular|Vpn|Unknown, vpn_active: bool, system_proxy: Option<ProxyConfig>, proxy_in_use: ProxyMode }`.

`ProxyConfig { http, https, all: Option<Url>, no_proxy: Vec<String>, pac: Option<Url> }`: a proxy per request scheme (`all` for any scheme without its own, e.g. SOCKS), hosts that go direct, and an auto-config script (reported, not evaluated). All `None` is direct. Its `Debug` and the CLI show proxy URLs with userinfo as `***` and without query or fragment (§5.3). `interface_kind` is the interface of the default route. `Engine::network_info()` asks the adapter on every call and fills in `proxy_in_use` itself: the adapter doesn't know what the HTTP client does. `proxy_in_use` is what the current client was built for or, with none built, what the next would be: `System` only when the system names an explicit proxy, else `None`.

Shell adapters:
- Apple: `NWPathMonitor` (`usesInterfaceType`, `.other` + `utun` for VPN), `CFNetworkCopySystemProxySettings`.
- Windows: `WinHttpGetIEProxyConfigForCurrentUser`, `GetAdaptersAddresses` (type `IF_TYPE_PPP`/`TUNNEL` for VPN heuristics), `NetworkListManager` for connectivity.
- Linux: env first (`http_proxy`, `https_proxy`, `all_proxy`, `no_proxy`; lowercase before uppercase), then the desktop's setting, chosen by `XDG_CURRENT_DESKTOP`: KDE `kioslaverc`; GNOME, Unity, Cinnamon, Pantheon or Budgie `gsettings list-recursively org.gnome.system.proxy` (a subprocess killed after 1 s; the GTK shell injects a `gio::Settings` reader instead); any other desktop or none, the environment only. Queries block, so async callers (M3.4's client rebuild) use `spawn_blocking`. VPN hints: an up `tun*`/`tap*`/`wg*`/`ppp*` interface or one of type `ARPHRD_NONE`/`ARPHRD_PPP` in `/sys/class/net` (PPP and tap can also be DSL or VMs); kind from `wireless`/`phy80211` (Wi-Fi), `wwan*`/`ARPHRD_RAWIP`/`DEVTYPE=wwan` (cellular), `ARPHRD_ETHER` with a `device` (Ethernet), for the IPv4 default route's interface in `/proc/net/route`. No `watch_network` yet (needs netlink or NetworkManager signals).
- Android: `ConnectivityManager` + `NetworkCapabilities.TRANSPORT_VPN`, `ProxyInfo`.

Core settings: `proxy_mode: System | None | Custom(ProxyConfig)`, default `System`, stored in `settings` (`SettingsPatch::proxy_mode`). `Custom` takes `http`, `https`, `socks5` and `socks5h` proxies and no PAC. The client never reads proxies itself (`.no_proxy()` plus the configured ones); credentials go in the proxy URL's userinfo (HTTP basic auth or SOCKS5 username/password). `no_proxy` entries `*.example.com` are passed as `.example.com` (reqwest knows only that form and `*`). Every lease (a probe, a download run) reads the mode and, in `System`, asks `system_proxy(url)` through `spawn_blocking`; when the answer differs from what the current client was built for, a new client is built and later leases get it, while running downloads finish on the old one. So a mode or system change applies from the next download, without polling, even when made by another process. The slot keeps one current route, so per-URL answers that alternate (PAC, once an adapter evaluates it) would rebuild the client per lease. An adapter that fails (`Err(Failed)`: gsettings timed out, `kioslaverc` unreadable) is not taken as direct: the engine keeps its last answer for the process's life, and only with none goes direct with a warning. PAC is not evaluated: a system with only a PAC script is direct, and a warning is logged when such a client is built. GNOME's `authentication-user`/`-password` keys are not read. Tests drive HTTP proxies with a test server answering absolute-form requests (HTTPS through a proxy only up to its `CONNECT`) and SOCKS with the test server's SOCKS5 relay. Informational only: Dhow never changes OS settings.

## 9. FFI notes

- UniFFI proc-macro style (`#[uniffi::export]`), not UDL. Async methods exposed as async; events via a callback interface (`EventListener`), one listener per `Dhow`.
- C ABI for C#: `dhow_new`, `dhow_command(json) -> json`, `dhow_poll_event(timeout_ms) -> json`, `dhow_free`. JSON on the C boundary is deliberate: WinUI shell is the last to be built and this keeps the surface stable without regenerating bindings. Revisit if profiling shows it matters (it won't for a download manager).
- Linux GTK shell depends on `dhow-core` as a normal crate. It is also where core API ergonomics get tested first.

## 10. What is deliberately not here

- No plugin system, no scripting. (Maybe later; the command API is the extension point.)
- No browser extension in core; when it comes (`ROADMAP M6`) it is a thin native-messaging host that sends `Add` commands.
- No torrent, FTP, SFTP, or LAN transfer in v1. The `engine` module is organized around a `Source` trait so these can come later without touching the scheduler or storage.
