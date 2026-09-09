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
peerbackup host quickstart alice
```

Starts a server in Docker and prints a URL to send them:

```
Ready. Send this to alice, over something you trust:

  rest:http://alice:nq7Y7PYN44nqKG83mNc9@your-host:51515/alice/

They run:  peerbackup connect '<that url>' --source /path/to/back/up

Storage:   /home/you/.local/share/peerbackup-data
Port 51515 must reach this machine. Use TLS if it is exposed to the
internet: see the README.
```

No root, no systemd. Storage goes to `~/.local/share/peerbackup-data`; change it
with `PB_DATA=`. Forward port 51515 to the machine, or pick another with
`PB_PORT=`.

That last line is not boilerplate. The URL above is `http://`, and HTTP basic
auth sends that password with every request. Backups stay encrypted either way,
but anyone on the path can read the credential and then append to or read your
friend's repository. Forward the port to the internet only behind [TLS](#tls) or
a reverse proxy that terminates it. Between two machines on the same LAN or over
a VPN, plain HTTP is fine.

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
# Create the storage directory yourself, owned by you. If Docker creates it as
# a missing bind-mount source it makes it root-owned, the container runs as
# you, and the server dies on its first write -- after `docker run -d` has
# already exited 0.
mkdir -p /srv/peerbackup-data

