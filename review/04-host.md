# peerbackup host-side audit — src/host/{mod,grant,server,size}.rs

Branch `review/full-audit`. Every finding below was traced in the source; line
numbers are from the files as they stand.

## Trust model as it actually is

Every input that reaches these four files comes from the machine's own operator:
the peer name (a `clap`-parsed `PeerName`, validated to `[A-Za-z0-9_-]{1,64}`),
the size string, an optional password, and a set of environment variables. No
peer-supplied or network-supplied value reaches any subprocess argument. There
is **no CRITICAL finding**: no command injection, no shell interpolation, no
path traversal. The interesting failures are in privilege handling, in what the
env vars are allowed to do, and in operations that report success they did not
achieve.

## Already correct — do not re-flag

* No shell anywhere. Every subprocess is `Command::new(prog).args(...)`; the one
  `sh -c` (server.rs:243-263) passes the peer name as `$1` positionally into a
  quoted expansion, not by interpolation.
* `PeerName` (config.rs:119-140) is validated at construction and carried in the
  type, so a traversal or a space cannot reach a path or a unit name.
* The htpasswd password goes in on **stdin** (`htpasswd -B -i`), never argv
  (server.rs:243-278); curl credentials go in on stdin via `--config -`
  (server.rs:472-487) with `\` and `"` escaped for curl's config grammar.
* `random_token` (config.rs:335-357) reads `/dev/urandom` and uses rejection
  sampling against `LIMIT = 224` over a 56-symbol alphabet — unbiased, ~139 bits
  at length 24. Good.
* `check_password` (server.rs:369-384) rejects control characters, which is what
  makes the two line-oriented sinks (htpasswd stdin, curl config) safe.
* `Ctx::run` (mod.rs:133-169) checks exit status, reports the exit code and
  stderr, and keeps arguments as `OsStr` so a non-UTF-8 root path is not mangled
  before reaching `chown -R` / `rm`.
* `unit_name` (mod.rs:258-277) shells out to `systemd-escape` and checks both
  the exit status and for an empty result.
* `is_mountpoint` (mod.rs:203-215) fails closed on both stat errors; `guard`
  (grant.rs:465-513) fails closed on an empty mount root and on any unmounted
  grant.
* `statvfs` usage (mod.rs:218-250) is sound: `CString` guards NUL, `MaybeUninit`
  is only read after `rc == 0`, and `f_frsize` (not `f_bsize`) is the multiplier.
* `verify_not_sparse` runs *after* `mkfs`, which is the correct point, and
  removes the image on its own failure path.
* `release`'s teardown order (disable → umount → losetup -d → rm image) is right,
  and the "already gone" steps are best-effort on purpose.
* `parse_size` uses `checked_mul` and rejects `5.5G`, `-5G`, `500 G`, `1e3`,
  `٥٠٠`, `5X`, `500GG`. I probed it further (`B`, `5BB`, `5K5`, `5T0`, `GB`) —
  all correctly rejected. `human` matches numfmt on the tested range.
* Admission control deliberately runs under `DRY_RUN` (grant.rs:44-74).
* `doctor` exits non-zero so it can gate something.
* `have()` does a PATH lookup without a shell.
* `first_numeric` parses PB_UID/SUDO_UID rather than interpolating, and names the
  offending variable.

---

# HIGH

## H1 — `DRY_RUN` is silently ignored by the whole server path, including two destructive `docker rm -f`
`src/host/server.rs:111-112`, `:130-149`, `:182-183`, `:243-278`; contract at
`src/host/mod.rs:66-69` ("Print what would happen and touch nothing").

`quickstart` and `adduser` build their docker calls with raw
`Command::new("docker")` instead of `ctx.run`, so `Ctx::dry_run` is never
consulted. Only `docker restart` (server.rs:293) and `docker compose`
(server.rs:394-396) go through `ctx.run`.

