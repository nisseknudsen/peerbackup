# Code review — peerbackup @ d538bc0

Scope: the whole tree (2,559 lines of Rust across 11 modules, 1,152 lines of
shell tests, CI, Dockerfile, compose, README). Reviewed against `main` at
`d538bc0`, which already carries one principal-level review pass.

Toolchain state at time of review: `cargo fmt --check` clean,
`cargo clippy --all-targets -- -D warnings` clean, `cargo test` 104 passed /
0 failed in 0.53s.

---

## Summary

This is a well-built codebase. The three-state verification model
(`Good` / `Bad` / `Indeterminate` rather than `Result`) is the right central
abstraction and it is carried consistently from `engine::outcome` through to the
`status` table. Fixtures are captured from real restic 0.19.1 output rather than
imagined. Comments explain *why*, and several of them document the exact bug the
line prevents, which is the most useful kind. Secrets handling — 0600 from
`open()` rather than a chmod afterwards, passwords on stdin rather than in argv,
rejection sampling in `random_token` — is better than most projects of any size.

The findings below are mostly in the seams between correct components, plus one
classification bug that silently defeats the product's central promise.

| # | Severity | Area | Finding |
|---|---|---|---|
| 1 | **High** | Correctness | Hex ids containing `403`/`507`/`401` downgrade real corruption to "unknown" |
| 2 | **High** | Correctness | `status` exits 0 for a peer that has never received a backup |
| 3 | **High** | Robustness | `provision` leaks a fully allocated image on three un-rolled-back paths |
| 4 | Medium | Feature gap | `connect` has no `--cacert`, but the README tells peers to pass it |
| 5 | Medium | Performance | `read_since` doc claims bounded I/O; it reads the whole log |
| 6 | Medium | Correctness | A supplied password containing a newline is silently truncated |
| 7 | Medium | Design | Two disagreeing peer-name validators |
| 8 | Medium | Docs/config | systemd unit ships `PB_PORT=8000`; everything else says 51515 |
| 9–18 | Low | Various | Doc placement, dead test scaffolding, test-name/assertion mismatch, CI interpolation, arch lock-in |

---

## High

### 1. Corruption is misclassified as "unreachable" when a hex id contains `403`, `507` or `401`

`src/engine/restic_error.rs:105-113`

The HTTP-status rules match bare digit sequences anywhere in the combined
output, and they run *before* the `check failed` damage rule at line 144. restic
output is full of hex identifiers — pack ids, tree ids, blob ids, snapshot ids —
and `403`, `507` and `401` are all valid hex substrings.

Confirmed empirically against the shipped `classify`:

```
input:  "error for tree 6403bc1e:\n  id 6403bc1e not found in repository\ncheck failed"
result: NoVerdict(AppendOnlyRefused)          // expected: Damage(CheckFailed)

input:  "Load(<data/a507f2>) failed: ...\ncheck failed: repository contains errors"
result: NoVerdict(OutOfSpace)                 // expected: Damage(CheckFailed)
```

Downstream: `verify_subset` returns `Indeterminate` → `cli::verify` records
`Verdict::Unknown` → `status` shows `unchecked` and exits 0 while `bad` stays at
zero. A peer with genuinely damaged packs never turns red. This is the single
failure mode the product exists to catch.

It fails in the safe direction (never a false red), which is why it has not
surfaced — but the `a_403_during_check_is_still_not_damage` test at line 309
pins the ordering that causes it, so it will not surface on its own either.

The rule ordering is right; the matching is too loose. restic emits the status
in a fixed shape, visible in this file's own fixtures:

```
unexpected HTTP response (403): 403 Forbidden
unexpected HTTP response (507): 507 Insufficient Storage
```

Matching `"(403)"` instead of `"403"` (and likewise for 507/401) keeps every
existing test passing and removes the collision. The prose alternatives
(`forbidden`, `insufficient storage`, `unauthorized`) already carry the cases
where the numeric form is absent.

Worth adding a regression test with a hex id that contains each of the three
sequences.

### 2. `status` exits 0 for a peer that has never received a backup

