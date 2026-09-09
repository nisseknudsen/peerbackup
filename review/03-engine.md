# peerbackup engine audit — src/engine/*

Scope: `src/engine/mod.rs`, `restic.rs`, `restic_error.rs`, `outcome.rs`, `fake.rs`
(+ `src/cli.rs` read only where it calls the engine). Branch `review/full-audit`.

Every finding below was traced in the source. Where a claim depends on restic's own
behaviour I say so explicitly.

---

## HIGH

### H1 — Observed corruption during a restore is laundered into `Unclassified`, so the peer never turns red
`src/engine/restic.rs:112-119`

```rust
let cause = match classify(code.unwrap_or(-1), &combined) {
    // Damage found during a backup or restore is still an operational
    // failure here; the damage verdict belongs to verification.
    Classified::Damage(d) => Cause::Unclassified { detail: d.to_string() },
    Classified::NoVerdict(c) => c,
};
```

`restore_path` is the canary path (`restic.rs:176-210`), and it is the *only* place
peerbackup reads real bytes back and compares a digest. When restic fails that restore
with hard evidence of damage — `ciphertext verification failed`, `blob not found`,
`pack … does not match its hash` — `classify` correctly returns `Classified::Damage`,
and this arm throws the verdict away and relabels it "we could not classify this".

Downstream (`src/cli.rs:667-680` → `635-665`) `restore_canary` returns `Err(String)`,
`verify_in` prints `could not restore` and records `Kind::Canary, Verdict::Unknown`,
and — critically — does **not** increment `bad`, so `peerbackup verify` exits 0.
`state.rs::status` (`src/state.rs:313-325`) only ever produces `PeerState::Bad` from a
`Verdict::Bad` record, so the peer displays `unknown`, never red.

**Failure scenario.** Alice's repository has a corrupt pack holding the canary blob.
`verify` runs: `probe` succeeds; `verify_subset(1)` samples 1% of packs, misses the
corrupt one, exits 0 → `Good { coverage_pct: 1 }` → printed `ok`, recorded
`Subset/Good`. The canary restore then dies with `ciphertext verification failed`.
Terminal output:

```
alice:
  checking 1% of the stored data... ok
  restoring a test file... could not restore
    Fatal: ciphertext verification failed
```

exit status **0**. The evidence log holds `Subset/Good` and `Canary/Unknown`. The
dashboard shows `unknown`, i.e. "we haven't looked lately", for a peer where we *did*
look and *did* see damage. This is precisely the "damage reported as indeterminate"
failure the three-state model exists to prevent.

The comment's reasoning ("the damage verdict belongs to verification") does not hold:
`restore_path` **is** verification — `verify_in` calls it as the second half of every
verify. Fix: keep the `Corruption` on `EngineError` (add a variant or a
`damage: Option<Corruption>` field) and have `restore_canary` / `verify_in` map it to
`Verdict::Bad`.

---

### H2 — A single transient retry line in `restic check` output downgrades real corruption to "unknown"
`src/engine/restic_error.rs:119-154` (transport rules) vs `159-163` (`CheckFailed`)

`classify` is an ordered chain of `contains` over the *whole* combined stdout+stderr.
Rules 6–11 (403/507/401, locked, `connection refused` / `i/o timeout` / `dial tcp` /
`tls handshake` / `certificate`, `unexpected eof` / `connection reset` /
`broken pipe` / `context deadline exceeded`) all sit **above** the generic damage rule:

```rust
if lc.contains("check failed") || lc.contains("repository contains errors") {
    return Classified::Damage(Corruption::CheckFailed { detail: first_line(&clean) });
}
```

