# Design

How peerbackup works, what it protects against, and why it is built this way.

## Overview

- Each peer holds a separate [restic](https://restic.net) repository with its own
  encryption key. A leaked password for one peer exposes nothing on the others.
- Backups are sent to one peer at a time. Sending to several in parallel only
  divides the same uplink.
- Backups are ordinary restic repositories. The recovery file contains
  everything needed to restore them with restic alone, so losing peerbackup, or
  the machine it ran on, does not lose access to the backups.
- On the host, each peer's repository sits behind a size limit, and deletion is
  refused by default.

## Verification

A backup that uploaded without errors is not the same as a backup that can be
restored. `verify` checks three things per peer:

1. **Stored data reads back correctly.** It runs `restic check` with
   `--read-data-subset`, reading back `verify_subset_pct` percent of the stored
   data and checking it against its hashes.
2. **A known file restores byte for byte.** Every backup includes a small set of
   test files whose digests were recorded when they were created. `verify`
   restores one from the latest backup and compares.
3. **The host still has your latest backup.** It checks that the peer still
   lists the snapshot your most recent backup produced.

The third check is about the host rather than the data. Append-only storage stops
a *client* from erasing its history, but nothing else here would notice a *host*
that rolled the repository back to an older copy: an old repository is
internally consistent, so `restic check` passes, and the test file is unchanged,
so it still restores. The local record of what was sent is the one thing the host
cannot rewrite.

Results are appended to a local log, which is what `status` reads. `status`
never contacts a peer.

### Three states, not two

Every check ends in one of three states:

| State | Meaning | Shown as |
|---|---|---|
| Good | Data was read back and was correct | `ok` |
| Bad | Data was read back and was wrong | `FAILED` |
| Unknown | Nothing could be read back | `unchecked` |

An unreachable peer, a full disk or a wrong login produces Unknown, never Bad.
Only data that was actually read and found wrong is reported as a failure. This
keeps `FAILED` meaningful: treating a network outage as corruption would teach
people to ignore it.

Unknown is not treated as fine either. A peer whose checks are older than the
windows in the config is shown as `unchecked` and makes `status` exit non-zero,
and `verify` exits 2 when it cannot check a peer at all. A peer that has been
unreachable for weeks is not silent.

## Security model

**The host cannot read your data.** Backups are encrypted by restic before they
leave your machine, with a key the host never sees.

**The host can see** how much you store and when you back up: the number and
size of the encrypted files restic writes, and when they change. Not your file
names, and not their contents.

**A compromised client cannot erase its backups.** The server is append-only, so
an attacker holding your login can add new backups but cannot delete or modify
existing ones. Deleting old backups needs the host to open a
[maintenance window](hosting.md#maintenance-windows).

**A host that loses or rolls back your data is noticed** by the checks above.

**What you must protect:**

- the recovery file, which contains every password needed to decrypt your
  backups;
- `~/.config/peerbackup/secrets/`, for the same reason;
- the login password, which lets anyone holding it add to or read your
  repository on that host, though not decrypt it. Use TLS for any server
  reachable from the internet, because HTTP basic authentication sends the login
  with every request.

## Why not a restic backend?

restic's storage backends are compiled into the binary, so adding one means
maintaining a fork. Repositories written by the fork would need the fork to
read, which is a poor property for the tool you reach for after losing a
machine.

Another option is a local process that presents itself as one repository and
mirrors writes to several peers. That has an unresolved failure mode: when one
peer accepts a write and another rejects it for lack of space, there is no
correct answer to give restic. Reporting success leaves one peer incomplete;
reporting failure makes restic retry against peers that already have the data.

Separate repositories, written one after another, avoid both problems.
