# Running in Docker

The image `ghcr.io/nisseknudsen/peerbackup` contains peerbackup and restic, for
`linux/amd64` and `linux/arm64`. It is for the sending side. The hosting side
runs the upstream rest-server image; see [Hosting](#hosting) below.

## Sending backups

Create the config and state directories first. Docker creates a missing
bind-mount source owned by root, and the container, which runs as you, then
cannot write to it.

```sh
mkdir -p ~/.config/peerbackup ~/.local/share/peerbackup

docker run --rm -i \
  --user "$(id -u):$(id -g)" \
  -v ~/.config/peerbackup:/config \
  -v ~/.local/share/peerbackup:/state \
  -v /srv/data:/srv/data:ro \
  ghcr.io/nisseknudsen/peerbackup \
  connect 'rest:http://alice@bob.example.net:51515/alice/' --source /srv/data < password.txt
```

After that, the same mounts with `backup`, `verify` or `status` in place of
`connect ...`. Without a command, the container runs `status`.

### Mount source directories at their real paths

```
-v /srv/data:/srv/data      correct
-v /srv/data:/data          wrong
```

restic records the path it backed up. Mount `/srv/data` at `/data` and the
backup contains `/data`, restores produce `/data`, and the commands in your
recovery file name a path that does not exist on a normal machine. The recovery
file is meant to work without peerbackup, possibly on a freshly installed
machine, so the paths need to be the real ones.

peerbackup refuses to back up when a configured source is missing, so a
forgotten mount fails immediately. It cannot detect a source mounted at the
wrong path.

### Volumes

Mount both `/config` (settings and secrets) and `/state` (recorded results and
restic's cache). Without `/state`, every run starts from nothing: `status` has
no history, and restic re-reads unchanged data. Source directories can be
mounted read-only.

### Passwords

Pass the password on standard input (`-i` and `< password.txt`), not on the
command line. Arguments to `docker run` are stored in the container's metadata
and can be read back with `docker inspect` for as long as the container exists.

### File ownership

Run with `--user` set to whoever owns the files you are backing up, or restic
cannot read them. Any uid works, including one with no account in the image.

The image runs peerbackup under `tini`, so `docker stop` stops restic cleanly
too and does not leave a stale repository lock behind. `--init` is not needed.

### Restoring

Create the target directory first. Docker creates a missing bind-mount source
as root, and the container runs unprivileged, so the restore would fail.

```sh
mkdir -p /tmp/restored
docker run --rm \
  --user "$(id -u):$(id -g)" \
  -v ~/.config/peerbackup:/config \
  -v ~/.local/share/peerbackup:/state \
  -v /tmp/restored:/restored \
  ghcr.io/nisseknudsen/peerbackup restore bob /restored
```

Files are restored under their original paths, so this produces
`/tmp/restored/srv/data/...`.

### Scheduling

Run the container from a systemd timer or cron the same way as the binary (see
the [README](../README.md#scheduling-and-alerts)), with `docker run --rm` and the mounts
above as the command.

## Hosting

`peerbackup host quickstart` starts rest-server in Docker for you. To do the
same by hand:

```sh
# Create the storage directory yourself, so it belongs to you. A missing
# bind-mount source is created as root, and the server, which runs as you,
# would fail on its first write.
mkdir -p /srv/peerbackup-data

docker run -d --name peerbackup-rest --restart unless-stopped \
  --user "$(id -u):$(id -g)" -p 51515:8000 \
  -v /srv/peerbackup-data:/data \
  -e OPTIONS="--private-repos --append-only --max-size 536870912000" \
  -e GODEBUG=http2server=0 \
  restic/rest-server:0.14.0

peerbackup host adduser alice
```

`GODEBUG=http2server=0` turns off HTTP/2, which matters for peers far away; see
[hosting.md](hosting.md#reverse-proxy). For anything long-lived, use the shipped
`compose.yml` instead, as described in [hosting.md](hosting.md).

## Building the image

```sh
docker build -t peerbackup .
```

The Dockerfile needs BuildKit, which is the default builder in current Docker.
To build for the other architecture:

```sh
docker buildx build --platform linux/arm64 -t peerbackup:arm64 --load .
```

A multi-platform image (`--platform linux/amd64,linux/arm64`) can be pushed to a
registry with `--push`; loading one into the local image store needs Docker's
containerd image store.

The Rust build cross-compiles on the build machine, so building the arm64 image
on an x86 machine needs QEMU only for a few short steps in the final stage.
