#!/usr/bin/env bash
# Black-box tests for release.sh. Run: .github/scripts/release.test.sh
#
# The cases that matter most are the floating tags. Getting `latest` wrong is
# silent: the release succeeds, and everyone pulling `latest` quietly gets an
# older line than the one they think they are on.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
R="$HERE/release.sh"

PASS=0; FAIL=0
ok()  { printf '  \033[32mPASS\033[0m  %s\n' "$*"; PASS=$((PASS+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL+1)); }

# expect_tags TAG "EXISTING TAGS" "WANT image_tags" WANT_LATEST
expect_tags() {
  local tag=$1 existing=$2 want=$3 want_latest=$4 out got latest
  out=$(tr ' ' '\n' <<<"$existing" | "$R" meta "$tag" "${tag#v}" 2>&1) \
    || { bad "$tag with [$existing]: refused: $out"; return; }
  got=$(sed -n 's/^image_tags=//p' <<<"$out")
  latest=$(sed -n 's/^latest=//p' <<<"$out")
  if [ "$got" = "$want" ] && [ "$latest" = "$want_latest" ]; then
    ok "$tag with [${existing:-none}] -> $got"
  else
    bad "$tag with [${existing:-none}]: got '$got' latest=$latest, want '$want' latest=$want_latest"
  fi
}

expect_refused() { # TAG CARGO_VERSION WHY
  if out=$("$R" meta "$1" "$2" </dev/null 2>&1); then
    bad "$3: '$1' was accepted"
  else
    ok "$3: refused '$1' ($(head -c 60 <<<"$out")...)"
  fi
}

echo "=== floating tags ==="
expect_tags v0.1.0 ""                      "0.1.0 0.1 latest"      true
expect_tags v0.1.1 "v0.1.0"                "0.1.1 0.1 latest"      true
# Patching an older line after a newer one exists: 0.1 moves, latest does not.
expect_tags v0.1.2 "v0.1.0 v0.1.1 v0.2.0"  "0.1.2 0.1"             false
# 0.x never gets a bare `0`: 0.1 -> 0.2 may break anything.
expect_tags v0.2.0 "v0.1.0"                "0.2.0 0.2 latest"      true
# From 1.0 the major tag floats too, and also only forwards.
expect_tags v1.0.0 "v0.9.0"                "1.0.0 1.0 1 latest"    true
expect_tags v1.2.4 "v1.2.3 v1.3.0"         "1.2.4 1.2"             false
expect_tags v1.3.1 "v1.2.4 v1.3.0 v2.0.0"  "1.3.1 1.3 1"           false
# Numeric, not lexical: 0.10 is newer than 0.9.
expect_tags v0.10.0 "v0.9.0 v0.9.1"        "0.10.0 0.10 latest"    true
expect_tags v0.9.2 "v0.9.1 v0.10.0"        "0.9.2 0.9"             false
# Prereleases never take a floating tag, and never block one.
expect_tags v0.2.0-rc.1 "v0.1.0"           "0.2.0-rc.1"            false
expect_tags v0.2.0 "v0.1.0 v0.2.0-rc.1 v0.2.0-rc.2" "0.2.0 0.2 latest" true
# Tags that are not versions are ignored rather than breaking the sort.
expect_tags v0.1.0 "not-a-version vfoo"    "0.1.0 0.1 latest"      true

echo
echo "=== refusals ==="
expect_refused v0.1.0 0.2.0 "tag and Cargo.toml disagree"
expect_refused 0.1.0 0.1.0 "missing the leading v"
expect_refused v0.1 0.1 "not three components"
expect_refused v01.2.3 01.2.3 "leading zero"
expect_refused v1.2.3+build.5 1.2.3+build.5 "build metadata"
expect_refused v1.2.3- 1.2.3- "empty prerelease"
expect_refused "" "" "empty tag"

echo
echo "=== other outputs ==="
out=$("$R" meta v0.3.0-beta.2 0.3.0-beta.2 </dev/null)
grep -qx 'branch=release/v0.3' <<<"$out" && ok "release branch is release/vMAJOR.MINOR" || bad "branch: $out"
grep -qx 'prerelease=true' <<<"$out" && ok "a -beta tag is a prerelease" || bad "prerelease: $out"
grep -qx 'version=0.3.0-beta.2' <<<"$out" && ok "version drops the v" || bad "version: $out"

echo
echo "=== changelog notes ==="
CHANGELOG='# Changelog

## [Unreleased]

- not yet

## [0.2.0] - 2026-11-01

### Added

- the thing

## [0.1.0] - 2026-10-06

- first

[0.2.0]: https://example.invalid/v0.2.0'
got=$("$R" notes 0.2.0 <<<"$CHANGELOG")
want='### Added

- the thing'
[ "$got" = "$want" ] && ok "a section stops at the next heading and is trimmed" \
  || bad "section: got [$got]"
got=$("$R" notes 0.1.0 <<<"$CHANGELOG")
[ "$got" = "- first" ] && ok "link references at the bottom stay out of the oldest section" \
  || bad "last section: got [$got]"
if "$R" notes 9.9.9 <<<"$CHANGELOG" >/dev/null 2>&1; then
  bad "a version with no changelog entry was accepted"
else
  ok "a version with no changelog entry is refused"
fi
# 0.1.0 must not match a heading for 0.1.0-rc.1 or 10.1.0.
got=$("$R" notes 0.1.0 <<<'## [0.1.0-rc.1] - 2026-09-01

- rc

## [10.1.0] - 2026-09-02

- ten

## [0.1.0] - 2026-09-03

- real')
[ "$got" = "- real" ] && ok "0.1.0 matches neither 0.1.0-rc.1 nor 10.1.0" || bad "prefix match: got [$got]"

# A release must not ship saying it has not happened yet.
for heading in "## [0.3.0] - Unreleased" "## [0.3.0]" "## [0.3.0] - 2026-9-1" "## [0.3.0] - TBD"; do
  if out=$("$R" notes 0.3.0 <<<"$heading

- stuff" 2>&1); then
    bad "undated heading accepted: '$heading'"
  else
    grep -q "no release date" <<<"$out" && ok "undated heading refused: '$heading'" \
      || bad "'$heading' refused for the wrong reason: $out"
  fi
done

printf '\n  passed: %d   failed: %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
