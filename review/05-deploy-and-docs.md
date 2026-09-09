# peerbackup — deployment & packaging review (branch `review/full-audit`)

Scope: `Dockerfile`, `compose.yml`, `deploy/**`, `tests/*.sh`, `spike/*.sh`,
`.github/workflows/*`, `Cargo.toml`, `Cargo.lock`, `.gitignore`, `README.md`.
`src/` read only to check claims made by docs and scripts.

Findings marked **REPRODUCED** were executed against real Docker on this machine.

---

## SECURITY

### S1. HIGH — the documented default deployment ships plaintext HTTP basic auth to the internet
`README.md:33-35`, `README.md:55-61`, `compose.yml:20-21`, `src/host/server.rs:130-133`

`compose.yml:21` publishes `"${PB_PORT:-51515}:8000"` — no bind address, so
0.0.0.0. `quickstart` does the same (`src/host/server.rs:124` builds
`format!("{}:8000", o.port)` and passes it to `docker run -p`). The invite URL
the tool emits is `rest:http://{peer}:{pw}@{host}:{port}/{peer}/`
(`src/host/server.rs:200-203`) — **HTTP**, credential in the URL, sent on every
request. `README.md:34-35` then says, with no warning attached: *"Forward port
51515 to the machine, or pick another with `PB_PORT=`."*

