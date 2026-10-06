# Configuration

Settings for the sending side: the config file, timeouts, throughput, and the
environment variables peerbackup reads. Hosting settings are in
[hosting.md](hosting.md#settings).

## Files

| Path | Contents | Override |
|---|---|---|
| `~/.config/peerbackup/config.toml` | Settings and peers | `PEERBACKUP_CONFIG_DIR` |
| `~/.config/peerbackup/secrets/<peer>` | Each peer's encryption password | follows the config directory |
| `~/.local/share/peerbackup/` | Recorded check results, test files, the default recovery file | `PEERBACKUP_STATE_DIR` |

The config and every secret are written readable only by you (mode 0600).

restic keeps its cache in its usual place, `~/.cache/restic`. (The container
image moves it to `/state/cache`.)

## The config file

`connect` and `peer add` create and update it. A complete file, with every
setting at its default:

```toml
[settings]
sources = []
upload_limit_kib = 0
verify_subset_pct = 1
liveness_hours = 48
subset_days = 10
canary_days = 35

[[peer]]
name = "bob"
url = "rest:http://alice:LOGIN-PASSWORD@bob.example.net:51515/alice/"
# ca_cert = "/path/to/ca.pem"   # only for a self-signed certificate
```

Every setting has a default, so a file only needs the ones you change.

| Setting | Default | Meaning |
|---|---|---|
| `sources` | `[]` | Directories to back up. `connect --source` adds to this list. |
| `upload_limit_kib` | `0` | Upload limit in KiB/s. `0` means no limit. A first backup can saturate a home uplink for many hours, so setting one is worth considering. |
| `verify_subset_pct` | `1` | Percentage of each peer's stored data that `verify` reads back and checks. 1 to 100. |
| `liveness_hours` | `48` | A peer whose last successful backup is older than this is shown as `unchecked`. |
| `subset_days` | `10` | Same, for the last successful data read-back. |
| `canary_days` | `35` | Same, for the last successful test-file restore. |

Settings are validated when the file is read. A value that cannot work (a zero
window, a percentage outside 1–100), or a key under `[settings]` that is not in
this table, is refused with an error naming it, rather than being ignored or
adjusted.

Keys under `[[peer]]` are not checked this way, so a misspelling there is
silently ignored. The certificate key is `ca_cert`, although the command-line
flag is `--cacert`.

### Two passwords per peer

- The **login password** is part of the peer's `url`. It only grants access to
  your friend's server.
- The **encryption password** decrypts the backups. peerbackup generates it per
  peer and keeps it in `secrets/`, never in the config. Lose it and the backups
  on that peer cannot be decrypted by anyone.

`peerbackup recovery export` writes both, for every peer, together with
ready-to-run restic commands. Keep that file somewhere other than the machine
being backed up. Commands that change your peers warn when it is out of date,
and `peerbackup recovery check` tells you on demand.

### Self-signed certificates

If the host uses a certificate your system does not trust, get a copy of it from
them and pass it when connecting:

```sh
peerbackup connect 'rest:https://alice@bob.example.net/alice/' --source /srv/data --cacert /path/to/ca.pem
```

`peer add` takes the same flag. It is stored as `ca_cert` for that peer.

### Non-interactive setup

When there is no terminal, `connect` and `peer add` read the password from
standard input instead of prompting:

```sh
peerbackup connect 'rest:http://alice@bob.example.net:51515/alice/' --source /srv/data < password.txt
```

Avoid putting the password in the URL on the command line. It ends up in your
shell history and is visible in the process list while the command runs;
peerbackup warns if you do.

## Timeouts

restic retries network failures with backoff and no overall deadline, so most
operations run under one. `backup` and `restore` have none, because a large
first backup or a full restore over a slow link legitimately takes many hours.

Override the others with these variables, in seconds:

| Variable | Default | Limits |
|---|---|---|
| `PEERBACKUP_PROBE_TIMEOUT` | 20 | Checking whether a peer answers at all |
| `PEERBACKUP_LIST_TIMEOUT` | 120 | Listing snapshots (in `snapshots`, `verify` and `restore`), and creating a repository |
| `PEERBACKUP_CANARY_TIMEOUT` | 1800 | Downloading the test file (in `verify`, `connect` and `peer add`) |
| `PEERBACKUP_VERIFY_TIMEOUT` | 3600 | Reading data back during `verify` |

## Throughput to a distant peer

Uploads to a distant peer are much slower over HTTP/2 than over HTTP/1.1,
because HTTP/2 puts all of restic's parallel uploads on one TCP connection.
restic cannot opt out, so this is decided by the **host**: the server set up by
`host quickstart` or `compose.yml` does not offer HTTP/2, and a host running a
reverse proxy has to turn it off there. See
[hosting.md](hosting.md#turn-off-http2-for-the-peerbackup-hostname).

Once a peer uses HTTP/1.1, more connections can help on a long link, at the cost
of memory on both sides:

```sh
PEERBACKUP_RESTIC_OPTS="-o rest.connections=10" peerbackup backup
```

## Environment variables

| Variable | Meaning |
|---|---|
| `PEERBACKUP_CONFIG_DIR` | Config directory. Default `~/.config/peerbackup`. |
| `PEERBACKUP_STATE_DIR` | State directory. Default `~/.local/share/peerbackup`. |
| `PEERBACKUP_RESTIC_OPTS` | Extra restic options, such as `-o rest.connections=10`. Only `-o key=value` pairs are accepted; other restic flags are refused. |
| `PEERBACKUP_RESTIC` | Path to the restic binary, instead of `restic` on `PATH`. peerbackup prints a warning when it is set, because results then come from that program. |
| `PEERBACKUP_*_TIMEOUT` | See [Timeouts](#timeouts). |
