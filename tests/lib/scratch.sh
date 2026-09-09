#!/usr/bin/env bash
# shellcheck shell=bash
# Guard for the scratch directories the test suites wipe.
#
# Every suite here takes its work directory from the environment or from $1, and
# then `rm -rf`s it. `WORK=$HOME ./tests/docker.sh` deleted a home directory --
# through a root container in that suite's case, so file permissions did not even
# slow it down -- and `sudo ./spike/lifecycle-spike.sh /` was `rm -rf /`. CI runs
# the spike under sudo.
#
# Source this, then call `scratch_claim "$WORK"` before the first `rm -rf`.
#
# Two rules, and the second is the one that matters:
#
#   1. The path must be absolute, at least two components deep, and not a
#      directory anything else lives in ($HOME, the repo, /tmp itself, and the
#      obvious system roots).
#
#   2. If it already exists, it must carry the marker this function writes. So a
#      directory this suite did not create is never deleted, whatever its name.
#      A fresh path is claimed and marked; a stale one from a previous run is
#      recognised and reused.
#
# The wipe removes the marker along with everything else, so call `scratch_mark`
# after it rather than `scratch_claim` again -- re-claiming a directory the
# script has just recreated would see it unmarked and refuse.

scratch_marker=.peerbackup-scratch

scratch_claim() {
  local dir="${1:-}" home repo
  home="${HOME:-/root}"
  repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

  [ -n "$dir" ] || { echo "scratch_claim: no directory given" >&2; return 1; }
  case "$dir" in
    /*) ;;
    *) echo "scratch_claim: '$dir' must be an absolute path" >&2; return 1 ;;
  esac

  # Normalise away `..`, a trailing slash and a doubled slash before comparing.
  dir="$(cd "$(dirname "$dir")" 2>/dev/null && pwd)/$(basename "$dir")" || {
    echo "scratch_claim: '$1' has no existing parent" >&2; return 1
  }

  case "$dir" in
    /|/tmp|/var|/var/tmp|/usr|/etc|/home|/root|/srv|/opt|/dev|/dev/shm)
      echo "scratch_claim: refusing to use '$dir' as a scratch directory" >&2; return 1 ;;
  esac
  # Depth: /a is one component, /a/b is two.
  case "${dir#/}" in
    */*) ;;
    *) echo "scratch_claim: '$dir' is too close to the root to wipe" >&2; return 1 ;;
  esac
  if [ "$dir" = "$home" ] || [ "$dir" = "$repo" ]; then
    echo "scratch_claim: refusing to wipe '$dir'" >&2; return 1
  fi
  # An ancestor of either is worse than either.
  case "$home/" in "$dir"/*) echo "scratch_claim: '$dir' contains \$HOME" >&2; return 1 ;; esac
  case "$repo/" in "$dir"/*) echo "scratch_claim: '$dir' contains the repository" >&2; return 1 ;; esac

  if [ -e "$dir" ] && [ ! -e "$dir/$scratch_marker" ]; then
    echo "scratch_claim: '$dir' exists and was not created by this test suite." >&2
    echo "  Refusing to delete it. Remove it yourself, or point WORK somewhere else." >&2
    return 1
  fi
  scratch_mark "$dir"
}

# Re-mark a directory this suite has already claimed and then wiped.
scratch_mark() {
  mkdir -p "$1" || return 1
  : > "$1/$scratch_marker" || return 1
}
