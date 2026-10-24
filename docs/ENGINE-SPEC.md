# Dhow Engine Specification

**Status: normative.** `dhow-core` implements this; `spec-tests/` verifies it against `dhow-testserver`. Where this document and the code disagree, the code is wrong unless a `DECISIONS.md` entry says otherwise.

Keywords MUST / MUST NOT / SHOULD / MAY as in RFC 2119.

---

## 1. Terms

- **Download**: one URL → one final file.
- **Range**: a half-open byte interval `[start, end)` of the target file.
- **Chunk**: a range assigned to one connection for transfer. Chunks are planned; ranges are what the journal records.
- **Part file**: `<final>.part`, the preallocated file being written.
- **Sidecar**: `<final>.dhow`, the journal for the part file.
- **Committed**: bytes that have been written *and fsync'd* *and* recorded in the sidecar.
- **Identity**: the tuple `(Content-Length, ETag, Last-Modified)` the server reported at probe time.

---

## 2. Probe

Before any bytes are written, the engine learns what it is dealing with.

### 2.1 Request

1. `HEAD <url>` with the user's headers.
2. If `HEAD` fails with 405/501 or returns no `Content-Length`, send `GET` with `Range: bytes=0-0`. Read headers only; abort the body.
3. Follow up to 10 redirects. Record the final URL as `effective_url`. Each `Add` re-resolves from the original URL (redirect targets may be signed and expire).

### 2.2 Result

```
ProbeInfo {
  effective_url, status,
  total_size: Option<u64>,         // Content-Length, or Content-Range total from the 0-0 request
  resumable: Resumable,            // Yes | No | Unknown — see 2.3
  etag: Option<String>, last_modified: Option<String>,
  content_type: Option<String>,
  content_disposition: Option<String>,
  digest: Option<Digest>,          // from Digest / Content-MD5 / Content-Digest headers, if any
  suggested_name: String,          // §3
  category: Category,              // §13
}
```

### 2.3 Resumability

- `Yes` if the 0-0 `GET` returned `206` with a valid `Content-Range`, **or** `HEAD` returned `Accept-Ranges: bytes` and a `Content-Length`.
- `No` if `Accept-Ranges: none`, or the 0-0 `GET` returned `200`.
- `Unknown` otherwise (e.g. `HEAD` succeeded with `Content-Length` but no `Accept-Ranges`, and the user disabled the `GET` probe). `Unknown` is treated as `Yes` for planning and downgraded to `No` on first `200`-to-a-`Range` response.

Shells MUST surface `No` before the user confirms the download ("This server does not support pausing or multiple connections").

### 2.4 Probe failure

If both requests fail, `Add` MAY still proceed in **blind mode**: single connection, no preallocation, `total_size = None`, `resumable = No`. The user is warned.

---

## 3. Naming

Compute `suggested_name` in this order; stop at the first non-empty result:

1. `Content-Disposition` `filename*` parameter (RFC 8187: charset, percent-decoding; only `UTF-8` and `ISO-8859-1` accepted).
2. `Content-Disposition` `filename` parameter (RFC 6266; strip quotes and paths).
3. Last non-empty path segment of `effective_url`, percent-decoded.
4. Last non-empty path segment of the original URL, percent-decoded.
5. `hostname` + `-download`.

Then:

- If the result has no extension and `Content-Type` maps to a known extension (`mime_guess`, with a curated override table for `application/octet-stream` → none), append it.
- Sanitize for the **target OS** (the shell passes `TargetFs`): strip control chars and `/`; on Windows additionally `\ : * ? " < > |`, trailing dots/spaces, and reserved names (`CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`). Truncate to 255 bytes *of the stem*, keeping the extension.
- On collision in `directory`: `name.ext` → `name (2).ext` → `name (3).ext`. Collision check includes existing `.part` files.

Naming MUST be a pure function `(ProbeInfo, original_url, TargetFs, existing_names) -> String` with a table-driven test suite.

---

## 4. Chunk planning

### 4.1 Connection count

```
n = min(
  user_override or settings.default_connections (default 8),
  settings.max_connections_per_download (default 16),
  per-host cap remaining (§10.2),
  global budget remaining (§10.1),
  ceil(total_size / settings.min_chunk_size)   // default min chunk 1 MiB
)
n = 1 if resumable == No or total_size == None
```

