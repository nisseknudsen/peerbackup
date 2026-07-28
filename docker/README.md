# Running the client in Docker

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