The binary itself does warn (`src/host/server.rs:208-212`: "Use TLS if it is
exposed to the internet"), but the README's transcript of quickstart's output
(`README.md:27-31`) **truncates that warning away**, and the TLS section is 200
lines further down under "Hosting".

Failure scenario: Alice port-forwards 51515 as instructed. Any observer on the
path (café Wi-Fi, ISP, transit) captures `alice:nq7Y7PYN44nqKG83mNc9`. Repo
contents stay confidential (client-side encryption) and `--append-only` blocks
deletion, but the attacker can (a) download the whole encrypted repo, and (b)
append arbitrary data into alice's namespace until `--max-size` — which is
**shared across every peer on the host** (`compose.yml:30`) — is exhausted,
denying service to every other peer on that server. Credential reuse elsewhere
is the second-order risk.

### S2. HIGH — the documented TLS setup cannot work as written, so S1's only mitigation never lands
`README.md:231-242`, `compose.yml:39-41`, `README.md:343-349`

Three independent breakages in one eight-line recipe:

1. **Path mismatch.** README says put certs in `~/.local/share/peerbackup-certs`
   (`README.md:232-234`) then "Uncomment the certs volume in `compose.yml`"
   (`:237`). The line you uncomment is
   `- "${PB_CERTS:-./certs}:/certs:ro"` (`compose.yml:41`) — default `./certs`,
   relative to the compose file. `PB_CERTS` is **never mentioned in the README**
   and is absent from the settings table at `README.md:343-349`. Following the
   README verbatim mounts an empty (or docker-created, root-owned) `./certs`;
   rest-server exits because `/certs/fullchain.pem` does not exist. Under the
   systemd unit (`WorkingDirectory=/usr/local/share/peerbackup`) `./certs`
   resolves somewhere different again.
2. **The `cp` fails.** `/etc/letsencrypt/live` and `/archive` are `0700 root`.
   `cp /etc/letsencrypt/live/example.org/privkey.pem ...` as a normal user is
   permission-denied.
3. **With `sudo` it fails differently.** `sudo cp` lands a `root:root 0600`
   privkey in the user's directory; the container runs as `PB_UID:-1000`
   (`compose.yml:18`) and cannot read it. No `chown`/`chmod` guidance is given,
   and the obvious fix a frustrated user reaches for is `chmod 644` on a TLS
   private key.

Failure scenario: the operator tries to fix S1, hits three walls in a row,
gives up, and leaves plaintext HTTP exposed.

### S3. MEDIUM — no container hardening on the only internet-facing service
`compose.yml:10-60`

The rest-server service sets `user:` (good) but has no
`security_opt: ["no-new-privileges:true"]`, no `cap_drop: [ALL]`, no
`read_only: true` (+ tmpfs for anything it needs to write outside `/data`), no
`pids_limit`, no `mem_limit`. `src/host/mod.rs:23-25` explicitly reasons that
"the internet-facing container must not have CAP_SYS_ADMIN" — the design intent
is there, it just is not expressed in the compose file. A rest-server RCE gets
the full default capability set and setuid escalation inside the container.

### S4. MEDIUM — base images pinned by mutable tag, not digest
`Dockerfile:13,32,56`; `compose.yml:12`; `tests/docker.sh:26`;
`tests/end_to_end.sh:54`; `spike/lifecycle-spike.sh:24`;
`.github/workflows/ci.yml:27`

`rust:1.88-alpine`, `alpine:3.20` (x2), `restic/rest-server:0.14.0`,
`koalaman/shellcheck:stable`. All floating. This sits oddly beside the
considerable care taken to pin restic by SHA256 (`Dockerfile:27-31` argues the
case at length). A retagged or compromised upstream is consumed silently, and
two builds of the same commit six months apart are not the same image.
`shellcheck:stable` additionally means a new shellcheck release can red the CI
lint job with no repo change.

### S5. MEDIUM — CI actions on mutable refs; no `permissions:` block
`.github/workflows/ci.yml:24,41,44,68,69,84-86,...`; `.github/workflows/spike.yml:30`

`actions/checkout@v5` and `Swatinem/rust-cache@v2` are mutable tags;
`dtolnay/rust-toolchain@stable` is a mutable **branch**. Neither workflow
declares `permissions:`, so `GITHUB_TOKEN` gets the repository default, which on
many repos is read/write. `ci.yml:71-75` argues against adding "an unreviewed
action" for cargo-audit while three third-party actions run on moving refs in
the same file.

Failure scenario: one of those refs is repointed; the next `push` runs attacker
code with a write-capable token in the same job that has already downloaded and
executed a binary from the network.

### S6. MEDIUM — the documented client invocation puts a live password in argv and shell history
`README.md:64-68`; `tests/docker.sh:88-89,181,195`; `tests/end_to_end.sh:119,124,214`

`peerbackup connect 'rest://user:PASSWORD@host/...'` places the peer credential
in the invoking shell's history file, in `ps` output for the duration, and — for
the Docker form — permanently in `docker inspect`'s `Config.Cmd` for as long as
the container exists. There is no `--url-file`, stdin, or env alternative.

This is the exact hazard `src/host/server.rs:244-249` refuses to accept on the
host side ("readable by any process on the host through `/proc/<pid>/cmdline`…
the restic side already avoids argv and env for the same reason") — the client
side does the opposite, and the README teaches it.

### S7. MEDIUM — unvalidated `rm -rf` of a caller-supplied path, one of them as root inside a container
`tests/docker.sh:10,26-27,45`; `spike/lifecycle-spike.sh:22,83`; `tests/end_to_end.sh:12,39`

- `tests/docker.sh:10` `WORK="${WORK:-/tmp/pb-docker-e2e}"`, then `:45`
  `rm -rf "$WORK"`, and `:26-27` a **root** alpine container bind-mounts `$WORK`
  and runs `rm -rf /w/* /w/.[!.]*` — deliberately, to delete files the invoking
  user cannot. `WORK=$HOME ./tests/docker.sh` therefore wipes the home
  directory with root privileges, bypassing file permissions entirely.
- `spike/lifecycle-spike.sh:22` `WORK="${1:-/tmp/pb-spike}"`, `:83`
  `rm -rf "$WORK"`. Documented usage is `./spike/lifecycle-spike.sh [workdir]`
  (`:17`) and `spike.yml:74` runs it as `sudo -E ./spike/lifecycle-spike.sh …`.
  `sudo ./spike/lifecycle-spike.sh /` is `rm -rf /`.

No path validation anywhere (no "must be under /tmp", no `mktemp -d`).
`deploy/test-provision-root.sh:79` gets this right with `mktemp -d`.

### S8. LOW — systemd unit runs as root with zero hardening directives
`deploy/systemd/peerbackup-rest.service:32-56`

No `NoNewPrivileges=`, `ProtectSystem=`, `ProtectHome=`, `PrivateTmp=`,
`ReadWritePaths=`, `RestrictAddressFamilies=`. Driving the docker CLI is
root-equivalent anyway, so the practical gain is limited — but `ExecStartPre`
runs `peerbackup host guard`, an ordinary binary, with full root and an
unrestricted filesystem view.

### S9. LOW — `.gitignore` does not cover secrets the docs steer into the tree
`.gitignore:1-2`

Only `/target` and `/peerbackup-data`. Missing `certs/`, `*.pem`, `*.key`,
`.env`, `recovery*.txt`. `compose.yml:41`'s cert volume defaults to `./certs`
— inside the working tree — so the (already broken, see S2) TLS recipe points a
TLS private key at an unignored directory. `docker compose` also auto-reads
`.env` from the project directory, which is the natural place for a user to put
`PB_*`.

### S10. LOW — CI test servers publish a known credential on 0.0.0.0
`deploy/test-compose-e2e.sh:22,71-72`; `tests/docker.sh:13,54-55,67`

Both drive `compose.yml`, which publishes without a bind address, so
`alice:alicepw` / `me:pw` are briefly reachable on every interface of the
runner. `tests/end_to_end.sh:92` and `spike/lifecycle-spike.sh:58` get this
right with `-p "127.0.0.1:$PORT:8000"`. On a self-hosted or shared runner this
is a live, writable, append-only restic endpoint.

---

## CORRECTNESS

### C1. HIGH — **REPRODUCED** — every documented raw docker/compose start fails on a fresh machine
`README.md:54-61`, `README.md:237-242`, `README.md:305-311`, `compose.yml:38`

Reproduced verbatim on this host:

```
$ docker run -d --name … --user "$(id -u):$(id -g)" -v "$PWD/rsdata:/data" \
    -e OPTIONS="--private-repos --append-only" restic/rest-server:0.14.0
$ ls -ld rsdata          → drwxr-xr-x 2 root root
$ docker logs …          → touch: /data/.htpasswd: Permission denied
$ docker exec … create_user alice
  Error response from daemon: container … is not running
```

and via compose:

```
$ PB_DATA="$PWD/cdata" docker compose -f compose.yml up -d
$ ls -ld cdata           → drwxr-xr-x 2 root root
$ docker logs peerbackup-rest → touch: /data/.htpasswd: Permission denied  ×5
$ docker inspect --format '{{.State.Health.Status}} {{.State.Status}}'
  unhealthy restarting
```

Cause: dockerd creates a missing bind-mount source as `root:root 0755`; the
container runs as `$(id -u)` / `${PB_UID:-1000}` and cannot create
`/data/.htpasswd`. With `restart: unless-stopped` (`compose.yml:14`) this is an
infinite crash loop. `docker run -d` still exits 0, so README's `&&` chain
(`:59-61`) proceeds to `docker exec … create_user alice` against a corpse.

`peerbackup host quickstart` is immune — `src/host/server.rs:73` does
`create_dir_all` and `:80` checks writability first. So the *tool* is fine and
the *docs* are broken.

Why CI never catches it: `deploy/test-compose-e2e.sh:40` pre-creates
`$PB_ROOT/mnt/alice`/`bob`, and `tests/docker.sh:45` pre-creates `$WORK/srv`,
before compose is ever invoked. Both tests mask the exact first-run condition.

### C2. HIGH — the systemd service can never start for anyone who used `quickstart`
`deploy/systemd/peerbackup-rest.service:37,50`; `README.md:303-319`;
`src/host/grant.rs:486-516`

`ExecStartPre=/usr/local/bin/peerbackup host guard`. `guard` errors when the
mount root is missing (`grant.rs:488-493`: "does not exist; nothing
provisioned") **and** when there are zero grant directories (`grant.rs:511-516`:
"refusing to start a server with nothing to serve"). `PEERBACKUP_ROOT` is not
set by the unit, so it defaults to `/srv/peerbackup` (`src/host/mod.rs:83-85`).

`README.md:315` describes the refusal as conditional — *"**If you are using
per-peer images**, the service refuses to start when storage is not mounted"* —
which is not what the code does.

Failure scenario: Bob follows the headline quick start (`host quickstart`, data
in `~/.local/share/peerbackup-data`), then follows "Running it as a service"
(`README.md:305-311`) to make it survive reboots. `systemctl enable --now`
fails with "no grant directories under /srv/peerbackup/mnt". Nothing in that
README section tells him `host provision` is a prerequisite. If he works around
the guard, the second trap fires: the unit hardcodes
`Environment=PB_DATA=/srv/peerbackup/mnt` (`:37`), a different directory from
the one his peers' repositories are in, so the server comes up empty and his
peers start failing. `README.md:313` only tells him to change `PB_UID`/`PB_GID`.

### C3. MEDIUM — **REPRODUCED** — enabling TLS makes the compose healthcheck permanently fail
`compose.yml:43-54`, `README.md:240`, `README.md:261-265`

The healthcheck hardcodes `http://127.0.0.1:8000/`. Verified experimentally:

| server on :8000 | `wget -q -S -O /dev/null -T 3 http://… 2>&1 \| grep -q 'HTTP/'` |
|---|---|
| HTTP 200 | match → healthy |
| HTTP 401 (`--private-repos`) | match → healthy ✅ (the comment at `:44-49` is correct) |
| **TLS** (`PB_EXTRA_OPTIONS=--tls …`) | `wget: error getting response: Connection reset by peer` → **no match → unhealthy forever** |

So the README's own TLS instruction (`:240`) produces exactly the silent
route-drop that `README.md:261-265` warns reverse-proxy users about. Nothing in
CI asserts the healthcheck ever reaches `healthy` — no `docker inspect
… .State.Health.Status` appears in any script — so the whole block is untested
despite carrying twelve lines of justification.

### C4. MEDIUM — the Dockerfile's advertised arm64 support does not exist
`Dockerfile:9-12` vs `Dockerfile:36,40-47`

The header argues at length that not naming `x86_64-unknown-linux-musl` is what
makes arm64 possible ("rules out a fair share of the homeservers this is for").
But `ARG RESTIC_SHA256_arm64=` is **empty** (`:36`), so `:43-47` aborts the
build: "no pinned restic checksum for TARGETARCH=arm64". The image cannot be
built on arm64 at all — the stated motivation is unrealised. `tests/docker.sh`
(the only CI consumer, `ci.yml:112`) builds amd64 only, so this never surfaces.

Failure scenario: a friend on a Raspberry Pi / Ampere box runs
`docker build -t peerbackup .` (`README.md:104`) and it fails.

### C5. MEDIUM — wrong-architecture restic under the classic builder
`Dockerfile:33`

`ARG TARGETARCH=amd64`. BuildKit populates `TARGETARCH` automatically; the
classic builder (`DOCKER_BUILDKIT=0`, still reachable and still the default on
some older engines) does not. On arm64 with the classic builder the default
wins, the amd64 restic downloads, its amd64 checksum verifies happily, and it is
installed next to a natively-built arm64 `peerbackup`. Every backup then dies
with `exec format error` at runtime rather than at build time. Defaulting an
auto-populated ARG converts a build failure into a runtime one.

### C6. MEDIUM — no `.dockerignore`; 1.1 GB of build context per build
repo root (file absent); `README.md:104`; `tests/docker.sh:49`

`target/` in this checkout is **1.1 GB** and is uploaded to the daemon on every
`docker build`. `COPY src ./src` (`Dockerfile:23`) keeps it out of the *image*,
but not out of the context transfer, and it also means every file in the working
tree — `.git`, local `recovery.txt`, `certs/` — is shipped to the daemon.
`ci.yml:113` gives that job `timeout-minutes: 15`, which is partly why this has
not bitten.

### C7. MEDIUM — two documented host setups run with no quota at all
`README.md:55-58`, `README.md:327-331` vs `README.md:437-438`, `compose.yml:30`

Both the `docker run` host quick start (`:58`, `OPTIONS="--private-repos
--append-only"`) and the maintenance-window container (`:331`,
`OPTIONS="--private-repos"`) omit `--max-size`. "How it works" states
flatly that *"Storage on the host sits behind a size limit"* (`:437`). Under
either documented path it does not.

The maintenance command additionally hardcodes
`-v ~/.local/share/peerbackup-data:/data` (`:330`). An operator using per-peer
grants has `PB_DATA=/srv/peerbackup/mnt`
(`deploy/systemd/peerbackup-rest.service:37`); running the documented
maintenance command brings up a server on the same port serving an **empty**
data directory, and the peer's cleanup fails against a repository that appears
not to exist. It also assumes the compose file is in `$PWD` (`:328,334`), which
is false after `README.md:308` copies it to `/usr/local/share/peerbackup`, and
running `docker compose down` by hand under the `RemainAfterExit=yes` unit
leaves systemd believing the service is still active.

### C8. MEDIUM — the spike's central test can be entirely inconclusive and still print a green verdict
`spike/lifecycle-spike.sh:145-215,330-341`; `.github/workflows/spike.yml:1-8`

Phase 2 is labelled "THE CRITICAL TEST" and is described as the question "that
could invalidate the retention design" (`spike.yml:5-6`). But when a prune
finishes before the kill lands, the loop `continue`s (`:167-170`) with only a
`note`. If that happens on all five attempts the only consequence is
`warn "…inconclusive, needs more data"` (`:214`) — `WARN` is tallied separately
and `FAIL` stays 0, so `:335` prints
**"VERDICT: the rev 5 assumptions hold."** and `:340` exits 0.

Failure scenario: an upstream restic change makes prune faster or the repo
smaller; the weekly job goes green every Monday while testing nothing. The
script should exit non-zero (or at minimum not print that verdict) when
`KILL_SURVIVED == 0`.

### C9. MEDIUM — README states a restic minimum that nothing enforces
`README.md:85`; `src/engine/restic.rs:367`

*"Sending backups needs Linux and restic 0.17+ on `PATH`"*. There is no version
check anywhere in `src/` (`grep` for a version gate returns only the comment at
`restic.rs:367`: "Since 0.17 this is how a partial backup is reported"). The
container path pins 0.19.1 (`Dockerfile:34`); the native path — the one
`README.md:88-95` documents first — takes whatever the distro ships.

Failure scenario: a user on Debian bookworm (restic 0.14) installs from source
as documented. Partial-backup detection silently changes shape, which is the
"false success" class of bug this project's own git history (`b66893e Fix false
success`) exists to prevent. It fails silently rather than at startup.

### C10. MEDIUM — the per-peer flow's command sequence does not work as printed
`README.md:273-277`

```sh
sudo peerbackup host provision alice 500G
sudo peerbackup host adduser alice
sudo peerbackup host list
```

Two problems:
1. `adduser` requires a **running** rest-server container
   (`src/host/server.rs:243-289` does `docker exec … htpasswd`, then
   `docker restart`). This section never starts one, and it is the section for
   people who are *not* using `quickstart`. As written it fails with "could not
   create the login for 'alice' on container 'peerbackup-rest'".
2. `sudo` without `-E` strips `PB_PORT`/`PB_CONTAINER`, so
   `ServerOpts::from_env()` (`server.rs:26-45`) falls back to 51515 /
   `peerbackup-rest`. An operator on a non-default port gets "Nothing is
   answering on port 51515" with no hint that their own `PB_PORT` was discarded.

### C11. LOW — `quickstart` and `compose.yml` are two different servers with the same container name
`src/host/server.rs:130-146` vs `compose.yml:13`; `README.md:237-242`

`quickstart` issues a raw `docker run --name peerbackup-rest`; `compose.yml`
declares `container_name: peerbackup-rest`. A quickstart user who follows the
TLS section's `docker compose up -d` (`README.md:241-242`) collides:
"container name /peerbackup-rest is already in use". There is no documented
migration path from the quickstart container to the compose stack, and
`quickstart` has no way to pass `PB_EXTRA_OPTIONS` at all — so **TLS is
unreachable from the quick-start path** without manually tearing the container
down.

### C12. LOW — license metadata contradicts the README, and no LICENSE file ships
`Cargo.toml:9` (`license = "MIT"`) vs `README.md:483-485` ("Not yet chosen"); no
`LICENSE` file in the tree.

Absent a license grant, the default is all-rights-reserved. The README's whole
premise is "hand this to a friend" (`Cargo.toml:5-8`, `README.md:96-98`), and
`Dockerfile`/`tests/docker.sh` build a redistributable image. Meanwhile the
crate metadata asserts MIT, so `cargo publish` would distribute it as MIT and
crates.io/`cargo about`/SBOM tooling would report MIT. Pick one and add the file.

### C13. LOW — MSRV is only tested by accident
`Cargo.toml:8` (`rust-version = "1.88"`); `.github/workflows/ci.yml:41,85,120,130,156`

Every Rust job uses `dtolnay/rust-toolchain@stable`. The only thing that
actually compiles against 1.88 is `Dockerfile:13` (`rust:1.88-alpine`), reached
via `tests/docker.sh` — i.e. the MSRV claim in `README.md:86` is validated as a
side effect of a container test, and would go untested the moment that job is
skipped or the Dockerfile's Rust tag is bumped.

### C14. LOW — `VOLUME` before `USER` turns a forgotten mount into an opaque error
`Dockerfile:75,80-81`; `README.md:370-374`

`/config` and `/state` do not exist in the image, so an anonymous volume for
either is created `root:root` and the unprivileged `USER` cannot write to it.
Forgetting `-v` yields "permission denied" rather than anything that names the
missing mount, which is at odds with the Dockerfile's own stated philosophy
(`Dockerfile:5-7`: "peerbackup refuses to run if a configured source is missing,
so a forgotten mount fails immediately"). Consider dropping `VOLUME` and
`mkdir`ing the directories owned by the default uid so the failure is a clear
one.

### C15. LOW — assertions that cannot fail, and skips counted as passes
- `tests/docker.sh:76` — `[ -r "$WORK/srv" ] && ok "stored data is readable by
  the host owner" || bad "stored data is root-owned; PB_UID/PB_GID not applied"`.
  `$WORK/srv` was created by this script at `:45` with the invoking user's uid;
  it is readable no matter what the container does. The failure branch is
  unreachable and its message describes a condition the test cannot detect.
- `deploy/test-compose-e2e.sh:128-133` — same shape; `$PB_ROOT/mnt/alice` is
  created by the test at `:40`. (The real assertion is `:53-55`, which does
  compare container uid — that one is sound.)
- `deploy/test-provision-root.sh:276-277` — `HOSTFREE_AFTER=$(df …); [ -n … ] &&
  ok "host filesystem still reports free space (grant was contained)"`. `df`
  always prints a number; this asserts nothing about containment.
- `deploy/test-provision-root.sh:320-321` — `else ok "skipped: could not mount
  second image"` counts a skip as a PASS. If loop devices are exhausted the
  ordering test silently does not run and the summary is still green.
- `deploy/test-provision-root.sh:245` — if `setpriv` is absent, "the server user
  can write to the grant" (the section's stated key assertion, `:244`) is
  skipped with no SKIP line and no PASS/FAIL, unlike `:120` and `:245` elsewhere
  which do print SKIP.
- `tests/end_to_end.sh:220` — `[ ! -d "$WORK/cfg" ] && ok "…deleted"` has no
  `bad` branch: if the deletion failed, nothing is recorded and the
  disaster-recovery premise is quietly false.

None of these can make a green run red, but each can let a real regression stay
green.

### C16. LOW — the restic pin is duplicated in three files with no cross-check
`.github/workflows/ci.yml:16-17`; `.github/workflows/spike.yml:21-22`;
`Dockerfile:34-35`

Same version and digest written out three times. Bumping restic in CI and
forgetting the Dockerfile ships a container running a restic version CI never
tested — precisely the drift the pinning exists to prevent. A single source
(a file read by all three, or a build-arg fed from the workflow env) would close
it.

---

## UX / DOCS

### D1. NIT — `rest://` is not a scheme restic accepts
`README.md:40,68`

Both quick-start `connect` examples use `rest://...`. restic wants
`rest:http://…` or `rest:https://…` — which is what `README.md:30`, `:174`,
`:248` and `src/main.rs:97` correctly show. A user extrapolating from the
quick-start builds an unusable URL.

### D2. NIT — port 8000 in config/help examples vs a documented default of 51515
`README.md:174`; `src/main.rs:97`

`deploy/systemd/peerbackup-rest.service:38-42` documents at length why 8000 was
abandoned ("collides constantly, which is why 51515 was picked"). Two examples
still show 8000.

### D3. NIT — command reference is incomplete
`README.md:106-127`, `README.md:341-349`

Missing from the command list: `peerbackup init`, `recovery check`, the
`--peer` flags on `backup`/`verify`, `--snapshot` on `restore`, `--name` on
`connect`, and `host guard | doctor | up | down`. Missing from the settings
table: `PB_CONTAINER` and `PB_COMPOSE_FILE` (`src/host/server.rs:41,391`).
`host guard` in particular is load-bearing for the systemd unit and is
documented only inside that unit's comments.

### D4. NIT — "None require root" is not quite true
`README.md:472-481`

`deploy/test-provision-root.sh --in-container` runs
`docker run --rm --privileged -v "$REPO:/repo:ro"`
(`deploy/test-provision-root.sh:30-31`) — no `sudo`, but root-equivalent on the
host (it `mknod`s loop devices and mounts filesystems, `:40-43`).
`spike/lifecycle-spike.sh` phase 4a needs root too (`:271`) and degrades to a
skip without it. Worth one sentence rather than a flat claim.

### D5. NIT — CI runs every branch PR twice and cannot cancel superseded runs
`.github/workflows/ci.yml:3-7`

`push: branches: ["**"]` plus `pull_request` double-runs seven jobs on every PR
push, including the 15-minute image build and the root provisioning job. No
`concurrency:` group, so pushing three times queues three full pipelines.

---

## What is right (so it does not get "fixed")

- The restic pin **is** checksum-verified in all three places, and the Dockerfile
  correctly refuses rather than falling back to an unverified download
  (`Dockerfile:43-48`). The spike's manual-override path re-fetches upstream
  `SHA256SUMS` instead of skipping verification (`spike.yml:59-64`).
- `spike.yml:36-39` passes `workflow_dispatch` input through `env:` rather than
  `${{ }}` interpolation into `run:` — the right instinct, correctly reasoned.
- No secrets in `ENV`, `ARG`, or `docker inspect`-visible environment on the
  server side; `adduser` deliberately avoids argv by piping to `htpasswd -i`
  (`src/host/server.rs:243-262`).
- The compose healthcheck's "any HTTP status line" design is **correct** — I
  verified busybox `wget -q -S` still emits the status line on a 401 (see C3
  table). Do not "fix" it to check for 200.
- `Cargo.lock` is committed with registry checksums for all 48 packages;
  `cargo audit` gates CI and is built from source rather than via a third-party
  action.
- `Cargo.toml:28-40` declaring lints in-manifest so local clippy == CI clippy is
  genuinely good, and `ci.yml:54-55` matches `README.md:463-470` exactly.
- `tests/docker.sh:110-124` and `deploy/test-compose-e2e.sh:104-115` document
  and avoid two real vacuous-pass traps (`find -exec diff`, `grep -q` +
  `pipefail` SIGPIPE). The `set -uo pipefail`-without-`-e` choice is deliberate
  and appropriate for tally-style harnesses.
- `deploy/test-provision-root.sh:79,87` uses `mktemp -d` and `${var:?}`
  (`deploy/test-host-tooling.sh:87`) — the destructive-path hygiene that
  `tests/docker.sh` and `spike/lifecycle-spike.sh` lack (S7).
