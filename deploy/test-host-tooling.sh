#!/usr/bin/env bash
# Tests for `peerbackup host`, driven as a black box.
#
# Everything here runs WITHOUT root, because the paths that matter most are the
# refusals: bad size strings, bad peer names, overcommit, and above all the
# guard refusing to start when a grant directory is not really mounted. A guard
# that fails open is the worst bug available in this design, so it gets the most
# tests.
#
# Run: ./deploy/test-host-tooling.sh

set -uo pipefail
# The command under test, as an array so callers can prefix environment
# variables normally. A shell function would leak `DRY_RUN=1 host ...` into the
# rest of the script, because bash keeps the assignment after a function call.
BIN="${PB_BIN:-$(dirname "$0")/../target/debug/peerbackup}"
HOST=("$BIN" host)
PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }
hdr() { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }

WORK=$(mktemp -d /tmp/pb-host-test-XXXXXX)
trap 'rm -rf "$WORK"' EXIT
export PEERBACKUP_ROOT="$WORK/srv"
export SYSTEMD_UNIT_DIR="$WORK/units"
mkdir -p "$PEERBACKUP_ROOT/mnt" "$SYSTEMD_UNIT_DIR"

hdr "the binary is there and answers"
if [ -x "$BIN" ]; then ok "$BIN is executable"
else bad "$BIN not found — run: cargo build"; exit 1; fi
if "${HOST[@]}" --help >/dev/null 2>&1; then ok "host --help works"
else bad "host --help failed"; exit 1; fi
if bash -n "$(dirname "$0")/../spike/lifecycle-spike.sh"; then ok "lifecycle-spike parses"; else bad "spike syntax error"; fi

hdr "size parsing — good input"
# This block used to reimplement to_bytes inside the test and assert against
# the copy, because the shell version could not be sourced without running its
# dispatcher. The shipped function was never executed, so a bug in it would
# have passed. That is the single clearest reason this moved into the binary.
#
# The unit tests in src/host/size.rs cover parsing directly. What is left here
# is the black-box half: prove the size a user types reaches the allocation.
#
# Sizes here stay small on purpose: admission control runs during a dry run, so
# asking for 500G on a CI disk is refused before it ever reaches fallocate.
# Magnitudes larger than the disk are covered by the overcommit test below,
# which proves the same parsing from the other side.
for pair in "2G:2147483648" "512M:536870912" "1024K:1048576" "4096:4096"; do
  in="${pair%%:*}"; want="${pair##*:}"
  out=$(DRY_RUN=1 HOST_MARGIN_GB=0 "${HOST[@]}" provision sizecheck "$in" 2>&1)
  got=$(echo "$out" | sed -n 's/.*would run: fallocate -l \([0-9]*\) .*/\1/p' | head -1)
  if [ "$got" = "$want" ]; then ok "$in -> fallocate -l $want"
  else bad "$in -> fallocate got '$got', want '$want'"; echo "$out" | sed 's/^/      /'; fi
done

hdr "size parsing — bad input must be refused, not guessed"
for bad_in in "500X" "abc" "" "-5G" "5.5G"; do
  out=$(DRY_RUN=1 "${HOST[@]}" provision testpeer "$bad_in" 2>&1)
  if echo "$out" | grep -qi 'bad size\|usage:'; then
    ok "refused '$bad_in'"
  else
    bad "accepted bad size '$bad_in': $out"
  fi
done

hdr "peer name validation — the name becomes a path and a systemd unit"
for bad_peer in "../etc" "a/b" "a b" "a;rm" ""; do
  out=$(DRY_RUN=1 "${HOST[@]}" provision "$bad_peer" 500G 2>&1)
  if echo "$out" | grep -qi 'peer name\|usage:'; then
    ok "refused peer name '$bad_peer'"
  else
    bad "accepted dangerous peer name '$bad_peer': $out"
  fi
done
out=$(DRY_RUN=1 "${HOST[@]}" provision "friend-a_1" 500G 2>&1)
if echo "$out" | grep -qi 'bad size\|peer name'; then
  bad "rejected a legitimate peer name"
else
  ok "accepted legitimate peer name 'friend-a_1'"
fi

hdr "guard: refuses when a grant is not mounted, and only then"

# 1. No grants and no images: this host does not use per-peer grants at all, so
#    there is nothing for the guard to protect. It used to refuse here, which
#    reads as fail-closed and is not: anyone who ran `host quickstart` has no
#    grants by design, and installing the service unit gave them a service that
#    could never start.
# ${var:?} so an unset PEERBACKUP_ROOT aborts instead of rm -rf /mnt.
rm -rf "${PEERBACKUP_ROOT:?}/mnt" "${PEERBACKUP_ROOT:?}/images"
mkdir -p "$PEERBACKUP_ROOT/mnt"
if "${HOST[@]}" guard >/dev/null 2>&1; then
  ok "guard lets a host with no grants start"
