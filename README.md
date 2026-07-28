# peerbackup

Encrypted, incremental backups between servers owned by people who know each
other.

You reserve disk space for a friend, they reserve space for you, and both of you
get an offsite copy without paying for cloud storage. Data is encrypted before it
leaves the source machine, so the host cannot read what they are storing. After
the first backup only changed data is transferred.

Backups are ordinary [restic](https://restic.net) repositories. peerbackup adds
the parts restic does not cover: managing several peers, keeping each one within
its allowance, and regularly reading data back to confirm it is still intact.

## Status

Working, but young. The commands below all function and are covered by tests,
including a disaster-recovery test that deletes every peerbackup file and
restores the data using restic alone.

Not yet implemented: scheduling (use a systemd timer or cron), retention and
pruning, and prebuilt binaries.

## Requirements

**On the machine being backed up**

- Linux
- [restic](https://restic.net) 0.17 or newer, on `PATH`
- Rust 1.88 or newer, to build from source

**On the machine storing the backups**

- Linux with systemd
- Docker
- Free disk space for each peer you host

## Installation

No prebuilt binaries yet.

```sh
git clone https://github.com/nisseknudsen/peerbackup
cd peerbackup
cargo build --release
sudo install -m 0755 target/release/peerbackup /usr/local/bin/
```

To host backups for others, also install the host tooling:

```sh
sudo install -m 0755 deploy/peerbackup-host /usr/local/bin/
sudo mkdir -p /usr/local/share/peerbackup
sudo cp deploy/compose.yml /usr/local/share/peerbackup/
sudo cp deploy/systemd/peerbackup-rest.service /etc/systemd/system/
```

## Usage

### Sending backups

```sh
peerbackup init
```

Creates `~/.config/peerbackup/config.toml`. Add the directories you want backed
up to the `sources` list, then add a peer:

```sh
peerbackup peer add alice rest:https://me:PASSWORD@alice.example.org:8000/me/
```

This creates the repository, uploads a small test file, downloads it again and
compares it. A wrong URL, password or certificate fails here rather than partway
through a first backup.

```sh
peerbackup backup                  # send a backup to every peer
peerbackup verify                  # read data back and check it
peerbackup status                  # summary per peer
peerbackup snapshots alice         # list backups stored on a peer
peerbackup restore alice /tmp/out  # restore the most recent backup
```

`status` reads locally recorded results and does not contact peers, so it
returns immediately and works offline:

```
PEER         STATE       BACKED UP    CHECKED      TEST FILE    READ BACK
alice        ok          2h ago       1d ago       3d ago       1%
bob          unchecked   6h ago       12d ago      40d ago      -
```

`unchecked` means results are older than the configured windows, not that
anything is wrong. A peer is only reported as `FAILED` when data was read back
and did not match.

### Recovery details

```sh
peerbackup recovery export
```

Writes repository URLs, passwords and ready-to-run restic commands to a file.
Keep a copy somewhere other than the machine being backed up. Without it, the
backups cannot be decrypted by anyone, including you.

peerbackup warns when the exported file no longer matches the current
configuration.

### Hosting backups for someone else

```sh
sudo peerbackup-host provision alice 500G   # reserve space
sudo peerbackup-host adduser alice          # create their login
sudo peerbackup-host list                   # show allowances and usage
sudo peerbackup-host release alice          # give the space back
```

Full setup instructions, including TLS: [docs/runbook.md](docs/runbook.md).

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

There is no built-in scheduler. A systemd timer:

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

## How it works

Each peer holds a separate restic repository with its own encryption key, so a
leaked password for one peer does not expose the others. Backups run one peer at
a time; running them in parallel only divides the same uplink.

Storage on the host is a fixed-size disk image mounted into an unprivileged
container. The size limit is enforced by the kernel, so a peer that fills their
allowance gets a clear error rather than filling the host disk. The container
refuses to start if the storage is not mounted, because writing to an unmounted
path would silently bypass the limit.

Deletion is refused by default. Removing old backups requires the host to open a
short maintenance window.

Verification restores a small test file that is included in every backup and
compares it against a digest recorded at the time it was created, then reads back
a percentage of stored data and checks it. Results are written to an append-only
log, which is what `status` reads.

## Why not implement this as a restic backend?

restic's backends are compiled into the binary, so adding one means maintaining a
fork. Repositories written by that fork would not be readable by upstream restic,
which is a poor property for the tool you reach for after losing a machine.

An alternative is a local process that presents itself as a repository and
mirrors writes to several peers at once. This has an unresolved failure mode:
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
./deploy/test-host-tooling.sh                     # host tooling
./deploy/test-compose-e2e.sh                      # container and isolation
./deploy/test-provision-root.sh --in-container    # storage provisioning
./spike/lifecycle-spike.sh                        # slow, ~2GB
```

## License

Not yet chosen.
