#!/usr/bin/env bash
# End-to-end test of the peer host stack: compose file, credentials, isolation,
# append-only, and host-side file ownership.
#
# Unlike test-host-tooling.sh (pure logic, no docker), this brings up a real
# rest-server and drives it with a real restic. It needs docker and a restic
# binary but NOT root, so it runs in CI unchanged.
#
# Run: ./deploy/test-compose-e2e.sh
#      RESTIC_BIN=/path/to/restic ./deploy/test-compose-e2e.sh

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
RESTIC="${RESTIC_BIN:-restic}"
export PB_ROOT="${PB_ROOT:-/tmp/pb-e2e}"
export PB_DATA="$PB_ROOT/mnt"
export PB_PORT="${PB_PORT:-8011}"
PB_UID="$(id -u)"; PB_GID="$(id -g)"; export PB_UID PB_GID
BASE="http://127.0.0.1:$PB_PORT"

PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }
hdr() { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }

cleanup() { docker compose -f "$HERE/../compose.yml" down -v >/dev/null 2>&1 || true; }
trap cleanup EXIT

command -v docker >/dev/null || { echo "docker required"; exit 1; }
"$RESTIC" version >/dev/null 2>&1 || { echo "restic required (set RESTIC_BIN)"; exit 1; }

hdr "bring up the stack"
cleanup
rm -rf "$PB_ROOT"; mkdir -p "$PB_ROOT/mnt/alice" "$PB_ROOT/mnt/bob" "$PB_ROOT/src"
echo "peerbackup e2e payload" > "$PB_ROOT/src/f.txt"

docker compose -f "$HERE/../compose.yml" config >/dev/null 2>&1 \
  && ok "compose.yml parses and interpolates" || bad "compose.yml invalid"
docker compose -f "$HERE/../compose.yml" up -d >/dev/null 2>&1
for _ in $(seq 1 40); do
  curl -s -o /dev/null "$BASE/" 2>/dev/null && break
  sleep 0.25
done
docker ps --filter name=peerbackup-rest --format '{{.Status}}' | grep -q Up \
  && ok "rest-server is up" || { bad "rest-server did not start"; exit 1; }

CUID=$(docker exec peerbackup-rest id -u 2>/dev/null)
[ "$CUID" = "$(id -u)" ] && ok "container runs as the host user (uid $CUID), not root" \
  || bad "container uid is $CUID, expected $(id -u) — repos will be root-owned 0700"

hdr "credentials — and the reload trap"

# rest-server loads .htpasswd ONCE at startup ("Loaded htpasswd file /data/.htpasswd"
# in its log) and never reloads it. Prove that directly, because it is the trap
# that makes a correct credential look like a wrong password.
docker exec peerbackup-rest create_user carol carolpw >/dev/null 2>&1
code=$(curl -s -o /dev/null -w '%{http_code}' -u carol:carolpw "$BASE/carol/config")
if [ "$code" = "401" ]; then
  ok "confirmed: a credential added without a restart returns 401 (the trap is real)"
else
  bad "expected 401 for a credential added post-startup, got $code — behaviour changed, update the README"
fi

# adduser exists precisely so nobody hits that. It creates, restarts, verifies.
PB_PORT="$PB_PORT" PB_CONTAINER=peerbackup-rest "$HERE/peerbackup-host" adduser alice alicepw >/dev/null 2>&1
PB_PORT="$PB_PORT" PB_CONTAINER=peerbackup-rest "$HERE/peerbackup-host" adduser bob bobpw >/dev/null 2>&1

code=$(curl -s -o /dev/null -w '%{http_code}' -u alice:alicepw "$BASE/alice/config")
[ "$code" = "404" ] && ok "adduser produces a working credential (404 = authed, no repo yet)" \
  || bad "adduser credential returned $code"

code=$(curl -s -o /dev/null -w '%{http_code}' -u alice:wrongpw "$BASE/alice/config")
[ "$code" = "401" ] && ok "wrong password rejected (401)" || bad "wrong password got $code"

# adduser must FAIL, not warn, when it cannot confirm the credential works.
# Warning here is what once let quickstart print a URL nothing could use.
if out=$(PB_PORT=9999 PB_CONTAINER=peerbackup-rest "$HERE/peerbackup-host" adduser dave davepw 2>&1); then
  bad "adduser exited 0 without confirming the credential works: $out"
else
  ok "adduser fails when it cannot confirm the credential works"
fi
echo "$out" | grep -qi 'could not confirm' && ok "and says so" || bad "unclear message: $out"

hdr "--private-repos isolation"
code=$(curl -s -o /dev/null -w '%{http_code}' -u alice:alicepw "$BASE/bob/config")
[ "$code" = "401" ] && ok "alice cannot reach bob's path (401)" \
  || bad "alice reached bob's path with $code — isolation is broken"

hdr "real restic lifecycle through the stack"
export RESTIC_PASSWORD="e2e-repo-password"
A="rest:http://alice:alicepw@127.0.0.1:$PB_PORT/alice/"
"$RESTIC" -r "$A" init >/dev/null 2>&1 && ok "restic init" || bad "restic init failed"
"$RESTIC" -r "$A" backup "$PB_ROOT/src" >/dev/null 2>&1 && ok "restic backup" || bad "restic backup failed"
"$RESTIC" -r "$A" backup "$PB_ROOT/src" >/dev/null 2>&1
"$RESTIC" -r "$A" check >/dev/null 2>&1 && ok "restic check" || bad "restic check failed"

OUT=$(mktemp -d); "$RESTIC" -r "$A" restore latest --target "$OUT" >/dev/null 2>&1
# Find the file FIRST and assert it exists. `find -exec diff` exits 0 when it
# matches nothing, so the obvious one-liner passes vacuously on a failed restore.
# The first version of this test did exactly that and reported green while
# backup, init and check were all failing.
RESTORED=$(find "$OUT" -type f -name f.txt | head -1)
if [ -z "$RESTORED" ]; then
  bad "restore produced no f.txt at all"
elif cmp -s "$PB_ROOT/src/f.txt" "$RESTORED"; then
  ok "restore round-trips byte-identical"
else
  bad "restored file differs from source"
fi
rm -rf "$OUT"

hdr "--append-only refuses deletion, loudly"
"$RESTIC" -r "$A" forget --keep-last 1 --prune >"$PB_ROOT/ao.out" 2>&1
RC=$?
[ "$RC" -eq 3 ] && ok "prune exits 3 under --append-only" || bad "prune exit was $RC, expected 3"
grep -q '403' "$PB_ROOT/ao.out" && ok "prune failure carries HTTP 403" || bad "no 403 in prune output"
"$RESTIC" -r "$A" unlock >/dev/null 2>&1
"$RESTIC" -r "$A" check >/dev/null 2>&1 && ok "repo undamaged by the blocked prune" \
  || bad "repo damaged by a blocked prune"

hdr "host-side file ownership"
if du -sh "$PB_ROOT/mnt/alice" >/dev/null 2>&1 && [ -r "$PB_ROOT/mnt/alice" ]; then
  ok "host owner can read and du its own peer directory"
  printf '        %s\n' "$(ls -ld "$PB_ROOT/mnt/alice")"
else
  bad "host owner cannot read its own peer directory — is the container running as root?"
fi

hdr "guard still fails closed on this layout"
if PEERBACKUP_ROOT="$PB_ROOT" "$HERE/peerbackup-host" guard >/dev/null 2>&1; then
  bad "guard PASSED on plain directories — fail-closed is broken"
else
  ok "guard refuses: these are directories, not mounted quota volumes"
fi

hdr "Summary"
printf '  passed: %d   failed: %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