else
  bad "guard refused on a host that uses no per-peer grants"
fi

# 1b. An image with no mount under it is the case it is actually for: a grant was
#     provisioned, so writes are expected to land inside it, and they would not.
mkdir -p "$PEERBACKUP_ROOT/images"
: > "$PEERBACKUP_ROOT/images/alice.img"
if "${HOST[@]}" guard >/dev/null 2>&1; then
  bad "guard PASSED with a provisioned image and nothing mounted"
else
  ok "guard refuses when a grant exists but is not mounted"
fi
rm -f "$PEERBACKUP_ROOT/images/alice.img"

# 2. A grant directory that exists but is NOT a mountpoint. This is the exact
#    boot race the guard exists for: bind mount resolves to the root filesystem.
mkdir -p "$PEERBACKUP_ROOT/mnt/nisse"
if "${HOST[@]}" guard >/dev/null 2>&1; then
  bad "guard PASSED on a non-mountpoint — writes would hit the host root fs with no quota"
else
  ok "guard refuses when a grant directory is not a mountpoint"
fi

# 3. The refusal must say why, or nobody will know what to fix at 3am.
out=$("${HOST[@]}" guard 2>&1)
if echo "$out" | grep -qi 'not a mountpoint\|NOT MOUNTED'; then
  ok "guard names the offending directory and the reason"
else
  bad "guard refused without explaining: $out"
fi

# 4. Success path needs a real mountpoint. Only reachable with root, so it is
#    conditional rather than skipped silently.
if [ "$(id -u)" -eq 0 ]; then
  mount -t tmpfs -o size=10M tmpfs "$PEERBACKUP_ROOT/mnt/nisse" 2>/dev/null && {
    if "${HOST[@]}" guard >/dev/null 2>&1; then ok "guard passes on a real mountpoint"
    else bad "guard refused a genuinely mounted directory"; fi
    umount "$PEERBACKUP_ROOT/mnt/nisse"
  }
else
  printf '  \033[33mSKIP\033[0m  guard success path (needs root to create a real mount)\n'
fi

hdr "admission control — refuse to overcommit the host"
# Runs under DRY_RUN on purpose: the check must happen even in a dry run, or the
# rehearsal proves nothing about the thing you are rehearsing.
out=$(DRY_RUN=1 HOST_MARGIN_GB=20 "${HOST[@]}" provision bigpeer 900T 2>&1)
if echo "$out" | grep -qi 'refusing to overcommit'; then
  ok "oversized grant refused, and refused during a dry run"
else
  bad "oversized grant was not refused: $(echo "$out" | tail -2)"
fi

# A grant that clearly fits must NOT be refused, or the check is just a wall.
out=$(DRY_RUN=1 HOST_MARGIN_GB=0 "${HOST[@]}" provision smallpeer 1M 2>&1)
if echo "$out" | grep -qi 'refusing to overcommit'; then
  bad "a 1M grant was refused — admission control is too aggressive"
else
  ok "a grant that fits is allowed through"
fi

# The operator has to be able to see the arithmetic, not just the verdict.
for want in requested available margin; do
  echo "$out" | grep -qi "$want" && ok "provision shows '$want'" || bad "provision hides '$want'"
done

hdr "list — reports all three numbers, and is honest that nothing enforces the reserve"
out=$("${HOST[@]}" list 2>&1)
for want in IMAGE USABLE RESERVE USED; do
  echo "$out" | grep -q "$want" && ok "list reports $want" || bad "list missing $want"
done
# This used to assert that the reserve was "enforced by the CLIENT". No client
# reserve model was ever built, so the test was asserting a false claim and
# locking it in. The reserve is advisory; the output has to say so.
if echo "$out" | grep -qi 'nothing enforces it'; then
  ok "list states plainly that the reserve is not enforced"
else
  bad "list must not imply the maintenance reserve is enforced by anything"
fi
if echo "$out" | grep -qi 'enforced by the CLIENT'; then
  bad "list still claims client enforcement, which does not exist"
else
  ok "list makes no false enforcement claim"
fi

hdr "release — must refuse without confirmation"
out=$(printf 'wrongname\n' | DRY_RUN=0 "${HOST[@]}" release nisse 2>&1)
if echo "$out" | grep -qi 'aborted\|must run as root\|no grant found'; then
  ok "release refuses on a mistyped confirmation"
else
  bad "release proceeded without correct confirmation: $out"
fi

