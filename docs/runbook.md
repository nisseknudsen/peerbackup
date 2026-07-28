# Holding backups for a friend

How to set up a machine so a friend can back up to it. Takes about ten minutes.

You need Linux with systemd, Docker, and enough free disk for whatever you're
giving away.

## 1. Set aside the space

```bash
sudo peerbackup-host provision alice 500G
```

This creates a 500GB disk image, formats it, and mounts it. The space is really
reserved, so you can't accidentally promise the same gigabytes to three people.
It's also not permanent: `release` later gives it all back.

Check what you've handed out:

```bash
$ sudo peerbackup-host list
PEER                  IMAGE       USABLE      RESERVE         USED  STATE
alice                  500GB        491GB         73GB        112GB  mounted
```

`USABLE` is less than `IMAGE` because filesystems have overhead. `RESERVE` is
headroom your friend's cleanup needs to work; they're expected to stay under it.

## 2. Start the server

Copy the compose file and the service unit into place:

```bash
sudo cp deploy/compose.yml /usr/local/share/peerbackup/
sudo cp deploy/peerbackup-host /usr/local/bin/
sudo cp deploy/systemd/peerbackup-rest.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now peerbackup-rest
```

Edit `PB_UID`/`PB_GID` in the service file to match your user, or the container
will write files you can't read.

## 3. Give your friend a login

```bash
sudo peerbackup-host adduser alice
```

It prints a password, restarts the server, and confirms the login actually works
before telling you it's done. Send the password over something you trust.

Use this rather than `create_user` directly. The server only reads its password
file at startup, so a login added any other way returns "unauthorized" until you
restart, which looks exactly like a wrong password.

## 4. Turn on TLS

Your friend's backups are already encrypted, but their password crosses the wire
on every connection, so don't skip this.

With a domain and Let's Encrypt:

```bash
sudo mkdir -p /srv/peerbackup/certs
sudo cp /etc/letsencrypt/live/example.org/{fullchain,privkey}.pem /srv/peerbackup/certs/
```

Then uncomment the certs volume in `compose.yml` and set:

```
PB_EXTRA_OPTIONS="--tls --tls-cert /certs/fullchain.pem --tls-key /certs/privkey.pem"
```

Without a domain you can use a self-signed certificate, but your friend will
need the `.pem` file and `restic --cacert` to use it.

## 5. Open a port

Forward port 8000 to this machine, or point a subdomain at it. Your friend's
machine only makes outbound connections, so nothing needs opening on their side.

## Check it works

```bash
sudo peerbackup-host doctor   # prerequisites and permissions
sudo peerbackup-host guard    # confirms the storage is really mounted
```

Then have your friend try it, or test locally:

```bash
export RESTIC_PASSWORD=test
restic -r "rest:http://alice:PASSWORD@localhost:8000/alice/" init
```

## Giving the space back

```bash
sudo peerbackup-host release alice
```

Unmounts, detaches, deletes, and returns the capacity. It asks you to type the
name first, because this destroys their backups and you can't undo it.

## Notes

**Reboots.** If Docker starts before the storage finishes mounting, the server
would write to your root filesystem with no size limit, and you'd find out when
the disk filled. The service refuses to start in that case instead. Worth
testing once: mask the mount unit, reboot, and confirm the service fails.

**Pruning.** Old backups don't disappear on their own, and cleaning them up
means deleting, which the server refuses by default. So a few times a year your
friend will ask you to open a maintenance window:

```bash
sudo systemctl stop peerbackup-rest
sudo docker run -d --name peerbackup-maint \
  --user "$(id -u):$(id -g)" -p 8000:8000 \
  -v /srv/peerbackup/mnt:/data \
  -e OPTIONS="--private-repos" restic/rest-server:0.14.0
# they run their cleanup, then:
sudo docker rm -f peerbackup-maint
sudo systemctl start peerbackup-rest
```

Everyone you host for is unprotected during that window, so keep it short.

## Troubleshooting

**Your friend gets "unauthorized" with the right password.** The server reads
logins at startup only. `sudo systemctl restart peerbackup-rest`, or use
`peerbackup-host adduser`, which handles it.

**You can't read your own backup directory.** The container is running as root.
Set `PB_UID`/`PB_GID` in the service file to your user and restart.

**`provision` refuses with "sparse grant".** The disk you're storing images on
can't really reserve space, so the limit wouldn't hold. This happens on
overlayfs, tmpfs and some network mounts. Put the images on a normal ext4, xfs
or btrfs filesystem.

**Their backups fail with "insufficient storage".** They've filled their
allowance. Either they clean up, or you give them more with `release` then
`provision`.

**restic prints something that looks like a crash.** It appends a Go stack trace
to ordinary errors. The real message is the line *above* the trace, so don't
pipe it through `tail`.

## Building the images by hand

If you'd rather not use `peerbackup-host`:

```bash
sudo fallocate -l 500G /srv/peerbackup/images/alice.img
sudo mkfs.ext4 -q -m 0 -E nodiscard -F /srv/peerbackup/images/alice.img
sudo mkdir -p /srv/peerbackup/mnt/alice
sudo mount -o loop /srv/peerbackup/images/alice.img /srv/peerbackup/mnt/alice
```

`-E nodiscard` matters. Without it `mkfs` hands the space straight back and your
500GB image only reserves about 4MB, so the limit is fiction.
