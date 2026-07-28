//! Turning restic output into a verdict.
//!
//! Two jobs: strip the Go error-location trace (not a panic, but it reads like
//! one), and decide whether a failure is evidence the *data* is damaged or only
//! that we could not look at it. Calling a network blip corruption trains the
//! user to ignore red, which destroys the only feature this product has.
//!
//! Fixtures below are real restic 0.19.1 output, not made up.

use super::outcome::{Cause, Corruption};

/// Remove restic's Go error-location trace from a message.
///
/// restic emits, after the real error:
/// ```text
/// main.init
///     /restic/cmd/restic/cmd_forget.go:67
/// runtime.doInit1
///     /usr/local/go/src/runtime/proc.go:8103
/// ```
/// Users read that as a crash. The real message is the line *above* it, which is
/// also why you must never pipe restic through `tail` to capture an error.
pub fn strip_go_trace(raw: &str) -> String {
    raw.lines()
        .filter(|line| {
            let t = line.trim_start();
            // Frame labels: `main.init`, `runtime.goexit`, `github.com/...`
            let is_frame =
                t.starts_with("runtime.") || t.starts_with("main.") || t.starts_with("github.com/");
            // Source locations are indented and start with a path.
            let is_location = line.starts_with(char::is_whitespace)
                && (t.starts_with('/') || t.contains(".go:") || t.contains(".s:"));
            !(is_frame || is_location)
        })
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// What restic told us, reduced to a verdict about the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// Positive evidence the data is damaged.
    Damage(Corruption),
    /// We learned nothing about the data.
    NoVerdict(Cause),
}

