# Code review, 2026-09-08

Full review of the tree at `12b5e02` for correctness, security and UX.

Five reviewers worked in parallel over separate areas, and every finding here was
traced to a line of source. Findings marked **(reproduced)** were additionally
demonstrated against the built binary or a real subprocess; the transcript of each
is in the finding.

| File | Area |
|---|---|
| [`00-summary.md`](00-summary.md) | Severity-ordered index, and what to fix first |
| [`01-status-and-config.md`](01-status-and-config.md) | `cli.rs`, `config.rs`, the status verdict |
| [`02-state-and-evidence.md`](02-state-and-evidence.md) | `state.rs`: evidence log, canary, staleness |
| [`03-engine.md`](03-engine.md) | `engine/*`: driving restic, error classification |
| [`04-host.md`](04-host.md) | `host/*`: grants, loop devices, the server |
| [`05-deploy-and-docs.md`](05-deploy-and-docs.md) | Dockerfile, compose, systemd, CI, README |
| [`STATUS.md`](STATUS.md) | Which pull request fixed each finding |

Report only. No behaviour was changed.

**Totals:** 114 numbered findings across the five areas, plus a nit list in each.
Two are critical and twenty-three reach the high band of the severity index.
