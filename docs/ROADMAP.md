# Dhow Roadmap

Milestones are sequential. A milestone is done when every item is merged with tests and the exit criterion passes. Items are numbered for PR titles (`M1.3: …`).

## M0 — Repo & skeleton

- M0.1 Cargo workspace with `dhow-core`, `dhow-cli`, `dhow-testserver`, `dhow-ffi` (empty), CI matrix building all targets in `ARCHITECTURE §2`.
- M0.2 `spec-tests/` harness: starts `dhow-testserver`, drives `dhow-cli`, asserts on files and CLI event output.
- M0.3 `tracing` setup, structured logs to the platform log dir.
- M0.4 SQLite schema v1 + migrations framework.

Exit: `cargo test --workspace` green on Linux, macOS, Windows CI runners; `dhow-cli --version` runs.

## M1 — Engine core (single download, bulletproof)

- M1.1 Probe (`ENGINE-SPEC §2`) incl. redirects and blind mode.
- M1.2 Naming (`§3`) with the table-driven test suite.
- M1.3 Sidecar journal (`§5`): write, replay, torn-tail handling, compaction.
- M1.4 Writer actor with preallocation, bounded writes, fsync discipline (`§6 I2, I6, I9, I10`).
- M1.5 Single-connection transfer loop with retries (`§7, §9`).
- M1.6 Resume with `If-Range` and overlap verification (`§6 I4, I5`).
- M1.7 Completion gate with digest check and atomic rename (`§8, I7, I8`).
- M1.8 CLI: `add`, `ls`, `pause`, `resume`, `cancel`, `watch`.

Exit: T-I1 through T-I10, T-9a, T-blind, T-3 pass. A `kill -9` at any random point during 100 runs never yields a bad final file.

## M2 — Multi-connection & scheduling

- M2.1 Chunk planning, ramp-up, work stealing (`§4`).
- M2.2 Global budget, per-host cap, oldest-first allocation (`§10`).
- M2.3 Queues, ordering, concurrency (`§10.4, §11`).
- M2.4 Schedules with a single sleep edge (`§11`, `ARCHITECTURE §7`).
- M2.5 Speed limiting at all three levels (`§12`).
- M2.6 Speed sampling, EMA, ETA (`§14`).
- M2.7 Idle guarantee test (T-idle).

Exit: T-4.3, T-9b, T-10, T-11, T-12, T-idle pass. 10 parallel downloads from `dhow-testserver` complete bit-exact with the ramp behaviour visible in CLI output.

## M3 — Organisation & platform adapter

- M3.1 Categories and storage breakdown (`§13.1`).
- M3.2 Auto-grouping (`§13.2`) with manual override.
- M3.3 `PlatformAdapter` trait, no-op default, network info model.
- M3.4 Proxy modes; reqwest client rebuild on change.
- M3.5 File tokens (lazy resolution on query).
- M3.6 Settings persistence and `SettingsPatch`.

Exit: T-13 passes; CLI shows network info on Linux via the env/gsettings adapter.

## M4 — FFI & macOS shell (first native app)

- M4.1 UniFFI bindings, Swift package `DhowCore`, built as an XCFramework for macOS + iOS targets.
- M4.2 macOS shell: main window, list, add dialog, detail pane, settings, notifications, Dock integration (`UI-GUIDELINES §3–8`).
- M4.3 macOS adapter: bookmarks, `NWPathMonitor`, system proxy, keep-awake.
- M4.4 Persian localization + RTL.
- M4.5 Signed, notarized DMG via CI.

Exit: a tester can use Dhow on macOS daily for a week without opening the CLI.

## M5 — Linux shell

- M5.1 GTK4/libadwaita app linking `dhow-core` directly.
- M5.2 Linux adapter: env/gsettings/kioslaverc proxy, `/sys/class/net` VPN heuristic, portal background.
- M5.3 Flatpak manifest, Flathub submission prep.

Exit: feature parity with macOS shell per `UI-GUIDELINES §2–7`.

## M6 — Windows shell

- M6.1 C ABI in `dhow-ffi`, `csbindgen` output, NuGet-style local package.
- M6.2 WinUI 3 shell + adapter (`FILE_ID_INFO`, WinHTTP proxy, metered networks).
- M6.3 MSIX + unpackaged installer.

## M7 — Android shell

- M7.1 Kotlin bindings via UniFFI, AAR with per-ABI `.so`.
- M7.2 Compose shell, foreground service, SAF, share target.
- M7.3 Metered/VPN via `ConnectivityManager`.

## M8 — iOS shell

- M8.1 SwiftUI shell reusing macOS view models where the two genuinely overlap (allowed: they are both Swift and both Apple; forbidden: making either less native to share more).
- M8.2 Background strategy per `UI-GUIDELINES §8`, Live Activity, share extension.

## M9 — Glue (post-v1)

- Browser extensions (Chrome/Firefox/Safari) via native messaging host → `Add`.
- Clipboard watching (opt-in).
- `dhow://` and `magnet:`-style URL handler (magnet reserved for the future torrent `Source`).

## Later horizons (not scheduled)

- Device-to-device transfer (LAN discovery, direct connection, cable). Introduces a second `Source`/`Sink` pair and a "Devices" sidebar section. The name was chosen for this.
- Torrent / HTTP mirrors (multi-source for one file).
- Headless daemon + remote control for servers.
- KDE/Qt shell if a maintainer appears.

## Explicitly out of scope for v1

- Video-site extraction (`yt-dlp`-style). Not a download manager concern; integrations can come as a separate project.
- Accounts, sync, cloud anything.
- Telemetry of any kind.
