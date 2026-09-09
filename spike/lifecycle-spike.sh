#!/usr/bin/env bash
# End-to-end check of restic + rest-server before relying on them.
#
# Purpose: answer the ONE question that could invalidate the retention design
# before any Rust is written:
#
#     If a prune is interrupted, is the repository still openable and restorable?
#
# Secondary questions answered here too:
#   - Does --append-only actually block prune, and how does the failure present?
#   - What happens when the backing store hits ENOSPC mid-backup?
#   - Does a repo written through rest-server restore byte-for-byte?
#
# No Rust. No peerbackup code. Just stock restic against stock rest-server,
# which is exactly the pair the design (rev 5) depends on.
#
# Usage:  ./spike/lifecycle-spike.sh [workdir]
# Requires: docker, bunzip2, a restic binary on PATH or in RESTIC_BIN.

set -uo pipefail

# shellcheck source=tests/lib/scratch.sh
. "$(cd "$(dirname "$0")/.." && pwd)/tests/lib/scratch.sh"
# Takes a directory as $1 and wipes it, and CI runs this under sudo:
# `sudo ./spike/lifecycle-spike.sh /` was `rm -rf /`.
WORK="${1:-/tmp/pb-spike}"
scratch_claim "$WORK" || exit 1
RESTIC="${RESTIC_BIN:-restic}"
IMAGE="restic/rest-server:0.14.0"
PORT="${PORT:-8000}"
CONTAINER="pb-spike-rest"
export RESTIC_PASSWORD="spike-not-a-real-password"

SRC="$WORK/src"
SRV="$WORK/srv"
OUT="$WORK/out"
LOG="$WORK/spike.log"

PASS=0; FAIL=0; WARN=0
ok()   { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }
warn() { printf '  \033[33mWARN\033[0m  %s\n' "$*"; WARN=$((WARN+1)); }
hdr()  { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }
note() { printf '        %s\n' "$*"; }

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

repo_url() { echo "rest:http://127.0.0.1:$PORT/spike/"; }

# The restic/rest-server image is configured by ENV, not argv:
#   DISABLE_AUTHENTICATION=1  -> passes --no-auth
#   OPTIONS="..."             -> appended to the rest-server command line
# Passing flags as docker args makes runc try to exec them.
start_server() { # $1 = extra rest-server flags, $2 = data dir (default $SRV)
  local extra="${1:-}" datadir="${2:-$SRV}"
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  # --user matters: the image runs as uid 0 by default and creates the repo
  # 0700 root:root on the host through the bind mount. The host owner then
  # cannot du/inspect/tear down their own peer directory without sudo, which
  # breaks quota monitoring. Running as the invoking user fixes it.
  docker run -d --name "$CONTAINER" \
    -p "127.0.0.1:$PORT:8000" \
    --user "$(id -u):$(id -g)" \
    -e DISABLE_AUTHENTICATION=1 \
    -e OPTIONS="$extra" \
    -v "$datadir:/data" \
    "$IMAGE" >/dev/null
  for _ in $(seq 1 60); do
    if (exec 3<>/dev/tcp/127.0.0.1/"$PORT") 2>/dev/null; then
      exec 3>&- 2>/dev/null || true
      sleep 0.3
      return 0
    fi
    sleep 0.25
  done
  echo "server failed to start" >&2; docker logs "$CONTAINER" >&2; return 1
}

# ---------------------------------------------------------------- phase 0
hdr "Phase 0 — setup"
# REUSE_DATA=1 keeps the generated source tree between runs. The 2GB generation
# is the slowest part of the spike and is identical every time.
if [ "${REUSE_DATA:-0}" = "1" ] && [ -d "$SRC" ]; then
  rm -rf "$SRV" "$OUT"; mkdir -p "$SRV" "$OUT"
  SKIP_GEN=1
else
  rm -rf "${WORK:?}"; mkdir -p "$SRC" "$SRV" "$OUT"
  scratch_mark "$WORK"
  SKIP_GEN=0
fi
: > "$LOG"

command -v docker >/dev/null || { echo "docker required"; exit 1; }
docker info >/dev/null 2>&1 || { echo "docker daemon not reachable"; exit 1; }
"$RESTIC" version >/dev/null 2>&1 || { echo "restic required (set RESTIC_BIN)"; exit 1; }
note "restic:      $("$RESTIC" version | head -1)"
note "rest-server: $IMAGE"
note "workdir:     $WORK"

# ~2GB across 200 files. Half incompressible (real pack bytes, no dedup wins),
# half compressible. Enough that prune has measurable repacking work to do.
if [ "$SKIP_GEN" = "1" ]; then
  note "reusing existing source tree (REUSE_DATA=1)"
