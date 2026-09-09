# Where each finding was fixed

Twenty-eight pull requests, stacked within an area and independent across areas.
Merge order is the stack order; each PR names its base.

## Stacks

**Client and engine** (#6 → #7 → #8 → #9 → #10 → #11 → #12 → #20 → #21 → #22 →
#23 → #24 → #25 → #26 → #30 → #31 → #32), all based on `main` through #6.

**Host** (#13 → #14 → #27 → #28 → #29), based on `main`.

**Deployment** (#15 → #19), and (#16 → #17), and #18, #33, each based on `main`.

## By finding

| Finding | Where | PR |
|---|---|---|
| C1 / CRITICAL-0 unresolved damage discarded | `state.rs` | #6 |
| C2 future-dated record pins a peer green | `state.rs` | #6 |
| L3 stale coverage shown as current | `state.rs` | #6 |
| L4 same-second Bad→Good resolves wrongly | `state.rs` | #6 |
| state H1 evidence never fsynced | `state.rs` | #7 |
| state H2 long line dropped by the walk | `state.rs` | #7 |
| state H3 one bad clock truncates history | `state.rs` | #7 |
| state M3 partial read reads as green | `state.rs` | #7 |
| state L1 evidence log world-readable | `state.rs` | #7 |
| HIGH-1 `liveness_hours` missing from the window | `cli.rs` | #7 |
| HIGH-2 damage expires out of the window | `state.rs` | #7 |
| LOW-11 secrets directory 0755 | `config.rs` | #7 |
| engine H4 credentials printed and logged | `engine/` | #8 |
| state H4 / MEDIUM-3 ANSI repaints the table | `redact.rs` | #8 |
| engine H1 canary damage laundered to Unknown | `engine/` | #9 |
| engine H2 retry line outranks corruption | `restic_error.rs` | #10 |
| engine M2 `decrypting`+`failed` across lines | `restic_error.rs` | #10 |
| engine L3 `strip_go_trace` eats detail lines | `restic_error.rs` | #10 |
| engine H3 repository URL in argv | `engine/restic.rs` | #11 |
| engine L2 ambient `RESTIC_*` honoured | `engine/restic.rs` | #11 |
| engine M1 disaster restore killed at 30 min | `engine/restic.rs` | #12 |
| host H1 `DRY_RUN` ignored by the server path | `host/server.rs` | #13 |
| host H2 `FORCE=1` destroys backups | `host/mod.rs` | #13 |
| host M10 `cargo test` deletes a container | `host/server.rs` | #13 |
| host M6 symlink becomes a root mount target | `host/grant.rs` | #14 |
| host M7 half-sparse image "verified" | `host/grant.rs` | #14 |
| host L10 mounting over a non-empty directory | `host/grant.rs` | #14 |
| deploy S7 suites `rm -rf` a caller's path | `tests/lib/` | #15 |
| deploy C1 documented docker start fails | `README.md` | #16 |
| deploy C2 service cannot start after quickstart | `host/grant.rs` | #16 |
| deploy C7 documented paths have no quota | `README.md` | #16 |
| deploy C10 per-peer flow does not work as printed | `README.md` | #16 |
| deploy C11 quickstart and compose collide | `README.md` | #16 |
| deploy S1 plaintext exposure hidden | `README.md` | #17 |
| deploy S2 TLS recipe cannot work | `README.md` | #17 |
| deploy S3 no container hardening | `compose.yml` | #17 |
| deploy C3 TLS breaks the healthcheck | — | **did not reproduce**, see #17 |
| deploy S10 CI servers on 0.0.0.0 | test scripts | #17 |
| deploy C12 licence contradiction | `LICENSE` | #18 |
| deploy C4 arm64 support absent | `Dockerfile` | #18 |
| deploy C5 wrong-arch restic under classic builder | `Dockerfile` | #18 |
| deploy C6 1.1GB build context | `.dockerignore` | #18 |
| deploy C14 `VOLUME` before `USER` | `Dockerfile` | #18 |
| deploy S4 mutable base image tags | `Dockerfile` | #18 |
| deploy S9 `.gitignore` misses secrets | `.gitignore` | #18 |
| deploy S5 actions on mutable refs | `ci.yml` | #19 |
| deploy C13 MSRV untested | `ci.yml` | #19 |
| deploy C15 assertions that cannot fail | test scripts | #19 |
| deploy C16 restic pin duplicated | `ci.yml` | #19 |
| deploy C8 spike green while testing nothing | `spike/` | #19 |
| deploy D5 CI runs everything twice | `ci.yml` | #19 |
| MEDIUM-4 every setting mandatory | `config.rs` | #20 |
| MEDIUM-5 `peer = []` breaks the hand-edit | `config.rs` | #20 |
| NIT-15 unknown keys silently dropped | `config.rs` | #20 |
| MEDIUM-6 `Display` ignores format width | `config.rs` | #21 |
| MEDIUM-7 `--out` creates a missing mount point | `cli.rs` | #22 |
| MEDIUM-8 recovery commands omit `--cacert` | `cli.rs` | #22 |
| LOW-9 recovery file dated with a timestamp | `state.rs` | #22 |
| LOW-10 fingerprint world-readable | `cli.rs` | #22 |
| NIT-13 fingerprint computed twice | `cli.rs` | #22 |
| state M1 canary manifest non-atomic | `state.rs` | #23 |
| state M2 manifest never reconciled with disk | `state.rs` | #23 |
| HIGH-2b a name is not a peer identity | `state.rs` | #24 |
| engine M4 empty repository passes `check` | `cli.rs` | #25 |
| engine M4 nothing-checked exits 0 | `cli.rs` | #25 |
| engine M5 no `--` before positionals | `engine/restic.rs` | #26 |
| engine L1 `null` rejected for an empty repo | `engine/restic.rs` | #26 |
| engine L4 `is_incomplete` substring match | `engine/restic.rs` | #26 |
| engine L5 `restore_all` asserts nothing | `engine/restic.rs` | #26 |
| engine L6 `PEERBACKUP_RESTIC` manufactures green | `cli.rs` | #26 |
| engine L7 fake engine scripting and coverage | `engine/fake.rs` | #25, #26 |
| `extract_pack_id` reports the wrong pack | `restic_error.rs` | #26 |
| host M1 rollback leaks the image | `host/grant.rs` | #27 |
| host M2 mkfs failure outside the boundary | `host/grant.rs` | #27 |
| host M3 release claims space it did not return | `host/grant.rs` | #27 |
| host L5 only the first loop device detached | `host/grant.rs` | #27 |
| host M4 `adduser` rotates a password silently | `host/server.rs` | #28 |
| host M8 grant uid vs container uid | `host/grant.rs` | #28 |
| host M9 sudo quickstart runs as root | `host/server.rs` | #28 |
| host M11 adopts a container unchecked | `host/server.rs` | #28 |
| host L1 no `--` terminators | `host/grant.rs` | #29 |
| host L2 env paths into a root-written unit | `host/mod.rs` | #29 |
| host L3 numeric env vars panic | `host/mod.rs` | #29 |
| host L4 btrfs subvolumes read as mounted | `host/mod.rs` | #29 |
| host L6 `list` shows apparent size | `host/grant.rs` | #29 |
| host L7 `list` prints zeroes on statfs failure | `host/grant.rs` | #29 |
| host L8 provision fails after succeeding | `host/grant.rs` | #29 |
| host L9 systemd-escape stderr discarded | `host/mod.rs` | #29 |
| host L11 restart aborts other peers' backups | `host/server.rs` | #29 |
| host L12 quickstart publishes on all interfaces | `compose.yml` | #17 |
| `size.rs` nits | `host/size.rs` | #29 |
| HIGH-2c host rollback undetected | `cli.rs` | #30 |
| state nits (32-bit, `/dev/urandom`, forward compat) | `state.rs` | #31 |
| engine nits (poll race, stdin, tags, threads) | `engine/restic.rs` | #31 |
| deploy D1–D4 documentation errors | `README.md` | #31 |
| deploy C9 restic version unenforced | `cli.rs` | #32 |
| deploy S6 password in the client's argv | `cli.rs` | #32 |
| deploy S8 systemd unit unconfined | service unit | #33 |

## Deliberately not changed

**LOW-12, the canary never rotates.** Still true. The threat it mattered for --
a host serving an old repository -- is closed by #30, which asks the peer whether
it still lists the snapshot the last backup produced. Rotating the canary as well
would give a slightly stronger read-back guarantee and costs a generation history
to avoid false reds; not worth it for what it adds on top of #30.

**state L2, `now()` answering 0 for a pre-1970 clock.** The `unwrap_or(0)` is
still there. It is no longer a silent green: after #6, a `now` of 0 puts every
record beyond the clock-skew horizon, so every peer reads `unchecked` and says
why.

**deploy C3, TLS breaking the healthcheck.** Checked against a real container and
it does not happen: Go answers a plaintext request to a TLS port with
`HTTP/1.0 400 Bad Request`, which contains the status line the check greps for.
#17 adds `PB_HEALTHCHECK_SCHEME` anyway, because passing on the strength of a
handshake failure is the wrong reason to be healthy.

**engine nit, 8-character snapshot ids could collide.** `list_snapshots` returns
restic's `short_id` and feeds it back as a selector. restic would refuse an
ambiguous prefix rather than pick wrongly, so the failure is loud and rare.
