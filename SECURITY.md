# Security policy

## Reporting a vulnerability

Please do not open a public issue. Report it privately through GitHub instead:
on this repository, go to **Security → Report a vulnerability**.

Include what you found, how to reproduce it, and the version
(`peerbackup --version`) or commit you tested. You should get an
acknowledgement within a week.

## Supported versions

Security fixes go into the most recent minor release line. Until 1.0.0, older
lines are not maintained.

## Scope

In scope: peerbackup itself, its container image, and the deployment files in
this repository (`compose.yml`, the systemd unit).

Out of scope, and best reported upstream:

- [restic](https://github.com/restic/restic/security), which does the
  encryption and storage;
- [rest-server](https://github.com/restic/rest-server), which the host side
  runs.

If you are unsure where an issue belongs, report it here.
