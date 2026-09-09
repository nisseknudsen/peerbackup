# Summary

The tree is unusually disciplined. There is no command injection, no shell
interpolation, no path traversal, no unguarded panic outside tests; `PeerName`
carries its validation in the type, the repository password goes to restic in a
file rather than argv or the environment, subprocess pipes are drained on
dedicated threads, and `parse_size`, `random_token` and the `statvfs` bindings
are all correct. Clippy and `cargo fmt` are clean and 123 unit tests pass.

The defects are almost all in one place, and they all point the same way.

## The one thing to fix first

**`peerbackup status` can print `alice ok` and exit 0 while the evidence log
holds an unrefuted `pack 4f2a hash mismatch` from an hour ago.** Reproduced.
Two reviewers found it independently, from opposite ends of the code.

`status` looks at the single most recent `Bad` record and clears it if a newer
`Good` **of that same kind** exists. An older `Bad` of a *different* kind that
nothing has superseded is never examined. Because `last(kind)` also takes the
newest **Good** timestamp, a good subset check that predates a bad one still
counts as fresh:

```
n-2h00m  subset  good
n-1h00m  subset  bad    "pack 4f2a hash mismatch"   <- never superseded
n-0h30m  canary  bad    "transient restore failure"
n-0h10m  canary  good                                <- clears only the canary Bad
n-0h05m  backup  good
```
```
PEER         STATE       BACKED UP    CHECKED      TEST FILE    READ BACK
alice        ok          5m ago       2h ago       10m ago      1%
exit=0
```

This is reachable from an ordinary sequence: one verify where both checks fail,
then one where the canary recovers and the subset check merely times out.

Five further defects converge on the same outcome — observed damage that never
turns the dashboard red:

| | Where | What |
|---|---|---|
| C2 | `state.rs:331` | A future-dated record is "fresh" forever, so one clock skew pins a peer green permanently |
| H1 | `restic.rs:112` | Damage seen during the canary restore is relabelled `Unclassified`, so it records `Unknown` and `verify` exits 0 |
| H2 | `restic_error.rs:119` | One non-fatal `retrying after 1s: unexpected EOF` line outranks `repository contains errors` |
| H1s | `state.rs:165` | The evidence append is never fsynced, so a power cut loses the `Bad` record that was just printed to the screen |
| M3s | `state.rs:199` | A read error on the evidence log returns a silently partial history, and partial reads resolve to green |

Fixing the top item alone is not enough. The three-state model is right; the
places that collapse it collapse it in the reassuring direction.

## Severity index

Each entry links to the reviewer's full write-up, with the failure scenario.

### CRITICAL

| ID | Area | Claim |
|---|---|---|
| [C1](02-state-and-evidence.md) / [CRITICAL-0](01-status-and-config.md) | `state.rs:315` | Unresolved corruption of one kind is discarded when a different kind's Bad was later cleared. **Reproduced** |
| [C2](02-state-and-evidence.md) | `state.rs:331` | `saturating_sub` makes a future-dated record permanently fresh; a skewed clock or `now() == 0` greens the whole dashboard. **Reproduced** |

### HIGH — false green

| ID | Area | Claim |
|---|---|---|
| [H1](03-engine.md) | `restic.rs:112` | `Classified::Damage` → `Cause::Unclassified` on the canary restore path, the only place bytes are read back and compared |
| [H2](03-engine.md) | `restic_error.rs:119` | Transport rules match anywhere in the combined output and sit above the `check failed` damage rule |
| [H1](02-state-and-evidence.md) | `state.rs:165` | No `sync_data` on the evidence append; the Bad verdict is the one a crash loses |
| [H2](02-state-and-evidence.md) | `state.rs:226` | A JSONL line spanning 3+ blocks is silently dropped; `detail` holds restic's entire output, which is megabytes for a failing backup. **Reproduced** |
| [H3](02-state-and-evidence.md) | `state.rs:240` | One out-of-order timestamp permanently truncates the readable history. **Reproduced** |
| [M3](02-state-and-evidence.md) | `state.rs:199` | `read_since` cannot distinguish "no history" from "could not read it"; a bad sector reads as green |
| [HIGH-1](01-status-and-config.md) | `cli.rs:696` | `liveness_hours` is missing from the read window, so a healthy old backup reports as "no backup at all". **Reproduced** |
| [HIGH-2](01-status-and-config.md) | `cli.rs:696` | An unresolved corruption report downgrades FAILED → unchecked once it ages past the window. **Reproduced** |
| [HIGH-2b](01-status-and-config.md) | `state.rs:141` | Evidence is keyed on the peer name alone, so re-using a name adopts the old peer's clean record. **Reproduced** |
| [HIGH-2c](01-status-and-config.md) | design | Nothing detects a host that rolls its repository back: the canary never rotates, and `status` never contacts a peer |
| [M4](03-engine.md) | `restic.rs:249` | An empty repository passes `check` and records `Subset/Good`; `Indeterminate` never affects the exit code, so a month of unreachable nights exits 0 every night |

### HIGH — security

