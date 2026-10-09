# peerbackup

[![CI](https://github.com/nisseknudsen/peerbackup/actions/workflows/ci.yml/badge.svg)](https://github.com/nisseknudsen/peerbackup/actions/workflows/ci.yml)

Encrypted, incremental backups between servers owned by people who know each
other.

You set aside disk space for a friend and they do the same for you, so both of
you get an offsite copy without paying for cloud storage. Backups are encrypted
before they leave your machine, so the host cannot read them. After the first
backup, only changed data is sent.

Backups are plain [restic](https://restic.net) repositories. peerbackup handles
what restic leaves to you: backing up to several peers, keeping each host's
storage limited, and regularly reading data back to confirm it is still intact.
If peerbackup is gone, restic alone can restore everything.

## Contents

- [Status](#status)
- [Requirements](#requirements)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Restoring](#restoring)
- [Commands](#commands)
- [Configuration](#configuration)
- [Scheduling and alerts](#scheduling-and-alerts)
- [Documentation](#documentation)
- [Development](#development)
- [License](#license)

## Status

Pre-1.0 and Linux only. The features below work and are covered by tests,
including a disaster-recovery test that deletes every peerbackup file and
restores the data with restic alone. Until 1.0.0, a minor release may include
breaking changes; see the [changelog](CHANGELOG.md).

Not implemented yet: a built-in scheduler (use a systemd timer or cron, as
[below](#scheduling-and-alerts)), and automatic removal of old backups (it can
be done by hand; see [docs/restore.md](docs/restore.md#removing-old-backups)).

## Requirements

| Role | Needs |
|---|---|
| Sending backups | Linux (amd64 or arm64) and [restic](https://restic.readthedocs.io/en/stable/020_installation.html) 0.17 or newer, or Docker |
| Hosting for a friend | Linux, Docker and `curl` |
| Per-peer storage limits (optional) | Root, systemd, util-linux, e2fsprogs, and ext4, xfs or btrfs on local storage |
| Building from source | Rust 1.88+ |

Distribution packages of restic are often older than 0.17 (Debian 12 ships
0.14). If yours is, install restic from its
[official releases](https://github.com/restic/restic/releases) instead.

## Installation

One binary covers both roles: sending backups, and hosting them (`peerbackup host`).

### Prebuilt binary

Static binaries for `amd64` and `arm64` are attached to each
[release](https://github.com/nisseknudsen/peerbackup/releases).

```sh
VERSION=0.1.0
ARCH=amd64            # or arm64
BASE=https://github.com/nisseknudsen/peerbackup/releases/download/v$VERSION

curl -fsSLO "$BASE/peerbackup-$VERSION-linux-$ARCH.tar.gz"
curl -fsSLO "$BASE/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS

tar -xzf "peerbackup-$VERSION-linux-$ARCH.tar.gz"
sudo install -m 0755 "peerbackup-$VERSION-linux-$ARCH/peerbackup" /usr/local/bin/
```

The tarball also contains the documentation, `compose.yml` and the systemd unit
used for [hosting](docs/hosting.md).

Each release carries a build provenance attestation. With the
[GitHub CLI](https://cli.github.com/), you can check that a download was built
by this repository's release workflow:

```sh
gh attestation verify "peerbackup-$VERSION-linux-$ARCH.tar.gz" --repo nisseknudsen/peerbackup
```

### Container image

```sh
docker pull ghcr.io/nisseknudsen/peerbackup:latest
```

Built for `linux/amd64` and `linux/arm64`, with restic included. Tags are the
exact version (`0.1.0`), the minor line (`0.1`), and `latest`. See
[docs/docker.md](docs/docker.md) for how to run it.

### From source

```sh
git clone https://github.com/nisseknudsen/peerbackup
cd peerbackup
cargo build --release --locked
sudo install -m 0755 target/release/peerbackup /usr/local/bin/
```

### Upgrading

Replace the binary, or pull the new image. Configuration and recorded results
carry over. Read the [changelog](CHANGELOG.md) first: until 1.0.0, a new minor
version may change behaviour.

## Quick start

Two friends, Bob and Alice. Bob has spare disk space and hosts; Alice backs up
to him. Each can also do the reverse for the other.

### 1. Bob: give Alice some space

```sh
peerbackup host quickstart alice
```

This starts [rest-server](https://github.com/restic/rest-server) in Docker,
creates a login for `alice`, checks that it works, and prints an invite:

```
Ready. Send both of these to alice, over something you trust:

  URL:      rest:http://alice@bob.example.net:51515/alice/
  Password: <generated>

They run:
  peerbackup connect 'rest:http://alice@bob.example.net:51515/alice/' --source /path/to/back/up

and paste the password when it asks.

Storage:   /home/bob/.local/share/peerbackup-data
Port 51515 must reach this machine. Use TLS if it is exposed to the
internet: see the README.
```

- The host part of the URL is this machine's hostname (`hostname -f`). If Alice
  cannot reach it by that name, give her an address she can reach instead, and
  make sure port 51515 is open or forwarded to this machine.
- `quickstart` needs no root, but your user must be able to use Docker, which is
  root-equivalent access to the machine.
- Storage goes to `~/.local/share/peerbackup-data` (change it with `PB_DATA=`),
  and the port is 51515 (change it with `PB_PORT=`). The server restarts on its
  own after a reboot.

> **Use TLS before exposing the port to the internet.** The invite URL is plain
> HTTP, which sends the login password with every request. Backups stay
> encrypted either way, but anyone on the network path could read the login and
> then read or add to Alice's repository. On a LAN or a VPN, plain HTTP is fine.
> Otherwise, put the server behind a reverse proxy or give it a certificate: see
> [docs/hosting.md](docs/hosting.md#tls).

### 2. Alice: connect and back up

```sh
peerbackup connect 'rest:http://alice@bob.example.net:51515/alice/' --source /srv/data
```

`connect` asks for the password, creates the repository, uploads a small test
file, downloads it again and compares it. A wrong address or password fails
within seconds rather than hours into the first backup. Repeat `--source` to
back up several directories.

On Alice's side the peer is named after the host, so this one is `bob`. Pass
`--name` to choose another name.

```sh
peerbackup backup     # send a backup to every peer
peerbackup verify     # read data back and check it
peerbackup status     # summary per peer
```

### 3. Alice: save the recovery file

```sh
peerbackup recovery export
```

This writes every repository address and password, with ready-to-run restic
commands, to `~/.local/share/peerbackup/recovery.txt` (or `--out <file>`).
**Without it, nobody can decrypt your backups, including you.** Print it or copy
it somewhere other than the machine you are backing up, then delete the local
copy. peerbackup warns when the file is out of date with your configuration.

## Restoring

```sh
peerbackup snapshots bob                 # list the backups on a peer
peerbackup restore bob /tmp/restored     # restore the latest one
peerbackup restore bob /tmp/restored --snapshot 4d14d8df
```

Files are restored under their original paths, so `/srv/data` comes back as
`/tmp/restored/srv/data`. The restore also contains peerbackup's small test
files, under the path of its state directory.

Restoring single files, restoring after losing the machine, and restoring with
restic alone are covered in [docs/restore.md](docs/restore.md).

## Commands

Sending backups:

| Command | Does |
|---|---|
| `connect <url> --source <dir>` | Set up a peer in one step (asks for the password) |
| `backup [--peer <name>]` | Send a backup |
| `verify [--peer <name>]` | Read stored data back and check it |
| `status` | Summary per peer, from local records only |
| `snapshots <peer>` | List the backups stored on a peer |
| `restore <peer> <target> [--snapshot <id>]` | Restore the latest backup, or a chosen one |
| `recovery export [--out <file>]` | Write the file needed to restore without peerbackup |
| `recovery check` | Check that file still matches your peers |
| `peer add\|list\|remove` | Manage peers individually |
| `init` | Create the config and test files, nothing else |

Hosting:

| Command | Does |
|---|---|
| `host quickstart <peer>` | Start a server and add a peer, without root |
| `host adduser <peer> [<password>]` | Add a login (generated unless given), restart the server, check the login works |
| `host provision <peer> <size>` | Create a storage area with its own size limit (root) |
| `host release <peer>` | Delete a peer's storage area and their backups (root) |
| `host list [<peer>]` | Storage areas, sizes and usage |
| `host guard` | Exit non-zero unless every storage area is mounted |
| `host doctor` | Check this machine is set up to host |
| `host up\|down` | `docker compose up -d` / `down`, using `compose.yml` |

Every `host` command accepts `--dry-run`, which prints what it would do and
changes nothing. Run `peerbackup <command> --help` for the details of each.

### Status

`status` reads results recorded locally and does not contact peers, so it is
instant and works offline:

```
$ peerbackup status
PEER         STATE       BACKED UP    CHECKED      TEST FILE    READ BACK
bob          ok          2h ago       20h ago      3d ago       1%
carol        unchecked   5h ago       12d ago      12d ago      -

Not checked recently: carol. Run `peerbackup verify`.
error: 1 peer(s) have not been checked within their windows (carol)
```

- `ok`: a backup, a data read-back and a test-file restore have all succeeded
  within their windows (see [configuration](docs/configuration.md#the-config-file)).
- `unchecked`: at least one of those has not succeeded recently. That can mean
  checks are simply overdue, that no backup has reached the peer yet, or that
  recent attempts failed; `status` prints which, and what to run.
- `FAILED`: data was read back and was wrong, or the peer no longer has a backup
  it previously accepted.

`status` exits 0 only when every peer is `ok`, which makes it the command to
alert on.

## Configuration

`connect` writes `~/.config/peerbackup/config.toml`. Every setting has a
default, so the file only needs what you change:

```toml
[settings]
sources = ["/srv/data", "/home/alice/documents"]
upload_limit_kib = 5000     # upload limit in KiB/s; the default 0 means none

[[peer]]
name = "bob"
url = "rest:http://alice:LOGIN-PASSWORD@bob.example.net:51515/alice/"
```

Each peer has two passwords, and they do different jobs:

- The **login password**, in the URL, only gets you into your friend's server.
  `connect` writes it into `config.toml`, which is readable only by you.
- The **encryption password** decrypts your backups. peerbackup generates it,
  keeps it in `~/.config/peerbackup/secrets/` (also readable only by you), and
  never puts it in the config. This is the one the recovery file exists to
  preserve.

All settings, timeouts and tuning for distant peers are in
[docs/configuration.md](docs/configuration.md).

## Scheduling and alerts

peerbackup has no scheduler of its own. A systemd timer works well. Run it as
the user who ran `connect`, since that is whose configuration it reads:

```ini
# /etc/systemd/system/peerbackup.service
[Unit]
Description=peerbackup

[Service]
Type=oneshot
User=alice
# "-" lets the next step run when this one fails. `status` runs last and
# decides whether the unit succeeds.
ExecStart=-/usr/local/bin/peerbackup backup
ExecStart=-/usr/local/bin/peerbackup verify
ExecStart=/usr/local/bin/peerbackup status
```

```ini
# /etc/systemd/system/peerbackup.timer
[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now peerbackup.timer
```

The unit fails when any peer is not `ok`, so alert on that, for example with
`OnFailure=` or your monitoring of failed units. A single failed backup does not
fail it straight away; a peer that keeps failing does, once its results are
older than `liveness_hours` (48 by default).

Exit codes, for scripts:

| Command | 0 | 1 | 2 |
|---|---|---|---|
| `status` | every peer is `ok` | any peer is not `ok`, or an error | |
| `verify` | no damage found, and at least one peer was checked | damage found, or an error such as a bad config | no peer could be checked at all |

`verify` exits 0 when one peer is unreachable but another was checked, so use
`status`, not `verify`, to notice a single peer going quiet.

## Documentation

| Document | Covers |
|---|---|
| [docs/restore.md](docs/restore.md) | Restoring single files, after losing the machine, with restic alone; removing old backups |
| [docs/configuration.md](docs/configuration.md) | Every setting, timeouts, throughput to distant peers, environment variables |
| [docs/hosting.md](docs/hosting.md) | Running a server long-term: reverse proxies, TLS, per-peer storage limits, systemd, removing peers |
| [docs/docker.md](docs/docker.md) | Running the client and the host in Docker |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Common errors and what to do about them |
| [docs/design.md](docs/design.md) | How verification works, the security model, and design decisions |
| [CHANGELOG.md](CHANGELOG.md) | Changes in each release |
| [SECURITY.md](SECURITY.md) | Reporting a vulnerability |

## Development

The unit tests need nothing installed and run in under a second:

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Lints are configured in `Cargo.toml`, so a local `cargo clippy` checks exactly
what CI checks.

The integration tests run a real rest-server and a real restic. They need Docker
and restic, but not root:

```sh
./tests/end_to_end.sh          # client lifecycle, ending in a restore with restic alone
./tests/docker.sh              # the container image, both roles
./deploy/test-host-tooling.sh  # host commands
./deploy/test-compose-e2e.sh   # compose.yml, isolation between peers
./spike/lifecycle-spike.sh     # slow; about 2 GB of data
```

Storage provisioning needs real loop devices, so its test needs root, either
directly or through a privileged container:

```sh
sudo ./deploy/test-provision-root.sh
./deploy/test-provision-root.sh --in-container
```

Each release is cut from its own `release/vX.Y.Z` branch; see [RELEASING.md](RELEASING.md).

## License

MIT. See [LICENSE](LICENSE).
