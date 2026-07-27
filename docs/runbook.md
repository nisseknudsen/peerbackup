# peerbackup — peer host runbook

What a friend actually runs to hold your backups, and what the T1 spike proved
about it. Every command here was executed against stock `restic 0.19.1` and
`restic/rest-server:0.14.0` on 2026-07-27.

Nothing in this document is aspirational. If it is written here, it ran.

---

## What the spike verified

Run it yourself: `./spike/lifecycle-spike.sh`
(needs docker and a restic binary; set `RESTIC_BIN` if restic is not on `PATH`).

| Assumption from design rev 5 | Result |
|---|---|
| An interrupted prune leaves the repo openable and restorable | **HOLDS** — 3/3 landed SIGKILLs survived: repo opened, `check` passed, canary restored byte-identical |
| `--append-only` blocks prune, and fails loudly | **HOLDS** — exit code `3`, `403 Forbidden`, `failed to remove one or more snapshots` |
| `--append-only` still allows new backups | **HOLDS** |
| A repo survives hitting its size limit | **HOLDS** — `507 Insufficient Storage`, exit `1`, repo still opens and checks clean |
| The host owner can inspect their own peer directory | **ONLY WITH `--user`** — see the gotcha below |

**Honest limitation.** All three successful kills landed within 0.5s, which on a
2GB repo means they hit the *repack* phase. Prune's riskiest window is between
rewriting the index and deleting the now-obsolete packs, and this spike never
landed a kill there because prune finished by 0.8s. What is proven: prune is
crash-safe during repack. What is not yet proven: crash-safety in the
index-rewrite-to-delete window. Re-run against a repo large enough that prune
takes 10+ seconds to close that gap.

---

## Gotchas found the hard way

**1. The image is configured by environment, not by arguments.**

```bash
# WRONG — runc tries to exec "--no-auth" as a binary
docker run restic/rest-server:0.14.0 --no-auth --path /data

# RIGHT
docker run -e DISABLE_AUTHENTICATION=1 -e OPTIONS="--append-only" restic/rest-server:0.14.0
```

**2. The image runs as uid 0 and creates repos `0700 root:root` on the host.**

Through a bind mount that means the *host owner cannot read their own peer
directory*. No `du`, no quota monitoring, no teardown without sudo:

```
drwx------ 7 root root 160 /srv/peerbackup/nisse
du: cannot read directory '/srv/peerbackup/nisse': Permission denied
```

Always pass `--user`:

```bash
--user "$(id -u):$(id -g)"
```

**3. restic appends a Go error-location trace to failures.**

It is not a panic, but it looks exactly like one:

```
failed to remove one or more snapshots
main.init
	/restic/cmd/restic/cmd_forget.go:67
runtime.doInit1
	/usr/local/go/src/runtime/proc.go:8103
...
```

peerbackup must strip lines matching `^(runtime|main)\.` and `^\s+/` before
showing a user anything, or every ordinary error will read as a crash.

**4. Capture exit codes without a pipe.**

`restic ... | tail -5` makes `$?` the exit code of `tail`, and `tail -5` also
shows only the Go trace, hiding the real error above it. Redirect to a file,
capture `$?`, then grep the whole thing.

Observed exit codes:

| Condition | Exit | Signature in output |
|---|---|---|
| prune blocked by `--append-only` | `3` | `403 Forbidden` |
| write past `--max-size` | `1` | `507 Insufficient Storage` |

---

## Host setup

### 1. Provision the quota volume

Preallocated, **not** sparse. A sparse image caps the inner filesystem but
reserves no host blocks, so granting 500GB each to three friends on a 1TB disk
still lets the host fill up.

```bash
sudo mkdir -p /srv/peerbackup
sudo fallocate -l 500G /srv/peerbackup/nisse.img      # NOT truncate
sudo mkfs.ext4 -q /srv/peerbackup/nisse.img
sudo mkdir -p /srv/peerbackup/mnt/nisse
```