### 4.2 Initial plan

Split `[0, total_size)` into `n` contiguous chunks of equal size (last takes the remainder). Each chunk is one range in the sidecar with state `pending`.

### 4.3 Dynamic rebalancing (work stealing)

When a connection finishes its chunk and the download is not complete:
1. Find the active chunk with the largest *remaining* bytes `r`.
2. If `r >= 2 * settings.min_chunk_size`, split it: the owner keeps `[pos, pos + r/2)`, the idle connection takes `[pos + r/2, end)`.
3. Splitting MUST be recorded in the sidecar (one range becomes two) **before** the new connection sends its request.
4. The owner learns of the split via a shared atomic `end` bound; it MUST stop writing at the new `end` even if the server keeps sending (it aborts the response).

### 4.4 Connection ramp-up

A download never opens all `n` connections at once. It opens 1, and opens one more each time 256 KiB has arrived without error, until `n`. On any connection error, ramp-up pauses for the retry backoff period. This is the mechanism that makes politeness (§10) smooth rather than bursty.

---

## 5. Sidecar journal

File: `<final>.dhow`, next to `<final>.part`. Little-endian binary. Designed for append-only updates with one `fsync` per commit.

### 5.1 Layout

```
Header (fixed 256 bytes, written once, fsync'd):
  magic        "DHOW"      4
  version      u16         2   (=1)
  flags        u16         2   (bit0: preallocated)
  total_size   u64         8   (u64::MAX if unknown)
  url_hash     [u8;32]     32  (blake3 of original url)
  etag_len     u16         2
  etag         [u8;120]    120 (truncated/padded)
  last_mod     i64         8   (unix seconds, 0 if none)
  created_at   i64         8
  reserved     ...         to 256
Records (append-only, each 24 bytes):
  kind         u8          1   (1=range_define, 2=range_commit, 3=range_split, 4=checkpoint)
  _pad         [u8;3]      3
  a            u64         8
  b            u64         8
  crc32        u32         4   (over kind..b)
```

- `range_define(a=start, b=end)`: a chunk exists.
- `range_commit(a=start, b=end)`: bytes `[a,b)` are on disk and fsync'd. Commits for one chunk are monotonic and contiguous from its start.
- `range_split(a=old_end_owner_keeps_to, b=new_chunk_start)`: informational; new chunk also gets its own `range_define`.
- `checkpoint`: all records before it have been compacted into the header's implied state. Written during compaction (§5.3).

### 5.2 Reading

On open, replay records in order, ignoring any record whose CRC fails **and every record after it** (a torn write at the tail). The result is the committed set: a sorted list of disjoint `[start, end)` intervals.

### 5.3 Compaction

When the sidecar exceeds 64 KiB, rewrite it to `<final>.dhow.tmp` with the header + one `range_commit` per committed interval + `checkpoint`, fsync, atomic rename over the old one, fsync the directory.

### 5.4 Recovery with no sidecar

A `.part` without a `.dhow` is **not** resumable. Start over. (Never trust bytes you cannot account for.)

---

## 6. Integrity invariants

These are the point of the project. Every one has a conformance test.

**I1 — Nothing exists at the final path until it is proven complete.**
The final filename is created only by the atomic rename in §8, after all checks pass.

**I2 — Committed means durable.**
A `range_commit` record is appended only after `fdatasync` (or platform equivalent: `F_FULLFSYNC` on macOS, `FlushFileBuffers` on Windows) of the part file has returned for those bytes.

**I3 — The journal is the only truth.**
After any crash, the engine resumes from the committed set in the sidecar and nothing else. Bytes present in the `.part` file but not committed are treated as absent and re-downloaded. The DB `ranges` table is a cache and is rebuilt from the sidecar on open.

**I4 — Identity is bound.**
Every range request carries `If-Range: <etag>` (or `If-Range: <last-modified>` if no ETag). If the server answers `200` instead of `206`, or the `Content-Range` total differs from `total_size`, the response is discarded, the download is reset (`.part` truncated, sidecar recreated) and the user is informed ("The file on the server changed; starting over"). Strong ETags are required for `If-Range`; a weak ETag (`W/`) falls back to `Last-Modified`; if neither exists, resume uses the overlap check (I5) alone and the download is flagged `IdentityWeak`.

