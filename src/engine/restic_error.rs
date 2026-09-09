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
            // Source locations are indented and name a Go source file. The
            // `.go:` / `.s:` suffix is required rather than merely "starts with
            // a path": restic indents its own detail lines under an error
            // header, and those are frequently absolute paths. Dropping every
            // indented `/...` line deleted real evidence -- `classify` runs on
            // the stripped text, so a damage keyword that appeared only on such
            // a line was destroyed before it could be matched, and the failure
            // fell through to `Unclassified`.
            let is_location =
                line.starts_with(char::is_whitespace) && (t.contains(".go:") || t.contains(".s:"));
            !(is_frame || is_location)
        })
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drop restic's non-fatal retry lines.
///
/// restic reports a transient failure and its retry on one line, then continues.
/// Those lines describe something that already recovered, so they say nothing
/// about how the run ended -- but they are full of exactly the words the
/// transport rules match on.
fn without_retry_lines(s: &str) -> String {
    s.lines()
        .filter(|l| {
            let lc = l.to_ascii_lowercase();
            !(lc.contains("returned error, retrying") || lc.contains("retrying after"))
        })
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
    // Retry chatter is not an outcome, and it must not be allowed to explain
    // one. restic prints `Load(<data/aa>) returned error, retrying after 1s:
    // unexpected EOF` on every transient blip and then carries on, usually
    // successfully. Because the transport rules below match anywhere in the
    // combined output and sit above the generic `check failed` rule, a single
    // such line during a forty-minute read-back downgraded
    // `Fatal: repository contains errors` to `TransferInterrupted` -- so a
    // repository restic had just called broken read `unknown` forever.
    //
    // The existing tests fed each transport string in isolation, which is
    // exactly the shape that cannot catch this.
    let outcome = without_retry_lines(&clean);
    let lc = outcome.to_ascii_lowercase();

    // ---- damage: only from evidence that the bytes were read and were wrong ----
    if lc.contains("does not match its hash") || lc.contains("pack id does not match") {
        let pack = extract_pack_id(&outcome).unwrap_or_else(|| "unknown".into());
        return Classified::Damage(Corruption::PackHashMismatch { pack });
    }
    // Same line, not merely both somewhere in the output. `contains` over the
    // whole combined text needed only the word `decrypting` (restic prints it
    // while loading keys and indexes) and the word `failed` anywhere else, and
    // this rule sits above every transport rule -- so a peer whose router was
    // down could be reported as having corrupt data. A false red costs the same
    // trust as a false green.
    if lc.contains("ciphertext verification failed")
        || lc
            .lines()
            .any(|l| l.contains("decrypting") && l.contains("failed"))
    {
        return Classified::Damage(Corruption::CiphertextInvalid {
            detail: first_line(&outcome),
        });
    }
    if lc.contains("blob not found")
        || lc.contains("pack file cannot be listed")
        || lc.contains("is not found in the repository")
    {
        return Classified::Damage(Corruption::MissingData {
            detail: first_line(&outcome),
        });
    }

    // ---- no verdict: transport, auth, capacity, locking ----
    // Repository-state answers first. `peer add` used to match these two on the
    // message text at the call site, which is knowledge about restic's prose
    // leaking out of this module -- the whole reason `Cause` is carried on the
    // error in the first place.
    // "config file already exists" is what 0.19.1 actually says; the phrase
    // "already initialized" appears in other versions. Both mean the same thing
    // to a caller, which is the point of naming the cause instead of matching
    // prose at three different call sites.
    if lc.contains("already initialized") || lc.contains("config file already exists") {
        return Classified::NoVerdict(Cause::AlreadyInitialized);
    }
    if lc.contains("wrong password") || lc.contains("no key found") {
        return Classified::NoVerdict(Cause::WrongPassword);
    }

    // Order matters: HTTP status checks come before the generic `check failed`
    // rule, because an append-only 403 during `forget --prune` also prints
    // "failed to remove one or more snapshots" and must not read as damage.
    //
    // Matched as `(403)`, not as a bare `403`. restic identifiers are hex, so
    // the digits 403, 507 and 401 turn up inside ordinary pack, tree and blob
    // ids -- and because these rules run before the `check failed` rule below,
    // a bare match downgraded real corruption to "we could not look at it".
    // Measured against the shipped classifier:
    //
    //     "error for tree 6403bc1e: ...\ncheck failed"
    //         -> NoVerdict(AppendOnlyRefused), should have been Damage
    //
    // A peer with damaged packs then reads `unchecked` forever and never turns
    // red, which is the one failure this program exists to catch. restic always
    // emits the parenthesised form (see the fixtures below), so requiring it
    // costs nothing and removes the collision.
    if http_status(&lc, 403) || lc.contains("forbidden") {
        return Classified::NoVerdict(Cause::AppendOnlyRefused);
    }
    if http_status(&lc, 507) || lc.contains("insufficient storage") || lc.contains("no space left")
    {
        return Classified::NoVerdict(Cause::OutOfSpace);
    }
    if http_status(&lc, 401) || lc.contains("unauthorized") {
        return Classified::NoVerdict(Cause::Unauthorized);
    }
    if lc.contains("repository is already locked") || lc.contains("unable to create lock") {
        return Classified::NoVerdict(Cause::Locked {
            detail: first_line(&outcome),
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
            detail: first_line(&outcome),
        });
    }
    if lc.contains("unexpected eof")
        || lc.contains("connection reset")
        || lc.contains("broken pipe")
        || lc.contains("context deadline exceeded")
    {
        return Classified::NoVerdict(Cause::TransferInterrupted {
            detail: first_line(&outcome),
        });
    }

    // `restic check` failing without any of the above is genuine damage: check
    // reads data and does not fail for transient reasons. This rule sits last so
    // that a 403 or 507 encountered during a check is classified by its cause.
    if lc.contains("check failed") || lc.contains("repository contains errors") {
        return Classified::Damage(Corruption::CheckFailed {
            detail: first_line(&outcome),
        });
    }

    Classified::NoVerdict(Cause::Unclassified {
        detail: format!("restic exited {code}: {}", first_line(&outcome)),
    })
}