`src/cli.rs:722-762`

Rows in the `Unknown` state are partitioned into `never` (no backup ever
arrived) and `stale` (backup arrived, checks are overdue). Only `stale` reaches
the exit code:

```rust
if rows.iter().any(|r| r.state == PeerState::Bad)          { return Err(...) }
if !rows.is_empty() && rows.iter().all(|r| ... == Unknown) { return Err(...) }
if !stale.is_empty()                                       { return Err(...) }
Ok(())
```

With two peers where one is good and one has never received a backup, none of
the three conditions holds, so `status` exits 0. The table does print
`No backup has reached: bob. Run peerbackup backup.` — but from cron, nobody
reads stdout; the exit code is the whole signal. `never` is a strictly more
alarming state than `stale`, and it is the only `Unknown` category that is
silent.

This looks like an oversight inside the block whose own comment reasons
carefully about collapsing three states into two "in the reassuring direction".
`never` should feed the exit code at least as strongly as `stale`.

There is no test for `status_cmd`'s exit-code logic — see finding 13.

### 3. `provision` leaves a fully allocated image behind on three paths

`src/host/grant.rs:142-178` vs the rollback block at `185-203`

The rollback exists and is well reasoned: "Everything from here can fail with
the image already on disk… Recovering from a half-provisioned grant should not
require running [`release`]." But it only wraps `finish()`. Three steps between
the successful `fallocate`+`mkfs` and that block use a bare `?`:

- `144-147` — `create_dir_all(&dir)` for the mount point
- `149` — `unit_name(&dir)?`, which shells out to `systemd-escape`
- `175-177` — `create_dir_all(&ctx.units)` and writing the mount unit

Any of these returns early with the image already allocated — potentially the
entire grant, e.g. 500GB — and no unit, no mount, no cleanup. The next
`provision` then refuses:

```
/srv/peerbackup/images/alice.img already exists; use `peerbackup host release alice` first
```

…and `release` is the type-the-name-to-confirm destructor whose own warning is
that it permanently destroys the peer's backups. That is precisely the recovery
path the rollback block was written to avoid.

`unit_name` failing is not hypothetical on a host where `systemd-escape` exists
at `REQUIRED`-check time but the call fails for another reason, and
`create_dir_all` on `/etc/systemd/system` fails on a read-only or
immutable-unit-dir system.

Moving the rollback boundary up to immediately after `verify_not_sparse` covers
all three. `verify_not_sparse` already removes the image itself on its own
failure path (line 234), so it is the natural seam.

---

## Medium

### 4. `connect` cannot accept a CA certificate, but the README says it should

`src/main.rs:28-37`, `src/cli.rs:206`, `README.md:229`

`--cacert` exists only on `peer add`. `Command::Connect` has no such field, and
`connect` calls `peer_add(&name, url, None)` with the certificate hardcoded to
`None`. The README's TLS section says:

> With a self-signed certificate they also need a copy of it and must pass
> `--cacert` when connecting.

`connect` is the documented one-command onboarding path — it is the second
command in the Quick start. A peer whose friend uses a self-signed certificate
has to know to abandon `connect` and drive `init` + `peer add` manually, which
the README never says.

Either add `--cacert` to `connect` and thread it through, or change the README
to name `peer add` explicitly in that sentence.

### 5. `read_since` reads the whole evidence log despite its doc

`src/state.rs:189-205`

The doc comment is explicit about the problem it solves:

> Reads from the end and stops early. The file is append-only and written in
> time order, so the first record older than the cutoff means the rest are too.

The implementation opens with `fs::read_to_string(path)` — the whole file, into
memory, before any of that. What is actually bounded is *parsing*: `serde_json`
runs only over lines inside the window. That is a real improvement (JSON parsing
dominates), but I/O and allocation are not bounded, and the comment reads as
though they are.

With the design's own stated growth rate — "hourly backups and daily verifies to
three peers is tens of thousands of lines a year, growing forever" — a
multi-year install reads tens of megabytes on every `peerbackup status`, the
command people run most.

