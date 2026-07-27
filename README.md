# peerbackup

Back up your homeserver to your friends' homeservers, and prove you can restore it.

Everyone needs a 3-2-1 backup strategy and nobody has the offsite copy. Meanwhile
you and your friends all have idle terabytes in basements. peerbackup lets you
reserve space for each other and, more importantly, tells you with evidence
whether you could actually restore right now.

**Status: early. The host side works; the client is a seam and a plan.**

## What it is

peerbackup does not implement backup. Stock [`restic`](https://restic.net) does
the encryption, chunking and dedup; stock
[`rest-server`](https://github.com/restic/rest-server) receives it. peerbackup
owns the parts nobody has built: multi-peer bookkeeping, canary verification,
an evidence store, a recovery bundle, and a dashboard that never claims more
than it checked.

The headline feature is proof. Every friend-backup arrangement runs on hope, and
you find out on the worst day of your year. `status` answers three ways per peer:

- `verified-good` — read back and matched, with the coverage percentage
- `verified-bad` — read back and did not match
- `unknown` — could not check, so nothing is claimed

A network problem can never produce `verified-bad`. That distinction is enforced
by the type system, not by convention, because a dashboard that cries wolf is
worse than no dashboard.

## Design constraints

- No VPN. A peer is a URL, reachable by port forward or your own domain.
- Trust is out-of-band. These are your friends; there is no defence against a
  malicious peer, only against accidents, bitrot and dead disks.
- N independent full replicas, one repository and key per peer. No erasure coding.
- Repositories stay readable by plain `restic`, so recovery never depends on
  peerbackup existing.

## Host side (works today)

What a friend runs to hold your backups:

```bash
sudo peerbackup-host provision nisse 500G   # preallocated image + mount unit
sudo peerbackup-host adduser   nisse        # credential, restart, verify
sudo peerbackup-host list                   # image / usable / reserve / used
sudo peerbackup-host release   nisse        # ordered teardown
```

Per-peer quota is a preallocated disk image mounted on the host and bind-mounted
into an unprivileged container, so `ENOSPC` lands per peer at the kernel. The
container refuses to start if any grant directory is not really a mountpoint:
that failure is otherwise silent, and the first symptom is a full disk.

See [`docs/runbook.md`](docs/runbook.md) for setup and the traps.

## Tests

```bash
cargo test                        # engine seam
./deploy/test-host-tooling.sh     # provisioning logic, no docker or root
./deploy/test-compose-e2e.sh      # real rest-server + real restic
./deploy/test-provision-root.sh --in-container   # real loop devices and quota
./spike/lifecycle-spike.sh        # ~2GB, interrupted prunes, slow
```

No mocks. The product's claim is that a printed page plus stock restic recovers
your data, and that claim is worth exactly the realism of the test behind it.

## Things that cost us, so they may cost you

Measured against restic 0.19.1 and rest-server 0.14.0:

- **`mkfs.ext4` discards by default**, punching holes back through `fallocate`.
  64M preallocated becomes 4.5M allocated. Use `-E nodiscard`, and verify the
  allocation *after* mkfs, not before.
- **rest-server reads `.htpasswd` once at startup.** A credential added later
  returns 401 until you restart, which is indistinguishable from a wrong password.
- **The rest-server image runs as uid 0** and creates repos `0700 root:root`
  through bind mounts, so you cannot `du` your own data. Pass `--user`.
- **restic retries transport failures forever.** A 1% check against an
  unreachable peer ran 631 seconds. Anything scheduled needs its own deadline.
- **restic appends a Go trace to ordinary errors.** It is not a panic but it
  reads like one. Strip it, and never pipe restic through `tail` to catch an
  error: the real message is *above* the trace.
- **An interrupted prune leaves the repository restorable.** Verified across
  three landed SIGKILLs, though only during the repack phase.

## Layout

```
src/engine/     the seam: trait, three-state outcome, restic driver
deploy/         host provisioning, compose, systemd, tests
spike/          the lifecycle spike that validated the design
docs/runbook.md what a peer host actually runs
TODOS.md        deferred work, with the reasoning kept
```

## License

Not yet chosen.