restic prints non-fatal retry lines on *every* transient blip and then carries on
successfully, e.g. (this repo's own fixture, `restic_error.rs:300`):

```
Load(<data/aa>) returned error, retrying after 1s: unexpected EOF
```

**Failure scenario.** A 5% `check --read-data-subset` runs for 40 minutes against
Alice. One pack load hiccups and is retried successfully; later, three packs are
genuinely damaged and restic ends with `Fatal: repository contains errors`. Combined
output contains both `unexpected eof` and `repository contains errors`. Rule 11 fires
first → `NoVerdict(TransferInterrupted)` → `VerifyOutcome::Indeterminate` →
`Verdict::Unknown`. The peer reads `unknown` forever, never `verified-bad`, even
though restic told us the repository is broken.

The existing test `transport_failures_never_produce_damage` feeds each transport
string *in isolation*, so it cannot catch this; `a_403_during_check_is_still_not_damage`
pins the opposite direction only. The same shape applies to `certificate` (rule 10),
`no space left` (rule 7 — e.g. the local check cache filling up mid-run) and
`forbidden`/`unauthorized` as bare substrings.

Fix: transport/capacity causes must only win when the run produced *no* damage
evidence, and non-fatal `returned error, retrying after` lines should be excluded from
classification input entirely (restic marks them as retries, not outcomes).

---

### H3 — The peer's HTTP credentials are passed to restic in argv, readable by any local user via `ps`
`src/engine/restic.rs:67-76`, esp. line 69

```rust
c.arg("-r").arg(&self.repo_url);
```

`repo_url` always embeds `user:password` — see the struct's own doc comment
(`restic.rs:24`: `rest:https://user:pw@peer.example.org:8000/nisse/`),
`src/host/server.rs:201` which prints `rest:http://{peer}:{pw}@{host}:{port}/{peer}/`
to the friend, `README.md:30`/`174`/`259`, and the whole existence of `redact()` in
`src/cli.rs:406-446`.

The doc comment immediately above the offending line claims the opposite threat model:

> The password goes in as a file rather than an environment variable or an argument:
> both are readable by other processes, and this one decrypts the whole repository.

That is true of the *repository* password and false of the *transport* credential
sitting three lines up. On Linux with default `hidepid=0`, any local user can read
`/proc/<pid>/cmdline` for the whole life of a backup — which for the 300GB seed this
project is designed around is ~17 hours of exposure.

**Failure scenario.** A shared/multi-user host, or any process the user runs that
shells out (`ps aux`, a CI agent, an unprivileged monitoring daemon) captures
`restic -r rest:https://nisse:nq7Y7PYN44nqKG83mNc9@alice.example.org:8000/nisse/ backup …`.
The holder can then delete or overwrite every backup at that peer (unless the peer runs
rest-server in append-only mode, which bounds it to read/write-new), and can read the
encrypted repository at will.

Fix: restic supports `--repository-file` / `RESTIC_REPOSITORY_FILE`. Write the URL to a
0600 file next to the password file and pass that instead.

---

### H4 — restic error text containing the credentialed URL is printed and written unredacted to the evidence log
`src/engine/restic.rs:121` (`message: strip_go_trace(&combined)`), consumed at
`src/cli.rs:317`, `src/cli.rs:494-524`, `src/cli.rs:645-664`

restic embeds the repository URL in its own error prose. This repo's own fixture proves
it (`restic_error.rs:206`):

```
Fatal: create repository at rest:http://me:pw@127.0.0.1:8023/me/ failed: config file already exists
```

`to_engine_error` copies that verbatim into `EngineError::message`, and every caller
prints it and persists it:

* `cli.rs:317` — `return Err(format!("could not create the repository: {e}"))`
* `cli.rs:508-516` — `println!("  {e}")` **and**
  `rt.record(&peer.name, Kind::Backup, Verdict::Unknown, Some(e.to_string()), None)`

`Evidence::append_to` (`src/state.rs:164-176`) opens the file with plain
`OpenOptions::new().create(true).append(true)` — no mode, so 0644 minus umask. The
peer's HTTP password therefore lands in cleartext in a world-readable
`evidence.jsonl`, and is replayed on screen by `peerbackup status` via
`PeerStatus::problem` (`state.rs:314-325`).

**Failure scenario.** `peerbackup peer add alice 'rest:https://alice:s3cret@host/alice/'`
against a host that already has a repo, or any backup that fails at connect time:
the password is echoed to the terminal (scrollback, CI logs, a screenshot pasted into
the friend-to-friend chat this product assumes) and appended forever to a 0644 file.

The codebase clearly knows this is wrong — `redact()` exists and is unit-tested twice
(`cli.rs:1025`, `cli.rs:1088`) — it is simply never applied to engine errors.

Fix: run `EngineError::message` (and `Cause::*{detail}`) through `redact` at the seam,
in `to_engine_error`, so no caller can forget; and create the evidence file 0600.

---

## MEDIUM

### M1 — One `restore_timeout` for both a 4KB canary and a full disaster restore; the default kills a real restore at 30 minutes
`src/engine/restic.rs:44`, used at `restic.rs:191` and `restic.rs:220`

```rust
pub const DEFAULT_RESTORE_TIMEOUT: Duration = Duration::from_secs(1800);
```

`restore_path` (canary, a few KB) and `restore_all` (**the disaster operation**, per
`mod.rs:134`) share the same knob. `backup` is deliberately unbounded with an explicit
justification — "a 300GB first seed at 40Mbit legitimately takes seventeen hours"
(`restic.rs:78-80`, `README.md:185-188`) — and the reverse of that same transfer is
capped at 30 minutes.

**Failure scenario.** The user's disk dies. `peerbackup restore alice /mnt/new` pulls
300GB. At 1800s `run_bounded` SIGKILLs restic mid-write, `run` returns
`Cause::TimedOut { after_secs: 1800 }`, and `cli.rs:880` prints
`restore failed: restic did not finish within 1800s and was killed`. The target holds a
silently partial tree, and nothing tells the user the data is fine and only the deadline
was wrong. The one moment the product exists for is the one it fails at by default.

Fix: separate the two (`canary_restore_timeout` bounded, `restore_all` unbounded or
progress-based), and never SIGKILL a full restore on a fixed wall clock.

### M2 — `decrypting` + `failed` is matched across unrelated lines, above every transport rule
`src/engine/restic_error.rs:70-75`

```rust
if lc.contains("ciphertext verification failed")
    || lc.contains("decrypting") && lc.contains("failed")
```

The two `contains` calls run over the whole multi-line combined output, so they need not
come from the same line, the same message, or the same operation. Because this rule sits
in the damage block (rules 1–3) it preempts *every* transport, auth, capacity and lock
rule below it.

**Failure scenario.** Any run whose output happens to contain the word `decrypting`
(restic prints `decrypting …` during index/key loading) together with the word `failed`
anywhere else — e.g. `Load(<index/…>) returned error, retrying after 1s: …` followed by
`Fatal: … failed: dial tcp …: connection refused` — is classified
`Damage(CiphertextInvalid)` before the `connection refused` rule is ever reached. In
`verify_subset` (`restic.rs:255-258`) that becomes `VerifyOutcome::Bad`,
`Verdict::Bad`, and `PeerState::Bad`. A dead router reddens a healthy peer — the exact
false-red the module header (`restic_error.rs:5-6`) says destroys the product.

Fix: require the two tokens on the same line (`lines().any(|l| l.contains("decrypting") && l.contains("failed"))`).

### M3 — `Good { coverage_pct }` reports the percentage *asked for*, not the percentage read; the type's own doc claims otherwise
`src/engine/restic.rs:249-250`, `src/engine/outcome.rs:97-100`

```rust
if out.status.success() {
    return VerifyOutcome::Good { coverage_pct: pct };
}
```

`pct` is the caller's request. restic's `check --read-data-subset` output states how
many packs / how much data it actually read, and that output is discarded entirely on
the success path. `outcome.rs:97-99` documents the field as

> the share of pack data actually read back, which is reported separately from canary
> success so the dashboard never implies more than was checked

which is not what the code stores. `status` surfaces it (`state.rs:346-350`) and
`Display` renders `verified-good (1% of pack data read back)` (`outcome.rs:142-144`).

**Failure scenario.** restic's percentage selection is pack-granular with a floor of one
pack: a repository of 3 large packs asked for 1% reads one whole pack (~33%), and a
repository whose packs are unevenly sized can read far less than the nominal share. In
both directions the number on the dashboard is an assertion the program never measured,
presented as a measurement. Fix: parse restic's reported pack count/size, or rename the
field to `requested_pct`.

### M4 — A verify that could check nothing still prints `ok` / exits 0
`src/engine/restic.rs:249` + `src/cli.rs:597-665`

Neither `VerifyOutcome::Indeterminate` nor a canary `Err` increments `bad`
(`cli.rs:628-634`, `cli.rs:658-664`), so `verify_in` returns `Ok(())`.

**Failure scenario A (empty repository).** Alice's disk was reimaged and rest-server
handed back a fresh empty repo. `restic check --read-data-subset 5%` on an empty
repository exits 0. `verify_subset` → `Good { coverage_pct: 5 }` → printed `ok`,
recorded `Subset/Good`. The canary restore then fails at `no snapshots on this peer yet`
(`cli.rs:674`) → `Canary/Unknown`. Terminal shows a green subset line, exit status 0,
and `status` reports `coverage_pct: 5` for a peer holding zero bytes of the user's data.
(`state.rs:333-343` does keep the *peer* at `Unknown` because the canary never went
good — that is the only thing preventing a full false green here.)

**Failure scenario B (cron).** README:197 says "There is no built-in scheduler", so
users will wire `peerbackup verify` into cron/systemd and alert on non-zero exit. A peer
that is unreachable every night for a month produces `unknown` records and exit 0 every
time: no alert ever fires. The three-state model is right that unknown ≠ bad, but the
process exit status collapses unknown into success.

Fix: distinct exit codes (0 verified / 1 damage / 2 nothing verified), and refuse
`Good` for a repository with no snapshots.

### M5 — No `--` separator: user- and config-supplied strings can be read by restic as flags
`src/engine/restic.rs:137` (sources), `restic.rs:182-192` and `212-221` (snapshot id),
reached from `src/cli.rs:869-872`

`restore --snapshot <s>` takes an unvalidated string (`cli.rs:869`,
`Some(s) => SnapshotId(s.to_owned())`) and it becomes the first positional argument to
`restic restore` with no `--` guard. `snapshot()` likewise appends every configured
`sources` entry as a bare positional.

**Failure scenario.** `peerbackup restore alice /mnt/new --snapshot --insecure-tls`
produces `restic -r … restore --insecure-tls --target /mnt/new`, disabling TLS
certificate verification for the run (restic then errors on the missing snapshot id, but
the flag was accepted; a value like `--insecure-tls=true abc1234` is not possible in one
argv slot, which is what keeps this from being worse). A `sources` entry in
`config.toml` that starts with `-` and names an existing file likewise reaches restic as
a flag — `check_sources` (`cli.rs:534-547`) only requires `metadata()` to succeed, which
a file literally named `--insecure-tls` satisfies.

Severity is bounded because the attacker here is the user's own CLI/config, but the fix
is one line: insert `"--"` before every positional argument. Also worth validating that
a `--snapshot` value is `latest` or hex.

---

## LOW

### L1 — `parse_snapshots` rejects restic's empty-repository output
`src/engine/restic.rs:418-431`

`serde_json::from_str::<Vec<SnapshotJson>>` errors on `null`. Go marshals a nil slice as
`null`, so if restic emits `null` rather than `[]` for a repository with no snapshots,
`list_snapshots` returns `EngineError("could not parse restic snapshot output: invalid
type: null…")` instead of an empty vec. That makes `newest_real_backup`'s carefully
worded "holds no backups at all" message (`cli.rs:846-849`) unreachable and turns the
"empty peer" case into a parser error. Not a false success (both paths fail), but the
error the user sees is wrong. *(The restic-side behaviour is an assumption — the
peerbackup-side brittleness is not; `#[serde(default)]` on a `Option<Vec<_>>` or an
`.unwrap_or_default()` on `null` costs nothing.)*

### L2 — The child inherits the whole environment; stray `RESTIC_*` vars silently change behaviour
`src/engine/restic.rs:293-295` — `run_bounded` never calls `Command::env_clear`/`env_remove`.

`--password-file` and `-r` do win over `RESTIC_PASSWORD` / `RESTIC_REPOSITORY`, so the
password itself is safe. But:
* `RESTIC_CACERT` set in the environment silently supplies the TLS trust store whenever
  `ca_cert` is `None` — peerbackup believes it is using the system store.
* `RESTIC_PASSWORD_COMMAND` set in the environment is mutually exclusive with
  `--password-file` in restic, so *every* peerbackup operation fails with a confusing
  restic message and everything reads `unknown`.
* `RESTIC_REPOSITORY_FILE` is mutually exclusive with `-r`, same effect.

Fix: `env_remove` the `RESTIC_*` set peerbackup does not intend to honour.

### L3 — `strip_go_trace` deletes any indented line beginning with `/`
`src/engine/restic_error.rs:31-33`

```rust
let is_location = line.starts_with(char::is_whitespace)
    && (t.starts_with('/') || t.contains(".go:") || t.contains(".s:"));
```

restic indents detail lines under an error header, and those details are frequently
absolute paths. Because `classify` runs on the *stripped* text (`restic_error.rs:62`),
any damage keyword that appears only on such a line is destroyed before matching, and
the failure falls through to `Unclassified` → `Indeterminate`. Also note `first_line`
(`restic_error.rs:180-186`) then reports the surviving header, so the operator loses the
path too. Tighten to require a `.go:`/`.s:` suffix rather than "starts with `/`".

### L4 — `is_incomplete` substring match can be triggered by file content/names
`src/engine/restic.rs:380-397`

`combined.contains("could not be read")` runs over restic's `--json` stream, which
includes `"current_files":[…]` status lines carrying arbitrary paths. A file or
directory named `could not be read` anywhere under `sources` makes every backup report
`INCOMPLETE`, record `Verdict::Unknown`, and `backup` exit non-zero. Contrived, but it
is a false red on the product's headline command, and the fix is to parse the summary
line as JSON rather than substring the whole stream.

### L5 — `restore_all` asserts success from the exit status alone
`src/engine/restic.rs:212-223`

Nothing checks that any file was written. `restic restore` exits 0 for an empty or
path-mismatched selection, and `cli.rs:881` then prints `Done.` Compare `restore_path`,
which at least stats and hashes the landed file. The `newest_real_backup` tag guard
(`cli.rs:840-859`) is the only thing standing between this and "restored the canary,
printed Done"; with an explicit `--snapshot` that guard is bypassed.

### L6 — `PEERBACKUP_RESTIC` swaps the engine binary in the production path
`src/cli.rs:59-61`

A test hook wired into the real command path. `PEERBACKUP_RESTIC=/bin/true peerbackup
verify` prints `checking 1% of the stored data... ok` and records `Subset/Good` for
every peer without touching the network. Same trust boundary as the user, so not an
escalation, but it is an env var that manufactures green.

### L7 — Fake engine diverges from the real one in the two places that matter
`src/engine/fake.rs:93-99`, `fake.rs:102-119`

* `snapshot_result` is a `RefCell<Option<_>>` consumed with `.take()`, so a scripted
  failure applies to the **first peer only**; every later peer in the same `backup_in`
  run silently succeeds. A multi-peer regression test would pass while asserting
  nothing.
* `restore_path` hashes the *source* file (`std::fs::read(path)`) and returns
  `target.join(path.file_name())`. The real engine reconstructs
  `target.join(path.strip_prefix("/"))` (`restic.rs:195`) and hashes what actually
  landed. The path-reconstruction logic — the one piece of restore semantics peerbackup
  itself owns — has no test coverage at all as a result.
* `restore_path` returns `Ok` even for `FakeEngine::unreachable(...)`, so no test
  exercises a canary restore against a dead peer.

---

## NIT

* `restic.rs:311-321` — `run_bounded` polls `try_wait` then checks the deadline; a child
  that exits inside that window is killed and reported as `TimedOut`. The window is
  microseconds and the misclassification is `Indeterminate` (fail-safe), so this is
  cosmetic. The 50ms sleep also costs ~72k wakeups over a 1h verify.
* `restic.rs:327-328` — `join().unwrap_or_default()`: a panicking reader thread yields
  empty output, which `classify` turns into `Unclassified`. Fail-safe, but silent.
* `restic_error.rs:188-195` — `extract_pack_id` takes the first `"pack "` anywhere in the
  output, which need not be the pack that failed its hash; the reported id can be wrong.
* `restic.rs:135` — tags are passed one per `--tag`, but restic also splits `--tag` on
  commas; a tag containing `,` would become two. Both current tags are constants.
* `restic.rs:294` — stdin is inherited. `snapshot()` runs with no timeout, so any restic
  prompt on a TTY hangs the backup forever with no deadline to break it.
* `restic.rs:422` — `list_snapshots` returns restic's 8-char `short_id`, which is later
  fed back as a snapshot selector. Collisions are unlikely but restic would then refuse
  the ambiguous prefix.

---

## Confirmed clean (checked, not findings)

* `run_bounded` drains both pipes on dedicated threads before waiting — no buffer
  deadlock; `run_bounded_does_not_deadlock_on_a_large_output` pins it at 2MB.
* The kill path does `kill()` then `wait()` then joins both readers — no zombie, no
  detached thread.
* Exit-3 handling in `snapshot()` (`restic.rs:146-159`) correctly recovers the snapshot
  id and refuses to claim success; `backup_in` records `Unknown` and counts it as a
  failure.
* `parse_snapshot_id` takes the *last* summary line and returns `None` rather than
  guessing; a missing `snapshot_id` on exit 0 is a hard error (`restic.rs:161-169`).
* `parse_snapshots` sorts on the timestamp instead of trusting restic's order, and errors
  rather than returning an empty list on garbage.
* The repository password is passed via `--password-file`, never argv or env. That part
  of the threat model is correctly implemented (see H3 for the part that is not).
* No TLS verification is disabled anywhere in the source (`--insecure-tls` never appears
  except as the injection vector in M5).
* `SnapshotId::short` truncates on a char boundary.
* `VerifyOutcome`/`Cause` never route a `Cause` to `Bad`, and
  `a_network_failure_is_never_bad` pins it.