Failure scenario: an operator with a running `peerbackup-rest` container on a
non-default port rehearses with
`DRY_RUN=1 peerbackup host quickstart alice`. `container_running` is true but
`http_reachable(51515)` is false, so the run errors out — fine. Now the same
operator, container stopped (state `exited`), runs the same "dry run":
`container_running` is false → line 111 executes `docker rm -f peerbackup-rest`
for real, destroying the container and its configuration; line 130 then starts a
brand new one; line 243 writes a real htpasswd entry; and if the health poll
fails, line 182 does a second real `docker rm -f`. A dry run deleted a container
and created a credential.

`std::fs::create_dir_all(&o.data)` (server.rs:73) and the `writable()` probe file
(server.rs:437-447) are also unconditional.

## H2 — `FORCE=1` defeats the "type the peer name" confirmation on a command that permanently destroys backups, and the dry-run switch fails unsafe
`src/host/mod.rs:82`, `:92-94`; `src/host/grant.rs:340`.

Three compounding problems on the same two lines:

1. The kill switch is `FORCE`, an unnamespaced, undocumented env var (every other
   knob in this program is `PB_*` or `PEERBACKUP_*`, and the README's settings
   table lists none of `FORCE`/`DRY_RUN`/`QUIET`/`HOST_MARGIN_GB`/
   `MAINTENANCE_RESERVE_PCT`/`PEERBACKUP_ROOT`/`SYSTEMD_UNIT_DIR`). `FORCE=1` is
   a common shell habit and is exported by plenty of build/deploy scripts.
   Scenario: an operator has `export FORCE=1` in the shell they use for a
   Makefile; `sudo -E peerbackup host release alice` (or root shell) destroys
   alice's 500GB of backups with no prompt at all. There is no `--force` CLI flag
   — the env var is the *only* way in, which is exactly backwards for a
   destructive gate.
2. `flag()` matches the literal string `"1"` only. `DRY_RUN=true`, `DRY_RUN=yes`
   and `DRY_RUN=on` are all "not a dry run". Scenario:
   `DRY_RUN=true peerbackup host provision alice 500G` as root actually
   allocates 500GB, formats it and mounts it.
3. Every command that needs `DRY_RUN` also needs root, and `sudo` resets the
   environment by default. `DRY_RUN=1 sudo peerbackup host provision alice 500G`
   — the ordering most people type — drops the variable and performs the real
   provision. The safe form is `sudo DRY_RUN=1 peerbackup ...`, which nothing in
   the code or docs says.

Fix shape: `--dry-run` / `--force` as clap flags on the `host` subcommands
(env-var fallbacks kept for the shell test suite), and accept
`1|true|yes|on` case-insensitively.

