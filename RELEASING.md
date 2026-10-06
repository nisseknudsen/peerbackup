# Releasing

How peerbackup versions, branches and publishes releases. Written for
maintainers; users only need the [README](README.md#installation).

## Versioning

[Semantic Versioning](https://semver.org/). Tags are `vMAJOR.MINOR.PATCH`,
optionally with a prerelease suffix (`v0.2.0-rc.1`). Before 1.0.0, a minor
release may break compatibility and a patch release may not.

The version in `Cargo.toml` is the source of truth. The release workflow
refuses a tag that does not match it.

## Branches

| Branch | Purpose |
|---|---|
| `main` | Development. Every change lands here first, through a pull request. |
| `release/vX.Y` | One per minor version, cut from `main` when `X.Y.0` is released. Receives backported fixes only, and is where every `X.Y.Z` tag is made. |

The release workflow enforces this: a tag must point at a commit on its own
`release/vX.Y` branch, or nothing is published. CI runs on pushes to `main` and
to every `release/*` branch.

## What a release publishes

Pushing a tag runs [`.github/workflows/release.yml`](.github/workflows/release.yml).
It validates the tag, runs the full CI suite, and only then:

- builds static binaries for `linux/amd64` and `linux/arm64` and packages them as
  `peerbackup-X.Y.Z-linux-<arch>.tar.gz`, with a `SHA256SUMS` file;
- pushes a multi-arch image to `ghcr.io/nisseknudsen/peerbackup`;
- checks that the binary inside each platform's image is byte-identical to the
  released one, that it runs, and that the bundled restic matches its
  architecture;
- records build provenance attestations for the tarballs and the image (public
  repositories only);
- creates the GitHub release, with that version's `CHANGELOG.md` section as its
  notes.

Image tags:

| Release | Tags |
|---|---|
| `v0.2.0`, newest overall | `0.2.0`, `0.2`, `latest` |
| `v0.1.4`, after `0.2.0` exists | `0.1.4`, `0.1` |
| `v1.3.0`, newest 1.x | `1.3.0`, `1.3`, `1`, plus `latest` if newest overall |
| `v0.3.0-rc.1` | `0.3.0-rc.1` only |

Floating tags only move forward: patching an older line never moves `latest`.
There is no bare `0` tag, because under semver 0.1 → 0.2 may break.

## One-time setup

Before the first release:

1. **Rehearse.** Actions → Release → Run workflow, on `main`. This builds and
   verifies both platforms exactly as a release would and publishes nothing.
2. **Make the repository public** before tagging, if it is going to be. Build
   provenance attestations are skipped while it is private (GitHub only offers
   them on private repositories under Enterprise Cloud), and the workflow warns
   when it skips them.
3. **Enable private vulnerability reporting**: Settings → Code security →
   Private vulnerability reporting. [SECURITY.md](SECURITY.md) points people
   there.
4. **Protect release refs** (recommended): add rulesets so that only
   maintainers can create `release/*` branches and `v*` tags, and so that
   `release/*` cannot be force-pushed or deleted.

After the first release:

5. **Check the image is public.** Open the package page (Your profile →
   Packages → peerbackup). If its visibility is private, change it to public
   under Package settings. Until then, `docker pull` fails for everyone else.

## Releasing X.Y.0

From an up-to-date `main`, with CI green:

1. In a pull request to `main`, prepare the release:
   - set `version = "X.Y.0"` in `Cargo.toml` and run `cargo check` so
     `Cargo.lock` follows;
   - in `CHANGELOG.md`, rename `## [Unreleased]` to `## [X.Y.0] - YYYY-MM-DD`
     (today's date), and add a new empty `## [Unreleased]` above it.

   Merge it.

2. Cut the release branch and tag it:

   ```sh
   git switch main && git pull
   git switch -c release/vX.Y
   git push -u origin release/vX.Y
   git tag -a vX.Y.0 -m "peerbackup X.Y.0"
   git push origin vX.Y.0
   ```

3. Watch the Release workflow. It takes about as long as CI plus a few minutes.

4. In a pull request to `main`, start the next version: set `Cargo.toml` to
   `X.(Y+1).0`.

## Releasing a patch, X.Y.Z

1. Land the fix on `main` first, through a normal pull request.

2. Backport it to the release branch, bump the version, and date the changelog:

   ```sh
   git switch release/vX.Y && git pull
   git cherry-pick -x <commit-on-main>
   # Cargo.toml: version = "X.Y.Z"; then `cargo check`
   # CHANGELOG.md: add "## [X.Y.Z] - YYYY-MM-DD" with the fix
   git commit -am "Release X.Y.Z"
   git push
   ```

   Wait for CI on the release branch.

3. Tag and push:

   ```sh
   git tag -a vX.Y.Z -m "peerbackup X.Y.Z"
   git push origin vX.Y.Z
   ```

4. Copy the `X.Y.Z` changelog entry to `main` too, so its history is complete.

## Prereleases

Tag `vX.Y.0-rc.N` on `release/vX.Y`, with `Cargo.toml` set to `X.Y.0-rc.N` and
a dated `## [X.Y.0-rc.N]` changelog entry. It is published as a GitHub
prerelease, and the image gets only its exact tag.

## If a release fails

Nothing is visible until the last two steps. The image is pushed untagged and
only tagged after it has been checked, and the GitHub release stays a draft
until then. So:

- **Failed before "tag the image":** nothing anyone can see has changed. Delete
  the draft release if one was created, fix the problem on the release branch,
  then move the tag (it has not been published, so this is safe):

  ```sh
  git tag -d vX.Y.Z && git push origin :refs/tags/vX.Y.Z
  # fix, commit, push the release branch, then tag again
  ```

- **Failed after the image was tagged:** the release is out. Do not move or
  reuse the tag. Fix forward with the next patch version.

Untagged image versions left behind by failed attempts can be deleted from the
package page.
