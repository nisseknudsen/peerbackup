# Troubleshooting

## Sending backups

**`connect` or `peer add` says restic is too old.**
peerbackup needs restic 0.17 or newer to tell a complete backup from one that
could not read everything. Distribution packages are often older; install restic
from its [official releases](https://github.com/restic/restic/releases).

**A backup refuses with "missing or unreadable".**
A directory in `sources` is gone, unreadable by the user running peerbackup, or,
in Docker, not mounted. peerbackup refuses rather than backing up less than you
asked for: restic on its own would save a snapshot without that directory and
report success.

**A backup reports INCOMPLETE.**
restic saved a snapshot but could not read some files, usually because of
permissions. Run peerbackup as a user who can read everything in `sources`
(in Docker, set `--user`).

**`connect` says a repository already exists but the password does not open it.**
The repository at that address was created with a different encryption
password, typically by an earlier setup whose configuration is gone. Copy that peer's password from your recovery file into
`~/.config/peerbackup/secrets/<peer>` and run the command again. If the
password is lost, the data on that peer cannot be decrypted; ask the host to
release the space so you can start again.

**`status` shows `unchecked`.**
The latest results are older than the windows in your config, or there are none
yet. If it says no backup has reached a peer, run `peerbackup backup`. If checks
are overdue, run `peerbackup verify`. A peer whose last attempt failed is listed
with the reason.

**`verify` exits with 2.**
No peer could be checked at all, most often because they were unreachable. This
says nothing about the data itself. Note that `verify` exits 0 if at least one
peer was checked, so a single unreachable peer does not show up here; `status`,
which exits 1 for any peer that is not `ok`, does.

**restic output ends with what looks like a crash.**
restic appends a trace of where an error happened to ordinary failures.
peerbackup strips it from its own messages; in raw restic output, the real
message is the line above it.

## Hosting

**A peer gets `401 Unauthorized` with the right password.**
rest-server reads logins only at startup. Restart it, or add logins with
`peerbackup host adduser`, which restarts it for you.

**`host adduser` says nothing is answering, on a server with TLS.**
A known issue: its final check uses plain HTTP, which a TLS server rejects. The
login was created. See the note in [hosting.md](hosting.md#tls) for how to
confirm it.

**A friend ran `peer remove` and asks you to run `host release`, which says
there is no grant.**
`host release` is for per-peer storage areas. On a `quickstart` or compose
server, remove the peer as described in
[hosting.md](hosting.md#removing-a-peer).

**A peer reports `507 Insufficient Storage`.**
They have reached their size limit. Either they remove old backups (which needs
a [maintenance window](hosting.md#maintenance-windows)), or you give them more
space.

**A peer's cleanup fails with `403 Forbidden`.**
The server is append-only and refuses deletion, as intended. Open a
[maintenance window](hosting.md#maintenance-windows) for the cleanup.

**`quickstart` says a container is running but nothing answers on the port.**
A container from an earlier setup, on a different port or storage directory,
is still running. Run `docker rm -f peerbackup-rest` and try again.

**You cannot read your own stored backups without `sudo`.**
The server container is running as root. Set `PB_UID` and `PB_GID` to your user
and restart it.

**`provision` refuses with "sparse grant".**
The filesystem holding the images does not reserve space when a file is
allocated, so the size limit would not hold. This happens on overlayfs, tmpfs and
some network filesystems. Put `PEERBACKUP_ROOT` on ext4, xfs or btrfs on local
storage.

**`provision` refuses to overcommit.**
The grant plus `HOST_MARGIN_GB` (default 20) does not fit in the free space on
the host. Choose a smaller size, or free up space.

**The `peerbackup-rest` service will not start.**
Run `systemctl status peerbackup-rest`. If `host guard` reports a grant that is
not mounted, check its mount unit with `systemctl status` on the unit it names.
If the service fails on a path, add that path to `ReadWritePaths` in the unit.
`sudo systemd-analyze verify peerbackup-rest.service` catches mistakes in the
unit file.

**Uploads from a distant peer are slow.**
Check that the server does not offer HTTP/2; see
[hosting.md](hosting.md#turn-off-http2-for-the-peerbackup-hostname).