## H3 — Nothing connects a provisioned grant to the server that is supposed to serve it; following the README leaves the quota unenforced while `host list` says "mounted"
`src/host/grant.rs:216-227` (provision's "next:" advice), `src/host/server.rs:36-40`
(`PB_DATA` default), README "A size limit per peer".

`provision` creates `/srv/peerbackup/mnt/<peer>` and ends by telling the operator
to run `peerbackup host adduser <peer>`. `adduser` only does `docker exec` into
whatever container `PB_CONTAINER` names; it never looks at `ctx.mnt()`, and
nothing anywhere verifies that the running container's `/data` bind source is
`ctx.root/mnt`. The only place the two are wired together is
`deploy/systemd/peerbackup-rest.service` (`Environment=PB_DATA=/srv/peerbackup/mnt`),
which the README's per-peer-limit section never mentions.

Failure scenario: operator follows the README top to bottom — `host quickstart
alice` (container comes up on `~/.local/share/peerbackup-data`), then later reads
"A size limit per peer" and runs `sudo peerbackup host provision alice 500G` and
`sudo peerbackup host adduser alice`. Everything reports success.
`sudo peerbackup host list` shows `alice 500GB 491GB 73GB 0B mounted`. But every
byte alice uploads goes to `~/.local/share/peerbackup-data/alice`, on the root
filesystem, with only the shared `--max-size`. The operator believes the kernel
is enforcing a per-peer quota; it is enforcing nothing, and 500GB of the disk is
additionally consumed by an image nobody writes to. The first symptom is a full
root filesystem — the exact failure `guard` exists to make loud.

Cheap fix: `provision` and `list` run
`docker inspect -f '{{range .Mounts}}{{.Source}}{{end}}' <container>` and warn
loudly when the server's `/data` is not `ctx.mnt()`.

---

# MEDIUM

## M1 — provision's rollback deletes the image without unmounting or detaching the loop device, then claims "Nothing was left behind" unconditionally
`src/host/grant.rs:201-215`.

The rollback is: `systemctl disable --now <unit>` (best effort, failure only
warns) → remove unit file (best effort) → `remove_dir(dir)` (`let _ =`, failure
ignored) → `remove_quietly(img)` (failure only warns) → `daemon-reload`. It then
returns a message asserting *unconditionally* that nothing was left behind.

Two concrete failures:

* `finish` fails at `take_ownership`'s `chown -R` (grant.rs:310-319, e.g. the
  mount is already busy, or a stale process holds it). Rollback runs
  `systemctl disable --now` — which fails because the mount is busy — so the
  filesystem is still mounted and its loop device is still open. `remove_dir`
  fails silently. `remove_quietly` then unlinks a **mounted** image file. That is
  precisely the mistake `release`'s own comment (grant.rs:352-354) exists to
  avoid: the blocks stay allocated to the open loop device, the grant directory
  keeps serving a deleted file, and the user is told nothing was left behind and
  that they can just run it again. The next `provision` succeeds and allocates a
  *second* full-size image, so the host is now overcommitted by one whole grant.
* The rollback never runs `losetup -d`, unlike `release` (grant.rs:365-366), so
  even the non-busy leak case is not cleaned up.

Also: the message should be conditional on the removals having actually
succeeded; `remove_quietly` and `remove_dir` both swallow failure.

## M2 — a `mkfs.ext4` failure leaves the whole preallocated image behind, outside the rollback boundary
`src/host/grant.rs:129-141` (the `?`), boundary comment at `:143-153`.

The comment says the rollback boundary was deliberately moved earlier so that a
part-way failure never requires running the destructor. But `fallocate`
(grant.rs:99-115) and `mkfs.ext4` (grant.rs:129-141) both sit *before* the
boundary, and only `verify_not_sparse` cleans up after itself.

Scenario: `sudo peerbackup host provision alice 4096` (or `... 0`, or any size
too small for ext4, or an interrupted/OOM-killed mkfs). `fallocate` succeeds,
`mkfs.ext4` exits non-zero, `?` returns immediately, and
`/srv/peerbackup/images/alice.img` — up to the full requested size for the
interrupted-mkfs case — is left on disk. The next `provision alice` refuses with
"already exists; use `peerbackup host release alice` first", and `release` is the
command that prints "this destroys alice's backups permanently". The comment's
stated goal is not met for the two largest-footprint steps.

## M3 — `release` reports "capacity returned to the host" when the loop device is still holding the space
`src/host/grant.rs:365-379`.

`losetup -d` is `run_best_effort` (warn only), `remove_quietly(img)` warns only,
`remove_dir` failure is discarded, and line 376-378 prints success
unconditionally.

Scenario, and it is the deployed configuration: the service unit bind-mounts
`/srv/peerbackup/mnt` into the container (`PB_DATA=/srv/peerbackup/mnt`,
compose.yml `volumes:`). Docker bind mounts are `rprivate`, so the container's
mount namespace holds its own reference to every grant that was mounted when it
started. `sudo peerbackup host release alice` with the container still running:
`systemctl disable --now` and the host `umount` both succeed (the host's
reference goes away), `losetup -j` still reports `/dev/loopN`, `losetup -d`
fails with EBUSY — a warning on stderr — and then the image file is unlinked.
The 500GB stays allocated to the still-open loop device until the container is
stopped, while the command prints "grant for 'alice' released, capacity returned
to the host". `df` disagrees, and the operator provisions the next friend into
space that does not exist.

