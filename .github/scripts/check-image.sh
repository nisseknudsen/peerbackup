#!/usr/bin/env bash
# Check that a built image is what the release says it is, per platform.
#
#   check-image.sh VERSION BINDIR PLATFORM=REF [PLATFORM=REF ...]
#
#   VERSION   the version `peerbackup --version` must report
#   BINDIR    the `--target binary` export: BINDIR/linux_<arch>/peerbackup
#   REF       the image to check for that platform, e.g.
#               linux/arm64=peerbackup:ci-arm64           (loaded locally)
#               linux/arm64=ghcr.io/o/peerbackup@sha256:… (pushed; set PULL=1)
#
# For each platform:
#   1. the peerbackup inside the image is byte-identical to the exported one,
#      read straight out of the image without running anything;
#   2. it runs, and reports VERSION;
#   3. the restic beside it runs and was built for the same architecture.
#
# (3) is the one that matters most for arm64. A mismatched restic installs
# cleanly, passes its checksum, and fails with `exec format error` at the first
# backup -- on the user's machine, not here.
#
# Running the arm64 image needs QEMU registered with binfmt on the host.
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }
[ $# -ge 3 ] || die "usage: check-image.sh VERSION BINDIR PLATFORM=REF..."
version=$1 bindir=$2; shift 2

fail=0
for pair in "$@"; do
  platform=${pair%%=*} ref=${pair#*=}
  arch=${platform#linux/}
  [ "$platform" != "$pair" ] && [ -n "$ref" ] || die "expected PLATFORM=REF, got '$pair'"
  want_bin="$bindir/linux_$arch/peerbackup"
  [ -f "$want_bin" ] || die "no exported binary at $want_bin"

  echo "--- $platform: $ref"
  if [ "${PULL:-0}" = 1 ]; then
    docker pull -q --platform "$platform" "$ref" >/dev/null
  fi

  cid=$(docker create --platform "$platform" "$ref")
  got=$(docker cp "$cid:/usr/local/bin/peerbackup" - | tar -xO | sha256sum | cut -d' ' -f1)
  docker rm "$cid" >/dev/null
  want=$(sha256sum "$want_bin" | cut -d' ' -f1)
  if [ "$got" = "$want" ]; then
    echo "  ok  binary is byte-identical to the release binary ($want)"
  else
    echo "  FAIL binary in image is $got, release binary is $want"; fail=1
  fi

  reported=$(docker run --rm --platform "$platform" "$ref" --version)
  if [ "$reported" = "peerbackup $version" ]; then
    echo "  ok  runs, and reports '$reported'"
  else
    echo "  FAIL reports '$reported', expected 'peerbackup $version'"; fail=1
  fi

  restic=$(docker run --rm --platform "$platform" --entrypoint restic "$ref" version)
  if grep -q "on linux/$arch\$" <<<"$restic"; then
    echo "  ok  restic matches: $restic"
  else
    echo "  FAIL restic is not built for $arch: $restic"; fail=1
  fi
done

[ "$fail" -eq 0 ] || die "image check failed"
echo "all platforms ok"
