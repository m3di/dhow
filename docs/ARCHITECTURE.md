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
| `dhow-cli` | `dhow add <url>`, `dhow ls`, `dhow pause/resume/cancel/rm`, `dhow watch` (TUI-less, prints events as JSON lines). Reference client for conformance tests. No daemon: the process that runs a download is the engine for it (see below). | `clap` |
| `dhow-testserver` | Local HTTP/1.1 server (HTTP/2 later) with knobs: drop connection after N bytes, lie about `Content-Length`, change ETag mid-download, ignore `Range`, return 200 to `If-Range`, throttle, inject garbage into a range response, slow headers. Knobs are added as the tests needing them land. | `tokio`; HTTP/1.1 hand-rolled so it can break the protocol on purpose; HTTP/2 when a test needs it |

The core must build for: `aarch64/x86_64-apple-darwin`, `aarch64-apple-ios` (+sim), `x86_64/aarch64-unknown-linux-gnu`, `x86_64/aarch64-pc-windows-msvc`, `aarch64/armv7/x86_64-linux-android`. Per-PR CI runs tests on Linux only; other targets are built locally and in the (later) release workflow.

**CLI process model.** `dhow add` probes, records the download in the database and runs it in the foreground; `--queue`/`--paused` only record it. `dhow resume <id>` runs one in the foreground, `dhow watch` runs queued downloads concurrently, as many per queue as its `max_concurrent` allows (re-checked whenever one finishes), and exits when none are left. Downloads `watch` runs share one connection budget (ENGINE-SPEC §10). `dhow queue add|ls|set|rm`, `dhow move` and `dhow reorder` manage queues; `dhow network` prints `NetworkInfo`. A process running a download holds an exclusive lock on `<data>/locks/<id>.lock` (`dhow_core::runlock`) holding its PID, so two processes never run the same download and a `kill -9` never leaves a stale lock; an app running downloads in process (§9) writes no PID, and `dhow pause`/`cancel` refuse its downloads. `dhow pause`/`cancel`/`rm` on a running download record the request in the database and send the process running it `SIGUSR1` ("re-check"); that process stops just that download (others it runs continue). Runners also register in `<data>/runners/`, and commands that change queues (`queue add|set|rm|start|stop`, `add --queue`, `move`, `reorder`) or limits (`limit`, `settings`, `queue add|set --limit`) poke them the same way, so an idle `watch` reacts at once instead of at its next schedule edge and a running download gets its new speed limit at once (a runner re-reads the stored limits on every wake). Every runner installs the `SIGUSR1` handler before taking any lock. `watch` stays alive while queued work waits for a future schedule edge, and exits otherwise. Ctrl-C/`SIGTERM` stop a runner: `add`/`resume` leave their download Paused, `watch` puts its downloads back in their queues. Everything printed to stdout is one JSON object per line. The CLI uses `dhow_core::engine`, the M1 slice of the `Dhow` object: database-backed state, probe → download → completion gate, no scheduler yet.

## 3. Command / event API

This is the whole surface a shell sees. Keep it small. Everything is a plain data type (serde-serializable, UniFFI-compatible: no lifetimes, no generics, no trait objects across the boundary).

### 3.1 Commands (shell → core)

```rust
pub enum Command {
    // Lifecycle
    Probe { url: Url, headers: Vec<Header>, target_fs: Option<TargetFs> }, // async call → ProbeResult (no event); records and starts nothing
    Add   { request: AddRequest },                        // → DownloadAdded; may start immediately per queue rules
    Pause { id: DownloadId },
    Resume { id: DownloadId },
    Cancel { id: DownloadId, delete_partial: bool },
    Remove { id: DownloadId, delete_files: bool },        // any state; a running one is stopped first → Removed
    Retry  { id: DownloadId },                            // Failed → Queued, keeps journal; others unchanged
    SetSpeedLimit { id: DownloadId, limit: Option<u64> }, // bytes/s; Some(0) = paused by limit (ENGINE-SPEC §12)
    SetConnections { id: DownloadId, connections: Option<u8> }, // §4.1 override; a running download follows at once
    WatchRanges { id: DownloadId, on: bool },             // RangeMap events (§3.3)

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
    pub blind: bool,                     // "Download anyway": a failed probe adds it blind (ENGINE-SPEC §2.4); default false
}
```

