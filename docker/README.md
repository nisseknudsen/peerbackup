# Docker

Two separate things run in containers: the **client**, which sends backups, and
the **host**, which stores them for someone else. They are independent; you can
run either, both, or neither.

- [Hosting backups](#hosting-backups) — storing a peer's backups
- [Sending backups](#sending-backups) — the client

---

# Hosting backups

There are two setups. Start with the first.

## Quick setup, no root

```sh
cd docker
PB_DATA=/srv/peerbackup-data \
PB_UID=$(id -u) PB_GID=$(id -g) \
docker compose -f compose.host.yml up -d
```

Create a login for each peer:

```sh
docker exec -it peerbackup-rest create_user alice
docker restart peerbackup-rest
```

The restart matters. rest-server reads its password file only at startup, so a
login added without one is rejected with `401 Unauthorized`, which looks exactly
like a wrong password.

Give your peer the URL and password:

```
rest:https://alice:PASSWORD@your-host.example.org:8000/alice/
```

Forward port 8000, or point a subdomain at the machine.

### What this setup does not do

The size limit (`PB_MAX_SIZE`, 500GB by default) is applied by rest-server and
is **shared across every peer on the server**, not per peer. One peer can
therefore consume all of it. It also depends on rest-server enforcing it
correctly rather than on the kernel.

That is usually fine among friends. If you want a hard, per-peer limit that
holds regardless, use the second setup.

## Full setup, with per-peer limits

Each peer gets a fixed-size disk image, mounted separately. Their limit is
enforced by the filesystem, so filling it produces an error on their side and
cannot affect you or the other peers. Requires root, because mounting does.

```sh
sudo peerbackup-host provision alice 500G
sudo peerbackup-host adduser alice
```

Full instructions, including the systemd units and TLS: **[../docs/runbook.md](../docs/runbook.md)**.

## TLS

Backups are encrypted before upload, but the login password is sent with every
request, so use TLS on anything reachable from the internet.

```sh
mkdir -p certs
cp /etc/letsencrypt/live/example.org/fullchain.pem certs/
cp /etc/letsencrypt/live/example.org/privkey.pem   certs/
```

Uncomment the certs volume in `compose.host.yml` and set:

```sh
PB_EXTRA_OPTIONS="--tls --tls-cert /certs/fullchain.pem --tls-key /certs/privkey.pem"
```

With a self-signed certificate, the peer needs a copy of it and must pass
`--cacert` when adding you.

## Maintenance windows

`--append-only` means deletion is refused, so a peer's cleanup of old backups
will fail until you open a window:

```sh
docker compose -f compose.host.yml down
docker run --rm -d --name peerbackup-maint \
  --user "$(id -u):$(id -g)" -p 8000:8000 \
  -v /srv/peerbackup-data:/data \
  -e OPTIONS="--private-repos" restic/rest-server:0.14.0
# peer runs their cleanup, then:
docker rm -f peerbackup-maint
docker compose -f compose.host.yml up -d
```

Deletion protection is off for every peer during the window, so keep it short.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `PB_DATA` | `./peerbackup-data` | Where backups are stored |
| `PB_PORT` | `8000` | Published port |
| `PB_UID` / `PB_GID` | `1000` | Owner of the stored files |
| `PB_MAX_SIZE` | `536870912000` | Total bytes, all peers |
| `PB_EXTRA_OPTIONS` | empty | Extra rest-server flags, e.g. TLS |

---

# Sending backups

An option for machines that already run everything in containers. The plain
binary is simpler and is the recommended path; see the main README.

## The one rule

**Mount source directories at the same paths they have on the host.**

```
-v /srv/data:/srv/data          correct
-v /srv/data:/data              wrong
```

restic records the path it backed up. Mount `/srv/data` at `/data` and your
backup contains `/data`, your restores produce `/data`, and the commands in your
recovery file refer to a path that does not exist on a normal machine. Since the
point of the recovery file is to work without peerbackup, on a machine that may
be freshly installed, this matters.

peerbackup refuses to start a backup when a configured source is missing, so a
forgotten mount fails immediately. It cannot detect a source mounted at the
*wrong* path, which is why the rule above is stated first.

## Build

```sh
docker build -f docker/Dockerfile -t peerbackup .
```

## Usage

```sh
docker run --rm \
  -v ~/.config/peerbackup:/config \
  -v ~/.local/share/peerbackup:/state \
  -v /srv/data:/srv/data:ro \
  peerbackup backup
```

`/config` and `/state` must both be mounted. `/state` holds the check results
that `status` reports; without it every run starts with no history.

Source directories can be mounted read-only.

### Compose

```yaml
services:
  peerbackup:
    build:
      context: ..
      dockerfile: docker/Dockerfile
    user: "1000:1000"
    volumes:
      - ~/.config/peerbackup:/config
      - ~/.local/share/peerbackup:/state
      - /srv/data:/srv/data:ro
    # One-shot; run with `docker compose run --rm peerbackup <command>`
    entrypoint: ["peerbackup"]
    command: ["status"]
```

```sh
docker compose run --rm peerbackup init
docker compose run --rm peerbackup peer add alice rest:https://...
docker compose run --rm peerbackup backup
docker compose run --rm peerbackup status
```

### Scheduling

```ini
# /etc/systemd/system/peerbackup.service
[Service]
Type=oneshot
ExecStart=/usr/bin/docker run --rm \
  -v /root/.config/peerbackup:/config \
  -v /root/.local/share/peerbackup:/state \
  -v /srv/data:/srv/data:ro \
  peerbackup backup
ExecStart=/usr/bin/docker run --rm \
  -v /root/.config/peerbackup:/config \
  -v /root/.local/share/peerbackup:/state \
  peerbackup verify
```

## File ownership

Pass `--user` matching whoever owns the files you are backing up, or restic will
not be able to read them. peerbackup reports unreadable sources as an error
rather than backing up less than you asked for.

Any uid works, including one that has no account inside the image. restic keeps
a cache under `/state`, so it does not depend on a home directory existing.

## Restoring

Create the target directory before mounting it:

```sh
mkdir -p /tmp/restored
docker run --rm \
  -v ~/.config/peerbackup:/config \
  -v ~/.local/share/peerbackup:/state \
  -v /tmp/restored:/restored \
  peerbackup restore alice /restored
```

Docker creates a missing bind-mount source as root. Since the container runs
unprivileged, the restore then fails with a permission error.

Restored files appear under their original paths, so the example above produces
`/tmp/restored/srv/data/...`.

## Recovery

The exported recovery file contains host paths and works with a plain restic
binary. It does not depend on this image. Recovering does not require Docker:

```sh
restic -r <url> restore latest --tag peerbackup --target /where/to/put/it
```

Keep that in mind when deciding where to store the recovery file.
