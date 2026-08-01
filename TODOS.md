# TODOS

Deferred work, captured with enough context to pick up cold.

## Client-side peer removal

**What:** a `peerbackup peer remove <name>` flow for when you stop using a friend
as a backup destination.

**Why:** success criterion 8 covers the host side (you stop hosting for someone
else and release their allocation). Nothing covers your side. Today, dropping a
peer means hand-editing config and hoping nothing else references it.

**Context:** the non-obvious part is not the config edit. Removing a peer must:
1. Stop scheduling backups to it.
2. Mark its verification evidence **historical**, not delete it. Evidence is
   append-only and the history is worth keeping.
3. Prompt you to ask the friend, out of band, to release the allocation on their
   host (`umount` → `losetup -d` → `rm`).
4. **Force a recovery bundle re-export.** This is the piece that gets forgotten.
   The printed bundle lists peers and credentials; a removed peer makes it stale
   and the fingerprint check should catch that automatically.

**Depends on:** the bundle fingerprint mechanism (eng review Issue 5).

**Not blocking v1.** No success criterion needs it.

## Measure resident memory during the first 300GB seed

**What:** run the first real seed under `/usr/bin/time -v` and record peak RSS.

**Why:** restic-format repositories hold an index in memory proportional to blob
count. At 300GB this is probably low hundreds of MB, which any homeserver
handles. But that is an assumption nobody has measured, and homeservers are
often RAM-constrained.

**Context:** confidence that this is a non-issue is only moderate (6/10). It is
worth one measurement rather than a design change. Collect it during the first
seed, when it costs nothing because the seed is running anyway. If peak RSS is
uncomfortable, that is a reason to know before adding peers two and three, since
they run sequentially but each holds its own index.

**Depends on:** the first real seed, which depends on getting a friend to say yes.

## Measure what `restic stats --mode raw-data` costs on a large repository

**What:** time `restic stats --mode raw-data` against a real 300GB+ peer repository
over a home uplink, cold cache and warm.

**Why:** no longer blocking — preflight now derives stored bytes from restic's own
backup summary rather than calling `stats`, so this is off the critical path. Still
worth knowing, because `stats` remains the way to reconcile accumulated evidence
against reality after a prune, and that reconciliation needs a cost budget.

**Context:** measure both cases. Cold cache is the one that matters, because a fresh
machine restoring after a disaster has no local index cache and that is exactly when
you least want a slow command. Do it in the same session as the peak-RSS measurement
above — same repository, same seed, no extra setup.

**Pros:** confirms or kills an assumption that three tasks (T17, T18, T20) rest on.
**Cons:** needs a real large repository, so it waits on the first seed.

**Depends on:** the first real seed. Same blocker as the RSS measurement.

## One wedged peer starves every peer after it

**What:** decide how a backup that has stopped making progress is distinguished from
one that is legitimately slow, and stop the first peer in the loop from blocking the
rest.

**Why:** `backup` runs peers sequentially, and the backup subprocess deliberately has
no timeout because a 300GB first seed at 40Mbit legitimately takes seventeen hours. So
a peer whose server hangs rather than fails never returns, and every peer after it in
the loop receives nothing. With one friend that is an inconvenience you would notice.
With three it is a silent single point of failure, and the peer that gets starved is
the one you added most recently.

**Context:** the tension is the whole problem. A wall-clock budget per peer either kills
legitimate long seeds or is set so high it never fires. The better shape is probably a
stall detector: kill a backup that has transferred zero bytes for N minutes, not one
that has been running a long time. That means parsing restic's progress output rather
than just its summary line, which is a real change to the engine seam. A third option
is reordering so the peer with the oldest successful backup goes first, which does not
fix starvation but does stop the same peer losing every time.

Do not build this speculatively. Watch for it when friend 2 lands: if a backup ever
appears to hang, that is the trigger.

**Pros:** removes the only silent single point of failure in multi-peer operation.
**Cons:** getting the threshold wrong is worse than not having it, and it needs
progress parsing that does not exist yet.

**Depends on:** nothing technically. Depends on evidence that it happens.

## Reclaim a peer's disk after a group is removed

**What:** the forget/prune handshake that actually returns space on a friend's disk
once the allocator stops sending them a group.

**Why:** under `--append-only`, dropping a placement leaves the data where it is until
retention and prune run in a host-controlled maintenance window. You rebalance to make
room and the room does not appear. The allocator reports orphaned bytes honestly, but
reporting is not fixing.

**Context:** the host refuses prune outside its window by design — append-only is a
process-global flag on rest-server, not per-user, so the client cannot simply ask for a
prune. Two shapes worth considering: the host runs a scheduled reconcile against a
marker the client publishes ("I no longer need these groups"), or the two negotiate a
window explicitly. Start from the maintenance handling in the host command and the
append-only premise. Note that a marker file written by the client is itself a write to
an append-only repo, which is allowed, and that may be the cheapest channel available.

**Pros:** closes the last honest-but-wrong number in the capacity feature.
**Cons:** a protocol between two people's machines with an append-only repository in
the middle. Larger than the allocator it supports.

**Depends on:** T19 and T23 — removing a placement has to exist before reclaiming one
means anything.

## Close the prune crash-safety gap in the index-to-delete window

**What:** re-run `spike/lifecycle-spike.sh` against a repository large enough that
`prune` takes 10+ seconds, and land SIGKILLs in the late phase.

**Why:** the lifecycle spike showed prune is crash-safe during *repack*:
three landed kills, all three repos reopened, checked clean, and restored the
canary byte-identical. But every kill landed within 0.5s and prune finished by
0.8s, so none reached the window between rewriting the index and deleting the
now-obsolete packs. That window is precisely where a real corruption would live.

**Context:** raise the source tree from 2GB to roughly 20GB, or reduce pack size
so there are far more objects to delete. Then sweep kill delays across the whole
prune duration rather than just the first second. The existing script already
loops over delays; it needs a bigger repo and a wider sweep.

**Pros:** closes the last unverified assumption in the retention design.
**Cons:** each run costs disk and several minutes.

**Depends on:** nothing. Can be done any time.