docker run -d --name peerbackup-rest --restart unless-stopped \
  --user "$(id -u):$(id -g)" -p 51515:8000 \
  -v /srv/peerbackup-data:/data \
  -e OPTIONS="--private-repos --append-only --max-size 536870912000" \
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
[Docker](#docker-sending-backups) below for why.

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

```sh
git clone https://github.com/nisseknudsen/peerbackup
cd peerbackup
cargo build --release
sudo install -m 0755 target/release/peerbackup /usr/local/bin/
```

One binary covers both roles: `peerbackup` to send backups, `peerbackup host`
to hold a friend's.

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
peerbackup host quickstart <peer>    # start a server and add a peer
peerbackup host adduser <peer>       # add another peer later
peerbackup host list                 # allowances and usage
peerbackup host provision <peer> <size>   # per-peer size limit (needs root)
peerbackup host release <peer>       # give the space back
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

Settings are checked when the file is read, so a value that cannot mean what it
says is refused by name rather than quietly turned into something else. The
windows must be non-zero, and `verify_subset_pct` must be between 1 and 100.

### Timeouts

restic retries transport failures with exponential backoff and no overall
deadline, so every operation except the backup itself runs under one. A backup
has none on purpose: a 300GB first seed at 40Mbit legitimately takes seventeen
hours. Override any of them, in seconds, if your link needs it:

| Variable | Default | Bounds |
|---|---|---|
| `PEERBACKUP_PROBE_TIMEOUT` | 20 | Deciding whether a peer answers at all |
| `PEERBACKUP_LIST_TIMEOUT` | 120 | `snapshots`, and creating a repository |
| `PEERBACKUP_RESTORE_TIMEOUT` | 1800 | `restore`, and the test-file check |
| `PEERBACKUP_VERIFY_TIMEOUT` | 3600 | Reading data back during `verify` |

### Scheduling

There is no built-in scheduler.

```ini
# /etc/systemd/system/peerbackup.service
[Service]
Type=oneshot
# The leading `-` matters. `backup` exits non-zero if any peer failed, and
# without it systemd would stop here and never verify the peers that worked.
ExecStart=-/usr/local/bin/peerbackup backup
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

## Hosting

`quickstart` is enough to get going. The rest of this section covers what you
need for anything long-lived.

`quickstart` and `docker compose` both name the container `peerbackup-rest`, so
they are two ways to run the same server, not two servers. Moving from one to
the other means `docker rm -f peerbackup-rest` first. `quickstart` also has no
way to pass `PB_EXTRA_OPTIONS`, so TLS means using compose.

### TLS

Backups are encrypted before upload, but the login password is sent with every
request. Use TLS on anything reachable from the internet.

The container runs as `PB_UID`, not as root, so it needs a copy of the
certificate it can actually read. Let's Encrypt keeps the live directory at
`0700 root`, so this needs `sudo` to read and a `chown` to be useful afterwards:

```sh
export PB_CERTS=~/.local/share/peerbackup-certs
mkdir -p "$PB_CERTS"
sudo cp /etc/letsencrypt/live/example.org/fullchain.pem "$PB_CERTS/"
sudo cp /etc/letsencrypt/live/example.org/privkey.pem   "$PB_CERTS/"
sudo chown "$(id -u):$(id -g)" "$PB_CERTS"/*.pem
chmod 600 "$PB_CERTS/privkey.pem"
```

Do not `chmod 644` the key instead. It is readable by the container because the
container runs as you, not because the key is world-readable.

Uncomment the certs volume in `compose.yml`, then bring it up with both
variables set — `PB_CERTS` is what the volume line reads, and leaving it unset
falls back to `./certs` inside the repository:

```sh
PB_CERTS=~/.local/share/peerbackup-certs \
PB_EXTRA_OPTIONS="--tls --tls-cert /certs/fullchain.pem --tls-key /certs/privkey.pem" \
  docker compose up -d
```

Renewals replace the files under `/etc/letsencrypt`, not your copies, so repeat
the four commands above and `docker compose restart` when the certificate
rolls.

Peers then use `rest:https://...`. With a self-signed certificate they also need
a copy of it, and pass it when they connect:

```sh
peerbackup connect 'rest:https://...' --source /srv/data --cacert /path/to/ca.pem
```

`peerbackup peer add` takes the same flag.

**Already running a reverse proxy** (Traefik, Caddy, nginx...) with its own
certificate for other services on this host? Skip the above entirely — leave
`PB_EXTRA_OPTIONS` unset, point the proxy at the container's plain HTTP port
(`8000` inside the container), and let it terminate TLS the way it does
everything else. Don't publish `8000`/`51515` to the internet in this case;
only the proxy's `80`/`443` need to be reachable. The invite URL loses the
port: `rest:https://alice:PASSWORD@your-domain.example/alice/`.

Set `PB_HEALTHCHECK_SCHEME=https` alongside `PB_EXTRA_OPTIONS`. The check does
not fail without it, but it passes for the wrong reason: Go answers a plaintext
request to a TLS port with `HTTP/1.0 400 Bad Request`, which contains the status
line the check looks for. So the container reads healthy on the strength of a
handshake failure, and would go on doing so with a certificate the server could
not load.

If your proxy routes on Docker's `HEALTHCHECK`, note that `compose.yml`'s check
looks for any HTTP status line rather than a specific code. With
`--private-repos` the server answers `401` on `/` forever, and busybox `wget`
exits `1` for that exactly as it does for nothing listening, so a check for one
exact code marks a working server unhealthy and the route silently disappears.

### A size limit per peer

`quickstart` gives the whole server one limit shared by everyone on it, so one
peer can consume all of it. To give each peer their own, enforced by the
filesystem:

```sh
sudo peerbackup host provision alice 500G
sudo peerbackup host adduser alice
sudo peerbackup host list
```

This creates a fixed-size disk image per peer and mounts it separately, so
filling it produces an error on their side and cannot affect anyone else. It
needs root, because mounting does.

```
$ sudo peerbackup host list
PEER                  IMAGE       USABLE      RESERVE         USED  STATE
alice                  500GB        491GB         73GB        112GB  mounted
```

`USABLE` is lower than `IMAGE` because of filesystem overhead. `RESERVE` is how
much headroom `prune` needs to repack. Nothing enforces it: it is shown so you
can leave room by hand. If `USED` climbs past `USABLE` minus `RESERVE`, cleanup
may not be able to run.

To give the space back:

```sh
sudo peerbackup host release alice
```

This destroys their backups and cannot be undone, so it asks you to type the
peer name first.

### Running it as a service

```sh
sudo install -m 0755 target/release/peerbackup /usr/local/bin/
sudo mkdir -p /usr/local/share/peerbackup
sudo cp compose.yml /usr/local/share/peerbackup/
sudo cp deploy/systemd/peerbackup-rest.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now peerbackup-rest
```

Set `PB_UID` and `PB_GID` in the service file to your own user.

The unit's `PB_DATA=/srv/peerbackup/mnt` assumes per-peer images. If you used
`quickstart`, your data is somewhere else -- `~/.local/share/peerbackup-data` by
default -- so change that line to match, or the service serves an empty
directory on the same port.

If you are using per-peer images, the service refuses to start when storage is
not mounted. Without that check a boot where Docker wins the race would write to
your root filesystem with no size limit, and the first symptom would be a full
disk. Worth confirming once by masking a mount unit and rebooting; the service
should fail.

If you are not using them, `host guard` finds no grants, says so, and lets the
service start. It used to refuse in that case, which meant anyone who had run
`quickstart` installed a service that could never start.

### Maintenance windows

Deletion is refused by default, so a peer's cleanup of old backups fails until
you open a window:

```sh
docker compose down
# Same storage and the same size limit as the real server. `$PB_DATA` is
# whatever you set it to; with per-peer grants it is /srv/peerbackup/mnt, not
# the quickstart default below. Point this at the wrong directory and your peer
# connects to an empty server on the right port.
docker run --rm -d --name peerbackup-maint \
  --user "$(id -u):$(id -g)" -p 51515:8000 \
  -v "${PB_DATA:-$HOME/.local/share/peerbackup-data}:/data" \
  -e OPTIONS="--private-repos --max-size ${PB_MAX_SIZE:-536870912000}" \
  restic/rest-server:0.14.0
# peer runs their cleanup, then:
docker rm -f peerbackup-maint
docker compose up -d
```

Protection is off for every peer during the window, so keep it short.

### Settings

`compose.yml` reads these from the environment:

| Variable | Default | Meaning |
|---|---|---|
| `PB_DATA` | `~/.local/share/peerbackup-data` | Where backups are stored |
| `PB_PORT` | `51515` | Published port |
| `PB_UID` / `PB_GID` | `1000` | Owner of the stored files |
| `PB_MAX_SIZE` | `536870912000` | Total bytes, all peers |
| `PB_EXTRA_OPTIONS` | empty | Extra rest-server flags, e.g. TLS |
| `PB_CERTS` | `./certs` | Directory holding `fullchain.pem` and `privkey.pem` |
| `PB_HEALTHCHECK_SCHEME` | `http` | Set to `https` when `PB_EXTRA_OPTIONS` turns on TLS |
| `PB_BIND` | `0.0.0.0` | Address the published port listens on |

## Docker (sending backups)

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

## Troubleshooting

**A peer gets `401 Unauthorized` with the right password.**
The server reads logins only at startup. Restart it, or use
`peerbackup host adduser`, which handles that.

**`quickstart` says a container is running but nothing answers on the port.**
Left over from an earlier setup on a different port or storage directory.
`docker rm -f peerbackup-rest`, then run it again.

**You cannot read your own stored backups without `sudo`.**
The container is running as root. Set `PB_UID`/`PB_GID` to your user and restart.

**`provision` refuses with "sparse grant".**
The filesystem holding the images does not reserve space on allocation, so the
limit would not hold. Affects overlayfs, tmpfs and some network filesystems. Use
ext4, xfs or btrfs on local storage.

**A peer reports `507 Insufficient Storage`.**
They have filled their allowance. Either they remove old backups, or you give
them more.

**A backup refuses with "missing or unreadable".**
A directory in `sources` is gone, unreadable, or in Docker was not mounted.
peerbackup refuses rather than backing up less than you asked for, because
restic on its own would save a snapshot anyway and report success.

**restic output ends with what looks like a crash.**
restic appends its own error-location trace to ordinary failures. The real
message is the line above it.

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

The fast loop is the unit tests, which need nothing installed and run in under
a second:

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Lints are declared in `Cargo.toml` rather than passed on the command line, so a
local `cargo clippy` enforces exactly what CI does.

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
