#!/usr/bin/env bash
# Runs the client from the container image against a real rest-server, and
# checks the failure that Docker makes easy: a source directory that was not
# mounted.
#
# Requires docker. Does not require root.

set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=tests/lib/scratch.sh
. "$HERE/tests/lib/scratch.sh"
WORK="${WORK:-/tmp/pb-docker-e2e}"
PORT="${PORT:-8023}"
# See the note in deploy/test-compose-e2e.sh: a known credential should not be
# reachable from off the machine.
export PB_BIND="${PB_BIND:-127.0.0.1}"
SERVER=peerbackup-rest   # the name compose.host.yml uses
IMAGE="${IMAGE:-peerbackup:test}"

PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }
hdr() { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }

HOST_COMPOSE="$HERE/compose.yml"

cleanup() {
  docker compose -f "$HOST_COMPOSE" down -v >/dev/null 2>&1 || true
  docker rm -f "$SERVER" peerbackup-maint >/dev/null 2>&1 || true
  # Some steps run as other uids and leave files this user cannot delete.
  [ -d "$WORK" ] && docker run --rm -v "$WORK:/w" alpine:3.20 \
    sh -c 'rm -rf /w/* /w/.[!.]* 2>/dev/null' >/dev/null 2>&1 || true
}
trap cleanup EXIT

command -v docker >/dev/null || { echo "docker required"; exit 1; }
docker info >/dev/null 2>&1 || { echo "docker daemon not reachable"; exit 1; }

# Run the client exactly the way the README tells people to.
pb() {
  docker run --rm --network host \
    --user "$(id -u):$(id -g)" \
    -v "$WORK/cfg:/config" \
    -v "$WORK/state:/state" \
    "$@"
}

hdr "setup"
# Before the first rm -rf, and before cleanup's root container gets near it:
# WORK comes from the environment, and this suite wipes it as root.
scratch_claim "$WORK" || exit 1
cleanup
rm -rf "${WORK:?}"; mkdir -p "$WORK"/{cfg,state,srv,data,other}
scratch_mark "$WORK"
echo "in the mounted directory" > "$WORK/data/kept.txt"
echo "in the unmounted one"     > "$WORK/other/missed.txt"

docker build -q -f "$HERE/Dockerfile" -t "$IMAGE" "$HERE" >/dev/null || { echo "build failed"; exit 1; }
ok "image built"

# Started exactly as the README instructs, so the documented host setup is
# covered rather than described.
PB_DATA="$WORK/srv" PB_PORT="$PORT" PB_UID="$(id -u)" PB_GID="$(id -g)" \
  docker compose -f "$HOST_COMPOSE" up -d >/dev/null 2>&1
for _ in $(seq 1 60); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break; sleep 0.25; done
# Same rule as the backup assertion below, for the same pipefail reason. This
# one rarely lost the race because `docker ps` exits right after writing, which
# is exactly what makes it the kind of bug that surfaces months later.
PS_OUT=$(docker ps --filter "name=$SERVER" --format '{{.Status}}')
[[ "$PS_OUT" == *Up* ]] \
  && ok "host compose file starts a server" || { bad "host compose failed"; docker compose -f "$HOST_COMPOSE" logs | tail -5; exit 1; }

# Checked, not assumed. This printed PASS whatever happened, so a server that
# came up but could not write its htpasswd file reported "login created" and
# then failed further down as a peerbackup problem.
docker exec "$SERVER" create_user me pw >/dev/null 2>&1 \
  || { bad "could not create the rest-server login"
       docker compose -f "$HOST_COMPOSE" logs | tail -5; exit 1; }
docker restart "$SERVER" >/dev/null 2>&1 \
  || { bad "rest-server did not restart after the login was created"; exit 1; }
for _ in $(seq 1 60); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break; sleep 0.25; done
ok "login created"

# The host must be able to read what it is storing.
[ -r "$WORK/srv" ] && ok "stored data is readable by the host owner" \
  || bad "stored data is root-owned; PB_UID/PB_GID not applied"

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
# Capture, then match with bash rather than a pipe.
#
# `cmd | grep -q PATTERN` under `set -o pipefail` is a race. grep -q exits the
# instant it matches and closes the pipe; the still-writing producer takes
# SIGPIPE and exits 141; pipefail then reports 141 for a pipeline whose match
# succeeded. `backup` prints "done (id)" and can print a recovery-file warning
# after it, so there is always something still to write when grep leaves.
#
# This is why this job failed intermittently. Capturing first is not enough on
# its own either: `echo "$BIG" | grep -q` races the same way once the string
# exceeds the 64KB pipe buffer. `[[ ]]` has no pipe and no subprocess, so it
# cannot race at all.
OUT=$(pb -v "$WORK/data:$WORK/data:ro" -v "$WORK/other:$WORK/other:ro" \
        "$IMAGE" backup 2>&1)
if [[ "$OUT" == *done* ]]; then
  ok "backup completes"
else
  bad "backup failed"; echo "$OUT" | sed 's/^/      /'
fi

hdr "verify and status through the container"
VOUT=$(pb "$IMAGE" verify 2>&1)
echo "$VOUT" | grep -q matches && ok "verify passes" || { bad "verify failed"; echo "$VOUT" | sed 's/^/      /'; }

SOUT=$(pb "$IMAGE" status 2>&1)
if echo "$SOUT" | grep -qE "alice +ok"; then
  ok "status reports ok"
else
  bad "status not ok"
  echo "$VOUT" | sed 's/^/      verify: /'
  echo "$SOUT" | sed 's/^/      status: /'
  echo "      evidence:"
  sed 's/^/        /' "$WORK/state/evidence.jsonl" 2>/dev/null | tail -6
fi

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
