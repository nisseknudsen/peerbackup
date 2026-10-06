#!/usr/bin/env bash
# Release helpers, kept out of the workflow YAML so they can be tested.
#
#   release.sh meta TAG CARGO_VERSION < existing-tags
#       Validate a release tag and print what the release workflow needs, as
#       key=value lines for $GITHUB_OUTPUT. Existing tags arrive on stdin, one
#       per line, as `git tag -l` prints them.
#
#   release.sh notes VERSION < CHANGELOG.md
#       Print that version's section of the changelog, which becomes the body
#       of the GitHub release.
#
# Both are pure: arguments and stdin in, stdout out. The workflow does the git
# and network parts, and release.test.sh drives this as a black box.
set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

# Semver 2.0.0 with the leading `v` this project tags with. No build metadata
# (`+...`): it carries no meaning for an image and Docker forbids `+` in tags.
SEMVER='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'

# The highest stable MAJOR.MINOR.PATCH among the lines on stdin. Prereleases are
# ignored: they never own a floating tag.
highest() {
  grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' | sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1
}

meta() {
  local tag=$1 cargo=$2
  [[ $tag =~ $SEMVER ]] \
    || die "'$tag' is not a release tag: expected vMAJOR.MINOR.PATCH, optionally with a prerelease suffix (v0.2.0, v0.2.0-rc.1)"
  local major=${BASH_REMATCH[1]} minor=${BASH_REMATCH[2]}
  local version=${tag#v}
  [ "$version" = "$cargo" ] \
    || die "tag $tag does not match the version in Cargo.toml ($cargo). Bump Cargo.toml on the release branch, then tag that commit."

  local prerelease=false
  [[ $version == *-* ]] && prerelease=true

  # Every version already tagged, plus this one, which may not be pushed yet.
  local known
  known=$( { sed -n 's/^v//p'; echo "$version"; } | sort -u)

  # Floating tags only ever move forward. Releasing 0.1.4 after 0.2.0 exists
  # must not drag `latest` back to the old line, and a prerelease never takes a
  # floating tag at all -- someone pulling `0.2` asked for a release.
  local tags=$version latest=false
  if [ "$prerelease" = false ]; then
    if [ "$version" = "$(grep -E "^$major\.$minor\." <<<"$known" | highest)" ]; then
      tags="$tags $major.$minor"
    fi
    # `0` would float across 0.x minors, which semver says may break anything.
    if [ "$major" != 0 ] && [ "$version" = "$(grep -E "^$major\." <<<"$known" | highest)" ]; then
      tags="$tags $major"
    fi
    if [ "$version" = "$(highest <<<"$known")" ]; then
      tags="$tags latest"
      latest=true
    fi
  fi

  echo "version=$version"
  echo "branch=release/v$major.$minor"
  echo "prerelease=$prerelease"
  echo "latest=$latest"
  echo "image_tags=$tags"
}

notes() {
  local version=$1 body changelog heading
  changelog=$(cat)
  # The heading must carry the release date. `## [0.1.0] - Unreleased` is what
  # the entry looks like while it is being written; shipping it would publish a
  # release that says it has not happened.
  heading=$(awk -v h="## [$version]" 'index($0, h) == 1 { print; exit }' <<<"$changelog")
  if [ -n "$heading" ] && ! [[ $heading =~ ^"## [$version] - "[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]; then
    die "the CHANGELOG.md heading for $version has no release date: '$heading'. Expected '## [$version] - YYYY-MM-DD'."
  fi

  # The section runs from `## [VERSION]` to the next `## [`. Link reference
  # definitions (`[0.1.0]: https://...`) are dropped: Keep a Changelog puts them
  # at the bottom of the file, so they would otherwise land in the oldest
  # release's notes. Leading and trailing blank lines go too.
  body=$(awk -v head="## [$version]" '
    index($0, "## [") == 1 { if (found) exit; if (index($0, head) == 1) { found = 1; next } }
    found && /^\[[^]]+\]: / { next }
    found { lines[++n] = $0 }
    END {
      first = 1; while (first <= n && lines[first] ~ /^[[:space:]]*$/) first++
      last = n;  while (last >= first && lines[last] ~ /^[[:space:]]*$/) last--
      for (i = first; i <= last; i++) print lines[i]
    }' <<<"$changelog")
  [ -n "$body" ] \
    || die "CHANGELOG.md has no entry for $version. Add a '## [$version]' section on the release branch before tagging."
  printf '%s\n' "$body"
}

case "${1:-}" in
  meta)
    shift
    [ $# -eq 2 ] || die "usage: release.sh meta TAG CARGO_VERSION < existing-tags"
    meta "$@"
    ;;
  notes)
    shift
    [ $# -eq 1 ] || die "usage: release.sh notes VERSION < CHANGELOG.md"
    notes "$@"
    ;;
  *)
    die "usage: release.sh {meta TAG CARGO_VERSION | notes VERSION}"
    ;;
esac