else
  note "generating ~2GB of test data (this takes a moment)..."
  for i in $(seq 1 100); do
    head -c 10485760 /dev/urandom > "$SRC/rand-$i.bin"
  done
  for i in $(seq 1 100); do
    yes "compressible line $i padding padding padding" | head -c 10485760 > "$SRC/text-$i.txt"
  done
  # Canary: the file we will prove survives everything.
  mkdir -p "$SRC/canary"
  head -c 1048576 /dev/urandom > "$SRC/canary/canary.bin"
fi
du -sh "$SRC" | awk '{print "        source size: " $1}'
CANARY_SHA=$(sha256sum "$SRC/canary/canary.bin" | awk '{print $1}')
note "canary sha256: $CANARY_SHA"

# ---------------------------------------------------------------- phase 1
hdr "Phase 1 — init and seed through rest-server"
start_server "" || exit 1
R=$(repo_url)

if "$RESTIC" -r "$R" init >>"$LOG" 2>&1; then ok "restic init through rest-server"
else bad "restic init failed"; tail -20 "$LOG"; exit 1; fi

if "$RESTIC" -r "$R" backup "$SRC" >>"$LOG" 2>&1; then ok "initial backup (snapshot 1)"
else bad "initial backup failed"; tail -20 "$LOG"; fi

# Churn: rewrite a chunk of the data and re-back-up several times. This creates
# partially-referenced pack files, which is what gives prune real repacking work.
for round in 2 3 4 5; do
  for i in $(seq 1 25); do
    head -c 10485760 /dev/urandom > "$SRC/rand-$i.bin"
  done
  "$RESTIC" -r "$R" backup "$SRC" >>"$LOG" 2>&1 && ok "churn backup (snapshot $round)" \
    || bad "churn backup $round failed"
done

SNAP_COUNT=$("$RESTIC" -r "$R" snapshots --json 2>/dev/null | grep -o '"short_id"' | wc -l)
note "snapshots: $SNAP_COUNT"
REPO_SIZE=$(du -sm "$SRV" | awk '{print $1}')
note "repo size on server: ${REPO_SIZE}MB"

if "$RESTIC" -r "$R" check >>"$LOG" 2>&1; then ok "restic check on a healthy repo"
else bad "check failed on a healthy repo"; fi

# ---------------------------------------------------------------- phase 2
hdr "Phase 2 — THE CRITICAL TEST: SIGKILL mid-prune"
note "Design rev 5 assumes an interrupted prune leaves the repo restorable."
note "Nothing in restic's docs promises this. Testing it directly."

KILL_SURVIVED=0; KILL_ATTEMPTS=0; KILL_TOO_FAST=0

# Short delays. Prune gets faster on each pass as snapshots are forgotten, so
# long delays land after it has already finished and prove nothing.
for delay in 0.15 0.3 0.5 0.8 1.2; do
  KILL_ATTEMPTS=$((KILL_ATTEMPTS+1))
  note "--- attempt $KILL_ATTEMPTS: killing prune after ${delay}s ---"

  "$RESTIC" -r "$R" forget --keep-last 1 --prune >>"$LOG" 2>&1 &
  PRUNE_PID=$!
  sleep "$delay"

  if kill -0 "$PRUNE_PID" 2>/dev/null; then
    kill -9 "$PRUNE_PID" 2>/dev/null
    wait "$PRUNE_PID" 2>/dev/null
    note "prune SIGKILLed while running"
  else
    wait "$PRUNE_PID" 2>/dev/null
    note "prune finished before the kill landed (repo may be too small now)"
    KILL_TOO_FAST=$((KILL_TOO_FAST+1))
    continue
  fi

  # A killed restic leaves a stale lock. That is expected and recoverable.
  if "$RESTIC" -r "$R" unlock >>"$LOG" 2>&1; then note "stale lock cleared"
  else warn "unlock failed after kill"; fi

  # The three questions that matter, in order of severity.
  if "$RESTIC" -r "$R" snapshots >>"$LOG" 2>&1; then
    note "repo still opens"
  else
    bad "attempt $KILL_ATTEMPTS: repo will not open after interrupted prune"
    continue
  fi

  if "$RESTIC" -r "$R" check >>"$LOG" 2>&1; then
    note "restic check passes"
  else
    bad "attempt $KILL_ATTEMPTS: check FAILS after interrupted prune"
    continue
  fi

  rm -rf "$OUT/r$KILL_ATTEMPTS"; mkdir -p "$OUT/r$KILL_ATTEMPTS"
  if "$RESTIC" -r "$R" restore latest --target "$OUT/r$KILL_ATTEMPTS" \
       --include "$SRC/canary/canary.bin" >>"$LOG" 2>&1; then
    GOT=$(find "$OUT/r$KILL_ATTEMPTS" -name canary.bin -exec sha256sum {} \; 2>/dev/null | awk '{print $1}')
    if [ "$GOT" = "$CANARY_SHA" ]; then
      ok "attempt $KILL_ATTEMPTS: repo opens, checks clean, canary restores byte-identical"
      KILL_SURVIVED=$((KILL_SURVIVED+1))
    else
      bad "attempt $KILL_ATTEMPTS: canary restored but digest MISMATCH"
    fi
  else
    bad "attempt $KILL_ATTEMPTS: canary restore failed after interrupted prune"
  fi

  # Re-seed hard so the next attempt has real repacking work again. Churning
  # 40 files across two snapshots leaves plenty of partially-referenced packs.
  for i in $(seq 1 40); do head -c 10485760 /dev/urandom > "$SRC/rand-$i.bin"; done
  "$RESTIC" -r "$R" backup "$SRC" >>"$LOG" 2>&1
  for i in $(seq 41 80); do head -c 10485760 /dev/urandom > "$SRC/rand-$i.bin"; done
  "$RESTIC" -r "$R" backup "$SRC" >>"$LOG" 2>&1