/// Classify a failed restic invocation.
///
/// `code` is the process exit status; `output` is stdout+stderr combined.
///
/// Observed exit codes (measured, not documented):
///   * `3` — prune blocked by `--append-only`, alongside HTTP 403
///   * `1` — generic failure, including HTTP 507 when the peer is full
///
/// The default for anything unrecognised is deliberately `NoVerdict`, not
/// `Damage`. Claiming corruption we cannot demonstrate is the worse mistake:
/// a false red is as corrosive to trust as a false green.
pub fn classify(code: i32, output: &str) -> Classified {
    let clean = strip_go_trace(output);
    let lc = clean.to_ascii_lowercase();

    // ---- damage: only from evidence that the bytes were read and were wrong ----
    if lc.contains("does not match its hash") || lc.contains("pack id does not match") {
        let pack = extract_pack_id(&clean).unwrap_or_else(|| "unknown".into());
        return Classified::Damage(Corruption::PackHashMismatch { pack });
    }
    if lc.contains("ciphertext verification failed")
        || lc.contains("decrypting") && lc.contains("failed")
    {
        return Classified::Damage(Corruption::CiphertextInvalid {
            detail: first_line(&clean),
        });
    }
    if lc.contains("blob not found")
        || lc.contains("pack file cannot be listed")
        || lc.contains("is not found in the repository")
    {
        return Classified::Damage(Corruption::MissingData {
            detail: first_line(&clean),
        });
    }

    // ---- no verdict: transport, auth, capacity, locking ----
    // Order matters: HTTP status checks come before the generic `check failed`
    // rule, because an append-only 403 during `forget --prune` also prints
    // "failed to remove one or more snapshots" and must not read as damage.
    if lc.contains("403") || lc.contains("forbidden") {
        return Classified::NoVerdict(Cause::AppendOnlyRefused);
    }
    if lc.contains("507") || lc.contains("insufficient storage") || lc.contains("no space left") {
        return Classified::NoVerdict(Cause::OutOfSpace);
    }
    if lc.contains("401") || lc.contains("unauthorized") {
        return Classified::NoVerdict(Cause::Unauthorized);
    }
    if lc.contains("repository is already locked") || lc.contains("unable to create lock") {
        return Classified::NoVerdict(Cause::Locked {
            detail: first_line(&clean),
        });
    }
    if lc.contains("connection refused")
        || lc.contains("no such host")
        || lc.contains("network is unreachable")
        || lc.contains("i/o timeout")
        || lc.contains("dial tcp")
        || lc.contains("tls handshake")
        || lc.contains("certificate")
    {
        return Classified::NoVerdict(Cause::Unreachable {
            detail: first_line(&clean),
        });
    }
    if lc.contains("unexpected eof")
        || lc.contains("connection reset")
        || lc.contains("broken pipe")
        || lc.contains("context deadline exceeded")
    {
        return Classified::NoVerdict(Cause::TransferInterrupted {
            detail: first_line(&clean),
        });
    }

    // `restic check` failing without any of the above is genuine damage: check
    // reads data and does not fail for transient reasons. This rule sits last so
    // that a 403 or 507 encountered during a check is classified by its cause.
    if lc.contains("check failed") || lc.contains("repository contains errors") {
        return Classified::Damage(Corruption::CheckFailed {
            detail: first_line(&clean),
        });
    }

    Classified::NoVerdict(Cause::Unclassified {
        detail: format!("restic exited {code}: {}", first_line(&clean)),
    })
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn extract_pack_id(s: &str) -> Option<String> {
    // e.g. "pack 1a2b3c4d does not match its hash"
    let idx = s.find("pack ")? + 5;
    let rest = &s[idx..];
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if id.is_empty() { None } else { Some(id) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real output: restic 0.19.1, prune blocked by --append-only.
    const APPEND_ONLY: &str = r#"Remove(<snapshot/f77a5f056b>) failed: unexpected HTTP response (403): 403 Forbidden
unable to remove snapshot/f77a5f056b3295baf18e3dc0c6c9c147d28a9586f456fe5f1d5987cd247056a2 from the repository
[0:00] 0.00%  0 / 1 files deleted
failed to remove one or more snapshots
main.init
	/restic/cmd/restic/cmd_forget.go:67
runtime.doInit1
	/usr/local/go/src/runtime/proc.go:8103
runtime.doInit
	/usr/local/go/src/runtime/proc.go:8070
runtime.main
	/usr/local/go/src/runtime/proc.go:258
runtime.goexit
	/usr/local/go/src/runtime/asm_amd64.s:1771"#;

    // Verbatim: write past rest-server --max-size.
    const OUT_OF_SPACE: &str =
        "Save(<data/e8c8f0d303>) failed: unexpected HTTP response (507): 507 Insufficient Storage";

    // Verbatim: wrong or unloaded credential.
    const UNAUTHORIZED: &str = r#"Stat(<config/>) failed: unexpected HTTP response (401): 401 Unauthorized
Fatal: unable to open config file: unexpected HTTP response (401): 401 Unauthorized"#;

    #[test]
    fn strips_the_go_trace_but_keeps_the_error() {
        let out = strip_go_trace(APPEND_ONLY);
        assert!(out.contains("403 Forbidden"), "the real error must survive");
        assert!(out.contains("failed to remove one or more snapshots"));
        assert!(!out.contains("runtime.doInit1"), "frame labels must go");
        assert!(
            !out.contains("/usr/local/go/src"),
            "source locations must go"
        );
        assert!(!out.contains("asm_amd64.s"));
    }

    #[test]
    fn append_only_refusal_is_not_damage() {
        // This is the one that matters most. The output contains "failed to
        // remove one or more snapshots", which a naive classifier reads as a
        // broken repository. It is a healthy repository refusing a delete.
        match classify(3, APPEND_ONLY) {
            Classified::NoVerdict(Cause::AppendOnlyRefused) => {}
            other => panic!("append-only refusal misclassified as {other:?}"),
        }
    }

    #[test]
    fn out_of_space_is_not_damage() {
        match classify(1, OUT_OF_SPACE) {
            Classified::NoVerdict(Cause::OutOfSpace) => {}
            other => panic!("507 misclassified as {other:?}"),
        }
    }

    #[test]
    fn unauthorized_is_not_damage() {
        match classify(1, UNAUTHORIZED) {
            Classified::NoVerdict(Cause::Unauthorized) => {}
            other => panic!("401 misclassified as {other:?}"),
        }
    }

    #[test]
    fn transport_failures_never_produce_damage() {
        let transport = [
            "Fatal: unable to open config file: Get \"http://peer:8000/config\": dial tcp 10.0.0.5:8000: connect: connection refused",
            "Load(<data/aa>) returned error, retrying after 1s: unexpected EOF",
            "Fatal: Get \"https://peer/config\": net/http: TLS handshake timeout",
            "Fatal: Get \"http://peer/config\": dial tcp: lookup peer: no such host",
            "Save(<data/bb>) failed: context deadline exceeded",
            "read tcp 10.0.0.2:5000->10.0.0.5:8000: read: connection reset by peer",
        ];
        for t in transport {
            match classify(1, t) {
                Classified::NoVerdict(_) => {}
                Classified::Damage(d) => {
                    panic!("transport failure classified as DAMAGE ({d}): {t}")
                }
            }
        }
    }

    #[test]
    fn real_corruption_is_damage() {
        let damage = [
            "pack 1a2b3c4d does not match its hash",
            "Fatal: ciphertext verification failed",
            "blob not found in the index",
        ];
        for d in damage {
            match classify(1, d) {
                Classified::Damage(_) => {}
                Classified::NoVerdict(c) => {
                    panic!("real damage classified as no-verdict ({c}): {d}")
                }
            }
        }
    }

    #[test]
    fn a_403_during_check_is_still_not_damage() {
        // A check that dies on a 403 learned nothing about the data. The
        // ordering of the rules is what guarantees this, so pin it.
        let mixed = "error: Load(<data/aa>) failed: unexpected HTTP response (403): 403 Forbidden\ncheck failed";
        match classify(1, mixed) {
            Classified::NoVerdict(Cause::AppendOnlyRefused) => {}
            other => panic!("403-during-check must not be damage, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_check_failure_is_damage() {
        match classify(1, "check failed: repository contains errors") {
            Classified::Damage(Corruption::CheckFailed { .. }) => {}
            other => panic!("bare check failure should be damage, got {other:?}"),
        }
    }

    #[test]
    fn unknown_failures_default_to_no_verdict() {
        // Fail safe: never invent corruption we cannot demonstrate.
        match classify(1, "something nobody anticipated happened") {
            Classified::NoVerdict(Cause::Unclassified { .. }) => {}
            other => panic!("unknown failure must not be damage, got {other:?}"),
        }
    }

    #[test]
    fn pack_id_is_extracted_for_the_operator() {
        match classify(1, "pack 1a2b3c4d does not match its hash") {
            Classified::Damage(Corruption::PackHashMismatch { pack }) => {
                assert_eq!(pack, "1a2b3c4d");
            }
            other => panic!("expected a pack hash mismatch, got {other:?}"),
        }
    }
}
