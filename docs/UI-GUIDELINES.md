# Dhow UI Guidelines

**Status: normative for shells.** The core knows nothing in this document. Everything here is a shell's job.

## 1. Frameworks (no exceptions)

| Platform | Framework | Design language | Minimum OS |
|---|---|---|---|
| macOS | SwiftUI, AppKit where SwiftUI falls short (tables, toolbars, menus) | Apple HIG, macOS | macOS 14 |
| iOS | SwiftUI, UIKit for document interaction | Apple HIG, iOS | iOS 17 |
| Windows | WinUI 3 (Windows App SDK), C# | Fluent 2 | Windows 10 1809+ |
| Linux | GTK4 + libadwaita, Rust (`gtk4-rs`, `libadwaita-rs`) | GNOME HIG | GNOME 46 runtime |
| Android | Jetpack Compose, Material 3 with dynamic color | Material 3 | API 26 |

Each shell is a separate project with its own build system (Xcode project / SwiftPM, Gradle, MSBuild, Meson or Cargo). Nothing is shared between shells except the core binding and these guidelines.

Linux note: libadwaita is GNOME-flavored. KDE users get it too in v1; a Qt/Kirigami shell is a possible later addition if someone wants to own it.

## 2. Shared principles

These hold on every platform; how they look is per-platform.

- **One main window.** The list of downloads is the app. No window-per-download. Detail lives in a pane or sheet attached to the main window (§5).
- **Add is one step.** Paste a URL anywhere in the window, or press the platform's "new" shortcut, and the Add dialog opens pre-filled. The dialog shows the probe result (name, size, resumable or not) *before* the user confirms. Confirming is one click/tap, Enter/Return works.
- **Resumable is a first-class signal.** If the probe says `No`, the Add dialog shows a clear, non-alarming notice ("This server doesn't support pausing. If the download is interrupted it will start over."). Not a modal, not red.
- **Quiet.** No notifications for routine events. One notification per download on completion (user-disableable), one on failure. Groups notify once when the whole group completes.
- **Idle is invisible.** No menu bar / tray resident process by default. When the main window closes with active downloads, the app keeps running and shows a platform-appropriate indicator (Dock badge, taskbar progress, libadwaita background status, Android foreground service notification with progress). With nothing active, closing the window quits on Windows/Linux, hides on macOS (Dock stays), as each platform expects.
- **Respect system appearance.** Light/dark, accent color, reduced motion, reduced transparency, dynamic type / font scaling, high contrast. Never ship a custom theme.
- **Keyboard everything** on desktop. Every list action has a shortcut following the platform's conventions; the list is fully navigable without a mouse.
- **No custom controls where a native one exists.** Progress bars are the platform's progress bar. Tables are the platform's table. The one custom view allowed is the segment map and the speed chart (§6), drawn with the platform's native drawing API.

## 3. Main window anatomy

```
┌──────────────────────────────────────────────────────────┐
│ [toolbar: + Add   ▶ Resume all  ⏸ Pause all   ⚡ 2.3 MB/s ▁▃▅▂]  [search] │
├───────────┬──────────────────────────────────────────────┤
│ Sidebar   │ List                                          │
│  All      │  ▾ Movie.Part (2 of 3)        ████████░░ 1.2 GB│
│  Active   │      Movie.Part1.rar   ✓     ██████████       │
│  Queued   │      Movie.Part2.rar   ↓ 4.1 MB/s ███████░░░ │
│  Done     │      Movie.Part3.rar   ⏳ waiting             │
│  Failed   │    ubuntu-24.04.iso    ↓ 1.8 MB/s ███░░░░░░░ │
│ ──────    │    paper.pdf           ✓                      │
│ Queues    │                                               │
│  Default  │                                               │
│  Night    │                                               │
│ ──────    │                                               │
│ Categories│                                               │
│  Video    │                                               │
│  …        │                                               │
└───────────┴──────────────────────────────────────────────┘
```

