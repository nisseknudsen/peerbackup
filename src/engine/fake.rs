//! A scriptable [`BackupEngine`] for tests.
//!
//! This is the fault-injection point the eng review asked for. Because the seam
//! is domain-shaped, a test can make a peer fail in a specific way without any
//! network, container or timing games.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::outcome::{Cause, VerifyOutcome};
use super::{
    BackupEngine, EngineError, RestoredFile, Snapshot, SnapshotId, SnapshotMeta, SnapshotOpts,
};

/// Scripted engine. `verify_subset` answers `verify_default`; the constructors
/// below cover the specific faults the command tests need to inject.
pub struct FakeEngine {
    pub verify_default: VerifyOutcome,
    pub snapshot_result: RefCell<Option<Result<Snapshot, EngineError>>>,
    /// What `probe` answers. `None` means reachable.
    pub probe_result: Option<Cause>,
    /// Forces the digest `restore_path` reports back.
    ///
    /// `None` is the healthy peer: it hashes the file it was asked for and
    /// returns the truth, so a canary check passes exactly when it should.
    /// `Some` is a peer that returns the wrong bytes.
    pub restored_digest: Option<String>,
    /// What `list_snapshots` answers. A canary restore needs one to pick.
    pub snapshots: Vec<SnapshotMeta>,
    /// Every operation asked of this engine, in order.
    ///
    /// Shared rather than owned so a test can keep a handle to the log while
    /// the engine itself disappears inside the command under test. `backup_in`
    /// and `verify_in` build their engines from a closure and drop them, so an
    /// owned `RefCell` here could never be read afterwards -- which is why this
    /// recorded every call and nothing ever asserted on one.
    pub calls: Rc<RefCell<Vec<String>>>,
}

impl FakeEngine {
    pub fn always(outcome: VerifyOutcome) -> Self {
        Self {
            verify_default: outcome,
            snapshot_result: RefCell::new(None),
            probe_result: None,
            restored_digest: None,
            snapshots: vec![SnapshotMeta {
                id: SnapshotId("fake0001".into()),
                time: "2026-07-01T10:00:00Z".into(),
                paths: vec![PathBuf::from("/srv/data")],
                tags: vec!["peerbackup".into()],
            }],
            calls: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// A peer that does not answer at all.
    pub fn unreachable(detail: &str) -> Self {
        Self {
            probe_result: Some(Cause::Unreachable {
                detail: detail.into(),
            }),
            ..Self::always(VerifyOutcome::Good { coverage_pct: 1 })
        }
    }

    pub fn failing_snapshot(e: EngineError) -> Self {
        Self {
            snapshot_result: RefCell::new(Some(Err(e))),
            ..Self::always(VerifyOutcome::Good { coverage_pct: 1 })
        }
    }

    pub fn incomplete_snapshot() -> Self {
        Self {
            snapshot_result: RefCell::new(Some(Ok(Snapshot {
                id: SnapshotId("fake0001".into()),
                incomplete: true,
            }))),
            ..Self::always(VerifyOutcome::Good { coverage_pct: 1 })
        }
    }
}

impl BackupEngine for FakeEngine {
    fn snapshot(&self, sources: &[PathBuf], opts: &SnapshotOpts) -> Result<Snapshot, EngineError> {
        self.calls.borrow_mut().push(format!(
            "snapshot({} sources, limit={:?})",
            sources.len(),
            opts.upload_limit_kib
        ));
        match self.snapshot_result.borrow_mut().take() {
            Some(r) => r,
            None => Ok(Snapshot {
                id: SnapshotId("fake0001".into()),
                incomplete: false,
            }),
        }
    }

    fn restore_path(
        &self,
        snapshot: &SnapshotId,
        path: &Path,
        target: &Path,
    ) -> Result<RestoredFile, EngineError> {
        self.calls
            .borrow_mut()
            .push(format!("restore_path({snapshot}, {})", path.display()));
        let sha256 = self.restored_digest.clone().unwrap_or_else(|| {
            std::fs::read(path).map_or_else(|_| "0".repeat(64), |b| crate::state::sha256_bytes(&b))
        });
        Ok(RestoredFile {
            path: target.join(path.file_name().unwrap_or_default()),
            sha256,
            bytes: 0,
        })
    }

    fn restore_all(&self, snapshot: &SnapshotId, _target: &Path) -> Result<(), EngineError> {
        self.calls
            .borrow_mut()
            .push(format!("restore_all({snapshot})"));
        Ok(())
    }

    fn verify_subset(&self, percent: u8) -> VerifyOutcome {
        self.calls
            .borrow_mut()
            .push(format!("verify_subset({percent})"));
        self.verify_default.clone()
    }

    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, EngineError> {
        self.calls.borrow_mut().push("list_snapshots()".into());
        Ok(self.snapshots.clone())
    }

    fn probe(&self) -> Option<Cause> {
        self.calls.borrow_mut().push("probe()".into());
        self.probe_result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // What is worth testing here is that this stands in for a real engine
    // faithfully enough for the command tests in `cli` to mean something.

    #[test]
    fn an_unreachable_engine_reports_it_from_probe() {
        let e = FakeEngine::unreachable("connection refused");
        assert!(e.probe().is_some());
    }

    #[test]
    fn the_call_log_survives_the_engine_it_recorded() {
        // The property the `cli` ordering test depends on: a handle taken before
        // the engine is handed over still sees what the engine was asked to do.
        let e = FakeEngine::always(VerifyOutcome::Good { coverage_pct: 1 });
        let log = e.calls.clone();
        e.probe();
        e.verify_subset(1);
        drop(e);
        assert_eq!(&*log.borrow(), &["probe()", "verify_subset(1)"]);
    }
}
