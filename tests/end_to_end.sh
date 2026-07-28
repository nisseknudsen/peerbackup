#!/usr/bin/env bash
# Full client run against a real rest-server, ending in a disaster-recovery test:
# delete every trace of peerbackup and get the data back with restic alone.
#
# Requires docker and a restic binary (RESTIC_BIN, or restic on PATH).
# Does not require root.

set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
RESTIC="${RESTIC_BIN:-restic}"
BIN="${PEERBACKUP_BIN:-$HERE/target/debug/peerbackup}"
WORK="${WORK:-/tmp/pb-e2e-client}"
PORT="${PORT:-8021}"
CONTAINER=pb-e2e-client

PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }
hdr() { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

command -v docker >/dev/null || { echo "docker required"; exit 1; }
"$RESTIC" version >/dev/null 2>&1 || { echo "restic required (set RESTIC_BIN)"; exit 1; }
[ -x "$BIN" ] || { echo "build first: cargo build"; exit 1; }

export PEERBACKUP_CONFIG_DIR="$WORK/cfg"
export PEERBACKUP_STATE_DIR="$WORK/state"
export PEERBACKUP_RESTIC="$RESTIC"
# The real defaults are minutes. A test should not sit through restic's retry
# backoff to learn something it can learn in seconds.
export PEERBACKUP_PROBE_TIMEOUT=5
export PEERBACKUP_VERIFY_TIMEOUT=120

hdr "setup"
cleanup
rm -rf "$WORK"; mkdir -p "$WORK"/{cfg,state,srv,data}
echo "the file that matters" > "$WORK/data/notes.txt"
head -c 3000000 /dev/urandom > "$WORK/data/photo.bin"
ORIGINAL_SHA=$(sha256sum "$WORK/data/photo.bin" | awk '{print $1}')

docker run -d --name "$CONTAINER" -p "127.0.0.1:$PORT:8000" \
  --user "$(id -u):$(id -g)" \
  -e OPTIONS="--private-repos --append-only" \
  -v "$WORK/srv:/data" restic/rest-server:0.14.0 >/dev/null
for _ in $(seq 1 40); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break; sleep 0.25; done
docker exec "$CONTAINER" create_user me pw >/dev/null 2>&1
docker restart "$CONTAINER" >/dev/null 2>&1; sleep 2
ok "rest-server running"

hdr "init"
"$BIN" init >/dev/null 2>&1 && ok "init" || bad "init failed"
[ -f "$WORK/cfg/config.toml" ] && ok "config written" || bad "no config"
[ -d "$WORK/state/canary" ] && ok "test files created" || bad "no test files"
# Only the owner should be able to read the config; it will hold URLs with
# passwords in them.
[ "$(stat -c %a "$WORK/cfg/config.toml")" = "600" ] && ok "config is private (0600)" \
  || bad "config is world-readable: $(stat -c %a "$WORK/cfg/config.toml")"

sed -i "s|sources = \[\]|sources = [\"$WORK/data\"]|" "$WORK/cfg/config.toml"

hdr "peer add checks the whole path before trusting it"
OUT=$("$BIN" peer add alice "rest:http://me:pw@127.0.0.1:$PORT/me/" 2>&1)
echo "$OUT" | grep -q "matches" && ok "upload and download round-trip verified" || { bad "peer add did not verify"; echo "$OUT"; }
[ "$(stat -c %a "$WORK/cfg/secrets/alice")" = "600" ] && ok "password file is private" || bad "password file is readable"

hdr "peer add rejects a peer that does not work"
OUT=$("$BIN" peer add broken "rest:http://me:wrongpw@127.0.0.1:$PORT/me/" 2>&1)
if echo "$OUT" | grep -qi "error"; then
  ok "bad credentials rejected at add time, not hours into a backup"
else
  bad "a broken peer was accepted: $OUT"
fi
"$BIN" peer list 2>/dev/null | grep -q broken && bad "broken peer was saved" || ok "broken peer not saved"

hdr "passwords are not printed"
"$BIN" peer list 2>/dev/null | grep -q "pw@" && bad "password shown in peer list" \
  || ok "peer list hides the password"

hdr "backup"
OUT=$("$BIN" backup 2>&1)
echo "$OUT" | grep -q done && ok "backup completed" || { bad "backup failed"; echo "$OUT" | sed 's/^/      /'; }

hdr "verify"
OUT=$("$BIN" verify 2>&1)
echo "$OUT" | grep -q "checking 1%" && ok "data checked" || bad "no data check"
echo "$OUT" | grep -q "matches" && ok "test file restored and matched" || bad "test file check failed"

hdr "status"
OUT=$("$BIN" status 2>&1)
echo "$OUT" | grep -qE "alice .*ok" && ok "status reports ok" || { bad "status not ok"; echo "$OUT"; }

hdr "an unreachable peer reads unchecked, never failed"
docker stop "$CONTAINER" >/dev/null 2>&1
START=$(date +%s)
"$BIN" verify >/dev/null 2>&1
ELAPSED=$(( $(date +%s) - START ))
if [ "$ELAPSED" -lt 60 ]; then
  ok "verify gives up quickly on a dead peer (${ELAPSED}s)"
else
  bad "verify took ${ELAPSED}s against a dead peer"
fi
OUT=$("$BIN" status 2>&1)
if echo "$OUT" | grep -q "FAILED"; then
  bad "a stopped peer was reported as FAILED"
else
  ok "a stopped peer is not reported as failed"
fi
docker start "$CONTAINER" >/dev/null 2>&1; sleep 2

hdr "recovery export"
"$BIN" recovery export --out "$WORK/recovery.txt" >/dev/null 2>&1
[ -f "$WORK/recovery.txt" ] && ok "recovery file written" || bad "no recovery file"
[ "$(stat -c %a "$WORK/recovery.txt")" = "600" ] && ok "recovery file is private" || bad "recovery file readable"
grep -q "restic -r" "$WORK/recovery.txt" && ok "contains ready-to-run restic commands" || bad "no restic commands"
"$BIN" recovery check 2>&1 | grep -q "matches" && ok "export matches current peers" || bad "fingerprint wrong"

hdr "recovery file goes stale when peers change"
"$BIN" peer remove alice >/dev/null 2>&1
"$BIN" recovery check 2>&1 | grep -q "out of date" && ok "staleness detected without asking the user" \
  || bad "stale recovery file not detected"
"$BIN" peer add alice "rest:http://me:pw@127.0.0.1:$PORT/me/" >/dev/null 2>&1

hdr "DISASTER: everything peerbackup ever wrote is gone"
REPO=$(grep -oP "(?<=^Repository: ).*" "$WORK/recovery.txt" | head -1)
PW=$(grep -oP "(?<=^Password:   ).*" "$WORK/recovery.txt" | head -1)
rm -rf "$WORK/cfg" "$WORK/state" "$WORK/data"
[ ! -d "$WORK/cfg" ] && ok "config, state and source data deleted"

# Only restic, only what is written on the recovery page.
export RESTIC_PASSWORD="$PW"
mkdir -p "$WORK/rescued"
if "$RESTIC" -r "$REPO" snapshots >/dev/null 2>&1; then
  ok "plain restic opens the repository using the recovery file"
else
  bad "could not open the repository with restic alone"
fi
# Exactly the command the recovery file prints.
RESTORE_CMD=$(grep -oP "(?<=^  )restic -r .*restore latest.*" "$WORK/recovery.txt" | head -1)
if [ -z "$RESTORE_CMD" ]; then bad "recovery file has no restore command"; fi
if "$RESTIC" -r "$REPO" restore latest --tag peerbackup --target "$WORK/rescued" >/dev/null 2>&1; then
  ok "plain restic restores"
else
  bad "restic could not restore"
fi

RESCUED=$(find "$WORK/rescued" -name photo.bin | head -1)
if [ -n "$RESCUED" ] && [ "$(sha256sum "$RESCUED" | awk '{print $1}')" = "$ORIGINAL_SHA" ]; then
  ok "recovered file is byte-identical to the original"
else
  bad "recovered file differs or is missing"
fi

hdr "Summary"
printf '  passed: %d   failed: %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