### 2. Mount it on boot, as a hard dependency

Mounting a loop file needs `CAP_SYS_ADMIN`, which the container must not have.
So the host mounts it and the container receives an already-mounted directory.

`/etc/systemd/system/srv-peerbackup-mnt-nisse.mount`:

```ini
[Unit]
Description=peerbackup quota volume for nisse

[Mount]
What=/srv/peerbackup/nisse.img
Where=/srv/peerbackup/mnt/nisse
Type=ext4
Options=loop,rw

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable --now srv-peerbackup-mnt-nisse.mount
```

### 3. Fail closed if the mount is missing

**This is the one that bites silently.** If Docker starts before the mount
settles, the bind mount resolves to an ordinary directory on the host root
filesystem with no size limit at all, and the first symptom is a full disk.

```bash
#!/usr/bin/env bash
# /usr/local/bin/peerbackup-guard — refuse to serve without real mounts
set -euo pipefail
for d in /srv/peerbackup/mnt/*; do
  mountpoint -q "$d" || { echo "FATAL: $d is not a mountpoint, refusing to start" >&2; exit 1; }
done
```

Wire it in as `ExecStartPre=` on the container unit, and make the `.mount` unit
a `Requires=` plus `After=` dependency. Verify by masking the mount unit and
rebooting: the container must refuse to start.

### 4. Run the server

```bash
docker run -d --name peerbackup-rest \
  --restart unless-stopped \
  --user "$(id -u):$(id -g)" \
  -p 8000:8000 \
  -v /srv/peerbackup/mnt:/data \
  -e OPTIONS="--private-repos --append-only --max-size 536870912000" \
  -v /srv/peerbackup/certs:/certs:ro \
  restic/rest-server:0.14.0
```

`--private-repos` confines each credential to its own subdirectory, so
`/data/nisse` is reachable only by the `nisse` credential. Per-peer quota comes
from that subdirectory being its own mount; `--max-size` is a global backstop.

Create a credential per grantee:

```bash
docker exec -it peerbackup-rest create_user nisse
```

### 5. Teardown, when someone stops hosting

Order matters. `rm` on a mounted image is not a teardown.

```bash
sudo systemctl disable --now srv-peerbackup-mnt-nisse.mount
sudo umount /srv/peerbackup/mnt/nisse    # if still mounted
sudo losetup -D                          # detach
sudo rm /srv/peerbackup/nisse.img        # only now
```

---

## Maintenance windows (prune)

`--append-only` is a **process flag, not a per-user permission**. There is no
per-grantee maintenance credential; the spike confirms prune fails with `403`
and exit `3` while the flag is set.

So pruning requires the host to briefly drop the guard for everyone:

```bash
docker stop peerbackup-rest
docker run -d --name peerbackup-rest-maint \
  --user "$(id -u):$(id -g)" -p 8000:8000 \
  -v /srv/peerbackup/mnt:/data \
  -e OPTIONS="--private-repos" \
  restic/rest-server:0.14.0
# ... grantee runs their prune ...
docker rm -f peerbackup-rest-maint
docker start peerbackup-rest
```

Coordinate it in the group chat. Exposure is bounded to the length of one prune,
a few times a year, against a threat the design already declines to defend
against (design rev 5, P4).

---

## Client side, for reference

```bash
export RESTIC_PASSWORD_FILE=~/.config/peerbackup/friendA.pass
export RESTIC_CACERT=~/.config/peerbackup/friendA-ca.pem
R="rest:https://user:pass@peerbackup.example.org:8000/nisse/"

restic -r "$R" init
restic -r "$R" backup /srv/data
restic -r "$R" snapshots
restic -r "$R" check --read-data-subset=1%
restic -r "$R" restore latest --target /tmp/restore --include /srv/data/canary
```

These are the exact commands the recovery bundle must print, because after a
disaster there is no peerbackup binary, only restic and a piece of paper.