- **Sidebar** (desktop): smart filters, then queues, then categories. On narrow windows it collapses to a dropdown/segmented control; on mobile it is a tab bar or top filter chips.
- **Toolbar**: Add, Resume all, Pause all, a compact global speed readout with a tiny sparkline (the only chart on the main view), and search. Per-platform placement rules apply (macOS unified toolbar, Fluent command bar, libadwaita header bar, Material top app bar).
- **Pause all / Resume all** act on queues, so queue order, concurrency and schedules survive them. Pause all stops every queue (`StopQueue`, which also stops downloads started by hand, back to `Queued`) and remembers how each queue ran and which downloads were running; it marks nothing `Paused`. Resume all puts each queue back as it was (on its schedule, or started) and starts again, by hand, the downloads that were running. Pause all then Resume all gives back the state before; downloads the user paused stay paused throughout. A stop lasts until the queue's next schedule edge (ENGINE-SPEC §11). Without a Pause all to undo (the app relaunched), Resume all puts stopped queues back on their schedules. Idle, the speed readout shows nothing.
- **List row**: name, status glyph, progress bar, right-aligned speed/ETA or size. Status text is one short phrase: "4.1 MB/s · 3 min left", "Waiting for 2 downloads to finish", "Paused", "Scheduled for 02:00", "Reconnecting (3)". Groups are collapsible rows with aggregate progress.
- **Selection** opens the detail pane. Multi-select enables batch pause/resume/cancel/move-to-queue. A selected group row stands for the members the list shows, never ones a filter or search hides; its primary action (double-click, Return) expands or collapses it.
- **Context menu / swipe actions**: Pause/Resume, Open, Show in Finder/Explorer/Files, Copy link, Move to queue, Set speed limit, Cancel, Remove, Remove and delete file. Cancel deletes what was downloaded (a cancelled download can't continue), so it asks first, as Remove and delete file does.
- **Empty state**: one line and the Add button. No illustration bigger than an icon.

## 4. Add dialog

Sheet (macOS/iOS), ContentDialog (Windows), AdwDialog (Linux), bottom sheet or full-screen dialog (Android).

Fields, top to bottom: URL (prefilled from clipboard if it parses as http(s)), filename (editable, prefilled from probe), destination folder (platform picker), queue (dropdown, default queue preselected), then an expandable "More" with connections, per-download speed limit, start mode (Now / Add to queue / Paused), and custom headers.

While the probe runs: a small inline spinner on the filename field. On result: name fills in, size appears, resumability notice if `No`. The primary button is disabled until the probe completes or fails; on failure the dialog offers "Download anyway" (blind mode).

Multiple URLs pasted at once → one dialog per URL is wrong; show a compact list with per-row names and one destination.

## 5. Detail pane

Desktop: inspector on the right (macOS `.inspector`), details pane (WinUI), `AdwNavigationSplitView` content (Linux). Mobile: push navigation (iOS), detail screen (Android).

Sections:
1. **Header**: filename, category icon, size, destination path (clickable), queue, group.
2. **Progress**: the segment map, a horizontal strip showing committed/active/pending ranges per connection, native-drawn, updated at ≤1 Hz, only while the pane is visible (shell toggles `WatchRanges`). The watch is on only while the pane shows a running download; one that isn't running gets its sidecar's map once (`WatchRanges` on, then off at once), so nothing stays watched. A finished download shows no map.
3. **Speed chart**: last 10 minutes, native-drawn (Swift Charts, WinUI/Win2D, Cairo, Compose Canvas). Y axis auto-scales; X axis labels every 2 minutes. Reads `speed_history`.
4. **Connection info**: active connections, resumable yes/no, server (host), identity (ETag/Last-Modified) in a disclosure.
5. **Controls**: per-download speed limit slider, connection count stepper (changes apply live via `SetSettings`-style patch on the download), retry/cancel. The slider has fixed steps in decimal kB/s (as sizes are shown) ending in "no limit"; a limit of 0 set elsewhere is a step of its own while it lasts, labelled as the list labels it. It applies once it rests, not on every step of a drag, and a change still resting is sent when the selection moves or the pane closes. A download's limit and connection changes reach the core in the order made. "Retry" is `Retry` (back to `Queued`, its queue starts it); "Retry Now" starts it by hand (`Resume`), as "Start Now" does for a queued one. The pane offers both; the list's context menu offers "Retry Now".
6. **Log**: last 20 engine events for this download (state changes, retries, warnings) with timestamps. Plain text, selectable. Events carry no time, so the shell stamps them on receipt and keeps them in memory: the log covers what happened since the app opened.

With several downloads selected the pane says how many; it shows one download's detail only.

The chart and segment map are the only place these visuals exist. They do not appear in list rows.

## 6. Storage view

Reachable from the sidebar footer ("Storage") or settings. A single stacked horizontal bar of completed downloads by category, color-coded with a fixed accessible palette (color-blind safe; categories are also labeled, never color-only). Below it, a list of categories with size and count; tapping filters the main list. Files the engine can no longer find (path missing, token unresolvable) are shown in a muted "moved or deleted" bucket with a "Forget" action. The bucket is the core's `missing` list from `StorageBreakdown` (it checks `final_path` and resolves tokens); Forget is `Remove` without `delete_files`. The view reads the breakdown again when a download completes, moves or goes, and when the app becomes active, never on a timer.

Colors are the one place where shells share a decision: the category palette is defined once in `docs/UI-GUIDELINES.md` (here) so screenshots across platforms match:

| Category | Light | Dark |
|---|---|---|
| Video | `#5B7CFA` | `#7E97FF` |
| Audio | `#C85FD6` | `#DA82E6` |
| Image | `#2CB67D` | `#4FD39A` |
| Document | `#F2A33A` | `#FFB95C` |
| Archive | `#8A6D3B` | `#B39160` |
| Software | `#E25555` | `#F27777` |
| Code | `#3BA7C4` | `#5EC4DD` |
| Data | `#7F8C8D` | `#A0ACAD` |
| Other | `#B0B7BC` | `#CAD0D4` |

## 7. Settings

Platform conventions decide the container: macOS Settings window with toolbar tabs; WinUI settings page with `SettingsExpander` groups; libadwaita `AdwPreferencesDialog` with pages; Android settings screen with Material sections; iOS grouped list.

Groups (same order everywhere):

1. **General**: default folder, ask where to save each time, default queue, open completed files from notification, launch at login (desktop).
2. **Connections**: default connections per download, max per download, global budget, per-host cap, new-download minimum connections (1/0), timeouts (advanced).
3. **Speed**: global limit, "limit only on cellular/metered" (mobile and Windows metered networks), commit interval (advanced).
4. **Network**: proxy mode (System / None / Custom), custom proxy fields, current network card (interface kind, VPN active, system proxy detected, read-only), "re-detect".
5. **Queues**: list of queues with schedule editor.
6. **Grouping & categories**: auto-grouping on/off, extra extensions per category, naming template toggles (use Content-Disposition, append extension from type).
7. **Notifications**: completion, failure, group completion, sounds.
8. **Advanced**: logging level, show segment map by default, open logs folder, reset engine (with confirmation).
9. **About**: version, license (GPL-3.0), credits, link to repo.

Items the core owns are `Settings` fields, changed with one `SetSettings` patch (ARCHITECTURE §3.1, §5.1): connections, timeouts and retries, speed limit and headroom, commit interval, proxy mode, auto-grouping, naming toggles, extra extensions. General, Notifications and Advanced items (default folder, ask where to save, launch at login, sounds, log level, segment map, …) are shell-owned settings, stored by the shell, unless ARCHITECTURE §5.1 lists them.

Core settings are edited as a draft and saved together (Save / Revert), so one `SetSettings` patch carries the user's edits, only those, and the core validates it whole; the shell doesn't repeat the core's ranges. Settings another process changes meanwhile come into the fields the user hasn't edited. A value the setting's type can't hold (a negative count, 300 for a byte) is refused at the field, never clamped. Closing the window keeps unsaved edits for the next time it opens (macOS: the draft lives in the app, not the window). Shell-owned settings (General, Notifications, Advanced) apply as they change, with no Save; each queue (Queues) is saved on its own. A refusal (`InvalidSetting`, which names the setting's key) is shown under that field, on its tab; for extra extensions, under the categories the refused patch changed. Speeds are entered in kB/s (1000 bytes). The connection budget (global, per host, newcomer) says "applies after restart": the engine reads it when it opens. `probe_range_get` sits with the connection timeouts (advanced).

## 8. Per-platform specifics

### macOS
- `NavigationSplitView` with sidebar; `Table` for the list (sortable columns: name, status, size, speed, added). Inspector via `.inspector(isPresented:)`.
- Unified toolbar; the speed sparkline is a `ToolbarItem` view, keep it ≤ 120 pt wide.
- Dock badge = number of active downloads; Dock progress bar for aggregate progress (via `NSDockTile` custom view).
  The tile is redrawn when the count or the whole percent changes, never on a timer; with nothing active it is the plain icon. No Dock menu.
- Services and Share extension: "Download with Dhow" accepting URLs. The Services item is in the app's Info.plist; the Share extension needs an app-extension target, so it waits for Xcode. Links dropped on the main window open Add.
- The inspector is toggled from the toolbar and View > Show Details (⌥⌘I) and stays as the user left it; Storage is the sidebar's last item.
- Settings via `Settings {}` scene. Menu bar has full menus (File/Edit/Download/View/Window/Help).
  - File: New Download… ⌘N. View: Show/Hide Details ⌥⌘I, the smart filters ⌘1–⌘5, Storage ⌘6. Help: the website.
  - Download (the context menu's items for the selection): Pause ⌘., Resume / Retry Now / Start Now ⌘/, Retry (requeue) ⌃⌘/, Open ⌘O, Show in Finder ⌘R, Copy Link ⇧⌘C, Cancel Download… ⇧⌘., Remove (plain Delete in the list), Remove and Delete File… ⌥⌘⌫, Pause All ⌥⌘., Resume All ⌥⌘/. The menu is disabled while a sheet or dialog is up (⌘. there is Cancel). Shortcuts follow Transmission where macOS has no convention.
- Notifications (UserNotifications), each switchable in Settings: a download completed (with "Show in Finder"; clicking opens the file, or shows it if General says so), failed, a group completed (its members don't notify alone), a queue finished several downloads (one download's own notification is enough). Permission is asked the first time there is something to say, never at launch.
- `on_complete` runs when a queue runs dry because its last download finished (completed or failed), not when a pause, cancel, removal or move empties it, and only with nothing else downloading. Sleep and Shut Down (Hibernate is sleep: the Mac's `hibernatemode` decides) show a floating 60-second countdown on every Space, with Cancel and "Now", plus a notification; it calls itself off if a download starts meanwhile, and Shut Down replaces a pending Sleep. Choosing Shut Down for a queue asks macOS for permission to send the request (Automation) then and there; a sleep or shutdown that fails is said in a notification and an alert. Never silent.
- General's shell settings: default folder, ask where to save (the Add dialog opens the folder picker), the queue Add preselects, launch at login (`SMAppService`; the system may ask for approval). Advanced: log level (applies at next launch), open the logs folder, segment map on/off (off watches no ranges). About: versions, GPL-3.0, UniFFI (MPL-2.0) with a link to its source.
- App Sandbox on with user-selected folder bookmarks; file tokens are security-scoped bookmarks.
- Notarized DMG outside the App Store first; MAS later if sandboxing allows everything needed.

### Windows
- `NavigationView` (left, collapsible) + `ListView` with a `DataTemplate` row; details via a `SplitView` pane.
- Taskbar: `TaskbarItemInfo` progress for aggregate progress; badge count.
- Toast notifications via `AppNotificationManager`. Jump list: "Add download" and recent downloads.
- Mica/Acrylic backdrop per system setting; never force it.
- Packaged (MSIX) for Store, plus an unpackaged installer. File tokens via `FILE_ID_INFO`.

### Linux
- `AdwApplicationWindow` + `AdwNavigationSplitView` (sidebar/content) + `AdwBottomSheet` or a third pane for details on wide windows.
- `GtkListView` with a factory, not `GtkListBox`, for large lists.
- Background: `xdg-desktop-portal` Background API for "keep running after close"; progress via `libunity`-style `launcher-entry-api` on desktops that support it, else nothing.
- Notifications via `GNotification`. Flatpak as the primary distribution (Flathub), with portals for file choosing. Also an AppImage and distro packages later.
- App ID: `app.dhow.Dhow`.

### Android
- Single-activity Compose app; `NavigationBar` with Downloads / Queues / Storage; detail as a destination.
- Foreground service with a progress notification while any download is active (required by the OS); service stops the moment nothing is active.
- Storage Access Framework for destination; default to `Downloads/`. No `MANAGE_EXTERNAL_STORAGE`.
- Share-sheet target for URLs. Quick Settings tile to pause/resume all (optional).
- Metered-network awareness: pause or limit on metered per setting; `ConnectivityManager` callback through the adapter.

### iOS
- `NavigationStack`; Files app integration via document picker and `UIFileSharingEnabled` so downloads are visible in Files.
- Background: `URLSession` background transfers are **not** usable with the core's chunked engine, so iOS uses `BGProcessingTask` opportunistically and is honest in the UI: "Downloads continue while Dhow is open; iOS may pause them in the background." Share extension to add URLs. Live Activity for an active download.
- This is the platform where the engine's journal pays off most: the app being killed by the OS is routine.

## 9. Copy & tone

Short, calm, specific. "Reconnecting (attempt 3)" not "Something went wrong". Never blame the user. Errors name the cause and the next step: "The file on the server changed. Dhow started the download over to make sure you get a complete file."

v1 ships in English, every string behind a localization key; translations, Persian (fa-IR) and RTL first, come later as language packs over those keys (DECISIONS 2026-10-25). Use platform localization systems (`.xcstrings`, `.resw`, gettext `.po`, Android `strings.xml`); the macOS shell uses `.strings` and `.stringsdict` while it builds without Xcode, which alone compiles `.xcstrings`.

## 10. What shells must not do

- Hold their own copy of download state. The core's snapshots are the state; the shell renders them.
- Talk to the network. All HTTP goes through the core, including the probe.
- Write to the part file, sidecar or database.
- Run timers for polling. Subscribe to events.
- Keep running when idle without a visible reason.
