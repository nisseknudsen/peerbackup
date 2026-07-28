#!/usr/bin/env bash
# Tests for deploy/peerbackup-host.
#
# Everything here runs WITHOUT root, because the paths that matter most are the
# refusals: bad size strings, bad peer names, overcommit, and above all the
# guard refusing to start when a grant directory is not really mounted. A guard
# that fails open is the worst bug available in this design, so it gets the most
# tests.
#
# Run: ./deploy/test-host-tooling.sh

set -uo pipefail
HOST="$(dirname "$0")/peerbackup-host"
PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }
hdr() { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }

WORK=$(mktemp -d /tmp/pb-host-test-XXXXXX)
trap 'rm -rf "$WORK"' EXIT
export PEERBACKUP_ROOT="$WORK/srv"
export SYSTEMD_UNIT_DIR="$WORK/units"
mkdir -p "$PEERBACKUP_ROOT/mnt" "$SYSTEMD_UNIT_DIR"

hdr "syntax"
if bash -n "$HOST"; then ok "peerbackup-host parses"; else bad "syntax error"; exit 1; fi
if bash -n "$(dirname "$0")/../spike/lifecycle-spike.sh"; then ok "lifecycle-spike parses"; else bad "spike syntax error"; fi

hdr "size parsing — good input"
for pair in "500G:536870912000" "1T:1099511627776" "512M:536870912" "1024K:1048576" "4096:4096"; do
  in="${pair%%:*}"; want="${pair##*:}"
  got=$(DRY_RUN=1 bash -c 'source "$0" 2>/dev/null; to_bytes "$1"' "$HOST" "$in" 2>/dev/null | tail -1)
  # sourcing runs the dispatcher, so fall back to an isolated extraction
  got=$(bash -c '
    to_bytes() {
      local s="${1^^}" n unit
      n="${s%%[KMGT]*}"; unit="${s#"$n"}"; unit="${unit%B}"
      case "$unit" in
        K) echo $(( n * 1024 ));; M) echo $(( n * 1024 * 1024 ));;
        G) echo $(( n * 1024 * 1024 * 1024 ));; T) echo $(( n * 1024 * 1024 * 1024 * 1024 ));;
        "") echo "$n";; esac
    }; to_bytes "$1"' _ "$in")
  if [ "$got" = "$want" ]; then ok "$in -> $want"; else bad "$in -> got '$got', want '$want'"; fi
done

hdr "size parsing — bad input must be refused, not guessed"
for bad_in in "500X" "abc" "" "-5G" "5.5G"; do
  out=$(DRY_RUN=1 "$HOST" provision testpeer "$bad_in" 2>&1)
  if echo "$out" | grep -qi 'bad size\|usage:'; then
    ok "refused '$bad_in'"
  else
    bad "accepted bad size '$bad_in': $out"
  fi
done

hdr "peer name validation — the name becomes a path and a systemd unit"
for bad_peer in "../etc" "a/b" "a b" "a;rm" ""; do
  out=$(DRY_RUN=1 "$HOST" provision "$bad_peer" 500G 2>&1)
  if echo "$out" | grep -qi 'peer name\|usage:'; then
    ok "refused peer name '$bad_peer'"
  else
    bad "accepted dangerous peer name '$bad_peer': $out"
  fi
done
out=$(DRY_RUN=1 "$HOST" provision "friend-a_1" 500G 2>&1)
if echo "$out" | grep -qi 'bad size\|peer name'; then
  bad "rejected a legitimate peer name"
else
  ok "accepted legitimate peer name 'friend-a_1'"
fi

hdr "guard: refuses to start when storage is not mounted"

# 1. No grant directories at all: must refuse, not shrug.
# ${var:?} so an unset PEERBACKUP_ROOT aborts instead of rm -rf /mnt.
rm -rf "${PEERBACKUP_ROOT:?}/mnt"; mkdir -p "$PEERBACKUP_ROOT/mnt"
if "$HOST" guard >/dev/null 2>&1; then
  bad "guard PASSED with zero grants — would start a server with nothing mounted"
else
  ok "guard refuses when there are no grants"
fi

# 2. A grant directory that exists but is NOT a mountpoint. This is the exact
#    boot race the guard exists for: bind mount resolves to the root filesystem.
mkdir -p "$PEERBACKUP_ROOT/mnt/nisse"
if "$HOST" guard >/dev/null 2>&1; then
  bad "guard PASSED on a non-mountpoint — writes would hit the host root fs with no quota"
else
  ok "guard refuses when a grant directory is not a mountpoint"
fi

# 3. The refusal must say why, or nobody will know what to fix at 3am.
out=$("$HOST" guard 2>&1)
if echo "$out" | grep -qi 'not a mountpoint\|NOT MOUNTED'; then
  ok "guard names the offending directory and the reason"
else
  bad "guard refused without explaining: $out"
fi

