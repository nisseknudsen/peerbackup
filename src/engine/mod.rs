//! The engine seam.
//!
//! peerbackup drives stock `restic` rather than implementing backup, and keeps
//! its own code on the parts nobody else has built: multi-peer bookkeeping,
//! canary verification, evidence, recovery bundles.
//!
//! The trait is domain-shaped, not a mirror of restic's CLI. Four methods named
//! for what peerbackup needs, so swapping the engine touches one file and tests
//! can inject faults at the boundary where the three-state model lives.
//!
//! `verify_subset` returns [`VerifyOutcome`], not `Result`. See [`outcome`].

pub mod outcome;
pub mod restic;
pub mod restic_error;

use std::path::{Path, PathBuf};

pub use outcome::{Cause, VerifyOutcome};

/// Identifies a snapshot in a peer's repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotId(pub String);

impl SnapshotId {
    /// restic reports the full 64-character id after a backup but shows the
    /// first 8 everywhere else. Match what people see in `snapshots`.
    ///
    /// Truncates on a character boundary rather than a byte one. Ids are hex in
    /// practice, but the field is public and byte-slicing a `String` panics if
    /// anyone ever puts something else in it.
    #[must_use]
    pub fn short(&self) -> &str {
        self.0
            .char_indices()
            .nth(8)
            .map_or(self.0.as_str(), |(i, _)| self.0.split_at(i).0)
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.short())
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
    /// What the snapshot was tagged with when it was made. This is how a real
    /// backup is told apart from the test upload `peer add` makes, and it has to
    /// be the tag rather than the paths: paths stop matching the moment someone
    /// edits `sources`, and a restore that refuses because you reorganised your
    /// folders is its own kind of failure.
    pub tags: Vec<String>,
}

/// A file pulled back out, with the digest computed on arrival. The digest is
/// the point: bytes restored is not the same as the *right* bytes restored.
///
/// `path` and `bytes` are for reporting; only `sha256` is compared today.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RestoredFile {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

/// A failed operation whose failure says nothing about data integrity.
///
/// `exit_code` is kept for diagnostics even though nothing branches on it yet;
/// restic's codes are part of the contract this wrapper depends on.
///
/// Verification does not use this: it returns [`VerifyOutcome`].
#[derive(Debug, Clone)]
#[allow(dead_code)]
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

/// Result of a snapshot, including whether restic managed to read everything.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub id: SnapshotId,
    /// restic exits 0 and saves a snapshot even when a source could not be
    /// read, printing only a warning. Silently backing up less than asked is
    /// exactly the failure this product exists to catch, so it is surfaced.
    pub incomplete: bool,
}

/// Options for taking a snapshot.
#[derive(Debug, Clone, Default)]
pub struct SnapshotOpts {
    /// Cap upload throughput, KiB/s. On the seam because a 300GB seed saturates
    /// a household uplink for ~17h at 40Mbps, and the likeliest way this project
    /// dies is someone switching it off after a ruined video call.
    pub upload_limit_kib: Option<u32>,
    pub tags: Vec<String>,
}

/// What peerbackup needs a backup engine to do. All four methods, no more.
pub trait BackupEngine {
    /// Take a snapshot of `sources`.
    fn snapshot(&self, sources: &[PathBuf], opts: &SnapshotOpts) -> Result<Snapshot, EngineError>;

    /// Restore a single path out of a snapshot into `target`, returning its
    /// digest so the caller can compare against a snapshot-time value.
    fn restore_path(
        &self,
        snapshot: &SnapshotId,
        path: &Path,
        target: &Path,
    ) -> Result<RestoredFile, EngineError>;

    /// Restore an entire snapshot. This is the disaster operation.
    fn restore_all(&self, snapshot: &SnapshotId, target: &Path) -> Result<(), EngineError>;

    /// Read back `percent` of the repository's pack data and verify it.
    ///
    /// Returns a three-state outcome, never a `Result`. A peer we cannot reach
    /// yields `Indeterminate`, which ages into `unknown` on the dashboard; only
    /// data we read and found wrong yields `Bad`.
    fn verify_subset(&self, percent: u8) -> VerifyOutcome;

    /// List snapshots, newest first.
    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, EngineError>;

    /// Cheap reachability check. `None` means the peer answered.
    ///
    /// On the trait rather than inherent to the restic engine because callers
    /// depend on it for ordering: verifying an unreachable peer without probing
    /// first waits out the whole verification timeout while restic retries.
    /// Anything standing in for an engine has to be able to say "not reachable"
    /// or that ordering cannot be tested.
    fn probe(&self) -> Option<Cause>;
}

#[cfg(test)]
pub mod fake;
