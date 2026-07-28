//! Canary files and the evidence log.
//!
//! The canary is a small set of files with recorded digests that rides along in
//! every backup. Verification restores it and compares, which is what makes the
//! difference between "the upload reported success" and "the data comes back".
//!
//! Evidence is an append-only JSONL file. Append-only because the history of
//! what was checked and when is worth more than any single result, and because
//! a crash mid-write should cost one line rather than the whole record.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Config, home};

pub fn state_dir() -> PathBuf {
    std::env::var_os("PEERBACKUP_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share/peerbackup"))
}

pub fn canary_dir() -> PathBuf {
    state_dir().join("canary")
}

pub fn evidence_path() -> PathBuf {
    state_dir().join("evidence.jsonl")
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// --------------------------------------------------------------------- canary

#[derive(Debug, Serialize, Deserialize)]
pub struct Canary {
    pub files: Vec<CanaryFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryFile {
    pub path: PathBuf,
    pub sha256: String,
}

impl Canary {
    fn manifest_path() -> PathBuf {
        state_dir().join("canary.json")
    }

    /// Create the canary files and record their digests.
    pub fn create() -> std::io::Result<Self> {
        let dir = canary_dir();
        fs::create_dir_all(&dir)?;
        let mut files = Vec::new();
        for i in 0..3 {
            let path = dir.join(format!("canary-{i}.bin"));
            let mut buf = vec![0u8; 64 * 1024];
            fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
            fs::write(&path, &buf)?;
            files.push(CanaryFile {
                path: path.clone(),
                sha256: sha256_bytes(&buf),
            });
        }
        let c = Canary { files };
        c.save()?;
        Ok(c)
    }

    pub fn load() -> std::io::Result<Self> {
        let p = Self::manifest_path();
        let text = fs::read_to_string(&p)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = Self::manifest_path();
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(
            p,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )
    }

    pub fn load_or_create() -> std::io::Result<Self> {
        match Self::load() {
            Ok(c) => Ok(c),
            Err(_) => Self::create(),
        }
    }

    pub fn first(&self) -> Option<&CanaryFile> {
        self.files.first()
    }
}

pub fn sha256_bytes(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    format!("{:x}", h.finalize())
}

// ------------------------------------------------------------------- evidence

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A backup completed.
    Backup,
    /// Part of the repository was read back and checked.
    Subset,
    /// The canary was restored and compared against its recorded digest.
    Canary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Good,
    Bad,
    /// Could not tell. Never treated as a failure of the data.
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub at: u64,
    pub peer: String,
    pub kind: Kind,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_pct: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

pub struct Evidence;

impl Evidence {
    pub fn append(r: &Record) -> std::io::Result<()> {
        let p = evidence_path();
        fs::create_dir_all(p.parent().unwrap())?;
        let mut f = fs::OpenOptions::new().create(true).append(true).open(p)?;
        let mut line = serde_json::to_string(r).map_err(std::io::Error::other)?;
        line.push('\n');
        f.write_all(line.as_bytes())
    }

    /// Read every record. Unparsable lines are skipped rather than fatal: a
    /// crash mid-append can leave a partial last line, and losing all history
    /// because of one truncated record would be a poor trade.
    pub fn read_all() -> Vec<Record> {
        let Ok(f) = fs::File::open(evidence_path()) else {
            return Vec::new();
        };
        BufReader::new(f)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect()
    }
}

// --------------------------------------------------------------------- status

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    /// Checked recently, and everything came back correct.
    Good,
    /// Something came back wrong.
    Bad,
    /// Not checked recently enough to say.
    Unknown,
}

impl PeerState {
    pub fn label(&self) -> &'static str {
        match self {
            PeerState::Good => "ok",
            PeerState::Bad => "FAILED",
            PeerState::Unknown => "unchecked",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PeerStatus {
    pub name: String,
    pub state: PeerState,
    pub last_backup: Option<u64>,
    pub last_subset: Option<u64>,
    pub last_canary: Option<u64>,
    /// Share of stored data read back in the most recent successful check.
    pub coverage_pct: Option<u8>,
    pub problem: Option<String>,
}

/// Work out where each peer stands from the evidence on disk.
///
/// Reads local records only. It never contacts a peer, so it is instant, works
/// offline, and cannot confuse "your friend's router is rebooting" with "your
/// backup is damaged".
pub fn status(cfg: &Config, records: &[Record], now_ts: u64) -> Vec<PeerStatus> {
    let s = &cfg.settings;
    let liveness = s.liveness_hours * 3600;
    let subset_max = s.subset_days * 86400;
    let canary_max = s.canary_days * 86400;

    cfg.peers
        .iter()
        .map(|peer| {
            let mine: Vec<&Record> = records.iter().filter(|r| r.peer == peer.name).collect();
            let last = |k: Kind| mine.iter().filter(|r| r.kind == k).map(|r| r.at).max();

            // Anything that came back wrong and has not since been superseded by
            // a good check of the same kind.
            let bad = mine.iter().rev().find(|r| r.verdict == Verdict::Bad);
            let problem = bad.and_then(|r| {
                let newer_good = mine
                    .iter()
                    .any(|o| o.kind == r.kind && o.at > r.at && o.verdict == Verdict::Good);
                (!newer_good).then(|| {
                    r.detail
                        .clone()
                        .unwrap_or_else(|| "a check failed".to_string())
                })
            });

            let last_backup = last(Kind::Backup);
            let last_subset = last(Kind::Subset);
            let last_canary = last(Kind::Canary);

            let fresh =
                |t: Option<u64>, window: u64| t.is_some_and(|t| now_ts.saturating_sub(t) <= window);

            let state = if problem.is_some() {
                PeerState::Bad
            } else if fresh(last_backup, liveness)
                && fresh(last_subset, subset_max)
                && fresh(last_canary, canary_max)
            {
                PeerState::Good
            } else {
                PeerState::Unknown
            };

            let coverage_pct = mine
                .iter()
                .filter(|r| r.kind == Kind::Subset && r.verdict == Verdict::Good)
                .max_by_key(|r| r.at)
                .and_then(|r| r.coverage_pct);

            PeerStatus {
                name: peer.name.clone(),
                state,
                last_backup,
                last_subset,
                last_canary,
                coverage_pct,
                problem,
            }
        })
        .collect()
}

/// "3 hours ago", for humans reading a table.
pub fn ago(then: Option<u64>, now_ts: u64) -> String {
    let Some(t) = then else {
        return "never".into();
    };
    let d = now_ts.saturating_sub(t);
    match d {
        0..=90 => "just now".into(),
        91..=5400 => format!("{}m ago", d / 60),
        5401..=172_800 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Peer;

    fn cfg_with_peer() -> Config {
        let mut c = Config::default();
        c.peers.push(Peer {
            name: "alice".into(),
            url: "rest:http://x/".into(),
            ca_cert: None,
        });
        c
    }

    fn rec(kind: Kind, verdict: Verdict, at: u64) -> Record {
        Record {
            at,
            peer: "alice".into(),
            kind,
            verdict,
            detail: None,
            coverage_pct: Some(1),
            snapshot: None,
        }
    }

    // A realistic unix timestamp: the tests subtract up to 60 days from it.
    const NOW: u64 = 1_800_000_000;

    #[test]
    fn all_checks_fresh_means_ok() {
        let recs = vec![
            rec(Kind::Backup, Verdict::Good, NOW - 3600),
            rec(Kind::Subset, Verdict::Good, NOW - 86400),
            rec(Kind::Canary, Verdict::Good, NOW - 86400 * 5),
        ];
        assert_eq!(
            status(&cfg_with_peer(), &recs, NOW)[0].state,
            PeerState::Good
        );
    }

    #[test]
    fn a_stale_check_is_unchecked_not_failed() {
        // The distinction the whole product rests on. Old evidence means we do
        // not know, which is not the same as knowing something is wrong.
        let recs = vec![
            rec(Kind::Backup, Verdict::Good, NOW - 86400 * 30),
            rec(Kind::Subset, Verdict::Good, NOW - 86400 * 30),
            rec(Kind::Canary, Verdict::Good, NOW - 86400 * 60),
        ];
        assert_eq!(
            status(&cfg_with_peer(), &recs, NOW)[0].state,
            PeerState::Unknown
        );
    }

    #[test]
    fn unknown_verdicts_never_make_a_peer_fail() {
        // An unreachable peer produces Unknown records. However many pile up,
        // they must not add up to a failure.
        let recs: Vec<Record> = (0..20)
            .map(|i| rec(Kind::Subset, Verdict::Unknown, NOW - i * 60))
            .collect();
        assert_ne!(
            status(&cfg_with_peer(), &recs, NOW)[0].state,
            PeerState::Bad
        );
    }

    #[test]
    fn a_bad_check_fails_the_peer_and_says_why() {
        let mut bad = rec(Kind::Canary, Verdict::Bad, NOW - 600);
        bad.detail = Some("canary digest mismatch".into());
        let recs = vec![rec(Kind::Backup, Verdict::Good, NOW - 60), bad];
        let st = &status(&cfg_with_peer(), &recs, NOW)[0];
        assert_eq!(st.state, PeerState::Bad);
        assert!(st.problem.as_ref().unwrap().contains("mismatch"));
    }

    #[test]
    fn a_later_good_check_clears_an_earlier_failure() {
        let mut bad = rec(Kind::Canary, Verdict::Bad, NOW - 86400);
        bad.detail = Some("mismatch".into());
        let recs = vec![
            bad,
            rec(Kind::Canary, Verdict::Good, NOW - 3600),
            rec(Kind::Backup, Verdict::Good, NOW - 60),
            rec(Kind::Subset, Verdict::Good, NOW - 120),
        ];
        assert_eq!(
            status(&cfg_with_peer(), &recs, NOW)[0].state,
            PeerState::Good
        );
    }

    #[test]
    fn a_peer_with_no_records_is_unchecked() {
        assert_eq!(
            status(&cfg_with_peer(), &[], NOW)[0].state,
            PeerState::Unknown
        );
    }

    #[test]
    fn evidence_survives_a_truncated_last_line() {
        // A crash mid-append should cost the last record, not the history.
        let dir = std::env::temp_dir().join(format!("pb-ev-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PEERBACKUP_STATE_DIR", &dir) };

        Evidence::append(&rec(Kind::Backup, Verdict::Good, 1)).unwrap();
        Evidence::append(&rec(Kind::Backup, Verdict::Good, 2)).unwrap();
        let mut raw = fs::read_to_string(evidence_path()).unwrap();
        raw.push_str("{\"at\": 3, \"peer\": \"alic");
        fs::write(evidence_path(), raw).unwrap();

        assert_eq!(Evidence::read_all().len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ago_reads_sensibly() {
        assert_eq!(ago(None, NOW), "never");
        assert_eq!(ago(Some(NOW - 30), NOW), "just now");
        assert_eq!(ago(Some(NOW - 600), NOW), "10m ago");
        assert_eq!(ago(Some(NOW - 7200), NOW), "2h ago");
        assert_eq!(ago(Some(NOW - 86400 * 3), NOW), "3d ago");
    }
}