done

# A warning is not enough here. This is the phase the whole spike exists for --
# "is a repository still openable after an interrupted prune?" -- and if every
# prune finished before the kill landed, it was never asked. A `warn` leaves
# FAIL at 0, so the run printed "VERDICT: the rev 5 assumptions hold" in green
# and exited 0 while testing nothing. On a weekly schedule that is a job that
# goes green every Monday forever.
if [ "$KILL_SURVIVED" -eq 0 ]; then
  bad "phase 2 never landed a kill inside a running prune, so the assumption it exists to test was not tested"
  note "raise the source tree, or lower --pack-size so prune has more work to do"
fi

# ---------------------------------------------------------------- phase 3
hdr "Phase 3 — does --append-only actually block prune?"
note "Design rev 5 P13 depends on this being true and on the failure being loud."
start_server "--append-only" || exit 1
sleep 1

if "$RESTIC" -r "$R" backup "$SRC" >>"$LOG" 2>&1; then
  ok "backup still works under --append-only (as designed)"
else
  bad "backup BLOCKED under --append-only — that would break the whole model"
fi

# Capture the WHOLE output and the real exit code. Two traps here, both hit
# during development of this spike:
#   1. `| tail -5` sees only restic's Go error-location trace and misses the
#      actual 403 further up.
#   2. piping to anything makes $? the exit code of the pipe's last command.
"$RESTIC" -r "$R" forget --keep-last 1 --prune >"$WORK/append-only.out" 2>&1
APPEND_RC=$?
note "restic exit code under --append-only: $APPEND_RC"

if [ "$APPEND_RC" -eq 0 ]; then
  bad "prune SUCCEEDED under --append-only — the ransomware guarantee is void"
elif grep -qiE '403|forbidden|denied|refus|not allowed' "$WORK/append-only.out"; then
  ok "prune blocked under --append-only: non-zero exit ($APPEND_RC) and a clear 403"
  note "$(grep -iEm1 '403|forbidden|denied' "$WORK/append-only.out")"
  note "$(grep -m1 'failed to remove' "$WORK/append-only.out")"
else
  warn "prune failed (exit $APPEND_RC) but with no recognisable permission error"
fi

# Restic appends a Go error-location trace on failure. It is NOT a panic, but it
# reads like one. peerbackup must strip these lines before showing users an error.
if grep -qE '^\s+/usr/local/go/src/runtime' "$WORK/append-only.out"; then
  warn "restic appends a Go stack trace to errors — peerbackup must strip it"
  note "callers see what looks like a crash; filter lines matching ^(runtime|main)\\. and ^\\s+/"
fi

"$RESTIC" -r "$R" unlock >>"$LOG" 2>&1 || true
if "$RESTIC" -r "$R" check >>"$LOG" 2>&1; then
  ok "repo still healthy after a blocked prune"
else
  bad "repo damaged by a blocked prune"
fi

# ---------------------------------------------------------------- phase 4
hdr "Phase 4 — quota exhaustion: what does the client see?"
note "Design rev 5 P5 says the kernel enforces quota and the client reports it"
note "cleanly. Two ways to hit the wall; the second needs no root."

QUOTA_DIR="$WORK/quota"
mkdir -p "$QUOTA_DIR"