`Probe` returns what `Add` would learn (ENGINE-SPEC §2.2): `suggested_name` for `target_fs` (as the `Add` will have it; default the host's) following the naming settings, without collisions, `total_size`, `resumable`, the name's `category`, the validators. `Remove` forgets the row, its ranges and speed samples, and tidies its group (§13.2); `delete_files` also deletes the file of a completed download: the one its token resolves to, wherever it is (a file put at `final_path` after the download's moved away is left alone; one the adapter reports gone deletes nothing; an adapter that fails makes `Remove` fail, removing nothing), or with no token (platforms without them, §6) `final_path` only if it has the download's size; or the part file and sidecar of any other download; nothing on disk without the flag. A shell that prefers the Trash removes a completed file itself and calls `Remove` without `delete_files`. `SetConnections` stores the override; a running download raises its limit and resumes ramp-up (§4.4), or stops its newest connections at once, their rest going back to pending work as for a newcomer (§10.3). `WatchRanges` on sends one `RangeMap` at once, from the sidecar, then (while it runs) at most one a second and one from the sidecar when the run ends; off, or with no run, nothing runs for it.

### 3.2 Events (core → shell)

Delivered on a single ordered channel. Shells subscribe once. The core never blocks on a slow subscriber; events are coalesced per `§3.3`.

```rust
pub enum Event {
    DownloadAdded { snapshot: DownloadSnapshot },            // by Add, or by another process (seen at list/recheck)
    Removed { id: DownloadId },                              // by Remove, or by another process (likewise)
    StateChanged { id: DownloadId, from: State, to: State, reason: Option<FailReason> },
    Progress { id: DownloadId, downloaded: u64, total: Option<u64>, connections: u32, speed_bps: u64, eta_secs: Option<u64> },
    Reset { id: DownloadId, reason: ResetReason },           // started over from zero (I4, §5.4)
    Retrying { id: DownloadId, failures: u32, delay_ms: u64 }, // "Reconnecting (n)" (§9)
    Renamed { id: DownloadId, path: PathBuf },               // final rename done, or file moved (see §6)
    Warning { id: Option<DownloadId>, kind: WarningKind },   // NotResumable, SizeUnverified, IdentityWeak, ServerIgnoredRange
    GroupChanged { id: DownloadId, group: Option<GroupSnapshot> },
    SettingsChanged { settings: Settings },
    RangeMap { id: DownloadId, map: RangeMap },              // segment map, while watched; throttled
    QueueChanged { snapshot: QueueSnapshot },
    QueueRemoved { id: QueueId },
    NetworkChanged { info: NetworkInfo },
}
```

`State`: `Queued | Waiting | Active | Paused | Completing | Completed | Failed | Cancelled`; `Waiting` is the connection budget only (ENGINE-SPEC §10.3). The other reasons a row's status text names come from data the shell has: a `Queued` download waits for its queue's concurrency (`QueueSnapshot.running` = `max_concurrent`) or its schedule (`active_now`, `next_edge`); "Reconnecting (n)" is the last `Retrying` until the next `Progress`. `RangeMap { total, done, connections: [{ start, at, end }], pending }` covers the file once: `done` is committed (from the sidecar) or, while running, written and committed within the commit interval; each connection has received `[start, at)` of its chunk.

`QueueSnapshot` counts `unfinished` (not completed or cancelled), `queued` and `running` (`Active`, `Waiting`, `Completing`); `queued + running` reaching 0 is the queue running dry, when the shell carries out `on_complete`. `QueueChanged` comes after a queue is created, edited, started, stopped or put back on its schedule, after any event that changes its counts (a download added, removed, moved, or crossing between queued, running, finished and the rest), at a schedule edge the engine woke for (only while work waits, §7; otherwise `next_edge` says when the shell's own snapshot goes stale), and for every queue on `recheck`. An override that has ended reads as `Auto`.

The variant lists are part of the contract: Swift (a non-evolution module) and Kotlin make shells' `switch`/`when` exhaustive, so a new variant breaks every shell and comes only with a deliberate break.

### 3.3 Event rate

- `Progress` at most every 250 ms per download, and immediately on state change.
- `RangeMap` at most every 1 s per download, only while a subscriber has opted in (`Command::WatchRanges { id, on: bool }`); the ticks live in the download's run and die with it.
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

- `downloads` — id (UUIDv7), url, headers (json), final_path, part_path, state, fail_reason, queue_id, position, group_id, group_pinned, category, created_at, started_at, completed_at, total_size, etag, last_modified, digest, resumable, size_unverified, identity_weak, connections, speed_limit, file_token (blob), blind (added in blind mode, ENGINE-SPEC §2.4)
- `ranges` — download_id, start, end, state (`pending|active|committed`) — **mirror** of the sidecar journal, for queries only; the sidecar is authoritative (§5.2)
- `queues` — id, name, max_concurrent, schedule (json), speed_limit, on_complete, is_default. The default queue is created by migration 1 with a fixed id and a NULL name (the shell localizes it); `UpdateQueue` with an empty name gives it back its NULL name (other queues need one); a trigger prevents deleting it.
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

`ProxyConfig { http, https, all: Option<Url>, no_proxy: Vec<String>, pac: Option<Url>, exclude_simple_hosts: bool }`: a proxy per request scheme (`all` for any scheme without its own, e.g. SOCKS), hosts that go direct, whether names without a dot also go direct (macOS's "Exclude simple hostnames"; the client then routes through one `Proxy::custom` that applies it with `no_proxy`), and an auto-config script (reported, not evaluated). All `None` is direct. Its `Debug` and the CLI show proxy URLs with userinfo as `***` and without query or fragment (§5.3). `interface_kind` is the interface of the default route. `Engine::network_info()` asks the adapter on every call and fills in `proxy_in_use` itself: the adapter doesn't know what the HTTP client does. `proxy_in_use` is what the current client was built for or, with none built, what the next would be: `System` only when the system names an explicit proxy, else `None`.

Shell adapters:
- Apple: `NWPathMonitor` (the default route's interface type; a tunnel — `.other` named `utun`, `ipsec`, `ppp`, `tun` or `tap` — among the path's interfaces is a VPN, and the kind when it is the default route), `CFNetworkCopySystemProxySettings` (HTTP and HTTPS proxies as `http://host:port`, SOCKS as `all` `socks5h://` (names resolve at the proxy, as CFNetwork does), the exceptions list as `no_proxy`, "exclude simple hostnames" as `exclude_simple_hosts`, the PAC URL when enabled, a field that can't make a URL skipped; credentials live in the keychain, never in the config). The macOS monitor calls `network_changed()` from its own queue on each change, never from inside an adapter method. Metered and Low Data Mode aren't in `NetworkInfo`; the macOS shell shows them from the monitor.
- Windows: `WinHttpGetIEProxyConfigForCurrentUser`, `GetAdaptersAddresses` (type `IF_TYPE_PPP`/`TUNNEL` for VPN heuristics), `NetworkListManager` for connectivity.
- Linux: env first (`http_proxy`, `https_proxy`, `all_proxy`, `no_proxy`; lowercase before uppercase), then the desktop's setting, chosen by `XDG_CURRENT_DESKTOP`: KDE `kioslaverc`; GNOME, Unity, Cinnamon, Pantheon or Budgie `gsettings list-recursively org.gnome.system.proxy` (a subprocess killed after 1 s; the GTK shell injects a `gio::Settings` reader instead); any other desktop or none, the environment only. Queries block, so async callers (M3.4's client rebuild) use `spawn_blocking`. VPN hints: an up `tun*`/`tap*`/`wg*`/`ppp*` interface or one of type `ARPHRD_NONE`/`ARPHRD_PPP` in `/sys/class/net` (PPP and tap can also be DSL or VMs); kind from `wireless`/`phy80211` (Wi-Fi), `wwan*`/`ARPHRD_RAWIP`/`DEVTYPE=wwan` (cellular), `ARPHRD_ETHER` with a `device` (Ethernet), for the IPv4 default route's interface in `/proc/net/route`. No `watch_network` yet (needs netlink or NetworkManager signals).
- Android: `ConnectivityManager` + `NetworkCapabilities.TRANSPORT_VPN`, `ProxyInfo`.

Core settings: `proxy_mode: System | None | Custom(ProxyConfig)`, default `System`, stored in `settings` (`SettingsPatch::proxy_mode`). `Custom` takes `http`, `https`, `socks5` and `socks5h` proxies and no PAC. The client never reads proxies itself (`.no_proxy()` plus the configured ones); credentials go in the proxy URL's userinfo (HTTP basic auth or SOCKS5 username/password). `no_proxy` entries `*.example.com` are passed as `.example.com` (reqwest knows only that form and `*`). Every lease (a probe, a download run) reads the mode and, in `System`, asks `system_proxy(url)` through `spawn_blocking`; when the answer differs from what the current client was built for, a new client is built and later leases get it, while running downloads finish on the old one. So a mode or system change applies from the next download, without polling, even when made by another process. The slot keeps one current route, so per-URL answers that alternate (PAC, once an adapter evaluates it) would rebuild the client per lease. An adapter that fails (`Err(Failed)`: gsettings timed out, `kioslaverc` unreadable) is not taken as direct: the engine keeps its last answer for the process's life, and only with none goes direct with a warning. PAC is not evaluated: a system with only a PAC script is direct, and a warning is logged when such a client is built. GNOME's `authentication-user`/`-password` keys are not read. Tests drive HTTP proxies with a test server answering absolute-form requests (HTTPS through a proxy only up to its `CONNECT`) and SOCKS with the test server's SOCKS5 relay. Informational only: Dhow never changes OS settings.

## 9. FFI notes

- UniFFI proc-macro style (`#[uniffi::export]`), not UDL. Async methods exposed as async; events via a callback interface (`EventListener`), one listener per `Dhow`.
- `dhow-ffi` exports one object, `Engine` (the `Dhow` of §1): `Engine.open(data_dir, EngineConfig, listener, adapter?)`. `EngineConfig` holds only the engine knobs of §3.1 (ramp-up window and gain, steal gain, saturation TTL, minimum chunk), each optional; test hooks (`WriterConfig::slow_sync`, retry base) are not exposed. Free functions: `init_logging(dir, filter?)` (§5.3, once per process), `default_data_dir()`, `default_log_dir()`, `core_version()`.
- Records and enums mirror the core types as plain data: ids, paths and URLs are strings, durations milliseconds, times Unix ms, file tokens bytes. Names SwiftUI also uses are prefixed (`DownloadState`, `DhowSettings`, `ScheduleWindow`, `ScheduleAction`); the core's `ProxyMode::None` and `BackgroundCapability::None` are `Direct` and `SystemOnly`, since `.none` is `Optional`'s in Swift. Errors are one enum (`DhowError`) whose messages are diagnostics, not display text (the Swift bindings omit `LocalizedError`); `InvalidSetting` also carries the refused setting's `key` (§5.1), so a shell can word the refusal at that field.
- The engine owns its tokio runtime (min(4, cores) workers, §7). `async` calls (`probe`, `add`, `pause`, `cancel`, `remove`, `shutdown`) run on it whatever executor polls them; dropping the call does not stop the command (a dropped `add` still records the download and emits `DownloadAdded`). The other calls are synchronous (§3.1) and run on the caller's thread. Of §3.1's commands, `retry`, `set_connections` and `watch_ranges` are synchronous; `remove` stops a running download as `cancel` does, under its run lock. `network_changed()` is for the shell's network monitor (M4.3): it asks the adapter and emits `NetworkChanged`.
- **Another process's changes.** The CLI doesn't poke the app (below), so a download it adds or removes is announced (`DownloadAdded`, `Removed`, with its queue's `QueueChanged`) the next time the engine reads the list: at `list()` or `recheck()`, which shells call when the app becomes active as well as on wake. The engine keeps the set of ids the shell has heard of, so each is announced once, whichever path sees it first.
- **In-process runner.** The engine runs downloads itself, applying the core's queue policy (`next_runnable`, `to_stop`, `next_edge`) the way `dhow watch` does: on open (including downloads a dead process left `Active`), after every command that can change what runs, and when a run ends. It is not a task: a pass runs on the thread that changed something; only while a schedule edge can start or stop waiting work does one task sleep until it. `resume` and `add` with `StartMode::Now` start by hand (§11: no schedule applies, the queue's concurrency still counts them); `stop_queue` stops the queue's runs, by-hand ones too; runs the runner stops go back to `Queued`. `pause` and `cancel` stop a running download and return once it has stopped. A `resume` while a stop is under way starts it again once stopped; of several stops, a user's (`pause`) wins over the runner's, so a paused download stays `Paused` through `shutdown`, `stop_queue` or a window's end. A run the engine can't carry out (an engine error, not a transfer failure) is recorded as `Failed(Io)` (ENGINE-SPEC §9 Local), which frees its slot. The sleeper's clock is monotonic and stops while the machine sleeps: shells call `recheck()` on wake and on clock or time-zone changes. `QueueSnapshot` carries `active_now` and `next_edge`, computed by the core's schedule rules, so shells don't redo schedule or DST logic.
- **With the CLI.** Each run takes the run lock of §2 (`dhow_core::runlock`) without a PID, so the CLI and an app never run one download at once; the CLI refuses to pause or cancel a download an app runs (it can't signal one) and the app returns `RunningElsewhere` for one the CLI runs. The app does not poke CLI runners, nor they it.
- **Events** go into a queue a dedicated thread (not a runtime task) hands to the listener in order; a `Progress` or `RangeMap` still queued is replaced by a newer one of the same kind for the same download, looking back past the other of the two kinds (they alternate while watched) but not past any other event of that download (§3.3). A download the shell hasn't heard of (another process added it) gets its `DownloadAdded` before its first event. The core never waits for the listener; the listener may call the engine.
- **The adapter** is a foreign trait (`PlatformAdapter`: file tokens, system proxy, network info, keep-awake, background capability; no `watch_network` yet). Its methods may block and are called on engine threads: file tokens on the thread of the query that needs them (§6), proxy lookups on the blocking pool, `keep_awake` with an engine lock held (it must not call the engine).
- **Shutdown and drop.** `shutdown()` stops every run cleanly (committed, back to `Queued`, so the next engine continues them) and starts nothing more. Dropping the engine without it cancels runs where they are (the journal makes that safe at any instant, ENGINE-SPEC §5) and blocks the dropping thread up to 5 s for file work in flight, so shells call `shutdown` and don't drop it on the main thread; runs left `Active` are resumed by the next engine. The listener and adapter must hold the engine weakly, or the reference cycle keeps it alive.
- The generated Swift and Kotlin contain UniFFI's runtime code (MPL-2.0, compatible with GPL-3.0); the app's credits must list it: `build-app.sh` puts `LICENSE` and a `CREDITS.txt` (from `cargo metadata`: UniFFI with its source link, then every Rust crate dhow-ffi links, with its license) in the app's `Contents/Resources`, and `shells/macos/make-dmg.sh` (M4.5) puts both beside the app in the DMG. That script builds a universal release into its own Rust target dir (`core/target/dmg`, `build-swift.sh` honouring `CARGO_TARGET_DIR`), signs ad hoc (or with `--sign`, hardened runtime and `Dhow.entitlements`; `--notarize` runs `notarytool` and `stapler`), and checks the image: signature, `hdiutil verify`, a launch from the mounted volume, and Gatekeeper, which rejects an ad-hoc signature.
- `core/dhow-ffi/build-swift.sh` builds the static libraries (`cargo rustc --crate-type staticlib`, so `cargo test` never links one), generates the Swift sources with the crate's `uniffi-bindgen` binary (feature `bindgen`) in library mode, and lays out the SwiftPM package `shells/apple/DhowCore` around `DhowCoreFFI.xcframework`, then runs `swift build` and the package's smoke program (`swift run dhow-core-smoke`); generated files are not committed. By default the XCFramework holds one universal macOS archive and is written by the script itself, so Command Line Tools suffice (the macOS app, M4.2, is a SwiftPM package); `--ios` adds the iOS and simulator slices through `xcodebuild -create-xcframework` (needs Xcode); `--debug` is a quick host-only build.
- The macOS app, `shells/macos`, is a SwiftPM package on `../apple/DhowCore` (build that first): `swift build`, `./test.sh` (Swift Testing; passes the Command Line Tools framework paths), `swift format lint -r Sources Tests`, and `./build-app.sh [--debug | --universal]`, which assembles `build/Dhow.app` (Info.plist, bundle id `app.dhow.Dhow`, an `iconutil` icon, the strings bundle in `Contents/Resources`, the Services entry with its `en.lproj` from `shells/macos/Bundle`) and signs it ad hoc. It opens the CLI's data directory, or `DHOW_DATA_DIR`, with its platform adapter (`MacAdapter`, M4.3): plain bookmarks as file tokens (security-scoped ones come with the sandbox), a file in the user's Trash (`~/.Trash/`) or its volume's (`<volume>/.Trashes/`) reported gone, a bookmark not found on a volume that isn't mounted `Failed`, a stale bookmark re-created; one `PreventUserIdleSystemSleep` assertion while `keep_awake` is on; `Unrestricted` background.
- C ABI for C#: `dhow_new`, `dhow_command(json) -> json`, `dhow_poll_event(timeout_ms) -> json`, `dhow_free`. JSON on the C boundary is deliberate: WinUI shell is the last to be built and this keeps the surface stable without regenerating bindings. Revisit if profiling shows it matters (it won't for a download manager).
- Linux GTK shell depends on `dhow-core` as a normal crate. It is also where core API ergonomics get tested first.

## 10. What is deliberately not here

- No plugin system, no scripting. (Maybe later; the command API is the extension point.)
- No browser extension in core; when it comes (`ROADMAP M6`) it is a thin native-messaging host that sends `Add` commands.
- No torrent, FTP, SFTP, or LAN transfer in v1. The `engine` module is organized around a `Source` trait so these can come later without touching the scheduler or storage.
