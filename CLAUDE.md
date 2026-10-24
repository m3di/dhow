# CLAUDE.md — agent entry point for Dhow

You are working on **Dhow**, a free and open-source (GPL-3.0) file-movement app.
v1 is a modern download manager: a shared Rust core with a fully native UI shell per platform.

Read these before writing code, in this order:

1. `README.md` — what Dhow is, principles, platform list.
2. `docs/ARCHITECTURE.md` — core/shell split, crate layout, FFI contract, event model, storage.
3. `docs/ENGINE-SPEC.md` — the download engine, chunking, journaling, resume, integrity, politeness. **Normative.** Tests are written against this document.
4. `docs/UI-GUIDELINES.md` — per-platform UI rules. Normative for shells.
5. `docs/ROADMAP.md` — milestones and what is out of scope.
6. `docs/DECISIONS.md` — decision log. Append, never rewrite history.

## Non-negotiables

- **Integrity invariants in `ENGINE-SPEC.md §6` are absolute.** No change may weaken them. If a feature conflicts with them, the feature loses.
- **The core has zero UI opinions.** No strings for display, no colors, no layout, no platform `#[cfg]` UI code. It emits typed events and accepts typed commands.
- **Shells are 100% native.** No webviews, no cross-platform UI toolkits, no Electron/Tauri/Flutter/React Native. Each shell uses the platform's first-party framework (see `UI-GUIDELINES.md §1`).
- **Idle means idle.** With no active transfers, the core must have no running timers, no polling, no busy tasks. Measure it; a test asserts it.
- **Every byte written to disk is accounted for.** Writes go through the journaling layer; nothing writes to a `.part` file directly.
- **Do not borrow code from other download managers.** Borrowing ideas is fine (credit them in `DECISIONS.md`); copying code is not, regardless of license.

## Repo layout

```
dhow/
├── CLAUDE.md, README.md, LICENSE (GPL-3.0)
├── docs/                       # the handoff docs listed above
├── core/                       # Rust workspace (the only shared code)
│   ├── dhow-core/              # engine, queue, storage, naming — pure logic, no FFI
│   ├── dhow-ffi/               # UniFFI bindings (Swift, Kotlin) + C ABI (C#)
│   ├── dhow-cli/               # headless CLI; also the reference client for tests
│   └── dhow-testserver/        # local HTTP server with configurable misbehavior for tests
├── shells/
│   ├── macos/                  # Swift, SwiftUI + AppKit
│   ├── ios/                    # Swift, SwiftUI + UIKit
│   ├── linux/                  # Rust, GTK4 + libadwaita (links dhow-core directly)
│   ├── windows/                # C#, WinUI 3
│   └── android/                # Kotlin, Jetpack Compose (Material 3)
└── spec-tests/                 # black-box conformance tests any shell/CLI must pass
```

## Working conventions

- One PR per milestone item from `ROADMAP.md`, referenced in the PR title (`M1.3: journaled writes`).
- Core changes require tests in `dhow-core` **and**, where behavior is externally visible, a conformance test in `spec-tests/`.
- Run `cargo test --workspace` and `cargo clippy --workspace -- -D warnings` before every commit. Shells run their platform's linter.
- Commit messages: imperative, one line, body explains *why*. Reference the spec section when implementing it (`implements ENGINE-SPEC §7.2`).
- If the spec is ambiguous or wrong, **do not guess silently**. Fix the spec in the same PR, add a line to `DECISIONS.md`, and say so in the PR description.
- Keep `DECISIONS.md` short: date, decision, one-line reason, alternatives rejected.

## Build order (do not skip ahead)

`dhow-core` → `dhow-cli` → `dhow-testserver` + `spec-tests` → `dhow-ffi` → `shells/macos` → `shells/linux` → the rest.
The CLI must be able to download, pause, resume and survive a kill -9 before any shell work starts.