`release` should stop the server (or at least refuse/warn when the container is
running), and must not print the success line when `losetup -d` or the unlink
failed.

## M4 — `adduser` mutates state before it can verify anything, and its failure message implies it did not
`src/host/server.rs:243-337`; `deploy/test-compose-e2e.sh:83` exercises exactly this path.

`htpasswd -B -i` (line 243) **replaces** an existing entry, and the container is
restarted (line 293) before verification (lines 300-337) runs. There is no
rollback and no "the credential was created but could not be verified" wording.

Scenarios:
* `peerbackup host adduser alice` (no password) for an alice who already has one
  → alice's working credential is overwritten with a freshly generated one and
  the container is restarted. Alice's nightly backup starts returning 401. No
  prompt, no warning that the peer already existed.
* Same via `quickstart`: re-running `peerbackup host quickstart alice` — the
  natural thing to do after a reboot to "make sure it's up" — silently rotates
  alice's password.
* `PB_PORT` wrong (or the server bound elsewhere): the login *is* created, the
  container *is* restarted, then verification fails and the command exits
  non-zero with "could not confirm the login for 'alice' works". The operator
  reasonably concludes nothing happened and retries, restarting the shared
  container again (and aborting any other peer's in-flight backup each time).

## M5 — the host's credential file and data directory are left at default umask permissions
`src/host/server.rs:73` (`create_dir_all`), and nothing chmods `.htpasswd`.

`create_dir_all` gives 0755; `htpasswd` inside the container creates
`/data/.htpasswd` at 0644. So `~/.local/share/peerbackup-data/.htpasswd` — the
bcrypt hash of every peer's repository password — is world-readable to every
local account on the host. The client side of this same codebase is careful here:
`config::write_private` opens with `.mode(0o600)` precisely for password files.

Scenario: any unprivileged local user (or a compromised unrelated service) reads
`.htpasswd`. Generated passwords (139 bits) are not crackable, but
`peerbackup host adduser alice hunter2` — the documented operator-supplied path,
and what `deploy/test-compose-e2e.sh` itself uses — is. With the hash cracked the
attacker has append/read access to that peer's repository over the network.

Same argument, lower stakes, for the grant directories: `mkfs.ext4` leaves the
root dir 0755 and `take_ownership` only chowns, so `/srv/peerbackup/mnt/<peer>`
is world-readable (contents are restic-encrypted, so this is defence-in-depth).

## M6 — `take_ownership` hands `/srv/peerbackup/mnt` to an unprivileged uid, which turns the next `provision` into a root mount onto an attacker-chosen path
`src/host/grant.rs:296-303`, `:155-158` (`create_dir_all(&dir)`).

`chown <uid>:<gid> /srv/peerbackup/mnt` makes the *parent of every future grant
mountpoint* writable by a non-root account (SUDO_UID by default — the admin's own
login). Provisioning then does `create_dir_all(mnt/<peer>)` with no
`symlink_metadata` / `O_NOFOLLOW` check: `create_dir_all` treats an existing
symlink-to-a-directory as success, `systemd-escape` escapes the literal path, and
the generated unit's `Where=` is handed to `mount`, which canonicalises symlinks.

Scenario: an attacker with a foothold in the admin's unprivileged account (no
sudo password) creates `ln -s /etc /srv/peerbackup/mnt/carol`. The admin later
runs `sudo peerbackup host provision carol 500G`. The mount unit is written with
`Where=/srv/peerbackup/mnt/carol` and `systemctl enable --now` mounts a blank
ext4 over `/etc`. Best case that is an immediate host-wide DoS; the same trick
aimed at a directory that is already a mountpoint gets past the
`is_mountpoint(dir)` check at grant.rs:302 as well.

