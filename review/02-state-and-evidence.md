# peerbackup — review of `src/state.rs` (branch `review/full-audit`)

Scope: `src/state.rs` in full; `src/cli.rs` `status_cmd` / `verdict` / `backup_in` / `verify_in` /
`Runtime::record` as far as needed to see how state is produced and consumed.

Every finding below was confirmed by running the real code (a throwaway copy of the tree in the
scratchpad, `src/state.rs` with extra `#[cfg(test)]` probes appended). The reviewed repo was not
modified — `git status` is clean. Probe output is quoted inline.

---

## CRITICAL

### C1. A `Bad` verdict of one kind is silently discarded whenever a *different* kind produced a more recent `Bad` that has since been cleared
`src/state.rs:315-325`

```rust
let bad = mine.iter().rev().find(|r| r.verdict == Verdict::Bad);
let problem = bad.and_then(|r| {
    let newer_good = mine.iter()
        .any(|o| o.kind == r.kind && o.at > r.at && o.verdict == Verdict::Good);
    ...
});
```

Only **one** `Bad` record is ever examined: the most recent one in the file. The supersession test
is then applied to that single record. Every older `Bad` — including ones of a *different* kind that
have never been superseded — is thrown away without being looked at.

Failure scenario (run, output quoted):

```
day 3  Subset  Good
day 3  Canary  Good
day 2  Subset  Bad   "pack abc does not match its hash"   <-- never superseded
day 2  Canary  Bad   "restored test file did not match"
day 1  Subset  Unknown "peer unreachable"                 <-- Unknown does not supersede
day 1  Canary  Good                                       <-- supersedes the Canary Bad only
now-1h Backup  Good
```

```
CROSS-KIND state=Good problem=None
```

`peerbackup status` prints `alice   ok   1h ago   2d ago   1d ago   1%`, prints **no** problem line,
and exits **0**. The evidence log contains an unrefuted, observed pack-hash mismatch. `last_subset`
is the day-3 Good, which is inside the default 10-day `subset_days`, so freshness does not save it.

This is precisely the property the program exists to guarantee. It is not a corner case: any verify
run in which both the subset check and the canary check fail, followed by a run where the canary
recovers but the subset check cannot be completed (unreachable peer, timeout, lock — all `Unknown`),
lands here. It also silently drops the *second* problem in a two-problem peer even when the state is
correctly `Bad`, so the operator only ever sees one cause.

The fix is to evaluate supersession per kind over all `Bad` records (e.g. for each `Kind`, take the
newest `Bad` and compare against the newest `Good` of that kind), and to report all surviving ones.

### C2. A record with a future timestamp pins a peer green forever; a backwards clock turns the whole dashboard green
`src/state.rs:331-332` (and `src/state.rs:365-376`, `src/state.rs:31-36`)

```rust
let fresh = |t: Option<u64>, window: u64| t.is_some_and(|t| now_ts.saturating_sub(t) <= window);
```

`saturating_sub` clamps to `0` for any `t > now_ts`, so a record dated in the future is
unconditionally "fresh", for any window, forever. Nothing anywhere rejects, clamps or flags a
future `at`; `Runtime::record` writes `now()` verbatim and `last()` takes `.max()`, so a single
future record dominates the maximum permanently.

Probes:

```
FUTURE     state=Good ago=just now     (records dated now + 10 years)
BACKCLOCK  state=Good ago=just now     (records dated correctly, now_ts set back 5 years)
```

Concrete scenarios:

* A Raspberry Pi / VM / container whose clock is ahead (no RTC and `fake-hwclock` restoring a bad
  time, a hypervisor with a skewed TSC, a manual `date` typo). One `backup` + one `verify` cycle
  while the clock is wrong writes `Backup Good`, `Subset Good`, `Canary Good` at, say, 2035. NTP
  then corrects the clock. From that moment `peerbackup status` reports `alice ok / just now / just
  now / just now` and exits 0 **forever**, no matter that every subsequent backup fails and the peer
  holds nothing. Cron is silent in exactly the situation the tool is for.
* The mirror image: the clock is set backwards (dead RTC battery, restoring a VM snapshot, a
  container with `--date`). All existing correct records become "future" relative to `now_ts` and
  every peer immediately reads `ok`.
* Degenerate: `now()` returns `0` when `duration_since(UNIX_EPOCH)` fails (`state.rs:35`), i.e. any
  clock set before 1970. `now_ts == 0` makes every record future ⇒ everything green, and
  `status_cmd`'s `t.saturating_sub(window)` becomes `0` so the entire log is read and reported as
  "just now".