| ID | Area | Claim |
|---|---|---|
| [H3](03-engine.md) | `restic.rs:69` | The credentialed repository URL goes to restic in argv, readable via `/proc/<pid>/cmdline` for the whole ~17h of a seed |
| [H4](03-engine.md) | `restic.rs:121` | restic error text embeds that URL and is printed and appended to a 0644 `evidence.jsonl`; `redact()` exists and is never applied |
| [H4](02-state-and-evidence.md) / [MEDIUM-3](01-status-and-config.md) | `cli.rs:718` | Peer-controlled `detail` reaches the terminal unescaped; `\x1b[2K\r` repaints the status table with a forged `ok` row. **Reproduced** |
| [H1](04-host.md) | `server.rs:111` | `DRY_RUN` is ignored by the entire host server path, including two `docker rm -f` |
| [H2](04-host.md) | `mod.rs:82` | `FORCE=1` — unnamespaced, undocumented, no CLI equivalent — skips the type-the-name confirmation on the command that destroys a friend's backups |
| [H3](04-host.md) | `grant.rs:216` | Nothing links a provisioned grant to the server's `/data`; following the README leaves the quota unenforced while `host list` prints `mounted` |
| [M6](04-host.md) | `grant.rs:296` | `chown mnt` to an unprivileged uid plus `create_dir_all` with no symlink check lets a planted symlink mount ext4 over `/etc` |

### HIGH — deployment and documentation

| ID | Area | Claim |
|---|---|---|
| [C1](05-deploy-and-docs.md) | `README.md:54` | Every documented raw `docker run` / `compose up` start fails on a fresh machine: dockerd creates the missing bind source `root:root`, the container runs as `$(id -u)`, and the server dies on `/data/.htpasswd`. `docker run -d` still exits 0, so the README's `&&` chain runs `create_user` against a corpse. **Reproduced.** CI is green only because the test scripts pre-create the directory |
| [C2](05-deploy-and-docs.md) | `peerbackup-rest.service:37` | `host guard` refuses with zero grants unconditionally, so the systemd service can never start for anyone who used `quickstart` — and `PB_DATA` in the unit points somewhere else than their peers' repositories |
| [S1](05-deploy-and-docs.md) | `README.md:33` | The default deployment publishes plaintext HTTP basic auth on `0.0.0.0` and the README says "forward port 51515" with no adjacent warning. The quickstart transcript in the README truncates away the tool's own "use TLS" line |
| [S2](05-deploy-and-docs.md) | `README.md:231` | The TLS recipe cannot work as written — the certs go to one path and the volume you uncomment reads another, and the documented `cp` yields a key the container cannot read. The only mitigation for S1 never lands |
| [S7](05-deploy-and-docs.md) | `tests/docker.sh:10` | Caller-supplied paths are `rm -rf`'d unvalidated, one of them through a root container. `WORK=$HOME ./tests/docker.sh` wipes the home directory; CI runs the spike under `sudo` |

### MEDIUM and below

Twenty-six MEDIUM and thirty-odd LOW/NIT findings are in the per-area files. The
ones most likely to bite a real user:

- **`restore_all` shares the canary's 1800s timeout** ([M1](03-engine.md)). A
  300GB disaster restore is SIGKILLed at 30 minutes with a partial target. The
  backup in the other direction is deliberately unbounded for exactly this reason.
- **A truncated `canary.json` silently regenerates the canary** ([M1](02-state-and-evidence.md)),
  and the next verify reports `FAILED — restored test file did not match` for a
  purely local cause. A false red teaches the operator to discount reds.
- **Every setting in `config.toml` is mandatory**, and the error calls valid TOML
  invalid ([MEDIUM-4](01-status-and-config.md)). **Reproduced.**
- **`init` writes `peer = []` as the first line**, so the README's own hand-edit
  instructions produce `duplicate key`
  ([MEDIUM-5](01-status-and-config.md)). **Reproduced.**
- **`PeerName`'s `Display` uses `write_str`, which ignores format width**, so
  every table in the program is misaligned
  ([MEDIUM-6](01-status-and-config.md)). **Reproduced.**
- **`recovery export --out /mnt/usb/...` creates the mount point** when the stick
  is not mounted, and writes the only copy of your decryption keys to the disk
  you are backing up ([MEDIUM-7](01-status-and-config.md)).
- **`coverage_pct` is the percentage asked for, not the percentage read**
  ([M3](03-engine.md)), while the type's own doc says the opposite.
- **`quickstart` publishes on all interfaces and prints an `http://` invite**
  carrying the credential ([L12](04-host.md)).
- **No `.dockerignore`**, so 1.1 GB of `target/` plus `.git`, any local `recovery.txt`
  and `certs/` go to the Docker daemon on every build ([C6](05-deploy-and-docs.md)).
- **The Dockerfile's advertised arm64 support does not exist** — the arm64 checksum
  is empty ([C4](05-deploy-and-docs.md)) — and under the classic builder a
  wrong-architecture restic passes its own checksum and fails at backup time
  ([C5](05-deploy-and-docs.md)).
- **`Cargo.toml` asserts MIT, the README says "not yet chosen", and no LICENSE
  ships** ([C12](05-deploy-and-docs.md)). The default is all rights reserved, so the
  friend told to build and run it has no grant.

## Suggested order

1. The status verdict (C1, C2, HIGH-1/2/2b) and `to_engine_error`'s damage arm
   (engine H1). These are one coherent change: make observed damage reach
   `Verdict::Bad` from every path, and make it un-expire.
2. Durability and integrity of the evidence log (state H1/H2/H3/M3, L1's 0600).
   The log is the only thing `status` reads; it should be as hard to lose as the
   config beside it.
3. Redaction and control-character stripping at the engine seam (engine H3/H4,
   state H4). One function applied in one place fixes all three.
4. The host's `DRY_RUN` / `FORCE` handling (host H1/H2) before anyone else runs
   `host release`, and the unvalidated `rm -rf` in the test scripts (deploy S7)
   before anyone else runs the suite.
5. The documented deployment paths (deploy C1/C2/S1/S2). Everything a new user is
   told to type on the host side is broken, insecure, or both, and the two
   commands that do work — `host quickstart` and `connect` — are the two the
   README leads with, which is why this has not been noticed.
6. The rest, by area.