/// Did restic report this HTTP status, as opposed to merely printing an id that
/// happens to contain the digits?
///
/// restic writes `unexpected HTTP response (403): 403 Forbidden`. The
/// parenthesised form is the one that cannot appear inside a hex identifier,
/// which is the whole reason this is a function rather than a `contains`.
fn http_status(lowercased: &str, code: u16) -> bool {
    lowercased.contains(&format!("({code})"))
}

/// The one line of restic's output worth carrying on a `Cause`.
///
/// Redacted and stripped of control characters on the way out, because this is
/// the single place every `Cause` detail is built and those details are printed
/// under the status table and appended to the evidence log. restic embeds the
/// repository URL -- credentials and all -- in its own error prose, and prints a
/// server's HTTP status line verbatim, so this string is partly written by the
/// peer.
fn first_line(s: &str) -> String {
    crate::redact::detail(s.lines().find(|l| !l.trim().is_empty()).unwrap_or(""))
}

/// The id from the line that actually reported the mismatch.
///
/// This took the first `"pack "` anywhere in the output, which need not be the
/// pack that failed -- `check` mentions packs while it works -- so the id shown
/// to the operator could belong to a healthy one. Scoped to the reporting line
/// first, falling back to the old behaviour so a phrasing change costs the id
/// rather than the verdict.
fn extract_pack_id(s: &str) -> Option<String> {
    let line = s
        .lines()
        .find(|l| {
            let lc = l.to_ascii_lowercase();
            lc.contains("does not match its hash") || lc.contains("pack id does not match")
        })
        .unwrap_or(s);
    // e.g. "pack 1a2b3c4d does not match its hash"
    let rest = line.split_once("pack ")?.1;
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if id.is_empty() { None } else { Some(id) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real output: restic 0.19.1 refusing to init over an existing repository.
    // The wording is "config file already exists", not "already initialized" --
    // which is exactly why this knowledge belongs in one place instead of being
    // matched on at the call site.
    const ALREADY_THERE: &str = "Fatal: create repository at rest:http://me:pw@127.0.0.1:8023/me/ failed: config file already exists";

    #[test]
    fn an_existing_repository_is_recognised_by_its_real_message() {
        assert!(matches!(
            classify(1, ALREADY_THERE),
            Classified::NoVerdict(Cause::AlreadyInitialized)
        ));
    }

    #[test]
    fn a_wrong_password_is_recognised_rather_than_matched_on_at_the_call_site() {
        for msg in [
            "Fatal: wrong password or no key found",
            "Fatal: unable to open config file: wrong password",
        ] {
            assert!(
                matches!(
                    classify(1, msg),
                    Classified::NoVerdict(Cause::WrongPassword)
                ),
                "{msg}"
            );
        }
    }

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
    fn the_reported_pack_is_the_one_that_failed() {
        // The first `"pack "` in the output need not be the failing one --
        // `check` mentions packs while it works -- so the id shown to the
        // operator could belong to a perfectly healthy pack.
        let out = "load pack 1111aaaa\n\
                   check snapshots, trees and blobs\n\
                   pack 4f2a1b3c does not match its hash\n\
                   Fatal: repository contains errors\n";
        match classify(1, out) {
            Classified::Damage(Corruption::PackHashMismatch { pack }) => {
                assert_eq!(pack, "4f2a1b3c");
            }
            other => panic!("expected a pack mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_retry_line_does_not_bury_a_real_corruption_report() {
        // The finding. A forty-minute read-back hits one transient blip, restic
        // retries it successfully, and then ends by saying the repository is
        // broken. The transport rules match anywhere in the combined output and
        // sit above the generic damage rule, so the retry line won and the
        // damaged repository read `unknown` forever.
        let out = "using temporary cache in /tmp/restic-check-cache-123\n\
                   Load(<data/aa11bb22>) returned error, retrying after 1.3s: unexpected EOF\n\
                   check snapshots, trees and blobs\n\
                   pack 4f2a1b3c: not referenced in any index\n\
                   Fatal: repository contains errors\n";
        assert!(
            matches!(classify(1, out), Classified::Damage(_)),
            "got {:?}",
            classify(1, out)
        );
    }

    #[test]
    fn a_retry_line_still_does_not_invent_damage_on_its_own() {
        // Removing retry lines must not turn a run that only ever hiccuped into
        // a damage verdict by leaving nothing behind to explain it.
        let out = "Load(<data/aa11bb22>) returned error, retrying after 1.3s: unexpected EOF\n\
                   Fatal: unable to open repository: Get \"http://host/config\": \
                   dial tcp 10.0.0.9:8000: connect: connection refused\n";
        assert!(
            matches!(
                classify(1, out),
                Classified::NoVerdict(Cause::Unreachable { .. })
            ),
            "got {:?}",
            classify(1, out)
        );
    }

    #[test]
    fn a_fatal_transport_failure_during_a_check_is_still_not_damage() {
        // The reason the damage rule sits last, and it has to keep working: a
        // check that could not read the repository at all has learned nothing
        // about the data.
        let out = "Load(<index/9f>) failed\n\
                   Fatal: unable to open repository: unexpected HTTP response (403): 403 Forbidden\n\
                   check failed\n";
        assert!(
            matches!(
                classify(1, out),
                Classified::NoVerdict(Cause::AppendOnlyRefused)
            ),
            "got {:?}",
            classify(1, out)
        );
    }

    #[test]
    fn decrypting_and_failed_on_unrelated_lines_is_not_damage() {
        // Both words appeared somewhere in the output, which is all the rule
        // required -- and it sits above every transport rule, so a dead router
        // reddened a healthy peer.
        let out = "decrypting master key\n\
                   Fatal: unable to open repository: Get \"http://host/config\": \
                   dial tcp: i/o timeout, request failed\n";
        assert!(
            matches!(
                classify(1, out),
                Classified::NoVerdict(Cause::Unreachable { .. })
            ),
            "got {:?}",
            classify(1, out)
        );
    }

    #[test]
    fn decrypting_failing_on_one_line_is_still_damage() {
        let out = "Fatal: decrypting blob 9f2a failed: authentication check failed\n";
        assert!(
            matches!(
                classify(1, out),
                Classified::Damage(Corruption::CiphertextInvalid { .. })
            ),
            "got {:?}",
            classify(1, out)
        );
    }

    #[test]
    fn an_indented_detail_line_is_not_mistaken_for_a_go_frame() {
        // `starts_with('/')` on an indented line deleted restic's own detail,
        // and `classify` runs on the stripped text -- so a damage keyword that
        // appeared only there was destroyed before it could be matched.
        let out = "Fatal: repository contains errors\n    \
                   /srv/data/photos: pack 4f2a1b3c does not match its hash\n\
                   main.main\n    \
                   /restic/cmd/restic/main.go:98\n";
        let stripped = strip_go_trace(out);
        assert!(stripped.contains("/srv/data/photos"), "{stripped}");
        assert!(!stripped.contains("main.go:98"), "{stripped}");
        assert!(matches!(
            classify(1, out),
            Classified::Damage(Corruption::PackHashMismatch { .. })
        ));
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
    fn a_hex_id_containing_a_status_code_does_not_hide_damage() {
        // The bug this pins: the status rules matched a bare `403`/`507`/`401`
        // anywhere in the output, and they run before the `check failed` rule.
        // restic ids are hex, so those digits appear in ordinary pack, tree and
        // blob ids -- and a peer whose packs are damaged then reported
        // `unchecked` forever instead of turning red.
        for output in [
            "error for tree 6403bc1e:\n  id 6403bc1e not found in repository\ncheck failed",
            "Load(<data/a507f2>) failed: cannot load\ncheck failed: repository contains errors",
            "pack 3401ffab: not referenced in any index\ncheck failed",
        ] {
            match classify(1, output) {
                Classified::Damage(_) => {}
                Classified::NoVerdict(c) => panic!(
                    "a hex id swallowed real damage, classified as no-verdict ({c}): {output}"
                ),
            }
        }
    }

    #[test]
    fn the_real_status_lines_are_still_recognised() {
        // The other half of the same change: tightening the match must not stop
        // an actual 403/507/401 from being recognised, or every append-only
        // refusal starts reading as corruption.
        assert!(matches!(
            classify(3, APPEND_ONLY),
            Classified::NoVerdict(Cause::AppendOnlyRefused)
        ));
        assert!(matches!(
            classify(1, OUT_OF_SPACE),
            Classified::NoVerdict(Cause::OutOfSpace)
        ));
        assert!(matches!(
            classify(1, UNAUTHORIZED),
            Classified::NoVerdict(Cause::Unauthorized)
        ));
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