Either correct the doc to say "parses from the end and stops early", or read
backwards in blocks (`seek` from `SEEK_END`, read fixed chunks, split on
newlines) so the claim becomes true. A log-rotation story would also close it,
but rotation conflicts with the append-only-history property the module docs
call load-bearing.

### 6. A supplied password containing a newline is silently truncated

`src/host/server.rs:226-281`

`host adduser <peer> <password>` accepts an operator-supplied password. It
reaches `htpasswd -B -i` on stdin as `pw` followed by `\n` (lines 274-277).
`htpasswd -i` reads one line. A password containing an embedded newline is
therefore stored truncated at the first newline, while the operator believes the
full string is the credential and sends that to their friend.

The same value is interpolated into a curl config line at line 447-448, which
escapes `\` and `"` but not newlines — so the verification step at 305-311 is
also parsing something other than what was intended, and the operator sees a
confusing `401` from the "verified" path rather than a clear refusal.

Generated passwords are unaffected (`random_token` is alphanumeric). A check
rejecting control characters in the supplied password, at the top of `adduser`,
turns an obscure mis-store into a one-line error.

### 7. Two peer-name validators that already disagree

`src/config.rs:75-89` (`PeerName::new`) and `src/host/mod.rs:192-197`
(`peer_valid`)

Both enforce `[a-zA-Z0-9_-]` and non-empty. Only `PeerName::new` enforces the
64-byte limit. The host side is the one that turns the name into `{peer}.img`
and a systemd unit name — i.e. the side where a length limit matters most.

`PeerName`'s doc explains why the type exists: "Constructing one is the only way
to get a name into `Config::secret_path` or a grant directory, so a traversal
cannot reach either." The host side does not use it, so that sentence is not
currently true of grant directories — `grant.rs` takes `peer: &str` throughout
and calls `check_peer` by hand at each entry point.

Threading `PeerName` through `host::grant` and `host::server` would make the
guarantee one type instead of two functions that have already drifted.

### 8. The systemd unit's port contradicts every other documented path

`deploy/systemd/peerbackup-rest.service:39` sets `Environment=PB_PORT=8000`.
`compose.yml:21` defaults to `51515`. The README uses `51515` in the quickstart
output, the settings table (line 324), the troubleshooting section, and the
"Forward port 51515" instruction.

Someone following "Running it as a service" (README:281-291) gets a server
published on `8000:8000` while every other page of documentation, and the
`quickstart` invite URL format, assumes `51515`. `mod.rs:41-43` even explains
why 51515 was chosen: "8000 collides constantly."

---

## Low

