//! The engine seam.
//!
//! peerbackup does not implement backup. It drives stock `restic`, which has
//! years of people successfully restoring from it, and keeps its own code on the
//! part nobody else has built: multi-peer bookkeeping, canary verification,
//! evidence, and the recovery bundle.
//!
//! This trait is deliberately **domain-shaped**, not a mirror of restic's CLI.
//! Four methods named for what peerbackup needs. Two consequences:
//!
//!   * Swapping the engine (rustic once it leaves beta, or a direct
//!     implementation) touches one file rather than every caller.
//!   * Tests can substitute a fake and inject faults at exactly the boundary
//!     where the three-state model lives.
//!
//! The important asymmetry: `verify_subset` returns [`VerifyOutcome`], not
//! `Result`. See [`outcome`] for why.

pub mod outcome;
pub mod restic;
pub mod restic_error;

use std::path::{Path, PathBuf};

pub use outcome::{Cause, VerifyOutcome};

/// Identifies a snapshot in a peer's repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotId(pub String);

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What we know about a snapshot without opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub id: SnapshotId,
    /// RFC3339, as restic reports it. Kept as a string at this layer so the seam
    /// does not force a time library on callers that only display it.
    pub time: String,
    pub paths: Vec<PathBuf>,
}

/// A file pulled back out of a repository, with the digest we computed on
/// arrival. The digest is the point: a restore that produced bytes is not the
/// same as a restore that produced the *right* bytes.
#[derive(Debug, Clone)]
pub struct RestoredFile {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

/// Something went wrong doing an operation whose failure is not a statement
/// about data integrity (backup, restore, listing).
///
/// Verification does not use this type. It returns [`VerifyOutcome`], because
/// "could not check" and "checked and it is bad" must not share an arm.
#[derive(Debug, Clone)]
pub struct EngineError {
    /// restic's message, already stripped of its Go trace.
    pub message: String,
    pub exit_code: Option<i32>,
    /// The same classification the verifier uses, so callers can distinguish a
    /// full peer from an unreachable one without re-parsing text.
    pub cause: Cause,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for EngineError {}

/// Options for taking a snapshot.
#[derive(Debug, Clone, Default)]
pub struct SnapshotOpts {
    /// Cap upload throughput, in KiB/s. `None` means unlimited.
    ///
    /// This is on the seam rather than in a scheduler because a 300GB seed at
    /// 40Mbps saturates a household uplink for seventeen hours, and the most
    /// likely way this project dies is someone switching it off after a ruined
    /// video call.
    pub upload_limit_kib: Option<u32>,
    pub tags: Vec<String>,
}

/// What peerbackup needs a backup engine to do. All four methods, no more.
pub trait BackupEngine {
    /// Take a snapshot of `sources`.
    fn snapshot(&self, sources: &[PathBuf], opts: &SnapshotOpts)
    -> Result<SnapshotId, EngineError>;

    /// Restore a single path out of a snapshot into `target`, returning its
    /// digest so the caller can compare against a snapshot-time value.
    fn restore_path(
        &self,
        snapshot: &SnapshotId,
        path: &Path,
        target: &Path,
    ) -> Result<RestoredFile, EngineError>;

    /// Read back `percent` of the repository's pack data and verify it.
    ///
    /// Returns a three-state outcome, never a `Result`. A peer we cannot reach
    /// yields `Indeterminate`, which ages into `unknown` on the dashboard; only
    /// data we read and found wrong yields `Bad`.
    fn verify_subset(&self, percent: u8) -> VerifyOutcome;

    /// List snapshots, newest first.
    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, EngineError>;
}

#[cfg(test)]
pub mod fake;