Suggested handling: treat `at > now_ts + small_skew` as not-fresh (or as its own visible
"clock skew" state), and refuse to write records whose timestamp is older than the newest record
already in the log.

---

## HIGH

### H1. The evidence log is never fsynced, so the one verdict the code documents as unlosable is exactly the one a power cut loses
`src/state.rs:165-176`, consumed at `src/cli.rs:120-148`

```rust
let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
...
f.write_all(line.as_bytes())        // no sync_data, no sync_all, no parent dir fsync
```

`Runtime::record` goes to considerable trouble over write *errors* for a `Bad` verdict
(`cli.rs:135-147`: "The next `status` would call this peer healthy"), but a successful `write_all`
only reaches the page cache. On ext4 defaults that is up to ~30 s of exposure.

Scenario: `verify` restores the canary, it does not match, the `Canary Bad` record is written,
`verify` prints `DOES NOT MATCH` and exits 1. The machine loses power 3 seconds later (this is a
backup tool; the machines it runs on are the ones people lose). After reboot the last page of
`evidence.jsonl` is gone. The next `status` sees the previous day's `Canary Good`, reports `ok`, and
exits 0. The damage was observed, reported to the screen, and then un-remembered.

Same gap on the create path: `create_dir_all` + `create(true)` with no directory fsync, so on a
fresh install the file itself can vanish.

`sync_data()` on the file (and, for the first append, on the parent directory) is one line and this
file is written a handful of times a day.

### H2. Any evidence line longer than the 64 KiB block is silently dropped by `read_since`
`src/state.rs:226-232`

```rust
let split = (pos > 0).then(|| buf.iter().position(|b| *b == b'\n')).flatten();
let (keep, lines): (Vec<u8>, &[u8]) = match split {
    Some(i) => (buf[..i].to_vec(), &buf[i + 1..]),
    None    => (Vec::new(), &buf[..]),        // <-- carry silently discarded
};
```

When a backwards block contains **no** newline at all (which is exactly the situation "this whole
block is the middle of a line that starts further left"), the `None` arm treats the block plus the
accumulated `carry` as complete lines, fails to parse them, and then resets `carry` to empty at
`state.rs:245`. The right-hand half of the record is thrown away, so when the left-hand half is
finally reached it can never be rejoined. The record is lost.

A record spanning exactly two blocks survives (the first block still contains the terminating
newline), which is why the existing `records_spanning_a_block_boundary_are_not_lost_or_duplicated`
test passes — it uses ~130-byte records. A record spanning three or more blocks (> ~64 KiB of line)
does not. Probe: three records written, the middle one with a 200 000-byte `detail`:

```
LONGLINE read 2 of 3 records: [100, 300]
```

Reachable: `backup_in` (`cli.rs:512-522`) records `Some(e.to_string())` where `e` is an
`EngineError` whose `message` is `strip_go_trace(combined_output(out))` — the **entire** stdout +
stderr of `restic backup --json` (`engine/restic.rs:109-125`, `engine/mod.rs:92-95`). A failed
backup over a tree with many unreadable files emits one JSON line per file plus periodic status
messages; megabytes is normal. That whole blob becomes a single JSONL line (JSON escapes the
newlines), and that record then never appears in `status` again — so `status`'s "last attempt did
not succeed: <reason>" hint (`cli.rs:726-732`, via `last_failure`) goes missing precisely for the
noisiest, most broken peer.

Two independent fixes are needed: keep the carry in the `None` arm (`(buf, &[][..])` semantics), and
bound `detail` at write time (a few KB is plenty; the log is meant to be read by a human).

### H3. One out-of-order timestamp truncates the entire readable history
`src/state.rs:240-242`

```rust
if r.at < oldest { break 'blocks; }
```

The backwards walk stops at the first record older than the cutoff, on the documented assumption
that the file is written in time order (`state.rs:186-187`). One record written while the clock was
wrong breaks that assumption permanently, and the break discards everything to its left — not just
that record.

Probe: six records, the fourth written while the RTC read 2001, the rest correct, read with the
default 70-day window:

```
CLOCKSTEP read 2 of 6: [1800010800, 1800014400]
```

Four records vanish from `status`'s view for the rest of the file's life. Removing records is mostly
the safe direction (a peer reads `unchecked`), but it is not always: combined with C1 it removes
`Bad` records from consideration, and it makes the "last attempt did not succeed" diagnosis and the
`coverage_pct` column silently wrong. It also means the append-only log's central promise — "the
history of what was checked and when is worth more than any single result" (`state.rs:7-9`) — is not
actually kept.

A cheap fix: continue past an out-of-order record instead of breaking, and only break after N
consecutive records older than the cutoff (or cap the walk by bytes rather than by content).

### H4. Peer-controlled text reaches the terminal unescaped; a hostile peer can repaint the status table
`src/state.rs:147-148` (`detail`), printed at `src/cli.rs:718-732`

```rust
println!("{}: {}", r.name, r.problem.as_ref().unwrap());   // cli.rs:720
println!("  {reason}");                                     // cli.rs:730
```

`detail` is filled from restic's output (`engine/restic_error.rs` `first_line(&clean)`, and
`EngineError::message` = full combined output) with no control-character filtering anywhere in the
chain. JSON-serialising into the log escapes ESC as `<ESC>`, and deserialising restores the raw
byte — confirmed:

```
ANSI on disk: {"at":1,...,"detail":"\u001b[2K\u001b[1A\u001b[2Kalice        ok          just now",...}
ANSI round-tripped has ESC: true
```

`PeerName` is validated to `[A-Za-z0-9_-]` (`config.rs:119-141`), so the name column is safe; `detail`
is not. A peer running a modified REST server controls the HTTP reason phrase, which restic prints
verbatim in `unexpected HTTP response (500): <status line>` and which classify() copies into
`Cause::Unreachable{detail}` / `Cause::Unclassified{detail}`. Also reachable from filenames: restic's
plain-text stderr warnings during `backup` embed source paths, and an attacker-writable directory
inside `sources` (a downloads or upload folder) lets a filename carry ESC.

The problem lines are printed *after* the table, so `\x1b[1A\x1b[2K` (cursor up + erase line) or a
bare `\r` plus padding erases or rewrites the `FAILED` row the operator is looking at. The exit code
is unaffected, so scripted use is safe; the human reading the terminal is the target, and this
program's entire value is what that human believes about the table.

Escape or strip C0/C1 control characters at the point of writing `detail` (best: at record
construction, so the log itself is clean) and again when printing.

---

## MEDIUM

### M1. `Canary::save_at` is non-atomic and unsynced, and `load_or_create_at` silently regenerates on any error — producing a false `FAILED`
`src/state.rs:87-95`, `src/state.rs:101-106`

```rust
pub fn save_at(&self, p: &Path) -> std::io::Result<()> {
    ...
    fs::write(p, serde_json::to_vec_pretty(self)...)     // truncate + write, no temp+rename, no fsync
}

pub fn load_or_create_at(dir, manifest) -> ... {
    match Self::load_at(manifest) {
        Ok(c) => Ok(c),
        Err(_) => Self::create_at(dir, manifest),        // reason discarded
    }
}
```

`config.rs:274-320` already has exactly the helper this needs (`write_private`: temp file, fsync,
rename) and documents why (`fs::write` truncates first, so a crash mid-write leaves a half-file).
The canary manifest does not use it.

Failure chain: a crash or a full disk during `save_at` leaves `canary.json` truncated. The next
`backup` calls `load_or_create_at`, `load_at` fails, and `create_at` **regenerates all three canary
files with fresh random content and fresh digests** — silently, with the cause of the failure
discarded. If that backup then fails (peer unreachable, out of space, upload aborted), the peer's
newest snapshot still holds the *old* canary bytes while the local manifest holds the *new* digest.
The next `verify` restores the old bytes, compares against the new digest, and records
`Kind::Canary, Verdict::Bad, "restored test file did not match what was sent"` (`cli.rs:640-651`).
`status` then prints **FAILED** with the single most alarming message the product can emit, for a
purely local cause, and exits 1.

A false red is not merely annoying here: it is the same failure mode as a false green one iteration
later, because an operator who has been shown a phantom corruption learns to discount the red.

Fix: atomic + synced manifest writes; log the reason when falling back to regeneration; and refuse
to compare against a digest recorded after the newest snapshot's timestamp.

### M2. The canary manifest is never reconciled with the files on disk
`src/state.rs:81-85`, `src/state.rs:101-111`

`load_at` does no validation at all: it does not check that `files[].path` still exists, and it does
not re-hash the files against the recorded `sha256`. Nothing in the codebase ever does.

Consequences:

* If the local `canary/canary-0.bin` is modified (a stray edit, a bad block on the source disk, a
  restore of the state dir from a different machine), `backup` uploads the *new* bytes while the
  manifest keeps the *old* digest. `verify` restores exactly what it just uploaded, the digests
  disagree, and the peer is reported as `FAILED — restored test file did not match what was sent`.
  A local disk problem is reported as remote data corruption, against the wrong peer.
* If the canary *files* are deleted but the directory and manifest remain, `load_or_create_at`
  succeeds (manifest parses), `check_sources` passes (the directory exists and is readable), the
  backup succeeds carrying no canary, and every subsequent `verify` records `Canary Unknown`
  ("could not restore"). The peer ages into `unchecked` and never explains why. Note the deleted-
  *directory* case is caught by `check_sources`; the deleted-*files* case is not.

`load_or_create_at` should verify the recorded digests against the files on disk and regenerate (or
error) when they disagree, rather than trusting the manifest.

### M3. `read_since` cannot distinguish "no history" from "could not read the history"
`src/state.rs:199-219`

```rust
let Ok(mut f) = fs::File::open(path) else { return Vec::new(); };
let Ok(len) = f.seek(SeekFrom::End(0)) else { return Vec::new(); };
...
if f.seek(SeekFrom::Start(pos)).is_err() { break; }
let mut buf = vec![0u8; take];
if f.read_exact(&mut buf).is_err() { break; }
```

The signature is `-> Vec<Record>`; every I/O failure yields a silently *partial* result that the
caller cannot tell from a complete one. `status_cmd` (`cli.rs:697`) consumes it directly.

Scenario: the state directory sits on a failing disk (the reason someone runs this program). Block 1
— the newest 64 KiB — reads fine and contains yesterday's `Backup Good`, `Subset Good`,
`Canary Good`. Block 2 hits a bad sector and `read_exact` fails; the loop `break`s. The unsuperseded
`Subset Bad` that lives in block 2 is never seen, `problem` is `None`, and `status` reports `ok` and
exits 0. A read error on the evidence log is itself evidence that something is wrong and must not
resolve to green.

Also: `EPERM` on the evidence file (an install that once ran under `sudo`, leaving a root-owned
`evidence.jsonl`) makes `read_since` return empty. All peers then read `unchecked` with no
explanation of why the history disappeared.

Return `io::Result<Vec<Record>>` (or a `(records, truncated: bool)`) and let `status` refuse to say
`ok` on a partial read.

### M4. The `status` read window ignores `liveness_hours`, so a long backup interval reports "no backup at all"
`src/cli.rs:696-697`, consumed by `src/state.rs:331-343`

```rust
let window = cfg.settings.canary_days.max(cfg.settings.subset_days) * 86400 * 2;
let records = Evidence::read_since(&rt.evidence(), t.saturating_sub(window));
```

`liveness_hours` is not part of the max, but `status` compares `last_backup` against it
(`state.rs:295, 336`). `Settings::validate` (`config.rs:57-81`) permits `liveness_hours` up to a
century independently of the day windows.

Scenario: monthly backups of a large archive, weekly verification —
`liveness_hours = 720`, `subset_days = 7`, `canary_days = 7`. Window = 14 days. A perfectly healthy
`Backup Good` recorded 20 days ago is inside the 30-day liveness window but outside the read window,
so `last_backup` is `None`. `partition_unknown` (`cli.rs:758-764`) then classes the peer as *never
backed up*, `status` prints **"No backup has reached: alice. Run `peerbackup backup`."** and
`verdict` exits 1 with "1 peer(s) hold no backup at all" — about a peer that holds a fine 20-day-old
backup. The direction is safe but the message is flatly untrue, and the recommended action is wrong.

Include `liveness_hours * 3600` in the window, or have `validate` reject a liveness window wider than
the day windows.

---

## LOW

### L1. State files are created world-readable; nothing in `state.rs` sets a mode
`src/state.rs:63-95` (canary), `src/state.rs:165-176` (evidence)

Measured on this machine (umask 022):

```
PERMS evidence=644 dir=755  canary.json=644  canary-0=644
```

`config.rs:274-320` deliberately writes the config and the repository passwords at 0600 and explains
at length why creating-then-chmodding is wrong. The evidence log next to it gets the process umask.
It is not writable by others (so records cannot be forged), but it is readable: peer names, the
exact backup and verification schedule, snapshot ids, and restic's error text — which is the same
text noted in H4 as coming from a peer and from source-tree filenames — are exposed to every local
account. A default umask of 002 (Debian/Ubuntu with per-user groups, and common in containers) makes
it group-writable, at which point records *can* be forged.