**I5 — Overlap verification on resume.**
When resuming a chunk whose committed end is `c > start`, the request begins at `max(start, c − OVERLAP)` where `OVERLAP = 64 KiB`. The first `min(OVERLAP, c − start)` bytes received MUST be byte-equal to what is already on disk. On mismatch → treat as I4 violation (reset).

**I6 — Every write is bounded.**
A connection writes only within its chunk's current `[pos, end)`. Bytes beyond `end` are dropped and the connection is closed. Bytes beyond `total_size` are an error (`ServerSentExtraData`) and the download fails rather than completing.

**I7 — Size is exact.**
Completion requires the committed set to be exactly `[0, total_size)` with no gaps and no overlaps. For `total_size = None` (blind mode), completion requires the single connection to see EOF and the server not to have sent `Content-Length`; the file is additionally flagged `SizeUnverified` in metadata.

**I8 — Digest when offered.**
If the server sent `Digest`, `Content-Digest` (RFC 9530) or `Content-MD5`, the engine computes the hash over the full part file *in order* after I7 and before rename. Mismatch → `Failed(DigestMismatch)`, part file kept for inspection, not renamed. Supported: sha-256, sha-512, sha-1 (verify only), md5 (verify only).

**I9 — Writes never race.**
All writes to one part file go through a single `Writer` actor that owns the file handle and serializes `pwrite` + `fdatasync` + journal append. Connection tasks send `(offset, bytes)` messages; they never hold the file handle.

**I10 — Preallocation is honest.**
Part files are preallocated to `total_size` with `fallocate`/`F_PREALLOCATE`/`SetFileInformationByHandle(FileAllocationInfo)` where supported, else `set_len` (sparse). Insufficient space is detected at `Add` time and fails fast (`Failed(NoSpace)`), not at 99%.

---

## 7. Transfer loop (per connection)

```
loop:
  req = GET effective_url, Range: bytes={req_start}-{end-1}, If-Range: <identity>, user headers
  resp = send(req)                       // timeout: connect 15s, first byte 30s, idle 30s
  validate(resp):                        // I4
    status == 206 && Content-Range matches {req_start}-{end-1}/{total}
  if overlap_pending: compare first bytes (I5)
  stream body:
    for each buffer (64 KiB):
      clamp to [pos, end)                 // I6
      writer.send(pos, bytes)             // I9
      pos += len
      limiter.consume(len)                // §12
      if pos == end: break
  on EOF before end: reconnect with backoff (§9), same chunk, from pos
```

The `Writer` batches: it issues `pwrite` immediately, but `fdatasync` + `range_commit` at most every `settings.commit_interval` (default 2 s) **or** every 8 MiB per chunk, whichever first, and always on pause/cancel/shutdown. Uncommitted window on crash is therefore bounded by ~2 s of throughput.

---

## 8. Completion

1. All chunks report `pos == end`; committed set is `[0, total_size)` (I7).
2. Writer: final `fdatasync`, then append a `checkpoint`.
3. Digest check if applicable (I8).
4. Close the handle. On Windows, release the share lock.
5. `rename(<final>.part, <final>)` — atomic on the same volume. If the final name was taken since `Add` (another download finished with the same name), apply the collision rule (§3) at this moment, not earlier.
6. `fsync` the directory (POSIX).
7. Delete the sidecar.
8. Create a file token via the adapter (`ARCHITECTURE §6`) and store it.
9. Emit `StateChanged → Completed`, `Renamed`.

Steps 5–7 are the only non-idempotent moment. If the process dies between 5 and 7, the next open finds `<final>` present and a stale sidecar: the sidecar is deleted and the download is marked complete (the rename is the commit point).

---

## 9. Errors & retries

