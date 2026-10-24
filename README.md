# Dhow

**Move files between any two shores.**

Dhow is a free, open-source file-movement app. Today it is a modern download manager: multi-connection downloads that are *structurally incapable* of producing a corrupt file, with a light, native UI on every platform. Tomorrow it carries files between your own devices too, over the network, over a cable, over whatever is there.

A *dhow* is the cargo sailing vessel of the Persian Gulf and Indian Ocean. For over a thousand years it carried goods between any two coasts. Pronounced "dow", rhymes with "how".

## Principles

1. **Arrives whole or doesn't arrive.** A file that reaches its final name is byte-for-byte what the server sent. Crashes, power loss, changed files on the server, broken proxies: none of them can produce a silent corruption. See `docs/ENGINE-SPEC.md §6`.
2. **Native everywhere.** macOS, iOS, Windows, Linux and Android each get an app built with that platform's own UI framework and design language. No webviews, no cross-platform toolkits. The engine is shared; the experience is not.
3. **Near-zero cost when idle.** No background polling, no resident helpers, no RAM growth. If nothing is transferring, Dhow is asleep.
4. **Polite by default.** Many downloads at once share the connection budget fairly and never hammer a single server.
5. **Free and open.** GPL-3.0. No telemetry, no accounts, no upsell.

## Platforms

| Platform | UI | Status |
|---|---|---|
| macOS | SwiftUI + AppKit | first |
| Linux | GTK4 + libadwaita | second |
| Windows | WinUI 3 | planned |
| Android | Jetpack Compose (Material 3) | planned |
| iOS | SwiftUI + UIKit | planned |

All shells sit on `dhow-core`, a Rust library that owns downloading, queuing, scheduling, persistence and naming.

## v1 features

- Multi-connection, chunked downloads with journaled writes and atomic completion
- Pause/resume, with up-front detection of servers that don't support it
- Download queues with scheduled start/stop windows
- Fair scheduling: older downloads finish first, new ones ramp up as budget frees
- Per-download and global speed limits
- Smart file naming from `Content-Disposition`, URL and `Content-Type`
- Automatic grouping of related files (`part1`, `part2`, …)
- Categorization by file type, with a storage breakdown by category
- System proxy and VPN awareness, with an option to use custom connection settings
- Speed history per download
- Moved-file tracking on platforms that provide it natively (macOS, Windows)

## Status

Pre-alpha. The engine and CLI come first; see `docs/ROADMAP.md`. Done so far (M0–M2): bulletproof single and multi-connection downloads (journal, resume, kill -9 safe), the connection budget and ramp-up, queues, schedules, speed limits, speed history. No GUI yet.

### Try the CLI

```sh
cd core && cargo build --release
./target/release/dhow add https://example.com/file.zip -d ~/Downloads   # runs until done; Ctrl-C pauses
./target/release/dhow ls                                                # one JSON object per download
./target/release/dhow resume <id-or-prefix>
```

`add --queue` only records a download; `dhow watch` runs everything queued. `dhow queue …` manages queues and their schedules (`--window "mon-fri 22:00-06:00"`), `dhow limit <id> 500K` and `dhow settings --global-limit 2M` set speed limits, `dhow history` shows speed samples. Output is one JSON event per line.

## Contributing

Start with `CLAUDE.md` (it is written for AI agents but is the fastest orientation for humans too), then `docs/ARCHITECTURE.md`. Engine work must satisfy `docs/ENGINE-SPEC.md` and the conformance tests in `spec-tests/`.

## License

GPL-3.0-or-later. See `LICENSE`.