9. **`src/engine/restic.rs:61-67`** — the doc comment describing `probe` ("Quick
   check that the peer answers, before starting anything expensive…") is
   attached to `fn command`. The same text appears correctly on `probe` at
   257-262. Rustdoc for `command` currently describes a different function.

10. **`src/config.rs:273-280`** — `random_token`'s doc opens with
    "Random alphanumeric string from the kernel." twice, once with the
    "avoids pulling in a crate" note and once without.

11. **`src/engine/restic.rs:372`, `src/engine/mod.rs:67,79`** — `#[allow(...)]`
    attributes placed *between* doc-comment lines. Legal Rust, but it splits the
    rendered doc block and reads as an editing accident.

12. **`src/engine/fake.rs:17-70`** — `FakeEngine::scripted`, `verify_queue` and
    `calls` are referenced only from `fake.rs`'s own tests; no command test in
    `cli.rs` uses them. The "every operation asked of this engine, in order"
    recorder is the natural way to assert the probe-before-verify ordering, but
    `verify_probes_before_checking_so_a_dead_peer_costs_seconds` (cli.rs:1177)
    infers it from a record count instead. Either wire `calls` into that
    assertion or drop the scaffolding.

13. **`src/host/grant.rs:704-706`** — `a_non_numeric_uid_is_refused_by_name`
    asserts only that an *unset* variable yields `None`. It never exercises a
    non-numeric value, which is what the name promises and what the function's
    doc comment justifies. `numeric_env` reads `std::env` directly, which is why
    it cannot be tested properly — taking the values as a parameter is the same
    refactor `Runtime` already got in `cli.rs`, for the same reason.

14. **`tests/end_to_end.sh:172-178`** — `RESTORE_CMD` is extracted from the
    recovery file under the comment "Exactly the command the recovery file
    prints", checked for emptiness, and then never executed. The test runs a
    hand-written `restic … restore latest --tag peerbackup` instead. The
    disaster-recovery claim would be self-verifying if the extracted command
    were actually run.

15. **`.github/workflows/spike.yml:36`** — `${{ github.event.inputs.restic_version }}`
    is interpolated directly into a `run:` block. `workflow_dispatch` requires
    write access, so exposure is limited to people who could push anyway, but
    passing the input through `env:` and referencing `$RESTIC_VERSION_INPUT` is
    the standard mitigation and costs nothing.

16. **`src/cli.rs:223-242`** — `peer_name_from_url` splits on the last `@` in the
    whole URL, unlike `redact` (cli.rs:411-413), which was deliberately bounded
    to the authority segment for exactly this reason. A path containing `@`
    yields a nonsense suggested name. Cosmetic — `--name` overrides — but the
    two functions should agree on where the authority ends.

17. **`src/state.rs:250-252`, `src/cli.rs:680`** — `liveness_hours * 3600` and
    `canary_days.max(subset_days) * 86400 * 2` are unchecked multiplications on
    values read straight from user TOML. Release builds disable overflow checks,
    so an absurd value wraps to a small window silently. `Settings` has no
    validation on load at all; a `verify_subset_pct` of 0 is also accepted and
    then clamped to 1 much later, in `restic.rs:227`.

18. **`Dockerfile:14`** hardcodes `--target x86_64-unknown-linux-musl`, so the
    client image cannot be built on arm64 — which rules out a meaningful share
    of the homeservers this is aimed at. Separately, `COPY src ./src` precedes
    the only `cargo build`, so there is no dependency-caching layer and every
    source edit rebuilds the full tree.

---

## Testing

Coverage is genuinely good where it matters, and the test *names* are unusually
strong — most of them state the property rather than the mechanism, and several
document the bug they were written for.

Well covered: `classify` (real fixtures, ordering pinned), `parse_size` / `human`
(round-trips, numfmt differential noted), `parse_snapshots` (nested `summary`,
tags, ordering with real offsets), `run_bounded` (kill, large output, deadlock),
`redact` (the `@`-in-password case), `write_private` (mode, atomicity, stray temp
files), `status` state machine, `newest_real_backup`, `guard` fail-closed.

Gaps worth closing, in priority order:

1. **`status_cmd` exit codes.** The three-tier logic at cli.rs:748-762 has no
   test at all, which is why finding 2 is present. It is pure given `rows`;
   extracting the decision into a function taking `&[PeerStatus]` would make it
   directly testable.
2. **`classify` against outputs containing hex ids.** Finding 1. Add fixtures
   with ids containing `403`, `507`, `401`.
3. **`provision`'s rollback path.** Finding 3. The `finish()` failure path and
   the three un-rolled-back paths have no coverage; the existing grant tests all
   run under `DRY_RUN=1` and stop before the image exists.
4. **`host::server::adduser`.** The whole htpasswd-then-restart-then-verify
   sequence — the thing the function exists to make unavoidable — is untested.
   `compose()` is untested too.
5. **`Ctx::run` / `run_best_effort` / `remove_quietly`.** No tests. `Ctx::run`
   builds the command line that the shell tests grep for, so its formatting is
   load-bearing.
6. **Call ordering via `FakeEngine::calls`.** Finding 12.

The shell suites are strong and deliberately black-box. `test-host-tooling.sh`'s
opening comment about why the old bash `to_bytes` could not be tested is a good
example of a test file explaining its own history.

---

## Architecture

No structural concerns. The layering is clean:

```
main.rs      dispatch only
cli.rs       commands, Runtime carries paths (testable)
engine/      trait seam, restic impl, three-state outcome, classification
host/        grants, server, size — OS work stays in the OS
config.rs    config + secrets, PeerName as the safety type
state.rs     canary + evidence + status derivation (pure)
```

The `BackupEngine` trait is domain-shaped rather than a mirror of restic's CLI,
which is the right call and is what makes the `cli.rs` command tests possible.
`Runtime` carrying paths instead of reading `state_dir()` at each use is the
right fix for the `set_var`-in-tests problem, and the comment explaining why
`std::env::set_var` is `unsafe` in edition 2024 is accurate and useful.

Two smaller observations:

- `peer_add` (cli.rs:251) still reaches for the globals `canary_dir()`,
  `Canary::load_or_create()` and `Runtime::from_env()` directly, so it is the one
  command that cannot be driven against a temp directory the way `backup_in` and
  `verify_in` can. That is why it has no unit test and is covered only by
  `end_to_end.sh`.
- `host::grant` and `host::server` take `peer: &str` and validate at each entry
  point, rather than taking the `PeerName` type that exists for this. See
  finding 7.

---

## Documentation

The README is unusually good — it leads with two commands, explains failure
modes people actually hit, and the troubleshooting section reads like it was
written from real incidents. `TODOS.md` captures deferred work with enough
context to resume cold, including honest confidence levels ("6/10").

Issues: findings 4 (`--cacert`), 8 (port), 9 and 10 (misplaced/duplicated doc
comments). One more:

- README:88-90 says "No prebuilt binaries yet" under a heading called
  **Installation**, then gives source-build instructions. Fine, but the Status
  section already says the same thing 8 lines earlier.
- The `Development` section (README:436-452) lists six test scripts but not
  `cargo test`'s 104 unit tests as the fast inner loop, and does not mention the
  `PEERBACKUP_*_TIMEOUT` escape hatches that `engine_for` (cli.rs:65-68) exposes
  and whose comment says are "documented in the README". They are not.

---

## Recommended next steps

**Before the next release**

1. Tighten the HTTP-status matching in `classify` to `(403)` / `(507)` / `(401)`
   and add hex-id regression fixtures. *(finding 1)*
2. Make `never`-backed-up peers exit non-zero in `status`, and extract the
   exit-code decision into a testable function. *(findings 2, 13-gap-1)*
3. Move the `provision` rollback boundary up to just after `verify_not_sparse`.
   *(finding 3)*

**Soon**

4. Add `--cacert` to `connect`, or correct README:229. *(finding 4)*
5. Reject control characters in an operator-supplied `adduser` password.
   *(finding 6)*
6. Fix the `read_since` doc, or make it read backwards in blocks. *(finding 5)*
7. Align `PB_PORT` in the systemd unit with 51515. *(finding 8)*
8. Thread `PeerName` through `host::`, retiring `peer_valid`/`check_peer`.
   *(finding 7)*

**When convenient**

9. The doc-comment placement and duplication cleanups (9, 10, 11).
10. Wire `FakeEngine::calls` into an ordering assertion, or delete the
    scaffolding (12).
11. Make `numeric_env` take its inputs as parameters and test the non-numeric
    case its test name already claims (13).
12. Run the extracted `RESTORE_CMD` in `end_to_end.sh` (14).
13. Move the workflow_dispatch input through `env:` (15).
14. Validate `Settings` on load — non-zero windows, `verify_subset_pct` in
    1..=100 — rather than clamping silently three layers down (17).
15. Parameterise the Dockerfile target and add a dependency-caching layer (18).

**Tooling**

The existing setup is already strict — `clippy::all` at `deny` in `Cargo.toml`
rather than on the CI command line, a curated slice of `pedantic`,
`unsafe_op_in_unsafe_fn = "deny"`, shellcheck at `-S warning`, restic pinned by
checksum. Two additions worth considering:

- `cargo deny` or `cargo audit` in CI. The dependency set is small (7 direct),
  which makes this cheap to keep green.
- `overflow-checks = true` in `[profile.release]`. This is not a hot loop — the
  release profile already trades speed for size — and it would turn finding 17
  into a panic rather than a silently wrong window.
