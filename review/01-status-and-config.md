# Status, config and CLI

Reviewer scope: `src/cli.rs`, `src/config.rs`, `src/main.rs`, and the status verdict
they compute from `state.rs`. Every finding below was reproduced against the built
binary driven at a temporary `PEERBACKUP_CONFIG_DIR` / `PEERBACKUP_STATE_DIR`.

## CRITICAL-0 `status` reports "ok" and exits 0 with unresolved corruption in the log
src/state.rs:315-325.
```rust
let bad = mine.iter().rev().find(|r| r.verdict == Verdict::Bad);
let problem = bad.and_then(|r| {
    let newer_good = mine.iter().any(|o| o.kind == r.kind && o.at > r.at && o.verdict == Verdict::Good);
    (!newer_good).then(...)
});
```
Only the single most recent Bad record is ever examined. An older Bad of a *different* kind that
nothing has superseded is never looked at. Compounding it, `last(kind)` takes the max timestamp
among **Good** records only, so a Good subset check that predates a Bad one still counts as fresh.

Verified evidence log (defaults, all timestamps within the windows):
```
n-2h00m  subset  good
n-1h00m  subset  bad    "pack 4f2a hash mismatch"   <-- never superseded
n-0h30m  canary  bad    "transient restore failure"
n-0h10m  canary  good                                <-- supersedes only the canary Bad
n-0h05m  backup  good
```
Actual output:
```
PEER         STATE       BACKED UP    CHECKED      TEST FILE    READ BACK
alice        ok          5m ago       2h ago       10m ago      1%
exit=0
```
Observed data corruption, one hour old, reported as healthy and silent from cron. This is the
single failure mode the product exists to prevent.

Two weaker variants also confirmed:
 - subset Bad + canary Bad on day 1, canary Good on day 2, subset check times out on day 2
   -> "unchecked", the pack-hash-mismatch detail never printed at all.
 - the same with the corruption never re-checked -> "Not checked recently. Run `peerbackup verify`."

Fix: evaluate each Kind independently. A peer is Bad if, for any kind, the most recent record of
that kind is Bad. `problem` should collect every unsuperseded Bad, not just the newest one. Freshness
per kind should also consider whether the newest record of that kind is Bad, rather than reaching
past it for an older Good.

## HIGH-1 status drops evidence it needs: liveness_hours is not in the read window
src/cli.rs:696 `let window = cfg.settings.canary_days.max(cfg.settings.subset_days) * 86400 * 2;`
`status()` (src/state.rs:293) judges last_backup freshness against `liveness_hours * 3600`,
but that term is missing from the window used to bound the evidence read.
Repro: liveness_hours=8760, subset_days=2, canary_days=2, one good backup record 30 days old.
Window = 4 days, so the record is never read. Output: "BACKED UP: never" and
"No backup has reached: alice. Run `peerbackup backup`." -- a false claim that the peer
holds none of your data.
Fix: window = max(liveness_hours*3600, subset_days*86400, canary_days*86400) * 2.

