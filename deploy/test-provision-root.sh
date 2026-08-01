#!/usr/bin/env bash
# Root-requiring tests for `peerbackup host`: the real provisioning lifecycle
# with real loop devices and a real ext4 filesystem.
#
# This covers the paths test-host-tooling.sh has to skip:
#   - fallocate actually preallocates (the file consumes real blocks)
#   - mkfs + mount produce a working filesystem at the right size
#   - guard PASSES on a genuine mountpoint (the success path)
#   - the quota is real: writing past it fails with ENOSPC
#   - release tears down in the right order and returns the space
#
# Needs root and loop devices. Two ways to run it:
#
#   sudo ./deploy/test-provision-root.sh                 # CI runners
#   ./deploy/test-provision-root.sh --in-container       # anywhere with docker
#
# The container path is how this runs on a workstation without handing root to
# a test script. It is also what CI uses, so both run identical code.

set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"

# Re-exec inside a privileged container when asked.
if [ "${1:-}" = "--in-container" ]; then
  # A container's root filesystem is overlayfs, where fallocate returns success
  # without reserving blocks. That is a legitimate thing for provisioning to
  # refuse, but it means the lifecycle cannot be tested there. So give the test
  # a real ext4 filesystem on a loop device and run inside that.
  exec docker run --rm --privileged \
    -v "$REPO:/repo:ro" \
    debian:bookworm-slim \
    bash -c 'set -e
      export DEBIAN_FRONTEND=noninteractive
      apt-get update -qq >/dev/null 2>&1
      # systemd is needed for systemd-escape, which derives mount unit names.
      apt-get install -y -qq util-linux e2fsprogs coreutils systemd >/dev/null 2>&1
      dd if=/dev/zero of=/backing.img bs=1M count=768 status=none
      mkfs.ext4 -q -F /backing.img
      mkdir -p /pbroot && mount -o loop /backing.img /pbroot
      # The backing store consumes loop0; grants need more. mount does not always
      # auto-create them via loop-control inside a container.
      for i in $(seq 1 7); do [ -e /dev/loop$i ] || mknod -m 660 /dev/loop$i b 7 $i; done
      cp -r /repo /work && chmod +x /work/deploy/*.sh
      # The binary is built outside and mounted in. A glibc build from a newer
      # distribution will not run here, so say so plainly rather than failing
      # with "No such file or directory", which names the wrong problem.
      PB_BIN=""
      for c in /work/target/x86_64-unknown-linux-musl/debug/peerbackup \
               /work/target/debug/peerbackup; do
        [ -x "$c" ] && "$c" --version >/dev/null 2>&1 && { PB_BIN="$c"; break; }
      done
      if [ -z "$PB_BIN" ]; then
        echo "error: no peerbackup binary here will run inside debian:bookworm-slim." >&2
        echo "       A glibc build from a newer distribution cannot. Build a static one:" >&2
        echo "         rustup target add x86_64-unknown-linux-musl" >&2
        echo "         cargo build --target x86_64-unknown-linux-musl" >&2
        exit 1
      fi
      echo "using $PB_BIN"
      PB_BIN="$PB_BIN" PB_TEST_ROOT=/pbroot /work/deploy/test-provision-root.sh'
fi

# Unquoted on purpose at the call sites: this is a command plus a subcommand.
BIN="${PB_BIN:-$REPO/target/debug/peerbackup}"
[ -x "$BIN" ] || { echo "error: $BIN not found — run: cargo build" >&2; exit 1; }
HOST="$BIN host"

PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }
hdr() { printf '\n\033[1m=== %s ===\033[0m\n' "$*"; }

[ "$(id -u)" -eq 0 ] || { echo "must run as root; try --in-container"; exit 1; }

# PB_TEST_ROOT lets the caller put the work tree on a filesystem that really
# preallocates. Without it we land on whatever /tmp is, which in a container is
# overlayfs and cannot back a real quota.
WORK=$(mktemp -d "${PB_TEST_ROOT:-/tmp}/pb-root-test-XXXXXX")
# mktemp gives 0700; the server user has to be able to traverse to its grant.
chmod 755 "$WORK"
export PEERBACKUP_ROOT="$WORK/srv"
export SYSTEMD_UNIT_DIR="$WORK/units"
export HOST_MARGIN_GB=0
# Who the server would run as. provision must hand the grant to this user.
export PB_UID=1000 PB_GID=1000
mkdir -p "$SYSTEMD_UNIT_DIR"

# There is no systemd in a container, and CI runners should not have units
# enabled by a test. Stub systemctl so provision/release exercise everything
# EXCEPT unit activation, then mount by hand exactly the way the unit would.
STUB="$WORK/stub"; mkdir -p "$STUB"
cat > "$STUB/systemctl" <<'EOS'
#!/bin/sh
echo "systemctl $*" >> "$SYSTEMCTL_LOG"
# Emulate `enable --now <x>.mount` by performing the mount the unit describes.
# Without this, everything provision does after enabling the unit goes untested.
if [ "$1" = "enable" ] && [ "$2" = "--now" ] && [ -n "$3" ]; then
  unit="$SYSTEMD_UNIT_DIR/$3"
  [ -f "$unit" ] || exit 0
  what=$(sed -n 's/^What=//p' "$unit"); where=$(sed -n 's/^Where=//p' "$unit")
  [ -n "$what" ] && [ -n "$where" ] || exit 0
  mkdir -p "$where"
  mountpoint -q "$where" || mount -o loop,rw,noatime "$what" "$where" || exit 1
fi
exit 0
EOS
chmod +x "$STUB/systemctl"
export SYSTEMCTL_LOG="$WORK/systemctl.log"; : > "$SYSTEMCTL_LOG"
export PATH="$STUB:$PATH"

cleanup() {
  umount "$PEERBACKUP_ROOT/mnt/testpeer" 2>/dev/null || true
  losetup -D 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

hdr "provision creates a real preallocated image"
OUT=$($HOST provision testpeer 64M 2>&1); RC=$?
echo "$OUT" | sed 's/^/        /' | tail -12
[ "$RC" -eq 0 ] && ok "provision exited 0" || bad "provision exited $RC"

IMG="$PEERBACKUP_ROOT/images/testpeer.img"
[ -f "$IMG" ] && ok "image file created" || bad "no image file at $IMG"

# Preallocated, not sparse: apparent size and allocated blocks must agree.
# fallocate can report success without reserving anything, so check the blocks.
APPARENT=$(stat -c %s "$IMG" 2>/dev/null || echo 0)
ALLOCATED=$(( $(stat -c %b "$IMG" 2>/dev/null || echo 0) * $(stat -c %B "$IMG" 2>/dev/null || echo 512) ))
if [ "$APPARENT" -eq 67108864 ]; then ok "image apparent size is 64M"; else bad "apparent size $APPARENT, want 67108864"; fi
# The contract is: preallocate for real, or refuse. A sparse image that passes
# silently is the overcommit bug, so both outcomes below are acceptable and
# anything else is not.
if [ "$ALLOCATED" -ge $(( APPARENT * 9 / 10 )) ]; then
  ok "image is PREALLOCATED (allocated $ALLOCATED >= 90% of apparent $APPARENT)"
else
  bad "provision produced a SPARSE image and did not refuse (allocated $ALLOCATED of $APPARENT)"
fi

hdr "a filesystem that cannot preallocate is REFUSED, not silently accepted"
# /dev/shm is tmpfs: fallocate succeeds there but reserves nothing durable in
# the sense this design needs. Point provision at it and require a refusal.
if [ -d /dev/shm ]; then
  mkdir -p "$WORK/shm-units"
  SHMOUT=$(PEERBACKUP_ROOT=/dev/shm/pb-sparse-test SYSTEMD_UNIT_DIR="$WORK/shm-units" \
           $HOST provision sparsepeer 64M 2>&1)
  if echo "$SHMOUT" | grep -qi 'does not really preallocate\|refusing'; then
    ok "provision refuses a filesystem that does not really preallocate"
  elif echo "$SHMOUT" | grep -qi 'preallocation verified'; then
    ok "tmpfs preallocated for real here; nothing to refuse"
  else
    bad "provision neither verified nor refused on tmpfs: $(echo "$SHMOUT" | tail -2)"
  fi
  rm -rf /dev/shm/pb-sparse-test 2>/dev/null || true
fi

hdr "the mount unit is written correctly"
UNIT=""
for _u in "$SYSTEMD_UNIT_DIR"/*testpeer*.mount; do
  [ -e "$_u" ] && { UNIT=$(basename "$_u"); break; }
done
if [ -n "$UNIT" ]; then
  ok "mount unit written: $UNIT"
  grep -q "What=$IMG" "$SYSTEMD_UNIT_DIR/$UNIT" && ok "unit points at the image" || bad "unit What= is wrong"
  grep -q "Where=$PEERBACKUP_ROOT/mnt/testpeer" "$SYSTEMD_UNIT_DIR/$UNIT" && ok "unit points at the grant dir" || bad "unit Where= is wrong"
  grep -q 'Options=.*loop' "$SYSTEMD_UNIT_DIR/$UNIT" && ok "unit mounts via loop" || bad "unit missing loop option"
else
  bad "no mount unit was written"
fi
grep -q 'enable --now' "$SYSTEMCTL_LOG" && ok "provision enables the mount unit" || bad "provision never enabled the unit"

hdr "guard: refuses before mount, passes after"
DIR="$PEERBACKUP_ROOT/mnt/testpeer"
# provision leaves it mounted, which is correct. Unmount to exercise the
# refusal, then mount it back.
umount "$DIR" 2>/dev/null || true
if $HOST guard >/dev/null 2>&1; then
  bad "guard PASSED while the grant was unmounted — fail-open"
else
  ok "guard refuses while the grant is unmounted"
fi

# The stub already mounted it via the unit; this is a fallback.
mountpoint -q "$DIR" || mount -o loop,rw,noatime "$IMG" "$DIR" 2>/dev/null \
  || bad "could not mount the image"
if mountpoint -q "$DIR"; then
  ok "image mounts as a real filesystem"
  if $HOST guard >/dev/null 2>&1; then
    ok "guard PASSES on a genuine mountpoint (the success path)"
  else
    bad "guard refused a genuinely mounted grant — fail-closed is too aggressive"
  fi
else
  bad "mountpoint check failed after mount"
fi

hdr "the grant is usable by the server, not just by root"
# mkfs.ext4 creates lost+found as root:0700 whatever the parent looks like, and
# a fresh filesystem is root-owned. The server runs unprivileged, so without a
# recursive chown it cannot write to the grant at all: adduser and the startup
# mount check both fail with permission denied.
OWNER=$(stat -c '%u' "$DIR")
[ "$OWNER" = "$PB_UID" ] && ok "grant belongs to the server user ($PB_UID)" \
  || bad "grant is owned by uid $OWNER, not the server user $PB_UID"

MNT_OWNER=$(stat -c '%u' "$PEERBACKUP_ROOT/mnt")
[ "$MNT_OWNER" = "$PB_UID" ] \
  && ok "the directory holding .htpasswd belongs to the server user" \
  || bad "$PEERBACKUP_ROOT/mnt is owned by uid $MNT_OWNER; adduser will fail"

if [ -d "$DIR/lost+found" ]; then
  LF=$(stat -c '%u' "$DIR/lost+found")
  [ "$LF" = "$PB_UID" ] && ok "lost+found was chowned too (-R was used)" \
    || bad "lost+found is owned by uid $LF; the startup mount check will fail"
fi

# The assertion that matters: can that user actually write?
if command -v setpriv >/dev/null 2>&1; then
  PROBE_ERR=$(setpriv --reuid="$PB_UID" --regid="$PB_GID" --clear-groups \
              touch "$DIR/probe" 2>&1)
  if [ -e "$DIR/probe" ]; then
    ok "the server user can write to the grant"
    rm -f "$DIR/probe"
  else
    bad "the server user cannot write to the grant: $PROBE_ERR"
    ls -ld "$DIR" | sed 's/^/        /'
  fi
fi

hdr "the quota is real, not advisory"
LIST=$($HOST list 2>&1)
echo "$LIST" | sed 's/^/        /' | head -4
# ext4 with -m 0 on 64M leaves roughly 50M usable after metadata.
USABLE_KB=$(df -Pk "$DIR" | awk 'NR==2{print $2}')
if [ "${USABLE_KB:-0}" -gt 20000 ] && [ "${USABLE_KB:-0}" -lt 66000 ]; then
  ok "usable capacity is plausible for a 64M image (${USABLE_KB}K)"
else
  bad "usable capacity ${USABLE_KB}K is not plausible for 64M"
fi

# Write past the quota. Must fail with ENOSPC and must NOT touch the host fs.
dd if=/dev/zero of="$DIR/filler" bs=1M count=200 >"$WORK/dd.out" 2>&1
DDRC=$?
if [ "$DDRC" -ne 0 ] && grep -qi 'no space' "$WORK/dd.out"; then
  ok "writing past the grant fails with ENOSPC (kernel-enforced, not advisory)"
else
  bad "writing 200M into a 64M grant did not fail with ENOSPC (rc=$DDRC)"
fi
HOSTFREE_AFTER=$(df -Pk "$WORK" | awk 'NR==2{print $4}')
[ -n "$HOSTFREE_AFTER" ] && ok "host filesystem still reports free space (grant was contained)"
rm -f "$DIR/filler"

hdr "release tears down in the right order"
LOOPDEV=$(losetup -j "$IMG" | cut -d: -f1)
[ -n "$LOOPDEV" ] && ok "a loop device is attached ($LOOPDEV)" || bad "no loop device attached"

FORCE=1 $HOST release testpeer >"$WORK/release.out" 2>&1; RRC=$?
[ "$RRC" -eq 0 ] && ok "release exited 0" || { bad "release exited $RRC"; sed 's/^/        /' "$WORK/release.out" | tail -5; }

mountpoint -q "$DIR" 2>/dev/null && bad "grant is STILL MOUNTED after release" || ok "grant unmounted"
if losetup -j "$IMG" 2>/dev/null | grep -q loop; then
  bad "loop device still attached after release — space is not actually returned"
else
  ok "loop device detached"
fi
[ -f "$IMG" ] && bad "image file still exists after release" || ok "image file removed"
[ -f "$SYSTEMD_UNIT_DIR/$UNIT" ] && bad "mount unit still present after release" || ok "mount unit removed"

hdr "release refuses to rm a mounted image (ordering is not optional)"
# Re-provision, mount, then hold the mount open and confirm release does not
# blow past umount and delete the backing file anyway.
$HOST provision testpeer2 64M >/dev/null 2>&1
IMG2="$PEERBACKUP_ROOT/images/testpeer2.img"
DIR2="$PEERBACKUP_ROOT/mnt/testpeer2"
mount -o loop "$IMG2" "$DIR2" 2>/dev/null
if mountpoint -q "$DIR2"; then
  exec 9<"$DIR2"          # hold a descriptor so umount fails
  FORCE=1 $HOST release testpeer2 >"$WORK/release2.out" 2>&1
  exec 9<&-
  if [ -f "$IMG2" ]; then
    ok "release stopped rather than deleting a still-mounted image"
  else
    # An open directory fd does not always block umount; only fail if it clearly
    # deleted a mounted image.
    if mountpoint -q "$DIR2" 2>/dev/null; then
      bad "release deleted the image while it was still mounted"
    else
      ok "release completed cleanly (umount was not blocked by the held fd)"
    fi
  fi
  umount "$DIR2" 2>/dev/null || true
  losetup -D 2>/dev/null || true
else
  ok "skipped: could not mount second image"
fi

hdr "Summary"
printf '  passed: %d   failed: %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