Minimum fix: `symlink_metadata(&dir)` and refuse anything that is not a real
directory (or absent) before creating and before mounting; and consider chowning
only the grant directory plus the htpasswd file rather than the whole `mnt`
parent.

## M7 — `verify_not_sparse`'s 50% threshold lets a half-sparse image through and calls it verified
`src/host/grant.rs:246`.

`if allocated < apparent / 2` — a grant that is 51% allocated passes and prints
"preallocation verified after mkfs: 255GB reserved on the host" for a 500GB
grant. The whole point of the check is that a grant is a hard reservation; a
filesystem that half-honours `fallocate` (or a partially discarded image) is
exactly the "overcommitted by the mechanism this design exists to prevent" case
the module header names. With `-E nodiscard` on a working filesystem the ratio is
~1.0, so a 95% threshold would be both safe and meaningful.

## M8 — the uid `provision` chowns to and the uid the container runs as are chosen independently and never compared
`src/host/grant.rs:281-286` vs `deploy/systemd/peerbackup-rest.service`
(`PB_UID=1000`) and `compose.yml` (`user: "${PB_UID:-1000}"`).

`take_ownership` prefers `PB_UID`, else `SUDO_UID`, else `getuid()`. Under
`sudo peerbackup host provision`, sudo's `env_reset` drops any exported `PB_UID`,
so `SUDO_UID` wins.

