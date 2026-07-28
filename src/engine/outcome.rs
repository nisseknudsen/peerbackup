//! Three-state verification outcome.
//!
//! `Result` has two arms; this has three. Good (read it, matched), Bad (read it,
//! did not match), Indeterminate (could not read it, learned nothing). With a
//! `Result` the last two share an arm and the first `?` erases the difference,
//! so a rebooting router looks like a corrupted pack. Making it a value means
//! the compiler forces every caller to handle all three.

use std::fmt;

/// Why we believe the data is damaged. Only ever constructed from evidence we
/// actually observed, never inferred from a failure to connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Corruption {
    /// A pack's contents did not hash to its ID.
    PackHashMismatch { pack: String },
    /// Decryption or authentication of a blob failed.
    CiphertextInvalid { detail: String },
    /// The repository references data that is not there.
    MissingData { detail: String },
    /// restic's own integrity check reported a problem we could not classify
    /// more precisely. Still Bad: `check` does not fail for transient reasons.
    CheckFailed { detail: String },
}

impl fmt::Display for Corruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Corruption::PackHashMismatch { pack } => {
                write!(f, "pack {pack} does not match its hash")
            }
            Corruption::CiphertextInvalid { detail } => {
                write!(f, "ciphertext failed verification: {detail}")
            }
            Corruption::MissingData { detail } => {
                write!(f, "data missing from repository: {detail}")
            }
            Corruption::CheckFailed { detail } => write!(f, "integrity check failed: {detail}"),
        }
    }
}

/// Why we could not reach a verdict. None of these say anything about the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// Could not establish a connection.
    Unreachable { detail: String },
    /// Connected, then the transfer died.
    TransferInterrupted { detail: String },
    /// Credentials were rejected. A configuration problem, not a data problem.
    Unauthorized,
    /// The peer refused the write because the repository is append-only.
    AppendOnlyRefused,
    /// The peer is out of space.
    OutOfSpace,
    /// Another process holds the repository lock.
    Locked { detail: String },
    /// We gave up waiting. restic retries transport failures with exponential
    /// backoff; measured against an unreachable peer, a 1% check ran for over
    /// ten minutes before being killed. Waiting forever stalls the scheduler
    /// and the dashboard behind it, so we bound it and report what is true:
    /// we waited, and we learned nothing.
    TimedOut { after_secs: u64 },
    /// We could not classify the failure. Deliberately Indeterminate rather than
    /// Bad: claiming corruption we cannot demonstrate is the worse error.
    Unclassified { detail: String },
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cause::Unreachable { detail } => write!(f, "peer unreachable: {detail}"),
            Cause::TransferInterrupted { detail } => write!(f, "transfer interrupted: {detail}"),
            Cause::Unauthorized => write!(f, "credentials rejected by the peer"),
            Cause::AppendOnlyRefused => {
                write!(f, "peer is in append-only mode and refused the operation")
            }
            Cause::OutOfSpace => write!(f, "peer is out of space"),
            Cause::Locked { detail } => write!(f, "repository is locked: {detail}"),
            Cause::TimedOut { after_secs } => {
                write!(f, "gave up after {after_secs}s without an answer")
            }
            Cause::Unclassified { detail } => write!(f, "unclassified failure: {detail}"),
        }
    }
}

/// The result of a verification attempt. Not a `Result`. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Verified good. `coverage_pct` is the share of pack data actually read
    /// back, which is reported separately from canary success so the dashboard
    /// never implies more than was checked.
    Good { coverage_pct: u8 },
    /// One corrupt pack reddens the whole peer, deliberately.
    Bad(Corruption),
    /// No verdict. Ages into `unknown` on the dashboard; never into red.
    Indeterminate(Cause),
}

impl VerifyOutcome {
    /// True only for `Good`. Written out rather than derived so that adding a
    /// future variant is a compile error here instead of a silent green.
    pub fn is_good(&self) -> bool {
        matches!(self, VerifyOutcome::Good { .. })
    }

    /// True only with positive evidence of damage. Turns a peer red, so it must
    /// never be satisfied by a network problem.
    pub fn is_bad(&self) -> bool {
        matches!(self, VerifyOutcome::Bad(_))
    }

    /// A short label for the dashboard.
    pub fn label(&self) -> &'static str {
        match self {
            VerifyOutcome::Good { .. } => "verified-good",
            VerifyOutcome::Bad(_) => "verified-bad",
            VerifyOutcome::Indeterminate(_) => "unknown",
        }
    }
}

impl fmt::Display for VerifyOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyOutcome::Good { coverage_pct } => {
                write!(f, "verified-good ({coverage_pct}% of pack data read back)")
            }
            VerifyOutcome::Bad(c) => write!(f, "verified-bad: {c}"),
            VerifyOutcome::Indeterminate(c) => write!(f, "unknown: {c}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_network_failure_is_never_bad() {
        // The single most important property in this file. Every Cause must
        // produce Indeterminate, never Bad, because none of them observed the
        // data. If someone adds a Cause variant and routes it to Bad, this
        // fails.
        let causes = [
            Cause::Unreachable {
                detail: "connection refused".into(),
            },
            Cause::TransferInterrupted {
                detail: "EOF".into(),
            },
            Cause::Unauthorized,
            Cause::AppendOnlyRefused,
            Cause::OutOfSpace,
            Cause::Locked {
                detail: "held by another process".into(),
            },
            Cause::TimedOut { after_secs: 3600 },
            Cause::Unclassified {
                detail: "???".into(),
            },
        ];
        for c in causes {
            let outcome = VerifyOutcome::Indeterminate(c.clone());
            assert!(!outcome.is_bad(), "{c} must not redden a peer");
            assert!(!outcome.is_good(), "{c} must not green a peer either");
            assert_eq!(outcome.label(), "unknown");
        }
    }

    #[test]
    fn observed_damage_is_bad() {
        let o = VerifyOutcome::Bad(Corruption::PackHashMismatch {
            pack: "ab12".into(),
        });
        assert!(o.is_bad());
        assert!(!o.is_good());
        assert_eq!(o.label(), "verified-bad");
    }

    #[test]
    fn good_carries_its_coverage() {
        let o = VerifyOutcome::Good { coverage_pct: 5 };
        assert!(o.is_good());
        assert_eq!(o.label(), "verified-good");
        // Coverage must be visible in the rendered form: "green" without a
        // number would overstate what was actually checked.
        assert!(o.to_string().contains('5'));
    }

    #[test]
    fn indeterminate_never_claims_coverage() {
        let o = VerifyOutcome::Indeterminate(Cause::Unreachable {
            detail: "timeout".into(),
        });
        assert!(!o.to_string().contains("read back"));
    }
}
