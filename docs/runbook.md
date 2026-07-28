# Hosting backups for a peer

Setup instructions for a machine that stores someone else's backups.

## Requirements

- Linux with systemd
- Docker
- Free disk space for each peer
- A forwarded port, or a domain pointing at the machine

## 1. Reserve the space

```sh
sudo peerbackup-host provision alice 500G
```

Creates a fixed-size disk image, formats it, and mounts it under
`/srv/peerbackup/mnt/alice`. The space is reserved on creation, so the same
capacity cannot be promised to two peers.

```sh
$ sudo peerbackup-host list
PEER                  IMAGE       USABLE      RESERVE         USED  STATE
alice                  500GB        491GB         73GB        112GB  mounted
```

`USABLE` is lower than `IMAGE` because of filesystem overhead. `RESERVE` is
headroom required for the peer's own maintenance; they are expected to stay
below it.

## 2. Install the service

```sh
sudo install -m 0755 deploy/peerbackup-host /usr/local/bin/
sudo mkdir -p /usr/local/share/peerbackup
sudo cp deploy/compose.yml /usr/local/share/peerbackup/
sudo cp deploy/systemd/peerbackup-rest.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now peerbackup-rest
```

Set `PB_UID` and `PB_GID` in the service file to your own user and group.
Otherwise the container writes files as root, and you will not be able to
inspect or remove the stored data without `sudo`.

## 3. Create a login

```sh
sudo peerbackup-host adduser alice
```

Prints a generated password, restarts the server, and confirms the login works
before reporting success. Send the password over a channel you trust.

Do not call `create_user` directly. The server reads its password file only at
startup, so a login added without a restart is rejected with `401 Unauthorized`,
which is indistinguishable from a wrong password.

## 4. Enable TLS

Backups are encrypted before upload, but the login password is sent on every
request, so TLS is required in practice.

With a domain and an existing certificate:

```sh
sudo mkdir -p /srv/peerbackup/certs
sudo cp /etc/letsencrypt/live/example.org/fullchain.pem /srv/peerbackup/certs/
sudo cp /etc/letsencrypt/live/example.org/privkey.pem   /srv/peerbackup/certs/
```

Uncomment the certificate volume in `compose.yml` and set:

```
PB_EXTRA_OPTIONS="--tls --tls-cert /certs/fullchain.pem --tls-key /certs/privkey.pem"
```

With a self-signed certificate, the peer additionally needs a copy of the
certificate file and must pass `--cacert` when adding the peer.

## 5. Expose the port

Forward TCP 8000 to this machine, or point a subdomain at it. The peer sending
backups only makes outbound connections and does not need to open anything.

## 6. Verify the setup

```sh
sudo peerbackup-host doctor   # prerequisites and file ownership
sudo peerbackup-host guard    # confirms storage is mounted
```

Then have the peer run:

```sh
peerbackup peer add <your-name> rest:https://alice:PASSWORD@example.org:8000/alice/
```

## Removing a peer

```sh
sudo peerbackup-host release alice
```

Unmounts, detaches and deletes the image, returning the capacity. Requires
typing the peer name to confirm; the stored backups are destroyed and cannot be
recovered afterwards.

## Maintenance windows

Old backups are not removed automatically, and removing them requires deletion,
which the server refuses by default. A few times a year a peer will ask for a
maintenance window:

```sh
sudo systemctl stop peerbackup-rest
sudo docker run -d --name peerbackup-maint \
  --user "$(id -u):$(id -g)" -p 8000:8000 \
  -v /srv/peerbackup/mnt:/data \
  -e OPTIONS="--private-repos" restic/rest-server:0.14.0
# peer runs their cleanup, then:
sudo docker rm -f peerbackup-maint
sudo systemctl start peerbackup-rest
```

Deletion protection is disabled for every peer during this window, so keep it
short.

## Behaviour on reboot

If Docker starts before the storage finishes mounting, the server would write to
the root filesystem with no size limit, and the first symptom would be a full
disk. The service refuses to start in that case.

Worth confirming once:

```sh
sudo systemctl mask srv-peerbackup-mnt-alice.mount
sudo reboot
systemctl status peerbackup-rest    # expected: failed
sudo systemctl unmask srv-peerbackup-mnt-alice.mount
```

## Troubleshooting

**The peer gets `401 Unauthorized` with the correct password.**
The server reads logins only at startup. Restart it, or use
`peerbackup-host adduser`, which handles the restart.

**You cannot read the stored data without `sudo`.**
The container is running as root. Set `PB_UID` and `PB_GID` in the service file
and restart.

**`provision` reports a sparse grant and refuses.**
The filesystem holding the images does not reserve space on allocation, so the
size limit would not be enforced. This affects overlayfs, tmpfs and some network
filesystems. Use ext4, xfs or btrfs on local storage.

**The peer reports `507 Insufficient Storage`.**
They have filled their allowance. Either they remove old backups, or you release
and re-provision with a larger size.

**restic output ends with what looks like a stack trace.**
restic appends its own error location trace to ordinary failures. The actual
message is the line above it.

## Provisioning storage manually

```sh
sudo fallocate -l 500G /srv/peerbackup/images/alice.img
sudo mkfs.ext4 -q -m 0 -E nodiscard -F /srv/peerbackup/images/alice.img
sudo mkdir -p /srv/peerbackup/mnt/alice
sudo mount -o loop /srv/peerbackup/images/alice.img /srv/peerbackup/mnt/alice
```

`-E nodiscard` is required. Without it `mkfs` releases the allocated blocks, and
a 500GB image reserves roughly 4MB, leaving the size limit unenforced.
