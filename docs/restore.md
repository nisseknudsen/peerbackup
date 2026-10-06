# Restoring

How to get data back, in each situation, and how to remove old backups.

- [The latest backup](#the-latest-backup)
- [Single files](#single-files)
- [After losing the machine](#after-losing-the-machine)
- [With restic alone](#with-restic-alone)
- [Removing old backups](#removing-old-backups)

The examples use a peer named `bob`. `peerbackup peer list` shows yours.

## The latest backup

```sh
peerbackup snapshots bob                               # list backups, newest first
peerbackup restore bob /tmp/restored                   # the latest backup
peerbackup restore bob /tmp/restored --snapshot 4d14d8df
```

Files are restored under their original absolute paths: `/srv/data/notes.txt`
comes back as `/tmp/restored/srv/data/notes.txt`. The restore also contains
peerbackup's test files, under the path of its state directory
(`.../.local/share/peerbackup/canary`); you can delete them.

A bare `restore` picks the newest real backup. It never picks the small
connection-test snapshot that `connect` and `peer add` upload, and it refuses,
rather than restoring almost nothing, when a peer holds only that.

`restore` has no time limit, because restoring a large backup over a home
connection can take many hours.

## Single files

`peerbackup restore` restores whole snapshots. For single files or directories,
use restic directly with the same repository and password. On the machine where
peerbackup is set up:

```sh
URL='rest:http://alice:LOGIN-PASSWORD@bob.example.net:51515/alice/'   # the peer's url from config.toml
export RESTIC_PASSWORD_FILE=~/.config/peerbackup/secrets/bob

restic -r "$URL" restore latest --tag peerbackup \
  --target /tmp/restored --include /srv/data/notes.txt
```

Or browse every backup as a directory tree with
[`restic mount`](https://restic.readthedocs.io/en/stable/050_restore.html#restore-using-mount):

```sh
mkdir -p /tmp/backups
restic -r "$URL" mount /tmp/backups
```

## After losing the machine

You need the recovery file from `peerbackup recovery export`. For each peer it
lists the repository URL (including the login) and the encryption password.

On the new machine, install peerbackup and restic, then set the peer up again
with its **existing** encryption password. Write the password file before
adding the peer, or peerbackup generates a new one that cannot open the
repository:

```sh
peerbackup init
mkdir -p -m 700 ~/.config/peerbackup/secrets
install -m 600 /dev/null ~/.config/peerbackup/secrets/bob
"${EDITOR:-vi}" ~/.config/peerbackup/secrets/bob    # paste the Password line for bob
peerbackup peer add bob 'rest:http://alice@bob.example.net:51515/alice/'
```

`peer add` asks for the login password (from the `Repository` line, between
`alice:` and `@`), recognises that the repository already exists, and checks
that the password opens it. Then:

```sh
peerbackup restore bob /tmp/restored
```

Use `peer add` here, not `connect`: `connect` requires the source directories to
exist, and on a new machine they do not yet. Once the data is back, add the
directories to `sources` in `~/.config/peerbackup/config.toml`, or run
`connect` with `--source`, and carry on backing up.

## With restic alone

The recovery file is written to work without peerbackup. For each peer it
includes commands like these, which prompt for that peer's encryption password:

```sh
restic -r 'rest:http://alice:LOGIN-PASSWORD@bob.example.net:51515/alice/' snapshots
restic -r 'rest:http://alice:LOGIN-PASSWORD@bob.example.net:51515/alice/' \
  restore latest --tag peerbackup --target /where/to/put/it
```

Keep `--tag peerbackup`: without it, `latest` can be the small connection-test
snapshot instead of your data. If the host uses a self-signed certificate, the
file says so and includes `--cacert`.

## Removing old backups

peerbackup does not remove old backups yet, so a repository grows until it
reaches the host's limit, at which point backups fail with
`507 Insufficient Storage`.

Removing them needs both of you. The host's server refuses deletion by default,
so a compromised client cannot erase its own history:

1. **The host** opens a maintenance window, which allows deletion for a while.
   See [hosting.md](hosting.md#maintenance-windows).
2. **You** remove old snapshots with restic, keeping a schedule of your choice:

   ```sh
   URL='rest:http://alice:LOGIN-PASSWORD@bob.example.net:51515/alice/'
   export RESTIC_PASSWORD_FILE=~/.config/peerbackup/secrets/bob

   restic -r "$URL" forget --tag peerbackup \
     --keep-daily 7 --keep-weekly 4 --keep-monthly 12 --prune
   ```

   `--tag peerbackup` limits this to your backups. The `--keep-*` options always
   keep the newest snapshot, which `verify` expects to find.
3. **The host** closes the window.

Then run `peerbackup verify` to confirm everything still reads back.
`--prune` rewrites part of the repository and needs free space to do it; if the
peer is completely full, ask the host for a little more room first.
