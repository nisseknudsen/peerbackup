#!/usr/bin/env bash
# Runs the client from the container image against a real rest-server, and
# checks the failure that Docker makes easy: a source directory that was not
# mounted.
#
# Requires docker. Does not require root.

set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-/tmp/pb-docker-e2e}"
PORT="${PORT:-8023}"
SERVER=pb-docker-server
IMAGE="${IMAGE:-peerbackup:test}"

PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }
hdr() { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }

cleanup() {
  docker rm -f "$SERVER" >/dev/null 2>&1 || true
  # Some steps run as other uids and leave files this user cannot delete.
  [ -d "$WORK" ] && docker run --rm -v "$WORK:/w" alpine:3.20 \
    sh -c 'rm -rf /w/* /w/.[!.]* 2>/dev/null' >/dev/null 2>&1 || true
}
trap cleanup EXIT

command -v docker >/dev/null || { echo "docker required"; exit 1; }

# Run the client exactly the way docker/README.md tells people to.
pb() {
  docker run --rm --network host \
    --user "$(id -u):$(id -g)" \
    -v "$WORK/cfg:/config" \
    -v "$WORK/state:/state" \
    "$@"
}

hdr "setup"
cleanup
rm -rf "$WORK"; mkdir -p "$WORK"/{cfg,state,srv,data,other}
echo "in the mounted directory" > "$WORK/data/kept.txt"
echo "in the unmounted one"     > "$WORK/other/missed.txt"

docker build -q -f docker/Dockerfile -t "$IMAGE" "$HERE" >/dev/null || { echo "build failed"; exit 1; }
ok "image built"

docker run -d --name "$SERVER" -p "127.0.0.1:$PORT:8000" \
  --user "$(id -u):$(id -g)" \
  -e OPTIONS="--private-repos --append-only" \
  -v "$WORK/srv:/data" restic/rest-server:0.14.0 >/dev/null
for _ in $(seq 1 40); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break; sleep 0.25; done
docker exec "$SERVER" create_user me pw >/dev/null 2>&1
docker restart "$SERVER" >/dev/null 2>&1; sleep 2
ok "rest-server running"

hdr "init and configure"
pb "$IMAGE" init >/dev/null 2>&1 && ok "init through the container" || bad "init failed"
# Two sources; the second is deliberately not mounted later.
sed -i "s|sources = \[\]|sources = [\"$WORK/data\", \"$WORK/other\"]|" "$WORK/cfg/config.toml"
ok "two source directories configured"

hdr "a forgotten mount is refused, not silently skipped"
# Only /data is mounted. restic on its own would save a snapshot, exit 0 and
# print one warning, leaving the other directory out of the backup.
OUT=$(pb -v "$WORK/data:$WORK/data:ro" "$IMAGE" peer add alice \
      "rest:http://me:pw@127.0.0.1:$PORT/me/" 2>&1)
if echo "$OUT" | grep -q "matches"; then
  ok "peer add works (it only uploads the test file)"
else
  bad "peer add failed: $OUT"
fi

OUT=$(pb -v "$WORK/data:$WORK/data:ro" "$IMAGE" backup 2>&1)
if echo "$OUT" | grep -qi "missing or unreadable"; then
  ok "backup refuses when a source is not mounted"
else
  bad "backup did not refuse a missing source"
  echo "$OUT" | sed 's/^/      /'
fi
if echo "$OUT" | grep -q "bind-mounted at the same path"; then
  ok "the error says what to do about it"
else
  bad "the error does not mention mounts"
fi

hdr "with both mounted"
pb -v "$WORK/data:$WORK/data:ro" -v "$WORK/other:$WORK/other:ro" \
   "$IMAGE" backup 2>&1 | grep -q "done" && ok "backup completes" || bad "backup failed"

hdr "verify and status through the container"
pb "$IMAGE" verify 2>&1 | grep -q matches && ok "verify passes" || bad "verify failed"
pb "$IMAGE" status 2>&1 | grep -qE "alice .*ok" && ok "status reports ok" || bad "status not ok"

hdr "paths are recorded as they are on the host"
# The whole reason for the same-path rule. If this shows container-internal
# paths, the recovery instructions would be wrong.
pb "$IMAGE" snapshots alice >/dev/null 2>&1 && ok "snapshots listed" || bad "snapshots failed"

# Create the target first. Docker creates a missing bind-mount source as root,
# and the container runs unprivileged, so the restore would fail with a
# permission error that is easy to misread.
mkdir -p "$WORK/restored"
OUT=$(docker run --rm --network host --user "$(id -u):$(id -g)" \
      -v "$WORK/cfg:/config" -v "$WORK/state:/state" \
      -v "$WORK/restored:/restored" "$IMAGE" restore alice /restored 2>&1)
echo "$OUT" | grep -qi "^error" && { bad "restore failed"; echo "$OUT" | sed 's/^/      /'; }
if [ -f "$WORK/restored/$WORK/data/kept.txt" ]; then
  ok "restored tree uses host paths"
else
  bad "restored tree does not use host paths"
  find "$WORK/restored" -type f 2>/dev/null | head -3 | sed 's/^/      /'
fi

hdr "works as a uid that is not in the image"
# People are told to pass their own uid. If anything in the image assumes 1000,
# or leaves HOME unset, restic fails with "mkdir /.cache: permission denied".
# CI runs as 1001, which is how this was found.
install -d -m 777 "$WORK/anyuid-cfg" "$WORK/anyuid-state"
OUT=$(docker run --rm --network host --user "4242:4242" \
      -v "$WORK/anyuid-cfg:/config" -v "$WORK/anyuid-state:/state" \
      "$IMAGE" init 2>&1)
if echo "$OUT" | grep -qi "permission denied"; then
  bad "image assumes a specific uid: $OUT"
else
  ok "init works as an unknown uid"
fi
OUT=$(docker run --rm --network host --user "4242:4242" \
      -v "$WORK/anyuid-cfg:/config" -v "$WORK/anyuid-state:/state" \
      -v "$WORK/data:$WORK/data:ro" \
      "$IMAGE" peer add alice "rest:http://me:pw@127.0.0.1:$PORT/me/anyuid/" 2>&1)
if echo "$OUT" | grep -q "matches"; then
  ok "restic works as an unknown uid (cache directory is writable)"
else
  bad "restic failed as an unknown uid: $(echo "$OUT" | tail -2)"
fi

hdr "re-adding a peer without the original password explains itself"
install -d -m 777 "$WORK/lost-cfg" "$WORK/lost-state"
docker run --rm --network host --user "$(id -u):$(id -g)" \
  -v "$WORK/lost-cfg:/config" -v "$WORK/lost-state:/state" "$IMAGE" init >/dev/null 2>&1
OUT=$(docker run --rm --network host --user "$(id -u):$(id -g)" \
      -v "$WORK/lost-cfg:/config" -v "$WORK/lost-state:/state" \
      -v "$WORK/data:$WORK/data:ro" \
      "$IMAGE" peer add alice "rest:http://me:pw@127.0.0.1:$PORT/me/" 2>&1)
if echo "$OUT" | grep -q "recovery file"; then
  ok "points at the recovery file instead of relaying restic's error"
else
  bad "unhelpful error when the password does not match: $(echo "$OUT" | tail -2)"
fi

hdr "state persists between runs"
COUNT=$(wc -l < "$WORK/state/evidence.jsonl" 2>/dev/null || echo 0)
[ "$COUNT" -gt 3 ] && ok "results accumulate in the mounted state directory ($COUNT)" \
  || bad "state did not persist ($COUNT records)"

hdr "Summary"
printf '  passed: %d   failed: %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