Scenario: the admin's account is uid 1001. `sudo peerbackup host provision alice
500G` chowns the grant to 1001:1001. The service unit starts rest-server as
1000:1000. rest-server gets EACCES creating `/data/alice`, and alice's backups
fail with a 500 from a server that `host list`, `guard` and `doctor` all report as
healthy (`doctor` checks readability by *the invoking* user, not by PB_UID).
`provision` should either read the effective PB_UID from the container/compose or
print the uid it used next to the uid the server will run as.

## M9 — `quickstart` run under sudo silently produces a root-owned server
`src/host/server.rs:121` (`format!("{}:{}", uid(), gid())`).

`quickstart` does not need root and never says so. Under `sudo`, `uid()` returns
0, so the container is started `--user 0:0` and every repository is created
0700 root:root; `home()` also resolves to `/root`, so `PB_DATA` silently moves.
This is the README's own troubleshooting entry ("You cannot read your own stored
backups without sudo"), reachable by a single natural mistake — the neighbouring
`provision`/`release` commands *do* require sudo, so typing it here is the
expected habit. `is_root()` already exists in `mod.rs`; `quickstart` should warn.

## M10 — a unit test starts a real rest-server container and deletes a container named `x` on any machine with Docker
`src/host/server.rs:614-647` (`quickstart_cannot_be_handed_a_name_that_would_break_a_path`).

The test calls `quickstart` with `dry_run: true`, `container: "x"`,
`data: /tmp/pb-unused-data`, `port: 51515`. Because of H1, `dry_run` buys
nothing here. On any developer or CI machine where `docker info` succeeds,
`cargo test` will: create `/tmp/pb-unused-data`, run `docker rm -f x` (deleting
an unrelated container that happens to be called `x`), and — if port 51515 is
free — `docker run -d --name x -p 51515:8000 -v /tmp/pb-unused-data:/data
restic/rest-server:0.14.0`, leaving it behind with `--restart unless-stopped`.
It then proceeds into `adduser`. The test's assertion (`!e.contains("peer name")`)
passes either way, so nothing flags it.

## M11 — `quickstart`'s "already running" path adopts a container it has not checked, then prints a storage path that may be wrong
`src/host/server.rs:88-107`, `:196`.

The reuse branch only checks that *something* answers on `o.port`. It does not
check the running container's bind source, its `--user`, or its `--max-size`.

Scenario: the operator first ran `PB_DATA=/mnt/big peerbackup host quickstart
alice`. Months later they run `peerbackup host quickstart bob` without the
variable. The default data dir is created (empty, a red herring), the existing
container answers on 51515, bob's login is written into `/mnt/big/.htpasswd`, and
the final output says `Storage:   /home/me/.local/share/peerbackup-data`. Every
subsequent capacity decision the operator makes is based on the wrong
filesystem.

---

# LOW

## L1 — no `--` terminator on any subprocess argument list
`src/host/grant.rs:99-115` (`fallocate`), `:129-141` (`mkfs.ext4`), `:296-319`
(`chown`), `:359` (`umount`), `src/host/server.rs:112,131,257` (docker).

Paths derived from `PEERBACKUP_ROOT` and the container name from `PB_CONTAINER`
are passed positionally with no `--`. `PEERBACKUP_ROOT=-o` makes the image path
`-o/images/alice.img`, which `fallocate` parses as an option;
`PB_CONTAINER=-e` derails `docker exec`. Not exploitable across a trust boundary
(the operator sets these and is already root), but it is one `--` per call site
and removes the class.

## L2 — `PEERBACKUP_ROOT` / `SYSTEMD_UNIT_DIR` are unvalidated and feed a root-written systemd unit
`src/host/grant.rs:167-186`, `src/host/mod.rs:84-89`.

The unit body interpolates `img.display()` and `dir.display()` into `What=` and
`Where=`, and `ctx.units` decides where the file is written. A root path
containing a newline injects arbitrary unit directives (`ExecStartPost=`) into a
file that is then `systemctl enable --now`-ed. The operator is root already, so
this is only a real escalation under a sudoers rule that permits
`peerbackup host provision` with `env_keep`/`-E`. Cheap hardening: require
`ctx.root` to be absolute and reject any component containing a newline or `%`
(systemd specifier).

## L3 — unvalidated numeric env vars can panic instead of erroring
`src/host/mod.rs:76-81`, `src/host/grant.rs:60`, `:448`.

`host_margin_gb * 1024^3` and `total * reserve_pct / 100` are unchecked, and the
release profile sets `overflow-checks = true`. `HOST_MARGIN_GB=17179869184
peerbackup host provision alice 1G` panics with "attempt to multiply with
overflow" and a Rust backtrace rather than "HOST_MARGIN_GB is out of range".
`MAINTENANCE_RESERVE_PCT=200` is accepted and prints a RESERVE larger than
USABLE. Bound both at parse time.

## L4 — `is_mountpoint`'s st_dev comparison reports btrfs subvolumes as mounted
`src/host/mod.rs:203-215`.

Distinct btrfs subvolumes have distinct `st_dev` without being mount points, and
`verify_not_sparse`'s own error text recommends btrfs. If `/srv/peerbackup/mnt`
is btrfs and `<peer>` happens to be a subvolume (created by hand, or recreated by
a snapshot-based restore), `guard` prints "ok" and lets the server start against
storage with no quota — the one thing `guard` exists to prevent. Reading
`/proc/self/mountinfo` would be exact.

## L5 — `loop_device_for` only detaches the first loop device and ignores losetup's exit status
`src/host/grant.rs:395-405`.

`losetup -j <img>` prints one line per attachment. If the image was attached
twice (a failed earlier release, a manual `losetup`), only the first is detached
and the second keeps the space allocated after the image is unlinked. The
`.ok()?` also treats "losetup failed" the same as "no attachments".

## L6 — `list`'s IMAGE column reports apparent size, so a grant that lost its preallocation still looks full-size
`src/host/grant.rs:442`, `:453-461`.

`m.size()` rather than `m.blocks() * 512`. `provision` verifies allocation once,
at creation; `list` — the command the README presents as the capacity dashboard,
and the one `provision` calls to show "the numbers that matter" — can never
detect that an image later became sparse (hole-punching via a loop device with
discard, a `cp` without `--sparse=never`, a restore from a backup that did not
preserve allocation). Related: an image that is missing entirely prints `0B` next
to STATE `mounted`.

## L7 — `list` prints a mounted grant with zeroes when `statfs` fails
`src/host/grant.rs:445-452`.

`if let Ok((total, avail))` with no else: a mounted grant whose `statvfs` fails
shows `USABLE 0B / RESERVE 0B / USED 0B` and STATE `mounted`, which reads as "an
empty grant" rather than "I could not measure this".

## L8 — `provision` returns an error after fully succeeding if the closing `list` fails
`src/host/grant.rs:220`.

`list(ctx, Some(peer))?` is after the rollback boundary. A `read_dir` failure on
`mnt` makes a completed provision exit non-zero with no cleanup and no indication
the grant exists.

## L9 — `unit_name` discards systemd-escape's stderr
`src/host/mod.rs:258-277`.

`PEERBACKUP_ROOT=./data` (relative) makes `systemd-escape --path` fail with
"Path not absolute"; the user sees "systemd-escape produced an empty unit name
for ./data/mnt/alice" — after `fallocate` and `mkfs` have already run. Include
the child's stderr in the message.

## L10 — `provision` does not check that the grant directory is empty before mounting over it
`src/host/grant.rs:36-41`, `:155-158`.

The "already exists" guard only looks at the image. If `mnt/<peer>` exists with
data in it (a leftover from a `release` whose unmount failed, or a plain-directory
setup being migrated), provision mounts an empty ext4 over it. The data is not
destroyed but becomes invisible and keeps consuming the root filesystem, and
nothing says so.

## L11 — restarting the shared container for one peer aborts every other peer's in-flight backup
`src/host/server.rs:291-293`.

The restart is genuinely required (rest-server reads .htpasswd once), but
`adduser` says nothing about the blast radius. One line — "this interrupts any
backup currently uploading" — would let the operator pick a moment.

## L12 — `quickstart` publishes on all interfaces and hands out an `http://` URL carrying the credential
`src/host/server.rs:120` (`-p {port}:8000`), `:188-192` (the invite line).

