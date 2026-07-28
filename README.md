# peerbackup

Encrypted, incremental backups between servers owned by people who know each
other.

You reserve disk space for a friend, they reserve space for you, and both of you
get an offsite copy without paying for cloud storage. Data is encrypted before it
leaves the source machine, so the host cannot read what they are storing. After
the first backup only changed data is transferred.

Backups are ordinary [restic](https://restic.net) repositories. peerbackup adds
what restic does not cover: managing several peers, keeping each within its
allowance, and regularly reading data back to confirm it is still intact.

## Quick start

Two commands. One person hosts, the other sends.

### Host: give a friend some space

```sh
./deploy/peerbackup-host quickstart alice
```

Starts a server in Docker and prints a URL to send them:

```
Ready. Send this to alice, over something you trust:

  rest:http://alice:nq7Y7PYN44nqKG83mNc9@your-host:8000/alice/
```

No root, no systemd. Storage defaults to `/srv/peerbackup-data`; override with
`PB_DATA=`. Forward port 8000 to the machine.

### Send: back up to that URL

```sh
peerbackup connect 'rest://...' --source /srv/data
```

Creates the repository, uploads a test file, downloads it again and compares it,
so a wrong URL or password fails immediately. Then:

```sh
peerbackup backup      # send a backup
peerbackup status      # is everything still fine?
```

### The same two, in Docker

```sh
# Host
docker run -d --name peerbackup-rest --restart unless-stopped \
  --user "$(id -u):$(id -g)" -p 8000:8000 \
  -v /srv/peerbackup-data:/data \
  -e OPTIONS="--private-repos --append-only" \
  restic/rest-server:0.14.0 \
  && docker exec -it peerbackup-rest create_user alice \
  && docker restart peerbackup-rest

# Send
docker run --rm \
  -v ~/.config/peerbackup:/config \
  -v ~/.local/share/peerbackup:/state \
  -v /srv/data:/srv/data:ro \
  peerbackup connect 'rest://...' --source /srv/data
```

Source directories must be mounted at the same paths they have on the host. See
[Docker](#docker) below for why.

## Status

Working, but young. Everything above functions and is covered by tests,
including a disaster-recovery test that deletes every peerbackup file and
restores the data using restic alone.

Not yet implemented: scheduling (use a systemd timer or cron), retention and
pruning, prebuilt binaries.

## Requirements

Sending backups needs Linux and [restic](https://restic.net) 0.17+ on `PATH`, or
Docker. Hosting needs Linux and Docker. Building from source needs Rust 1.88+.

## Installation

No prebuilt binaries yet.

```sh
git clone https://github.com/nisseknudsen/peerbackup
cd peerbackup
cargo build --release
sudo install -m 0755 target/release/peerbackup /usr/local/bin/
sudo install -m 0755 deploy/peerbackup-host /usr/local/bin/   # only if hosting
```

Or build the client image, which bundles restic:

```sh
docker build -t peerbackup .
```

## Commands

```sh
peerbackup connect <url> --source <dir>   # set up and connect to a peer
peerbackup backup                         # send a backup to every peer
peerbackup verify                         # read data back and check it
peerbackup status                         # summary per peer
peerbackup snapshots <peer>               # list backups stored on a peer
peerbackup restore <peer> <target>        # restore the most recent backup
peerbackup recovery export                # save what you need to restore later
peerbackup peer add|list|remove           # manage peers individually
```

Hosting:

```sh
peerbackup-host quickstart <peer>         # start a server and add a peer
peerbackup-host adduser <peer>            # add another peer later
peerbackup-host list                      # allowances and usage
peerbackup-host provision <peer> <size>   # per-peer size limit (needs root)
peerbackup-host release <peer>            # give the space back
```

`status` reads locally recorded results and does not contact peers, so it
returns immediately and works offline:

```
PEER         STATE       BACKED UP    CHECKED      TEST FILE    READ BACK
alice        ok          2h ago       1d ago       3d ago       1%
bob          unchecked   6h ago       12d ago      40d ago      -
```

`unchecked` means results are older than the configured windows, not that
anything is wrong. A peer is reported as `FAILED` only when data was read back
and did not match.

## Recovery details

```sh
peerbackup recovery export
```

Writes repository URLs, passwords and ready-to-run restic commands to a file.
Keep a copy somewhere other than the machine being backed up. Without it the
backups cannot be decrypted by anyone, including you. peerbackup warns when the
exported file no longer matches your configuration.

## Configuration

`~/.config/peerbackup/config.toml`:

```toml
[settings]
sources = ["/srv/data", "/home/me/documents"]

# Upload ceiling in KiB/s. 0 disables the limit.
upload_limit_kib = 5000

# Percentage of stored data read back and checked by `verify`.
verify_subset_pct = 1

# A peer is reported as `unchecked` when results are older than these.
liveness_hours = 48
subset_days = 10
canary_days = 35

[[peer]]
name = "alice"
url = "rest:https://me:PASSWORD@alice.example.org:8000/me/"
```

Passwords are stored separately in `~/.config/peerbackup/secrets/`, mode 0600.

### Scheduling

There is no built-in scheduler.

```ini
# /etc/systemd/system/peerbackup.service
[Service]
Type=oneshot
ExecStart=/usr/local/bin/peerbackup backup
ExecStart=/usr/local/bin/peerbackup verify
```

```ini
# /etc/systemd/system/peerbackup.timer
[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

## Docker

### Mount source directories at their real paths

```
-v /srv/data:/srv/data          correct
-v /srv/data:/data              wrong
```

restic records the path it backed up. Mount `/srv/data` at `/data` and your
backup contains `/data`, your restores produce `/data`, and the commands in your
recovery file refer to a path that does not exist on a normal machine. Since the
recovery file exists to work without peerbackup, on a machine that may be freshly
installed, this matters.

peerbackup refuses to start a backup when a configured source is missing, so a
forgotten mount fails immediately. It cannot detect a source mounted at the
*wrong* path, which is why this is stated first.

### Volumes

`/config` and `/state` must both be mounted. `/state` holds the check results
`status` reports, and restic's cache; without it every run starts from nothing.

Source directories can be mounted read-only.

### File ownership

Pass `--user` matching whoever owns the files you are backing up, or restic
cannot read them. Any uid works, including one with no account inside the image.

### Restoring

Create the target directory first. Docker creates a missing bind-mount source as
root, and the container runs unprivileged, so the restore would fail on
permissions.

```sh
mkdir -p /tmp/restored
docker run --rm \
  -v ~/.config/peerbackup:/config \
  -v ~/.local/share/peerbackup:/state \
  -v /tmp/restored:/restored \
  peerbackup restore alice /restored
```

Restored files appear under their original paths, so this produces
`/tmp/restored/srv/data/...`.

### Host settings

`compose.yml` reads these from the environment:

| Variable | Default | Meaning |
|---|---|---|
| `PB_DATA` | `./peerbackup-data` | Where backups are stored |
| `PB_PORT` | `8000` | Published port |
| `PB_UID` / `PB_GID` | `1000` | Owner of the stored files |
| `PB_MAX_SIZE` | `536870912000` | Total bytes, all peers |
| `PB_EXTRA_OPTIONS` | empty | Extra rest-server flags, e.g. TLS |

### Maintenance windows

Deletion is refused by default, so a peer's cleanup of old backups fails until
you open a window:

```sh
docker compose down
docker run --rm -d --name peerbackup-maint \
  --user "$(id -u):$(id -g)" -p 8000:8000 \
  -v /srv/peerbackup-data:/data \
  -e OPTIONS="--private-repos" restic/rest-server:0.14.0
# peer runs their cleanup, then:
docker rm -f peerbackup-maint
docker compose up -d
```

Protection is off for every peer during the window, so keep it short.

## Going further

`quickstart` gives every peer on the server one shared size limit. For a limit
per peer, enforced by the filesystem so one peer cannot consume another's share,
see [docs/runbook.md](docs/runbook.md). It also covers TLS, systemd units and
maintenance windows.

## How it works

Each peer holds a separate restic repository with its own encryption key, so a
leaked password for one peer does not expose the others. Backups run one peer at
a time; running them in parallel only divides the same uplink.

Storage on the host sits behind a size limit, and deletion is refused by default,
so a compromised client cannot erase its own backup history. Removing old backups
requires the host to open a short maintenance window.

Verification restores a small test file included in every backup and compares it
against a digest recorded when it was created, then reads back a percentage of
stored data and checks it. Results go to an append-only log, which is what
`status` reads.

## Why not implement this as a restic backend?

restic's backends are compiled into the binary, so adding one means maintaining a
fork. Repositories written by that fork would not be readable by upstream restic,
which is a poor property for the tool you reach for after losing a machine.

An alternative is a local process that presents itself as a repository and
mirrors writes to several peers at once. That has an unresolved failure mode:
when one peer accepts a write and another rejects it for lack of space, there is
no correct answer to return. Reporting success leaves one peer incomplete;
reporting failure causes retries against peers that already hold the data.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

Integration tests use a real rest-server and a real restic. None require root:

```sh
./tests/end_to_end.sh                             # full client lifecycle
./tests/docker.sh                                 # container image, both sides
./deploy/test-host-tooling.sh                     # host tooling
./deploy/test-compose-e2e.sh                      # container and isolation
./deploy/test-provision-root.sh --in-container    # storage provisioning
./spike/lifecycle-spike.sh                        # slow, ~2GB
```

## License

Not yet chosen.