## HIGH-2b re-using a peer name inherits the old peer's clean bill of health
Evidence records are keyed on the peer name alone (src/state.rs:141 `Record.peer`), never on the
repository the name pointed at. `peer remove alice` deliberately keeps the evidence
(src/cli.rs:376-408), and `peer add alice <a-different-friends-url>` then adopts it.
Verified: an evidence log holding backup/subset/canary Good from an hour ago, with `alice` in
config now pointing at a brand-new empty server, prints
```
alice        ok          60m ago      58m ago      56m ago      1%
exit=0
```
`peer add` itself prints "No data has been sent yet"; `status` contradicts it one command later.
TODOS.md anticipates marking removed-peer evidence "historical" but `peer remove` already ships.
Fix: record the repository identity (URL, or restic's repository id) on each Record and ignore
records whose identity does not match the peer's current URL.

## HIGH-2c nothing detects a host that rolls its repository back
`verify` restores the canary from whatever the host lists as newest
(src/cli.rs:667-680, `list_snapshots().into_iter().next()`), and never compares the peer's snapshot
list against the local record of what was sent. `status` never contacts a peer at all
(src/state.rs:288-291, by design).

Because the canary never rotates (LOW-12: `load_or_create_at` only creates when the manifest is
missing, src/state.rs:99-105), the same three blobs satisfy the canary check in *every* snapshot
ever taken. A host who restores their disk from an old image, or deliberately reverts the
repository to last month's state, therefore passes:
 - `restic check --read-data-subset` -> the old repository is internally consistent -> Good
 - canary restore -> the unchanged canary is present in the old snapshot -> Good
 - `status` -> reads only local evidence, which still says "backed up 2h ago" -> `ok`, exit 0

Every recent backup is gone and nothing reports it until a restore. This is the one attack the
append-only flag on the host is meant to bound, and peerbackup has no client-side check that would
notice it failing.
Fix: rotate the canary on every backup and record the expected digest per snapshot, and have
`verify` assert that the peer still lists the snapshot id peerbackup recorded for its last backup.
Both are cheap; either alone closes most of it.

## HIGH-2 an unresolved corruption report silently downgrades from FAILED to unchecked
Same line. A Bad record older than the window is not read, so `problem` is None.
Repro (defaults, window 70d): a subset record verdict=bad, detail="pack 4f2a hash mismatch",
80 days old.
  80 days old -> "alice unchecked", error "no peer has been confirmed good"
  60 days old -> "alice FAILED",    error "one or more peers reported a problem", detail shown
Observed damage that nothing has superseded should not expire. Covered by the HIGH-1 fix only
partially; the Bad record needs a separate unbounded lookup, or the window must be driven by
the oldest unresolved Bad.

## MEDIUM-3 peer-controlled text is printed to the terminal unescaped
src/cli.rs:719-721 prints `r.problem` verbatim; the string is restic's error text, which can
carry a remote server's response body. Verified: a detail field containing
`\x1b[2K\r` + a forged table row repaints the screen with a fake "alice ok" line.
A malicious or compromised host can make `status` look healthy.
Fix: strip/escape C0 controls and ESC from `detail` on the way in (or on print).

## MEDIUM-4 every setting is mandatory, and the error calls valid TOML invalid
src/config.rs:24-38 `Settings` fields carry no `#[serde(default)]`.
Repro: config.toml containing only `[settings]` + `sources = ["/tmp"]`
  -> "config.toml is not valid TOML: TOML parse error at line 1, column 1 ... missing field `upload_limit_kib`"
The TOML is valid; the message says otherwise and points at the section header.
`Settings::default()` already exists. Every field should default, and a genuinely missing
required value should be reported as ConfigError::Invalid, not Parse.

## MEDIUM-5 `init` writes a config that breaks the documented hand-edit
`toml::to_string_pretty` emits `peer = []` as the FIRST line of a fresh config.toml.
The README's Configuration section shows the user adding a `[[peer]]` block by hand.
Doing that on a freshly-initialised file yields:
  "invalid table header / duplicate key `peer` in document root"
Fix: `#[serde(default, rename = "peer", skip_serializing_if = "Vec::is_empty")]` on Config::peers.

## MEDIUM-6 PeerName's Display ignores format width, so every table is misaligned
src/config.rs:142-146 `f.write_str(&self.0)` bypasses the formatter's fill/width.
`{:<12}` at src/cli.rs:370, 706 therefore does nothing. Verified output:
  PEER         STATE       BACKED UP ...
  alice unchecked   never  ...
Headers are &str and do pad, so header and rows never line up. Same class in
`impl Display for SnapshotId` (src/engine/mod.rs:41) and `EngineError` (src/engine/mod.rs:93),
which use `write!(f, "{}", ..)`.
Fix: `f.pad(&self.0)`. Also clamp/elide names longer than the column (names may be 64 chars).

## MEDIUM-7 `recovery export --out` silently creates a missing mount point
`write_private` calls `create_dir_all(parent)`. `peerbackup recovery export --out /mnt/usb/recovery.txt`
with the stick not mounted creates /mnt/usb on the root filesystem, writes the passwords there,
records the fingerprint, and prints "Written to /mnt/usb/recovery.txt". The user believes the
only copy of their decryption keys is on removable media. It is on the disk they are backing up.
Fix: for an explicit --out, require the parent directory to already exist.

## MEDIUM-8 the recovery file omits --cacert from the commands it tells you to run
src/cli.rs:930-946. When peer.ca_cert is set the file says "copy this file too" but the printed
`restic -r ... snapshots` / `restore` commands have no `--cacert`. Someone recovering onto a fresh
machine with a self-signed host cert gets a TLS error and no instruction. The file's whole purpose
is to work without peerbackup.

## LOW-9 the recovery file dates itself with a raw Unix timestamp
src/cli.rs:943 `Exported: 1788934589`. This document is meant to be printed and read years later.

## LOW-10 recovery.fingerprint is written world-readable and without fsync
src/cli.rs:947 uses `std::fs::write`; recovery.txt beside it uses write_private (0600).
The fingerprint is SHA-256 over name||url||password for every peer. Not practically attackable
against 32-char tokens, but it is derived from secrets and should match its sibling's handling.

## LOW-11 the secrets directory is 0755
`create_dir_all` uses the umask. Files inside are 0600, so contents are safe, but peer names are
enumerable by any local user. Create `secrets/` with 0700.

## LOW-12 the canary never rotates, so its evidence weakens over time
src/state.rs:99-105 `load_or_create_at` only creates when the manifest cannot be loaded. The same
three 64KiB blobs are backed up forever; restic deduplicates them, so every snapshot references the
blob written during the first backup. Restoring it proves that one old blob survives, not that
recent backups do. README ("a small test file included in every backup") reads stronger than what
is checked. Either rotate the canary per backup or state the limitation.

## NIT-13 fingerprint(&cfg) is computed twice in recovery_export (src/cli.rs:943, 947), re-reading every secret.
## NIT-14 `ago()` on a future timestamp (clock skew, or a peer record written before an NTP step back)
saturates to "just now" and counts as fresh until wall clock catches up. src/state.rs:361.
## NIT-15 unknown keys in config.toml are accepted and then erased by the next config-writing command
(no deny_unknown_fields; Config::save re-serialises). Comments in a hand-edited config are also lost.
Verified: a `# keep this comment!` line and a `some_future_key` both vanished after `peer remove`.