The README covers TLS, but the default happy path prints
`rest:http://alice:<password>@host:51515/alice/`, and HTTP Basic sends that
credential in cleartext on every request. A passive observer on any hop gets
append/read access to the repository (contents stay restic-encrypted). Binding
`127.0.0.1` by default, or refusing to print an `http://` invite for a
non-loopback hostname without an explicit opt-in, would make the choice
deliberate.

---

# NITs

* `size.rs:37-44` — a bare byte count that overflows u64 (`"99999999999999999999"`)
  reports "bad size … use e.g. 500G" instead of the "too large to represent"
  message the suffixed form gets.
* `size.rs:80-88` — at ≥1024 TB, `human` prints `1024TB` where numfmt prints
  `1.0PB`, because the promotion branch is guarded by `idx + 1 < UNITS.len()`.
  Harmless today.
* `grant.rs:218` — "Numbers that matter, all three of them" is followed by a
  five-column table.
* `grant.rs:436-461` — the table is `{:<14}` while `PeerName` allows 64
  characters, so a long name shifts every subsequent column.
* `grant.rs:522-528` — `doctor` counts a missing `docker` as a problem even on a
  host that only ever uses `provision`.
* `server.rs:437-447` — `writable()` leaves its probe file behind if the unlink
  fails, and races another process using the same pid namespace.
* `server.rs:26-31` — `PB_PORT=0` is accepted; docker then picks a random port,
  `port_free(0)`/`http_reachable(0)` both fail, and the container is torn down
  with "could not start the server on port 0".
* `server.rs:516-525` — `hostname()` falls back to `localhost` and prints it
  straight into the invite URL with no warning that the friend cannot use it.
* `server.rs:429-437` — `container_running` matches on `docker ps` names only, so
  a container in `restarting` state may or may not be seen depending on the
  Docker version.
