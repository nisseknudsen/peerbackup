# Changelog

All notable changes to peerbackup are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0.0, a new
minor version (0.x.0) may include breaking changes; patch versions never do.

## [Unreleased]

### Security

- Evidence records identify a repository by its URL without the login
  password. A password rotation on the host (`host adduser --force`) no longer
  orphans the client's history, so a standing failed verdict stays visible and
  `verify` keeps the baseline it uses to notice a dropped snapshot. Records
  written under the old identity are still matched.

## [0.1.0] - 2026-10-08

First public release.

### Added

- Back up one or more directories to several peers. Each peer holds a separate
  restic repository with its own encryption key. `connect` sets up a peer in one
  step and checks the connection by uploading a test file and reading it back.
- `verify` reads back a configurable share of the stored data, restores a test
  file and compares its digest, and checks that the peer still lists the most
  recent snapshot. It exits 1 on damage or error, and 2 when no peer could be
  checked at all.
- `status` summarises every peer from locally recorded results, without
  contacting them, and exits non-zero unless every peer is `ok`.
- `restore` (latest backup, or a chosen snapshot) and `snapshots`.
- `recovery export` writes repository addresses, passwords and plain restic
  commands to a file, so backups can be restored without peerbackup. Commands
  that change peers warn when that file is out of date.
- Hosting for a friend: `host quickstart` runs rest-server in Docker without
  root and prints an invite; `host adduser` adds logins.
- Per-peer storage limits enforced by the filesystem: `host provision` and
  `host release` manage a preallocated ext4 image per peer (needs root).
  `host list`, `host guard` and `host doctor` report on them.
- Static binaries for Linux on amd64 and arm64.
- Container image `ghcr.io/nisseknudsen/peerbackup` for linux/amd64 and
  linux/arm64, with restic 0.19.1 included.