| Class | Examples | Behaviour |
|---|---|---|
| Transient | connection reset, timeout, 5xx, 429, DNS failure | Retry the *connection* with exponential backoff 1s·2ⁿ, jitter ±25%, cap 60s, up to `settings.max_retries` (default 10) consecutive failures per connection. Download stays `Active` (shows "reconnecting"). After the cap: `Failed(Transient)`; `Retry` command restarts from the journal. |
| Identity | 200 to a Range, size mismatch, overlap mismatch | Reset per I4. Not a retry; a restart. |
| Permanent | 401/403/404/410, TLS failure, `NoSpace`, `DigestMismatch` | `Failed(...)` immediately. Journal kept; `Retry` re-probes first (links may be refreshed). |
| Protocol | server ignores `Range` on a *non-first* request after having honored it | Downgrade download to single connection, keep committed bytes, continue. Warn. |

`429` and `503` honor `Retry-After` if present (cap 5 min).

---

## 10. Politeness scheduler

The scheduler decides which downloads are `Active` and how many connections each may hold.

### 10.1 Global connection budget

`settings.global_connections` (default 16). The sum of all active connections across all downloads never exceeds it.

### 10.2 Per-host cap

`settings.per_host_connections` (default 4). Keyed by `effective_url` host (eTLD+1 is *not* used; `cdn1.x.com` and `cdn2.x.com` are different hosts, which is usually what the server operator intends).

### 10.3 Allocation rule: oldest first, newcomers ramp

Every time budget changes (download added/finished/paused, connection closed, setting changed):

1. Order all downloads that are `Active` or `Waiting(ConnectionBudget)` by `started_at` ascending (oldest first).
2. Walk the list. Each download is granted `min(its n, per-host remaining, global remaining, 1 + its current connections)` — i.e. it can grow by at most one connection per allocation round. Rounds happen on every budget change and every `settings.rampup_interval` (default 3 s) while anyone is waiting.
3. Downloads reached after budget hits zero get **one** connection if `settings.min_connections_for_new` is 1 (default), taken from the youngest over-allocated download, so a new download always makes visible progress. If that setting is 0, they wait.

Effect: start 10 downloads on a 16-connection budget → the first one ramps to 8, the next to 4–8 as space allows, the rest crawl at 1 connection each, and as the oldest finish the rest ramp up in order. This is the behaviour requested in the brief.

### 10.4 Queue concurrency

Independently, each queue has `max_concurrent` (default 3). A download in a queue that is at capacity is `Waiting(QueueConcurrency)` regardless of connection budget. Queue order is user-defined (`Reorder`), default FIFO by `created_at`.

### 10.5 Fairness within a round

Per-host cap applies before global; a single slow host cannot starve downloads from other hosts because the walk continues past a host-capped download.

---

## 11. Queues & schedules

```
QueueSpec {
  name, max_concurrent, speed_limit: Option<u64>,
  schedule: Option<Schedule>,
  on_complete: OnComplete,      // Nothing | Sleep | Shutdown | Hibernate — shell decides what's possible
}
Schedule {
  windows: Vec<Window>,         // Window { days: Weekdays, start: Time, end: Time } local time; may wrap midnight
  action: Start | StartAndStopOutside,
}
```

- `Start`: the queue starts at a window's `start`; downloads continue past `end`.
- `StartAndStopOutside`: downloads in the queue are paused at `end` (journal commit, connections closed) and resumed at the next `start`.
- The engine keeps a single `sleep_until(next_edge)` across all queues (`ARCHITECTURE §7`). DST and timezone changes recompute the edge; the adapter's `watch_network` is not involved.
- `StartQueue`/`StopQueue` override the schedule until the next edge.
- The **default queue** exists always, cannot be deleted, has no schedule.

---

## 12. Speed limiting

Token-bucket limiters at three levels: global, queue, download. A connection must acquire tokens from all applicable buckets before reading the next buffer from the socket (`limiter.consume` in §7 is a blocking acquire across the chain). Bucket refill: 100 ms granularity, burst = 2× the per-interval allotment. Reads are 64 KiB; at limits below 64 KiB/s the read size shrinks to the limit so that small limits are accurate.

Changing a limit takes effect within 200 ms. Setting `0` means "paused by limit", shown as such.

---

## 13. Categories & grouping

### 13.1 Category

Derived at probe time from extension first, `Content-Type` second:
`Video | Audio | Image | Document | Archive | Software | Code | Data | Other`. The mapping table lives in `dhow-core/src/naming/categories.toml`, user-extensible via settings (`extra_extensions: {Category: [ext]}`). `storage_breakdown()` sums `total_size` of `Completed` downloads by category and reports whether the file still exists (via path or token).

