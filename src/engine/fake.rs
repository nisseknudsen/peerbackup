//! A scriptable [`BackupEngine`] for tests.
//!
//! This is the fault-injection point the eng review asked for. Because the seam
//! is domain-shaped, a test can make a peer fail in a specific way without any
//! network, container or timing games.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use super::outcome::VerifyOutcome;
use super::{BackupEngine, EngineError, RestoredFile, SnapshotId, SnapshotMeta, SnapshotOpts};

/// Scripted engine. Each call pops the next queued response; when the queue is
/// empty the configured default is returned.
pub struct FakeEngine {
    pub verify_queue: RefCell<Vec<VerifyOutcome>>,
    pub verify_default: VerifyOutcome,
    pub snapshot_result: RefCell<Option<Result<SnapshotId, EngineError>>>,
    pub calls: RefCell<Vec<String>>,
}

impl FakeEngine {
    pub fn always(outcome: VerifyOutcome) -> Self {
        Self {
            verify_queue: RefCell::new(Vec::new()),
            verify_default: outcome,
            snapshot_result: RefCell::new(None),
            calls: RefCell::new(Vec::new()),
        }
    }

    /// Queue outcomes in the order they should be returned.
    pub fn scripted(mut outcomes: Vec<VerifyOutcome>, default: VerifyOutcome) -> Self {
        outcomes.reverse(); // pop() takes from the end
        Self {
            verify_queue: RefCell::new(outcomes),
            verify_default: default,
            snapshot_result: RefCell::new(None),
            calls: RefCell::new(Vec::new()),
        }
    }

    pub fn call_log(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl BackupEngine for FakeEngine {
    fn snapshot(
        &self,
        sources: &[PathBuf],
        opts: &SnapshotOpts,
    ) -> Result<SnapshotId, EngineError> {
        self.calls.borrow_mut().push(format!(
            "snapshot({} sources, limit={:?})",
            sources.len(),
            opts.upload_limit_kib
        ));
        match self.snapshot_result.borrow_mut().take() {
            Some(r) => r,
            None => Ok(SnapshotId("fake0001".into())),
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
        Ok(RestoredFile {
            path: target.join(path.file_name().unwrap_or_default()),
            sha256: "0".repeat(64),
            bytes: 0,
        })
    }

    fn verify_subset(&self, percent: u8) -> VerifyOutcome {
        self.calls
            .borrow_mut()
            .push(format!("verify_subset({percent})"));
        self.verify_queue
            .borrow_mut()
            .pop()
            .unwrap_or_else(|| self.verify_default.clone())
    }

    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, EngineError> {
        self.calls.borrow_mut().push("list_snapshots()".into());
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::outcome::Cause;

    #[test]
    fn scripted_outcomes_are_returned_in_order() {
        let e = FakeEngine::scripted(
            vec![
                VerifyOutcome::Good { coverage_pct: 5 },
                VerifyOutcome::Indeterminate(Cause::Unreachable { detail: "x".into() }),
            ],
            VerifyOutcome::Good { coverage_pct: 1 },
        );
        assert!(e.verify_subset(5).is_good());
        assert_eq!(e.verify_subset(5).label(), "unknown");
        // Queue exhausted: falls back to the default.
        assert!(e.verify_subset(5).is_good());
        assert_eq!(e.call_log().len(), 3);
    }

    #[test]
    fn upload_limit_reaches_the_engine() {
        // The bandwidth ceiling has to survive the seam or the governor above it
        // is decorative.
        let e = FakeEngine::always(VerifyOutcome::Good { coverage_pct: 1 });
        let opts = SnapshotOpts {
            upload_limit_kib: Some(5000),
            ..Default::default()
        };
        e.snapshot(&[PathBuf::from("/srv/data")], &opts).unwrap();
        assert!(e.call_log()[0].contains("5000"));
    }
}
