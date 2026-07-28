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