hdr "quickstart defaults and failure reporting"
# The first command a new user runs. It must not need root, and it must never
# print a URL when the server is not actually up.
#
# These used to grep the shell source for `DEFAULT_DATA=` and `DEFAULT_PORT=`.
# Greping an implementation for a constant proves the constant is written down,
# not that it is used. Ask the program instead: run it somewhere it must fail,
# and read which path and port it actually reached for.
#
# Both probes use their own container name so they can never disturb a real
# server running on this machine, and both are arranged to fail before anything
# is created, so the only thing observed is which path or port was reached for.

# Point XDG_DATA_HOME somewhere that cannot be created. The error then names the
# directory it tried, which is the default under test.
out=$(PB_CONTAINER=pb-defaults-probe XDG_DATA_HOME=/proc/nowhere \
      "${HOST[@]}" quickstart tester 2>&1)
if echo "$out" | grep -q '/proc/nowhere/peerbackup-data'; then
  ok "default storage follows XDG_DATA_HOME, not /srv"
elif echo "$out" | grep -qi 'docker is required\|cannot talk to docker'; then
  printf '  \033[33mSKIP\033[0m  default storage path (needs docker)\n'
else
  bad "default storage is not under the user's data dir: $out"
fi

# 51515 is in the dynamic range; 8000 collides constantly. Occupy the default
# and watch the collision get reported. If something already holds it, that is
# the same condition, so a failed bind is not a problem.
if command -v python3 >/dev/null 2>&1; then
  python3 -c "
import socket,time
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
try:
    s.bind(('127.0.0.1',51515)); s.listen(1)
except OSError:
    pass
time.sleep(8)
" >/dev/null 2>&1 &
  BLOCKER=$!
  sleep 1
  out=$(PB_DATA="$WORK/portprobe" PB_CONTAINER=pb-port-probe "${HOST[@]}" quickstart tester 2>&1)
  kill "$BLOCKER" 2>/dev/null; wait "$BLOCKER" 2>/dev/null
  docker rm -f pb-port-probe >/dev/null 2>&1 || true
  if echo "$out" | grep -q 'port 51515 is already in use'; then
    ok "default port is 51515, out of the commonly-used range"
  elif echo "$out" | grep -qi 'docker is required\|cannot talk to docker'; then
    printf '  \033[33mSKIP\033[0m  default port (needs docker)\n'
  else
    bad "did not reach for port 51515: $out"
  fi
else
  printf '  \033[33mSKIP\033[0m  default port (needs python3)\n'
fi

# A size limit the operator set deliberately must never be silently replaced by
# a default. This used to be passed straight through to the server, which died
# on it; now it is refused before docker is touched, which is the better place.
OUT=$(PB_DATA="$WORK/badsize" PB_MAX_SIZE=not-a-number "${HOST[@]}" quickstart tester 2>&1)
echo "$OUT" | grep -qi 'PB_MAX_SIZE' \
  && ok "an unparseable PB_MAX_SIZE is refused by name" \
  || bad "PB_MAX_SIZE=not-a-number was not rejected: $OUT"
echo "$OUT" | grep -q "Ready. Send both" \
  && bad "printed a peer URL despite a bad size limit" \
  || ok "no URL printed when the size limit is unusable"
OUT=$(PB_DATA="$WORK/sizeprobe" PB_MAX_SIZE=10G PB_CONTAINER=pb-size-probe "${HOST[@]}" quickstart tester 2>&1)
echo "$OUT" | grep -qi 'PB_MAX_SIZE' \
  && bad "rejected a valid size string: $OUT" \
  || ok "PB_MAX_SIZE accepts a human size like 10G"
docker rm -f pb-size-probe >/dev/null 2>&1 || true