Open the evidence log with `.mode(0o600)` and create the state directory 0700, for the same reason
`config.rs` already does.

### L2. `now()` swallowing a clock error as `0` is a silent green
`src/state.rs:31-36`

```rust
.unwrap_or(0)
```

Covered under C2, listed separately because the fix is independent: a clock before the epoch is a
condition worth refusing on, not defaulting on. As written it produces both bogus records (`at = 0`)
and, if it happens during `status`, a fully green dashboard.

### L3. `coverage_pct` reports a stale figure without any indication that it is stale
`src/state.rs:345-349`, printed at `src/cli.rs:711-715`

The column is the `coverage_pct` of the newest `Good` subset record *in the window*, with no
freshness condition of its own. A peer whose last successful subset check was 9 days ago and whose
state is `unchecked` still prints `100%` under `READ BACK`, on the same row as the word `unchecked`.
The reasonable reading of that row is "100% of it was read back and we are unsure about something
else". Also, if the newest `Good` subset happens to carry `coverage_pct: None` (e.g. a record from an
older version), `and_then` collapses to `-` even though an older record has a real figure —
"unknown coverage" and "no coverage recorded" print identically.

### L4. `verify` writes `Subset` and `Canary` records in one second; a same-second `Bad` is never cleared by that second's `Good`
`src/state.rs:319`

`o.at > r.at` is strict, and `now()` has one-second resolution. Two verify runs landing in the same
second (a retry loop, a fast local test peer) with `Bad` then `Good` of the same kind leave the peer
`FAILED` until the next run:

```
SAMESEC state=Bad
```

The direction is safe (never green), and the next verify clears it, so this is a nit in practice —
but it is the reason a supersession rule should compare (timestamp, file position), not timestamp
alone. Worth noting that fixing C1 must not accidentally flip this to the unsafe direction by using
`>=`.

---

## NITS

* `src/state.rs:212` — `BLOCK.min(pos as usize)`: `pos` is a `u64` file offset truncated to `usize`.
  Harmless on 64-bit; on a 32-bit target an evidence log over 4 GiB reads the wrong offsets.
* `src/state.rs:69` — `fs::File::open("/dev/urandom")` hard-codes a Linux path and produces a bare
  `No such file or directory` inside a minimal container or a chroot without `/dev`. `getrandom` is
  already an indirect dependency; failing here fails `init` with no explanation of what a canary is.
* `src/state.rs:66` — the canary is a fixed 3 × 64 KiB and is never rotated. Once created it is the
  same bytes forever, so what `verify` proves is "the peer can still return this one old file",
  which weakens over time relative to the rest of the repository. `restore_canary`
  (`cli.rs:667-681`) only ever checks `canary.first()` — files 1 and 2 are uploaded on every backup
  and never read back.
* `src/cli.rs:700-716` — `{:<12}` on the peer name does not truncate (Rust padding never truncates),
  so a 64-character peer name (allowed by `PeerName`) shifts every subsequent column right and
  destroys the table alignment for all rows. Nothing is hidden, but a script parsing by column
  breaks. Either compute the width from the longest name or truncate explicitly.
* `src/state.rs:145` — `Kind` and `Verdict` deserialise strictly, so a record written by a future
  version with an unknown `kind`/`verdict` is dropped by the `else { continue }` at
  `state.rs:235-239` rather than being preserved as "unrecognised". The comment there says this is
  intentional; the direction is unsafe if a future version ever introduces a new failure verdict,
  since an older binary would silently un-see it. A `#[serde(other)] Unrecognised` arm mapping to
  `Unknown` would be strictly safer.

---

## What is already correct, and worth not regressing

* `Verdict::Unknown` never contributes to freshness (`state.rs:306-311`) and never turns a peer red
  — the `a_peer_that_is_full_must_not_look_ok` scenario test pins this and it holds.
* `PeerName` validation (`config.rs:119-141`) keeps names out of the ANSI-injection surface and out
  of path traversal.
* `Settings::validate` bounds the window multiplications well clear of `u64` overflow, and rejects
  `0`, so `liveness_hours * 3600` and `days * 86400` in `status` cannot wrap.
* A truncated final line costs one record and not the history (`state.rs:234-239`) — the resilience
  claim holds for the truncated-tail case specifically; H2 is a different case.
* `verdict` (`cli.rs:781-812`) reaching the exit code for *all three* unknown shapes, including
  "never backed up", is right and is the kind of thing that is easy to regress.
