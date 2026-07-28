# peerbackup

Back up your server to your friends' servers.

If you run a homeserver, you probably have data you'd hate to lose and no copy of
it anywhere but the house it lives in. Cloud storage fixes that for a monthly
bill. Meanwhile you and your friends all have spare disk doing nothing.

peerbackup lets you trade space instead. You set aside 500GB for a friend, they
set aside 500GB for you, and you each get an offsite backup for free. Your data
is encrypted before it leaves your machine, so your friends can't read it, and
after the first upload only the changes go over the wire.

## Status

Early, and honest about it: **the receiving side works, the client doesn't exist
yet.**

Today you can set up a machine to hold a friend's backups, and they can send
backups to it with [restic](https://restic.net). That's a working offsite backup
for one friend. What's missing is the peerbackup command that manages several
friends at once, runs on a schedule, and checks your backups are still good.

If that's enough for you, the setup below works right now.

## How it works

peerbackup doesn't do the backing up. [restic](https://restic.net) does, because
it's been around for years and people restore from it every day. peerbackup
handles the part restic doesn't: keeping track of several friends, making sure
nobody runs out of space, and checking your backups regularly so you find out
about a problem before you need the data.

Practically:

- Your files are encrypted and split into chunks on your machine.
- Those chunks go to a fixed-size area your friend set aside for you. They can't
  read them, and you can't fill up their disk by accident.
- Their server refuses deletes by default, so if your machine gets compromised
  the attacker can't wipe your backup history.
- Every so often peerbackup pulls some of it back down and checks it still
  matches. That's the difference between believing you have a backup and knowing.

Because the storage is a normal restic repository, you can always get your data
back with restic alone, even if peerbackup has vanished.

## Setting up a machine to hold backups

You need Linux, Docker, and some spare disk.

```bash
# Set aside 500GB for a friend
sudo peerbackup-host provision alice 500G

# Give them a login
sudo peerbackup-host adduser alice

# Check on things later
sudo peerbackup-host list
```

Your friend then points restic at your server and backs up as normal. Full
walkthrough, including TLS and how to give the space back:
**[docs/runbook.md](docs/runbook.md)**.

## Sending backups (for now, by hand)

```bash
export RESTIC_PASSWORD_FILE=~/.config/peerbackup/alice.pass
R="rest:https://you:password@alice.example.org:8000/you/"

restic -r "$R" init
restic -r "$R" backup /srv/data
restic -r "$R" snapshots
restic -r "$R" restore latest --target /tmp/restore
```

Keep that password somewhere other than the machine you're backing up. If the
machine dies and the password dies with it, your backup is unreadable.

## What it deliberately doesn't do

- **No VPN required.** A friend just needs a port open, or a domain pointing at
  their box.
- **No strangers.** This is for people you know. There's no defence against a
  friend who actively lies to you, only against dead disks, bitrot and accidents.
- **No clever storage tricks.** Three friends means three complete copies, not
  fragments spread across a network.

## Why not write a restic backend instead?

restic's backends are compiled in, so adding one means forking restic. Then your
data would only be readable by your fork, which is a bad property for the thing
you reach for after losing a machine.

The other option is a shim that pretends to be a server and mirrors writes to
every friend at once. Tempting, but it breaks down when one friend is full and
another isn't: you either tell restic the write succeeded when it partly didn't,
or you make it retry against friends who already have the data.

## Building

```bash
cargo build
cargo test
```

The shell tooling has its own tests, none of which need root:

```bash
./deploy/test-host-tooling.sh
./deploy/test-compose-e2e.sh                     # needs docker + restic
./deploy/test-provision-root.sh --in-container   # needs docker
```

## License

Not chosen yet.