# 4a. Real kernel ENOSPC via tmpfs. Needs root, so it is best-effort.
if mount -t tmpfs -o size=100M tmpfs "$QUOTA_DIR" 2>/dev/null; then
  note "--- 4a: real kernel ENOSPC on a 100MB tmpfs ---"
  start_server "" "$QUOTA_DIR" || exit 1
  R2="rest:http://127.0.0.1:$PORT/tiny/"
  "$RESTIC" -r "$R2" init >>"$LOG" 2>&1
  "$RESTIC" -r "$R2" backup "$SRC" >"$WORK/enospc.out" 2>&1
  note "exit code: $?"
  if grep -qiE 'no space|enospc|507|insufficient' "$WORK/enospc.out"; then
    ok "kernel ENOSPC surfaces as a clear client-side error"
    note "$(grep -iEm1 'no space|enospc|507|insufficient' "$WORK/enospc.out")"
  else
    warn "kernel ENOSPC produced no recognisable error message"
  fi
  "$RESTIC" -r "$R2" unlock >>"$LOG" 2>&1 || true
  "$RESTIC" -r "$R2" check >>"$LOG" 2>&1 \
    && ok "repo still openable after a full disk" \
    || bad "repo damaged by ENOSPC — quota exhaustion would destroy a peer"
  umount "$QUOTA_DIR" 2>/dev/null || true
else
  note "no root for tmpfs — skipping 4a, running 4b instead"
fi

# 4b. rest-server's own --max-size. No root needed, and it is the belt-and-braces
# cap the design already calls for, so its behaviour needs to be known anyway.
note "--- 4b: rest-server --max-size cap (no root required) ---"
rm -rf "$QUOTA_DIR"; mkdir -p "$QUOTA_DIR"
start_server "--max-size 52428800" "$QUOTA_DIR" || exit 1
R3="rest:http://127.0.0.1:$PORT/capped/"
"$RESTIC" -r "$R3" init >>"$LOG" 2>&1
"$RESTIC" -r "$R3" backup "$SRC" >"$WORK/maxsize.out" 2>&1
MAXSIZE_RC=$?
note "restic exit code against a 50MB-capped repo: $MAXSIZE_RC"
if [ "$MAXSIZE_RC" -eq 0 ]; then
  bad "backup SUCCEEDED against a 50MB cap with 2GB of source — cap not enforced"
elif grep -qiE 'quota|too large|413|507|insufficient|no space|exceed' "$WORK/maxsize.out"; then
  ok "--max-size rejects the write with a recognisable error (exit $MAXSIZE_RC)"
  note "$(grep -iEm1 'quota|too large|413|507|insufficient|no space|exceed' "$WORK/maxsize.out")"
else
  warn "backup failed (exit $MAXSIZE_RC) but with no quota-shaped error message"
  note "$(grep -vE '^\s|^runtime\.|^main\.' "$WORK/maxsize.out" | tail -2)"
fi
"$RESTIC" -r "$R3" unlock >>"$LOG" 2>&1 || true
if "$RESTIC" -r "$R3" check >>"$LOG" 2>&1; then
  ok "repo still openable after hitting the size cap"
else
  bad "repo damaged by hitting the size cap"
fi

# Ownership check: can the host owner inspect their own peer directory?
hdr "Phase 5 — can the host owner inspect their own peer data?"
if du -sh "$QUOTA_DIR" >/dev/null 2>&1; then
  ok "host owner can read the peer data directory (docker --user worked)"
  note "size: $(du -sh "$QUOTA_DIR" 2>/dev/null | awk '{print $1}')"
else
  bad "host owner CANNOT read the peer directory — quota monitoring impossible without sudo"
  note "$(ls -ld "$QUOTA_DIR"/* 2>/dev/null | head -1)"
fi

# ---------------------------------------------------------------- summary
hdr "Summary"
printf '  passed: %d   failed: %d   warnings: %d\n' "$PASS" "$FAIL" "$WARN"
printf '  interrupted-prune survivals: %d/%d attempts (%d finished too fast to kill)\n' \
  "$KILL_SURVIVED" "$KILL_ATTEMPTS" "$KILL_TOO_FAST"
printf '  full log: %s\n' "$LOG"
if [ "$FAIL" -eq 0 ] && [ "$KILL_SURVIVED" -gt 0 ]; then
  printf '\n  \033[32mVERDICT: the rev 5 assumptions hold.\033[0m\n'
else
  printf '\n  \033[31mVERDICT: %d assumption(s) in the design are WRONG. Read the log.\033[0m\n' "$FAIL"
fi
[ "$FAIL" -eq 0 ] || exit 1
exit 0