# `command -v docker` finds the CLI, which says nothing about whether the daemon
# is reachable. On a machine with docker installed but not running, quickstart
# stops at "cannot talk to docker" and this block reported two failures that had
# nothing to do with the code under test.
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  # Make docker itself refuse: a container name with a slash in it is invalid.
  # The point is the reaction, not the cause -- no URL, and a clear reason.
  OUT=$(PB_DATA="$WORK/dies" PB_PORT=51599 PB_CONTAINER="bad/name" \
        "${HOST[@]}" quickstart tester 2>&1)
  if echo "$OUT" | grep -q "Ready. Send both"; then
    bad "printed a peer URL even though the server failed to start"
  else
    ok "a server that fails to start does not print a URL"
  fi
  echo "$OUT" | grep -qi "could not start" && ok "says the server did not start" \
    || bad "no clear message when the server fails"
  docker rm -f peerbackup-rest >/dev/null 2>&1 || true

  # A container that is up is not a server that works. Both of these printed a
  # peer URL that could not possibly work, on a real first install.

  # 1. Left over from an earlier setup, listening on a different port.
  mkdir -p "$WORK/stale"
  docker run -d --name peerbackup-rest --user "$(id -u):$(id -g)" -p 8099:8000 \
    -v "$WORK/stale:/data" -e OPTIONS="--private-repos" \
    restic/rest-server:0.14.0 >/dev/null 2>&1
  sleep 2
  OUT=$(PB_DATA="$WORK/d1" PB_PORT=51598 "${HOST[@]}" quickstart tester 2>&1)
  if echo "$OUT" | grep -q "Ready. Send both"; then
    bad "printed a URL while a stale container held the name"
  else
    ok "refuses when a container is up but not serving the expected port"
  fi
  echo "$OUT" | grep -q "docker rm -f" && ok "tells you how to clear the stale container" \
    || bad "no remedy offered for the stale container"
  docker rm -f peerbackup-rest >/dev/null 2>&1 || true

  # 2. Running and reachable, but its storage was deleted underneath it.
  mkdir -p "$WORK/vanish"
  docker run -d --name peerbackup-rest --user "$(id -u):$(id -g)" -p 51597:8000 \
    -v "$WORK/vanish:/data" -e OPTIONS="--private-repos" \
    restic/rest-server:0.14.0 >/dev/null 2>&1
  sleep 2
  rm -rf "$WORK/vanish"
  OUT=$(PB_DATA="$WORK/d2" PB_PORT=51597 "${HOST[@]}" quickstart tester 2>&1)
  if echo "$OUT" | grep -q "Ready. Send both"; then
    bad "printed a URL when the login could not be created"
  else
    ok "refuses when the login cannot be created"
  fi
  docker rm -f peerbackup-rest >/dev/null 2>&1 || true
else
  printf '  \033[33mSKIP\033[0m  quickstart failure path (needs docker)\n'
fi

hdr "the invite keeps the address and the password apart"
# A password on a command line lands in shell history, in `ps`, and permanently
# in `docker inspect`. The invite used to be one URL with the password in it, so
# the only way to use it was to paste it onto a command line.
OUT=$(PB_DATA="$WORK/invite" PB_CONTAINER=pb-invite-probe PB_PORT=51598 "${HOST[@]}" quickstart invitee 2>&1)
if echo "$OUT" | grep -qE 'URL: +rest:http://invitee@'; then
  ok "the invite URL carries a username and no password"
else
  bad "the invite URL is not in the expected shape"
  echo "$OUT" | sed 's/^/        /' | tail -6
fi
if echo "$OUT" | grep -qE '^ *Password: +[A-Za-z0-9]{16,}'; then
  ok "the password is printed on its own line"
else
  bad "no separate password line in the invite"
fi
if echo "$OUT" | grep -q "connect 'rest:http://invitee@"; then
  ok "the suggested command contains no secret"
else
  bad "the suggested connect command is not password-free"
fi
docker rm -f pb-invite-probe >/dev/null 2>&1 || true

hdr "compose.yml"
COMPOSE="$(dirname "$0")/../compose.yml"
grep -q 'user:' "$COMPOSE" && ok "compose sets user (host can read its own data)" \
  || bad "compose missing user: — repos will be root-owned 0700"
grep -q 'OPTIONS:' "$COMPOSE" && ok "compose configures via OPTIONS env, not command args" \
  || bad "compose does not use OPTIONS env — runc will try to exec the flags"
grep -q 'private-repos' "$COMPOSE" && ok "compose sets --private-repos" || bad "missing --private-repos"
grep -q 'append-only' "$COMPOSE" && ok "compose sets --append-only" || bad "missing --append-only"
# Match an actual YAML key, not the word "command:" inside a comment. The first
# version of this test failed on its own documentation.
if grep -qE '^[[:space:]]+command:' "$COMPOSE"; then
  bad "compose uses command: — the image takes config via env, this will fail at runc"
else
  ok "compose does not pass flags as command args"
fi
# HTTP/2 puts every upload on one TCP connection and caps a distant peer at a
# fraction of the link. restic's client cannot be told to avoid it, so the
# server must not offer it.
if grep -qE '^[[:space:]]+GODEBUG:[[:space:]]+http2server=0' "$COMPOSE"; then
  ok "compose turns HTTP/2 off on the server"
else
  bad "compose leaves HTTP/2 on, which caps a distant peer at one TCP connection"
fi

hdr "systemd unit — the guard must actually be wired in"
UNIT="$(dirname "$0")/systemd/peerbackup-rest.service"
grep -q 'ExecStartPre=.*guard' "$UNIT" && ok "unit runs the guard as ExecStartPre" \
  || bad "unit does not run the guard — fail-closed is not wired in"
grep -q 'RequiresMountsFor' "$UNIT" && ok "unit declares a mount dependency" \
  || bad "unit missing RequiresMountsFor"

hdr "Summary"
printf '  passed: %d   failed: %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