# 4. Success path needs a real mountpoint. Only reachable with root, so it is
#    conditional rather than skipped silently.
if [ "$(id -u)" -eq 0 ]; then
  mount -t tmpfs -o size=10M tmpfs "$PEERBACKUP_ROOT/mnt/nisse" 2>/dev/null && {
    if "$HOST" guard >/dev/null 2>&1; then ok "guard passes on a real mountpoint"
    else bad "guard refused a genuinely mounted directory"; fi
    umount "$PEERBACKUP_ROOT/mnt/nisse"
  }
else
  printf '  \033[33mSKIP\033[0m  guard success path (needs root to create a real mount)\n'
fi

hdr "admission control — refuse to overcommit the host"
# Runs under DRY_RUN on purpose: the check must happen even in a dry run, or the
# rehearsal proves nothing about the thing you are rehearsing.
out=$(DRY_RUN=1 HOST_MARGIN_GB=20 "$HOST" provision bigpeer 900T 2>&1)
if echo "$out" | grep -qi 'refusing to overcommit'; then
  ok "oversized grant refused, and refused during a dry run"
else
  bad "oversized grant was not refused: $(echo "$out" | tail -2)"
fi

# A grant that clearly fits must NOT be refused, or the check is just a wall.
out=$(DRY_RUN=1 HOST_MARGIN_GB=0 "$HOST" provision smallpeer 1M 2>&1)
if echo "$out" | grep -qi 'refusing to overcommit'; then
  bad "a 1M grant was refused — admission control is too aggressive"
else
  ok "a grant that fits is allowed through"
fi

# The operator has to be able to see the arithmetic, not just the verdict.
for want in requested available margin; do
  echo "$out" | grep -qi "$want" && ok "provision shows '$want'" || bad "provision hides '$want'"
done

hdr "list — reports all three numbers, and says who enforces the reserve"
out=$("$HOST" list 2>&1)
for want in IMAGE USABLE RESERVE USED; do
  echo "$out" | grep -q "$want" && ok "list reports $want" || bad "list missing $want"
done
if echo "$out" | grep -qi 'enforced by'; then
  ok "list states that the reserve is client-enforced, not server-enforced"
else
  bad "list does not say who enforces the maintenance reserve"
fi

hdr "release — must refuse without confirmation"
out=$(printf 'wrongname\n' | DRY_RUN=0 "$HOST" release nisse 2>&1)
if echo "$out" | grep -qi 'aborted\|must run as root\|no grant found'; then
  ok "release refuses on a mistyped confirmation"
else
  bad "release proceeded without correct confirmation: $out"
fi

hdr "quickstart defaults and failure reporting"
# The first command a new user runs. It must not need root, and it must never
# print a URL when the server is not actually up.
grep -q 'DEFAULT_DATA=.*XDG_DATA_HOME' "$HOST" \
  && ok "default storage is under the user's home, not /srv" \
  || bad "default storage path needs root"
grep -q 'DEFAULT_PORT=51515' "$HOST" \
  && ok "default port is out of the commonly-used range" \
  || bad "default port is likely to collide"

# An exec carrying only redirections applies them to the whole shell. Having
# that swallow every later error is how a failed start once printed a URL.
# Skip comments: the warning about this pattern contains the pattern. The
# equivalent check for compose.yml made the same mistake first.
if grep -vE '^\s*#' "$HOST" | grep -qE 'exec [0-9]>&-\s+2>/dev/null'; then
  bad "an exec redirection is silencing stderr for the rest of the script"
else
  ok "no exec redirection that would silence later errors"
fi

if command -v docker >/dev/null 2>&1; then
  OUT=$(PB_DATA="$WORK/dies" PB_PORT=51599 PB_MAX_SIZE=not-a-number \
        "$HOST" quickstart tester 2>&1)
  if echo "$OUT" | grep -q "Ready. Send this"; then
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
  OUT=$(PB_DATA="$WORK/d1" PB_PORT=51598 "$HOST" quickstart tester 2>&1)
  if echo "$OUT" | grep -q "Ready. Send this"; then
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
  OUT=$(PB_DATA="$WORK/d2" PB_PORT=51597 "$HOST" quickstart tester 2>&1)
  if echo "$OUT" | grep -q "Ready. Send this"; then
    bad "printed a URL when the login could not be created"
  else
    ok "refuses when the login cannot be created"
  fi
  docker rm -f peerbackup-rest >/dev/null 2>&1 || true
else
  printf '  \033[33mSKIP\033[0m  quickstart failure path (needs docker)\n'
fi

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

hdr "systemd unit — the guard must actually be wired in"
UNIT="$(dirname "$0")/systemd/peerbackup-rest.service"
grep -q 'ExecStartPre=.*guard' "$UNIT" && ok "unit runs the guard as ExecStartPre" \
  || bad "unit does not run the guard — fail-closed is not wired in"
grep -q 'RequiresMountsFor' "$UNIT" && ok "unit declares a mount dependency" \
  || bad "unit missing RequiresMountsFor"

hdr "Summary"
printf '  passed: %d   failed: %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
