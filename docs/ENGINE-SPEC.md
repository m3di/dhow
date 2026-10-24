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
2. If `HEAD` fails (any non-2xx or network error: servers reject `HEAD` with more than 405/501, e.g. 403 from URLs signed only for `GET`) or does not settle both size and resumability (no `Content-Length`, or no `Accept-Ranges`), send `GET` with `Range: bytes=0-0`, unless `settings.probe_range_get` is off (for links that count every `GET`; then no `GET` is ever sent). Read headers only; abort the body.
3. Follow up to 10 redirects. Record the final URL as `effective_url`. Each `Add` re-resolves from the original URL (redirect targets may be signed and expire).
4. Every request carries `Accept-Encoding: identity` (a compressed response makes byte ranges meaningless). `Range`, `If-Range` and `Accept-Encoding` belong to the engine; user-supplied values for them are dropped.

### 2.2 Result

```
ProbeInfo {
  effective_url, status,
  total_size: Option<u64>,         // Content-Length, or Content-Range total from the 0-0 request
  resumable: Resumable,            // Yes | No | Unknown — see 2.3
  etag: Option<String>, last_modified: Option<String>,
  content_type: Option<String>,
  content_disposition: Option<String>,
  digest: Option<Digest>,          // strongest of Repr-Digest, Digest, Content-Digest, Content-MD5; the last two only
                                   // from HEAD or 200 responses, since on a 206 they describe the part, not the file
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

Compute `suggested_name` in this order; stop at the first candidate that is non-empty **after sanitizing**:

1. `Content-Disposition` `filename*` parameter (RFC 8187: charset, percent-decoding; only `UTF-8` and `ISO-8859-1` accepted; a malformed value is skipped).
2. `Content-Disposition` `filename` parameter (RFC 6266; strip quotes and paths). In quoted strings a backslash escapes only `"` and `\`, so unescaped Windows paths (`"C:\dir\f.exe"`) are still path-stripped. Header bytes are decoded as UTF-8, else ISO-8859-1.
3. Last non-empty path segment of `effective_url`, percent-decoded (a stray `%` is literal; bytes that aren't UTF-8 are read as ISO-8859-1).
4. Last non-empty path segment of the original URL, percent-decoded.
5. `hostname` + `-download`.

Then:

- An **extension** is 1–10 ASCII alphanumerics after the last dot of a non-empty stem, or one of `.tar.gz`, `.tar.bz2`, `.tar.xz`, `.tar.zst` (treated as one).
- If the result has no extension and `Content-Type` maps to a known extension, append it. A curated table (`naming/mime.rs`) wins; `mime_guess` is used only when it knows exactly one extension (its first-of-many is alphabetical: `image/jpeg` → `jfif`). Opaque types (`application/octet-stream` and friends) map to none.
- Sanitize for the **target filesystem** (the shell passes `TargetFs`): strip control chars and `/`, surrounding whitespace and leading dots (a download never becomes a hidden file); on Windows (also FAT/exFAT/SMB volumes) additionally `\ : * ? " < > |` and trailing dots/spaces, and prefix reserved device names with `_` (`CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`, case-insensitive, with any extension: `con.txt` → `_con.txt`).
- Truncate the stem (at a char boundary, keeping the extension) so the **whole name, including any collision suffix, is at most 246 bytes**: 255 minus `.dhow.tmp`, the longest suffix the engine appends (§5.3).
- On collision in `directory`: `name.ext` → `name (2).ext` → `name (3).ext`. Comparison is case-insensitive (macOS and Windows volumes are) and also against existing `.part` and `.dhow` files.

`ProbeInfo.suggested_name` is the sanitized name for the host filesystem *without* collision handling (the probe doesn't know the target directory); `Add` applies the request's `TargetFs` and the collision rule, and §8 applies the collision rule again at rename.

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

Also capped by a recent **link saturation** (§4.4): if this process saw throughput stop growing at `k` connections in the last `settings.saturation_ttl` (default 60 s), `n <= k`.

### 4.2 Initial plan

There is no up-front split. A download starts with its unfinished ranges as *pending* work: the whole file for a fresh download, the committed set's gaps on resume (each with its I5 overlap). The first connection takes the first pending range. Every further connection, whether opened by ramp-up (§4.4) or refilling a slot whose chunk just finished, takes the next pending range if any, else **steals** (§4.3). An equal up-front plan would leave chunks waiting for connections that throughput-aware ramp-up may never open.

### 4.3 Dynamic rebalancing (work stealing)

When a connection needs work and nothing is pending:
1. Among the active chunks, consider first the one whose owner would finish **last** (remaining bytes ÷ the owner's measured speed).
2. With both speeds known (the owner's, and the idle connection's own or else the average of the open connections), split so both parts finish together: the owner keeps `remaining · v_owner / (v_owner + v_new)` (at least one byte, it is mid-response) and the new connection takes the rest. Split only if that finishes the work at least `settings.min_steal_gain` (default 1 s) sooner than leaving it to the owner, however few bytes move: a crawling owner's last kilobytes are exactly what must be taken (T-4.3). Without speed estimates, halve, and only chunks of at least `2 · min_chunk_size`.
3. Splitting MUST be recorded in the sidecar (`range_split` + `range_define`) **before** the new connection sends its request. The writer performs the split, so the split point is checked against what it has actually written.
4. The owner learns of the split via a shared atomic `end` bound, re-read for every buffer; it MUST stop writing at the new `end` even if the server keeps sending (it aborts the response). Bytes it already handed to the writer past the new end are dropped by the writer; they belong to the new chunk.
5. A chunk that ends at `total_size` still reads to EOF, so bytes past the file are seen (I6).

### 4.4 Connection ramp-up

A download never opens all `n` connections at once. It opens one; then, after each new connection, it lets it settle for a third of `settings.rampup_window` (default 3 s) and measures aggregate throughput for one window. It opens connection `k + 1` only if data flowed in that window and throughput with `k` connections beat the measurement with `k − 1` by at least `settings.rampup_min_gain` (default 10 %). Otherwise ramp-up stops at `k`, and "the link saturates at `k`" is recorded process-wide for `settings.saturation_ttl` (60 s) so other downloads start from that ceiling (§4.1). A window in which a speed limit (§12) made any of the download's connections wait measured the limit, not the link: it counts as neither gain nor saturation, and is measured again. On any connection error, ramp-up pauses for the retry backoff period. This is what keeps a download from crowding the user's own link (§10.A) and makes politeness smooth rather than bursty.

---

## 5. Sidecar journal

File: `<final>.dhow`, next to `<final>.part`. Little-endian binary. Designed for append-only updates with one `fsync` per commit.

### 5.1 Layout

```
All integers little-endian.

Header (fixed 256 bytes, written once, fsync'd, directory fsync'd):
  magic        "DHOW"      4
  version      u16         2   (=1)
  flags        u16         2   (bit0: preallocated)
  total_size   u64         8   (u64::MAX if unknown)
  url_hash     [u8;32]     32  (blake3 of original url)
  etag_len     u16         2   (full length; > 120 means truncated and is read back as no ETag)
  etag         [u8;120]    120 (truncated/padded)
  last_mod     i64         8   (unix seconds, 0 if none)
  created_at   i64         8   (unix milliseconds)
  reserved     [u8;66]     66  (zero)
  crc32        u32         4   (CRC-32/ISO-HDLC over bytes 0..252)
Records (append-only, each 24 bytes):
  kind         u8          1   (1=range_define, 2=range_commit, 3=range_split, 4=checkpoint)
  _pad         [u8;3]      3
  a            u64         8
  b            u64         8
  crc32        u32         4   (CRC-32/ISO-HDLC over kind..b, padding included)
```

- `range_define(a=start, b=end)`: a chunk exists.
- `range_commit(a=start, b=end)`: bytes `[a,b)` are on disk and fsync'd. Commits for one chunk are monotonic and contiguous from its start.
- `range_split(a=old_end_owner_keeps_to, b=new_chunk_start)`: informational; new chunk also gets its own `range_define`.
- `checkpoint`: all records before it have been compacted into the header's implied state. Written during compaction (§5.3).

### 5.2 Reading

On open, replay records in order, ignoring any record that is not one the writer could have produced **and every record after it** (a torn write at the tail). Not producible: CRC mismatch, partial record, unknown kind, non-zero padding, `start >= end` (`Define`/`Commit`), `keep_to > new_start` (`Split`), an end past a known `total_size`, or a `Checkpoint` with non-zero fields. The writer refuses to append such records, so they can only come from damage. The result is the committed set: a sorted list of disjoint `[start, end)` intervals.

A header that fails magic, CRC or version checks makes the sidecar unusable: treat as §5.4.

Before appending anything after replay, the file is truncated to the end of the last valid record and synced. Otherwise new records would land behind the garbage and the next replay would stop before them.

### 5.3 Compaction

When the sidecar exceeds 64 KiB, rewrite it to `<final>.dhow.tmp` with the header + one `range_commit` per committed interval + `checkpoint`, fsync, atomic rename over the old one, fsync the directory. (The old handle is closed before the rename: Windows will not replace an open file.) A crash leaves either the old or the new sidecar; a leftover `.dhow.tmp` is deleted on open.

### 5.4 Recovery with no sidecar

A `.part` without a `.dhow` is **not** resumable. Start over. (Never trust bytes you cannot account for.) The same applies to a `.dhow` without its `.part`, a sidecar whose header fails its checks, a part file whose length isn't the `total_size` it was created with, and committed bytes on a server that can no longer resume (non-resumable or blind).

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

Identity is also checked before any request on resume: every start re-probes, and the sidecar's URL hash, `total_size` and ETag (else Last-Modified) must match the fresh probe, or the download resets without fetching a byte. **Reset** means: delete the sidecar, then the part file, then create both afresh from the new probe (a crash in between leaves a part without sidecar, which §5.4 discards). At most `max_resets` (default 3) resets happen in one run; then the download fails, so a server whose file changes on every request cannot loop forever.

**I5 — Overlap verification on resume.**
When resuming a chunk whose committed end is `c > start`, the request begins at `max(start, c − OVERLAP)` where `OVERLAP = 64 KiB`. The first `min(OVERLAP, c − start)` bytes received MUST be byte-equal to what is already on disk, and are compared before any new byte is written. On mismatch → treat as I4 violation (reset). The expected bytes are read from the part file before the writer starts (reading is not writing; I9 holds). If the connection drops inside the overlap window, the next request starts at the first unverified byte. A `200` to a resumed request that started at offset 0 is the whole file and is verified the same way.

**I6 — Every write is bounded.**
A connection writes only within its chunk's current `[pos, end)`. Bytes beyond `end` are dropped and the connection is closed. Bytes beyond `total_size` are an error (`ServerSentExtraData`) and the download fails rather than completing.

**I7 — Size is exact.**
Completion requires the committed set to be exactly `[0, total_size)` with no gaps and no overlaps. For `total_size = None` (blind mode), completion requires the single connection to see EOF. If the server sent no `Content-Length`, the file is additionally flagged `SizeUnverified` in metadata; if it did, the HTTP layer already enforced that length (a short body is an error, not EOF), so the size is verified and no flag is set.

**I8 — Digest when offered.**
If the server sent `Digest`, `Content-Digest` (RFC 9530) or `Content-MD5`, the engine computes the hash over the full part file *in order* after I7 and before rename. Mismatch → `Failed(DigestMismatch)`, part file kept for inspection, not renamed. Supported: sha-256, sha-512, sha-1 (verify only), md5 (verify only).

**I9 — Writes never race.**
All writes to one part file go through a single `Writer` actor that owns the file handle and serializes `pwrite` + `fdatasync` + journal append. Connection tasks send `(chunk, offset, bytes)` messages; they never hold the file handle. The writer also enforces I6 itself: a write must start exactly at its chunk's position and end by its chunk's end, and chunks may not overlap each other or committed bytes, so durable data is never overwritten. Any writer error is fatal for the download.

**I10 — Preallocation is honest.**
Part files are preallocated to `total_size` with `fallocate`/`F_PREALLOCATE`/`SetFileInformationByHandle(FileAllocationInfo)` where supported, else `set_len` (sparse, header flag `preallocated` cleared). Insufficient space is detected at `Add` time and fails fast (`Failed(NoSpace)`), not at 99%: available space is checked before the file is created, and an out-of-space error from preallocation is also `NoSpace`. A failed creation leaves no part file.

---

## 7. Transfer loop (per connection)

```
loop:
  req = GET original_url, Range: bytes={pos}-{end-1}, If-Range: <identity>, user headers
                                         // redirects are re-followed on every request: signed
                                         // redirect targets expire mid-download
  resp = send(req)                       // timeout: connect 15s, first byte 30s, idle 30s
  validate(resp):                        // I4
    206: Content-Range is {pos}-{last}/{total} with last <= end-1
         (a server may cap a range: a shorter prefix is accepted and the rest re-requested)
    200 at pos 0 (chunk starts at 0): the whole file. If the response's ETag/Last-Modified
         still match If-Range, the server just ignores ranges: continue, download becomes
         non-resumable (§2.3 downgrade). If they don't, the file changed: identity reset.
    200 past pos 0: identity reset (or §9 Protocol downgrade with several connections)
    a 200 whose Content-Length is below total_size, a Content-Range total that differs,
    or 416: identity reset
  if overlap_pending: compare first bytes (I5)
  stream body:
    for each buffer (64 KiB):
      clamp to [pos, end)                 // I6
      writer.send(pos, bytes)             // I9
      pos += len
      limiter.consume(len)                // §12
      if pos == end and end < total_size: break
      // a chunk ending the file reads on to EOF, so bytes past total_size are seen (I6)
  on EOF before end: reconnect with backoff (§9), same chunk, from pos
  on EOF exactly where a capped 206 said it would end: reconnect at once, not a failure
```

Blind mode (`total_size = None`) and non-resumable downloads send no `Range`. Losing the connection after bytes arrived cannot be resumed there: the download restarts from zero (I4 reset procedure).

The `Writer` batches: it issues `pwrite` immediately, but `fdatasync` + `range_commit` (then a journal sync) at most every `settings.commit_interval` (default 2 s) after the oldest uncommitted write **or** once any chunk has 8 MiB uncommitted, whichever first, and always on pause/cancel/shutdown. Uncommitted window on crash is therefore bounded by ~2 s of throughput. With nothing uncommitted the writer has no deadline.

---

## 8. Completion

1. All chunks report `pos == end`; committed set is `[0, total_size)` (I7).
2. Writer: final `fdatasync`, then append a `checkpoint`.
3. Digest check if applicable (I8).
4. Close the handle. On Windows, release the share lock.
5. `rename(<final>.part, <final>)` — atomic on the same volume, and **never replacing** an existing file (`renameat2(RENAME_NOREPLACE)` on Linux/Android, `renameatx_np(RENAME_EXCL)` on Apple, else hard link + unlink; a plain `rename` would destroy a file that appeared after the name was chosen). If the final name was taken since `Add` (another download finished with the same name), apply the collision rule (§3) at this moment, not earlier, ignoring this download's own `.part` and `.dhow`; if the rename then finds the name taken, take the next one. The chosen name is recorded (DB `final_path`) *before* each rename attempt, so recovery after a crash knows which name was used.
6. `fsync` the directory (POSIX).
7. Delete the sidecar.
8. Create a file token via the adapter (`ARCHITECTURE §6`) and store it.
9. Emit `StateChanged → Completed`, `Renamed`.

Steps 5–7 are the only non-idempotent moment. If the process dies between 5 and 7, the next open finds `<final>` present, no `.part`, and a stale sidecar: the sidecar is deleted and the download is marked complete (the rename is the commit point). If `.part` is still there, the rename didn't happen and completion simply runs again. A `DigestMismatch` leaves `.part` and `.dhow` in place for inspection; nothing appears at the final name (I1).

---

## 9. Errors & retries

| Class | Examples | Behaviour |
|---|---|---|
| Transient | connection reset, timeout, 408, 429, 5xx, DNS failure, body cut short | Retry the *connection* with exponential backoff 1s·2ⁿ⁻¹ after the n-th failure, jitter ±25%, cap 60s. `settings.max_retries` (default 10) is the number of consecutive failed *attempts* per connection that gives up (the 10th failure fails, T-9a); an attempt that wrote at least one byte resets the count, so a server that keeps cutting responses short still finishes (T-I7). Download stays `Active` (shows "reconnecting"). After the cap: `Failed(Transient)`; `Retry` command restarts from the journal. |
| Identity | 200 to a Range (see §7), size mismatch, `Content-Range` not what was asked, 416, overlap mismatch | Reset per I4. Not a retry; a restart. |
| Permanent | any other 4xx (401/403/404/410 …), TLS failure, too many redirects, `NoSpace`, `DigestMismatch`, `ServerSentExtraData` | `Failed(...)` immediately. Journal kept; `Retry` re-probes first (links may be refreshed). |
| Protocol | server ignores `Range` on a *non-first* request after having honored it | Downgrade download to single connection, keep committed bytes, continue. Warn. The refused range is merged back into the chunk it was split from, whose earlier response still covers it; if that connection already passed the split point, there is nothing to continue from and the download resets. |

`429` and `503` honor `Retry-After` if present (cap 5 min).

---

## 10. Politeness scheduler

The scheduler decides which downloads are `Active` and how many connections each may hold. "Politeness" has two audiences, and the user's own network comes first: too many connections on a saturated link or a small NAT router make every download crawl. §10.A protects the local network, §10.B the remote server.

### 10.A Local network

#### 10.1 Global connection budget

`settings.global_connections` (default 16). The sum of all active connections across all downloads never exceeds it. The budget belongs to one engine instance (one process); every download in it holds a ticket and asks it for each connection.

#### 10.3 Allocation rule: oldest first, newcomers ramp

Every download has a connection limit `n` (§4.1) and grows toward it through ramp-up (§4.4), one connection at a time. The budget decides whether it may:

1. Downloads are ordered by `started_at` (oldest first; kept across restarts).
2. **A first connection** needs only the caps: global and per-host room.
3. **One more connection** for a running download needs the caps *and* room beyond the **claims** of every older download. A download that is still ramping claims the connections it may yet open (`n − held`), counted only up to what its host has room for, so a host-capped elder does not hold up other hosts (§10.5). A download stops claiming when its ramp-up ends: saturated, nothing left worth splitting, downgraded, or at `n`.
4. **Newcomers always make progress** if `settings.min_connections_for_new` is 1 (default): a download that can't get its first connection because the budget is spent asks the youngest download holding more than one connection to give one up (on the same host, if it's the host that is full). That download stops its newest connection where it is; the rest of that chunk goes back to its pending work (never a connection still verifying an I5 overlap). The freed slot is reserved for the newcomer. If nobody has a connection to spare (e.g. the per-host cap is reached by downloads holding one each), the newcomer is `Waiting(ConnectionBudget)` until a slot frees. With the setting 0, it waits whenever the budget is spent.

Effect: start 10 downloads on a 16-connection budget → the first one ramps to its limit, the next ones grow only into what the older ones don't claim, the rest crawl at 1 connection each, and as the oldest finish (or stop ramping) the rest ramp up in order. This is the behaviour requested in the brief. (The rule is evaluated whenever a download asks; it replaces the periodic "rounds" of earlier drafts with the same outcome, growth still being one connection per ramp-up window.)

#### 10.6 Adaptive ramp-up and headroom

Ramp-up is throughput-aware (§4.4): a download stops adding connections where the link stops getting faster, and that ceiling is shared for a minute.

`settings.headroom_percent` (default 0, at most 99) keeps part of the link free for everything else on it: the global speed limit (§12) becomes `min(user limit, (1 − headroom) × peak)`. The **peak** is the highest aggregate throughput (all downloads together) over a ramp-up measurement window (§4.4: settled, unlimited). The cap is **lifted while any download is ramping up** (settling or measuring; not while the budget holds its ramp back), and applies in between. Each ramp-up period, i.e. from the first download starting to ramp while none was, measures the peak afresh, so a link that got slower is noticed; one that got faster is noticed at the next ramp-up. Without a measured peak there is no cap. With 0 there is no implicit limit.

Why not the plain "observed peak": throughput under the cap can never exceed the cap, so a peak taken from all traffic would freeze at whatever was seen before the cap first applied (often a single connection's speed), and ramp-up under the cap would see no gain and stop.

### 10.B Remote server

#### 10.2 Per-host cap

`settings.per_host_connections` (default 4). Keyed by `effective_url` host **and port** (`host:port`, the scheme's default port if none is given; eTLD+1 is *not* used: `cdn1.x.com` and `cdn2.x.com` are different hosts, which is usually what the server operator intends, and so are two servers on different ports). The cap is absolute: it also bounds the newcomer guarantee below.

`429` and `503` with `Retry-After` are honored per connection (§9).

#### 10.4 Queue concurrency

Independently, each queue has `max_concurrent` (default 3). A download in a queue that is at capacity has not started: it stays `Queued` until a slot opens, regardless of connection budget. (`Waiting` is for downloads that have started and wait for the connection budget, §10.3.) Queue order is user-defined (`Reorder`), default FIFO: each download gets the next position in its queue when added or moved there.

Which downloads run is re-evaluated when one finishes, is added, or a queue changes; nothing polls in between. Downloads left `Active`/`Waiting`/`Completing` by a process that died count as wanting to run and are resumed in their queue's order (the caller tells the engine which downloads are really running, e.g. by their run locks).

#### 10.5 Fairness within a round

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

- Window times are local wall-clock `HH:MM` or `HH:MM:SS`; `days` are the days a window *starts* on. An `end` at or before `start` runs past midnight into the next day; `start == end` is a whole day. Local times are turned into instants with the current time zone's rules, so a window keeps its local time across DST changes (a start inside a DST gap begins right after it).
- `Start`: downloads in the queue start only inside a window; downloads running at `end` continue.
- `StartAndStopOutside`: downloads in the queue start only inside a window, and are paused at `end` (journal commit, connections closed) and resumed at the next `start`: the runner puts them back in the queue (`Queued`), unlike a user's pause.
- The engine keeps a single `sleep_until(next_edge)` across all queues (`ARCHITECTURE §7`): the earliest window start/end or override expiry of any queue; none if no queue has a schedule or a timed override. Edges are recomputed on every wake, so DST is followed exactly; a time-zone change while asleep takes effect at the next wake (a platform notification can wake it sooner); the adapter's `watch_network` is not involved.
- `StartQueue`/`StopQueue` override the schedule until the next edge (indefinitely for a queue without a schedule). Stopping also pauses the queue's running downloads, which go back to the queue.
- Starting a download by hand (`Resume`, an `Add` that starts now) ignores its queue's schedule.
- The **default queue** exists always, cannot be deleted, has no schedule. Its name is NULL until the user renames it (the shell shows a localized one).
- `DeleteQueue` appends the queue's downloads to `move_items_to` in their order; `MoveToQueue` appends one; `Reorder` places one before another in the same queue, or last.

---

## 12. Speed limiting

Token-bucket limiters at three levels: global, queue, download. A connection must acquire tokens from all applicable buckets before reading the next buffer from the socket (`limiter.consume` in §7 is a blocking acquire across the chain). Bucket refill: 100 ms granularity, burst = 2× the per-interval allotment. Reads are 64 KiB; at limits below 64 KiB/s the read size shrinks to the limit so that small limits are accurate.

Precisely: bytes are debited from every bucket in the chain as they arrive, which may put a bucket in debt; the connection reads again only once no bucket is in debt, sleeping exactly until the refill covers it (no polling). The HTTP library decides the socket's read sizes, so "read size" means what the connection hands to the writer before the next acquire: buffers are split into pieces of at most `min(64 KiB, lowest limit in the chain)`. All bytes received count, including any dropped past a chunk's end (I6). A bucket starts with its burst; when a limit is lowered the bucket keeps its debt and at most the new burst.

- **Global:** `settings.global_speed_limit`, one bucket per engine instance (process), like the connection budget (§10.1), lowered by headroom (§10.6).
- **Queue:** `QueueSpec.speed_limit`, one bucket shared by every download of the queue running in the process.
- **Download:** `AddRequest.speed_limit` / `SetSpeedLimit`.

Changing a limit takes effect within 200 ms: waiting connections are woken at once and pay off at most one tick of debt at the new rate. Setting `0` means "paused by limit", shown as such: the download stays `Active`, its connections stay open and read nothing until the limit changes (a server that drops an idle connection meanwhile is reconnected to, as in §9). Shells show it from the snapshots: the download's, its queue's or the global limit is `0`.

---

## 13. Categories & grouping

### 13.1 Category

Derived at `Add` from the file name's extension first, `Content-Type` second:
`Video | Audio | Image | Document | Archive | Software | Code | Data | Other`. The mapping table lives in `dhow-core/src/naming/categories.toml`, user-extensible via settings (`extra_extensions: {Category: [ext]}`). `storage_breakdown()` sums `total_size` of `Completed` downloads by category and reports whether the file still exists (via path or token). (`ProbeInfo.category`, §2.2, is the same derivation on `suggested_name` at probe time, once a standalone `Probe` exists.)

- At `Add` the name is the one chosen by §3 or the user's `filename`, *before* collision handling: ` (n)` goes before what §3 treats as the extension, which can sit inside a longer extension a user added (`model (2).tar.lz`), so the deduplicated name could categorise differently.
- On `Renamed` (the file was renamed or moved outside Dhow, found via its token, M3.5) the category is derived again from the new name: `x.mkv` renamed to `x.zip` becomes `Archive`.
- Extensions match case-insensitively, longest suffix first (`a.tar.gz`: `tar.gz`, then `gz`); at each length the user's extras beat the built-in table. This is a lookup rule only; §3's stem/extension split (used for ` (n)` and §13.2) is unchanged. An extension is in at most one category; adding it to one removes it from the others. Adding it to `Other` takes a built-in out of its category.
- No known extension: the `Content-Type`'s usual extension (§3's table) is looked up the same way, then its top-level type (`video/*`, `audio/*`, `image/*`); otherwise `Other`.
- Per category, `storage_breakdown()` gives the bytes and count of completed downloads whose file still exists, and the ids of those whose file is gone (not counted in the bytes). Categories with neither are omitted.

### 13.2 Automatic grouping

Goal: `Movie.Part1.rar`, `Movie.Part2.rar`, `Movie.Part3.rar` appear as one group.

Algorithm on each `Add` (and on `Renamed`):
1. Split the final name into stem and extension as §3 does (` (n)` lands in the stem). An all-digit extension (`x.7z.001`) is the sequence, not a type: the whole name is the stem and the extension is empty.
2. Normalize the stem: replace `[._\-\s]+` with a single space and trim; then strip, in this order and each at most once, at the end: a collision or index mark `\(\d+\)` or `\[\d+\]`; `of \d+`; a sequence token, which is 1–4 digits, together with the word right before them if that word is one of `part pt cd disc disk vol episode ep e` or `s\d+e` (so `s01e02`, `CD 2`, `Part3`, but `deep 3` keeps `deep`). Words match case-insensitively and only whole words (after a space or at the start). Five or more digits are not a sequence number (dates, ids). Trim again.
3. Key = normalized stem, lowercased, **plus extension**, lowercased (parts of one subject share an extension; `foo.mp4` and `foo.srt` are related but not a group).
4. A name whose normalized stem is empty, or whose key is shorter than 4 characters (extension and its dot included: `A.part1.rar` → `a.rar` groups, `x2.c` → `x.c` doesn't), never auto-groups.
5. If an existing *auto* group has a member with the same key in the same `directory` → join it. Else if another *ungrouped* download there has the same key, and the user didn't place it (6) → create an auto group containing both. Every download counts, whatever its state.
6. `SetGroup { id, group }` is the user's decision: the target group is marked `auto = false`, and the download is *placed*: auto-grouping never moves it again, not even after `SetGroup { group: None }`. `RenameGroup` also marks the group `auto = false`. Auto-grouping never modifies manual groups.
7. A group a member leaves is deleted when empty; an *auto* group left with one member dissolves (that member becomes ungrouped, not placed). Manual groups keep a single member.
8. On `Renamed`, a member of an auto group that no longer shares its key with any other member leaves it (7), then is grouped again from step 1.

The group's display name is the normalized stem in the original casing of its oldest member (`Movie.Part1.rar` → `Movie`). Groups report aggregate progress, speed and ETA, computed when queried from their members that aren't `Cancelled`: bytes downloaded, total size (unknown if any member's is), speed = sum over running members of the EMA (§14) of their 1 s samples of the last 10 s (none if the newest is older than 3 s), and ETA = what unfinished members have left / that speed, under §14's ETA rules (`None` below 1 KiB/s, or until every running member has 3 samples). `GroupChanged` is emitted for each download whose group changes; on `Add` the new download's group is in its snapshot and the others it joined get `GroupChanged`.

---

## 14. Speed sampling

Per active download, once per second: `bytes_received_in_last_second` (counted at the socket read, before the limiter). Stored in the ring (`ARCHITECTURE §5.1`). `Progress.speed_bps` is a 5-sample EMA; ETA = `remaining / EMA`, reported as `None` when EMA < 1 KiB/s or fewer than 3 samples exist. A global aggregate sample (sum) is kept while any download is active.

Precisely:
- A download samples on a 1 s ticker that exists only while it runs (probing and `Waiting` included, which sample 0). A sample is the bytes received since the previous one (counted on arrival, before any limiter wait, §12) divided by the time since it, so a late tick doesn't inflate it. `at` is the start of the wall-clock second it was taken in (unix ms).
- EMA: α = 2 / (5 + 1), starting at the first sample. ETA = `ceil(remaining / EMA)` seconds. `Progress` carries both, and is also emitted on every sample.
- The global sample for a second is the sum of the samples downloads took in it.
- Ring: a 1 s sample is kept until it is 10 minutes old **and** the hour it falls in has ended (so 10–70 minutes), then that hour's samples become one hourly row per download (and one global row) holding their mean: the average speed while it ran, not over the whole hour. Rollup runs when the engine opens and whenever a further hour becomes due while sampling; hourly rows are kept until the download is removed (global ones indefinitely, a few KB a year).

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
| T-10 | 10 downloads started together, budget 16, per-host 4 | the 6 slots left after the first connections are what the two oldest claim: they ramp to 4 while the other 8 stay at 1 until one of the two gives a connection back; never more than 16 in all or 4 per host. Newcomers arriving with the budget spent still get a connection |
| T-10b | `n = 8`, each connection 500 kB/s, link saturates at 2 MB/s (4 connections) | ramp-up stops at 4 ± 1; the next download starts at that ceiling |
| T-11 | schedule window ends during transfer (`StartAndStopOutside`) | paused with committed journal; resumed at next start |
| T-12 | global limit 100 KiB/s, 3 downloads | aggregate within ±10% over 10 s |
| T-13 | add `A.part1.rar`, `A.part2.rar`, `B.rar` | one auto group of two; `B` ungrouped |
| T-3 | naming table (50+ cases incl. RFC 8187, Windows reserved names, collisions) | exact strings |
| T-idle | complete all downloads, wait 60 s | zero alive tasks, no event emitted, RSS flat |
| T-blind | server with no `Content-Length`, no `Accept-Ranges` | single connection, completes, flagged `SizeUnverified` |

Bit-exactness is asserted with blake3 over the final file versus the server's source.