### 13.2 Automatic grouping

Goal: `Movie.Part1.rar`, `Movie.Part2.rar`, `Movie.Part3.rar` appear as one group.

Algorithm on each `Add` (and on `Renamed`):
1. Normalize the stem: lowercase; replace `[._\-\s]+` with a single space; strip a trailing sequence token matching `(part|pt|cd|disc|disk|vol|episode|ep|e|s\d+e)?\s*\d{1,4}$|\(\d+\)$|\[\d+\]$`; strip a trailing `of \d+`.
2. Candidate = normalized stem **plus extension** (parts of one subject share an extension; `foo.mp4` and `foo.srt` are related but not a group).
3. If an existing *auto* group has the same candidate key and the same `directory` → join it. Else if another *ungrouped* download has the same key → create an auto group containing both.
4. Minimum key length after normalization: 4 characters; shorter stems never auto-group.
5. A manual `SetGroup`/`RenameGroup` marks the group `auto = false`; auto-grouping never modifies manual groups.

The group's display name is the normalized stem with original casing of the first member. Groups report aggregate progress, speed and ETA.

---

## 14. Speed sampling

Per active download, once per second: `bytes_received_in_last_second` (counted at the socket read, before the limiter). Stored in the ring (`ARCHITECTURE §5.1`). `Progress.speed_bps` is a 5-sample EMA; ETA = `remaining / EMA`, reported as `None` when EMA < 1 KiB/s or fewer than 3 samples exist. A global aggregate sample (sum) is kept while any download is active.

---

## 15. Conformance test matrix

All run against `dhow-testserver` through `dhow-cli`. Each maps to a section above.

| ID | Scenario | Expect |
|---|---|---|
| T-I1 | kill -9 at 50% | no file at final path; `.part` + `.dhow` present |
| T-I2 | kill -9 immediately after a commit record | committed set on reopen includes that range |
| T-I3 | corrupt the tail of `.dhow` (torn write) | replay stops at corruption; download resumes and completes bit-exact |
| T-I3b | delete `.dhow`, keep `.part` | download restarts from zero |
| T-I4a | server changes ETag mid-download | reset, user warned, final file matches *new* content |
| T-I4b | server returns 200 to `If-Range` | same |
| T-I5 | server serves different bytes for the overlap window | reset |
| T-I6 | server sends 1 MiB past the requested range end | extra bytes discarded, file bit-exact |
| T-I6b | server sends more than `Content-Length` on a single-connection download | `Failed(ServerSentExtraData)` |
| T-I7 | server truncates one chunk's response repeatedly | retries until complete; never completes early |
| T-I8 | `Content-Digest` sha-256 mismatch | `Failed(DigestMismatch)`, `.part` kept |
| T-I10 | disk with 1 MiB free, 10 MiB file | `Failed(NoSpace)` at `Add` |
| T-4.3 | one connection throttled to 1 KiB/s, others fast | work stealing splits its chunk; completion time within 1.5× of ideal |
| T-9a | 10 consecutive resets on one connection | `Failed(Transient)`; `Retry` completes |
| T-9b | server ignores `Range` after honoring it once | downgrade to 1 connection, completes, `Warning` emitted |
| T-10 | 10 downloads, budget 16, per-host 4 | oldest reaches 4 first; connection counts over time match the ramp model |
| T-11 | schedule window ends during transfer (`StartAndStopOutside`) | paused with committed journal; resumed at next start |
| T-12 | global limit 100 KiB/s, 3 downloads | aggregate within ±10% over 10 s |
| T-13 | add `A.part1.rar`, `A.part2.rar`, `B.rar` | one auto group of two; `B` ungrouped |
| T-3 | naming table (50+ cases incl. RFC 8187, Windows reserved names, collisions) | exact strings |
| T-idle | complete all downloads, wait 60 s | zero alive tasks, no event emitted, RSS flat |
| T-blind | server with no `Content-Length`, no `Accept-Ranges` | single connection, completes, flagged `SizeUnverified` |

Bit-exactness is asserted with blake3 over the final file versus the server's source.
