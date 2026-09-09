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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Config, PeerName, home};

pub fn state_dir() -> PathBuf {
    std::env::var_os("PEERBACKUP_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share/peerbackup"))
}

pub fn canary_dir() -> PathBuf {
    state_dir().join("canary")
}

/// Seconds since the epoch.
///
/// A clock set before 1970 is the one case `duration_since` fails, and this used
/// to answer `0` for it. Zero is not a harmless default here: every record then
/// looks future-dated relative to it, `status` reads the whole log and calls
/// every peer fresh, and the dashboard goes green because the clock is broken.
/// Saturating to the epoch keeps the failure visible -- every window is exceeded
/// and every peer reads `unchecked` -- instead of inverting it.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
        Self::create_at(&canary_dir(), &Self::manifest_path())
    }

    /// The same, against explicit paths, so a test can build one in a temporary
    /// directory without pointing the whole process at it.
    pub fn create_at(dir: &Path, manifest: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(dir)?;
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
        c.save_at(manifest)?;
        Ok(c)
    }

    pub fn load_at(p: &Path) -> std::io::Result<Self> {
        let text = fs::read_to_string(p)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
        serde_json::from_str(&text).map_err(std::io::Error::other)
    }

    pub fn save_at(&self, p: &Path) -> std::io::Result<()> {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            p,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )
    }

    pub fn load_or_create() -> std::io::Result<Self> {
        Self::load_or_create_at(&canary_dir(), &Self::manifest_path())
    }

    pub fn load_or_create_at(dir: &Path, manifest: &Path) -> std::io::Result<Self> {
        match Self::load_at(manifest) {
            Ok(c) => Ok(c),
            Err(_) => Self::create_at(dir, manifest),
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

impl Kind {
    /// For naming a kind in a message when the record carried no detail.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Kind::Backup => "backup",
            Kind::Subset => "read-back",
            Kind::Canary => "test file",
        }
    }
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
    pub peer: PeerName,
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
    /// The same, against an explicit path.
    ///
    /// These exist so the tests do not have to point the whole process at a
    /// temporary directory with `std::env::set_var`, which is `unsafe` in
    /// edition 2024 for a real reason: other tests in the same binary read the
    /// environment concurrently, so a test that sets a variable is a data race
    /// against every one of them, and it passed only by luck of scheduling.
    pub fn append_to(path: &Path, r: &Record) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let mut line = serde_json::to_string(r).map_err(std::io::Error::other)?;
        line.push('\n');
        f.write_all(line.as_bytes())
    }

    /// Records newer than `oldest`, newest last. Pass `0` for everything.
    ///
    /// `status` needs at most `canary_days` of history, but this read the whole
    /// file and parsed every line of it. Hourly backups and daily verifies to
    /// three peers is tens of thousands of lines a year, growing forever,
    /// re-read on every invocation of the command people run most.
    ///
    /// Seeks to the end and walks backwards a block at a time, stopping at the
    /// first record older than the cutoff. The file is append-only and written
    /// in time order, so everything before that point is older too.
    ///
    /// The intermediate version of this stopped *parsing* early but still began
    /// with `read_to_string`, so the JSON cost was bounded and the I/O was not
    /// -- while the comment claimed both. Now the whole cost is the window.
    pub fn read_since(path: &Path, oldest: u64) -> Vec<Record> {
        use std::io::{Seek, SeekFrom};

        // Comfortably more than a window's worth of records for a normal
        // schedule, so the common case is one read.
        const BLOCK: usize = 64 * 1024;

        let Ok(mut f) = fs::File::open(path) else {
            return Vec::new();
        };
        let Ok(len) = f.seek(SeekFrom::End(0)) else {
            return Vec::new();
        };

        let mut out: Vec<Record> = Vec::new();
        let mut pos = len;
        // Bytes already read that belong to a line beginning further left.
        let mut carry: Vec<u8> = Vec::new();

        'blocks: while pos > 0 {
            let take = BLOCK.min(pos as usize);
            pos -= take as u64;
            if f.seek(SeekFrom::Start(pos)).is_err() {
                break;
            }
            let mut buf = vec![0u8; take];
            if f.read_exact(&mut buf).is_err() {
                break;
            }
            buf.extend_from_slice(&carry);

            // While there is still file to the left, the bytes before the first
            // newline are the tail of a line that starts in the next block.
            // At pos == 0 there is nothing to the left, so they are a whole line.
            let split = (pos > 0)
                .then(|| buf.iter().position(|b| *b == b'\n'))
                .flatten();
            let (keep, lines): (Vec<u8>, &[u8]) = match split {
                Some(i) => (buf[..i].to_vec(), &buf[i + 1..]),
                None => (Vec::new(), &buf[..]),
            };

            for line in lines.split(|b| *b == b'\n').rev() {
                let Ok(r) = serde_json::from_slice::<Record>(line) else {
                    // A truncated tail, a blank line, or a record from a future
                    // version. None of them should cost the history.
                    continue;
                };
                if r.at < oldest {
                    break 'blocks;
                }
                out.push(r);
            }
            carry = keep;
        }

        out.reverse();
        out
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
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            PeerState::Good => "ok",
            PeerState::Bad => "FAILED",
            PeerState::Unknown => "unchecked",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PeerStatus {
    pub name: PeerName,
    pub state: PeerState,
    pub last_backup: Option<u64>,
    pub last_subset: Option<u64>,
    pub last_canary: Option<u64>,
    /// Share of stored data read back in the most recent successful check,
    /// and only while that check is still within its window.
    pub coverage_pct: Option<u8>,
    pub problem: Option<String>,
    /// This peer has records dated further ahead than the clock can explain.
    /// They are excluded from freshness rather than believed; see
    /// [`CLOCK_SKEW_TOLERANCE_SECS`].
    pub clock_skew: bool,
}

/// How far ahead of the local clock a record may be dated and still be treated
/// as evidence.
///
/// Records carry the wall clock of the machine that wrote them, and that clock
/// can be wrong: a homeserver with no RTC, a container started with a skewed
/// time, a hypervisor with a drifting TSC. A record dated in the future used to
/// be unconditionally "fresh", because the freshness test is
/// `now - at <= window` on saturating arithmetic and `now - at` clamps to zero.
/// One backup taken while the clock was ahead therefore pinned a peer green
/// permanently, and a clock set backwards greened every peer at once.
///
/// A few minutes of tolerance covers ordinary NTP correction. Anything past it
/// is not evidence of anything, and is reported rather than believed.
const CLOCK_SKEW_TOLERANCE_SECS: u64 = 300;

/// Work out where each peer stands from the evidence on disk.
///
/// Reads local records only. It never contacts a peer, so it is instant, works
/// offline, and cannot confuse "your friend's router is rebooting" with "your
/// backup is damaged".
///
/// Two orderings are in play here and they are deliberately different.
///
/// **Supersession is decided by position in the log, not by timestamp,** and
/// only a `Good` supersedes. The log is append-only and written in the order
/// things happened, so folding it in order says what currently stands whatever
/// the clock was doing. Deciding it by timestamp let a future-dated `Good` bury
/// a real `Bad`, and let two records written in the same second -- `verify`
/// writes its subset and canary results within one -- resolve in the wrong
/// order, because the comparison was strictly `>`.
///
/// **Freshness is decided by timestamp,** because it is the only thing that can
/// answer "how long ago". Records dated past [`CLOCK_SKEW_TOLERANCE_SECS`] are
/// excluded from it entirely.
pub fn status(cfg: &Config, records: &[Record], now_ts: u64) -> Vec<PeerStatus> {
    let s = &cfg.settings;
    let liveness = s.liveness_hours * 3600;
    let subset_max = s.subset_days * 86400;
    let canary_max = s.canary_days * 86400;
    let horizon = now_ts.saturating_add(CLOCK_SKEW_TOLERANCE_SECS);

    cfg.peers
        .iter()
        .map(|peer| {
            let mine: Vec<&Record> = records.iter().filter(|r| r.peer == peer.name).collect();

            // Damage of a given kind that nothing has since refuted, folded in
            // log order: `Bad` raises it, `Good` clears it, and `Unknown` leaves
            // it exactly where it was. That last arm is the point -- "we could
            // not check" says nothing about whether the damage is still there,
            // so a timed-out verify after a pack-hash mismatch must not read as
            // the mismatch having gone away.
            let unresolved = |k: Kind| {
                let mut standing: Option<&Record> = None;
                for r in mine.iter().filter(|r| r.kind == k) {
                    match r.verdict {
                        Verdict::Bad => standing = Some(r),
                        Verdict::Good => standing = None,
                        Verdict::Unknown => {}
                    }
                }
                standing
            };

            // Only successful checks count towards freshness. Counting attempts
            // means a peer that is full, unreachable or misconfigured keeps
            // reporting "backed up just now" while receiving nothing.
            let last = |k: Kind| {
                mine.iter()
                    .filter(|r| r.kind == k && r.verdict == Verdict::Good && r.at <= horizon)
                    .map(|r| r.at)
                    .max()
            };

            // Every kind whose latest word is `Bad`, not just the single most
            // recent `Bad` overall.
            //
            // The rule this replaces looked at one record: the newest `Bad` of
            // any kind, cleared by a newer `Good` of that same kind. An older
            // `Bad` of a *different* kind that nothing had superseded was never
            // examined. One verify where both the subset check and the canary
            // fail, followed by one where the canary recovers and the subset
            // check merely times out, was enough to report `ok` and exit 0 with
            // an unrefuted pack-hash mismatch in the log -- the single failure
            // this program exists to prevent.
            let problems: Vec<String> = [Kind::Backup, Kind::Subset, Kind::Canary]
                .into_iter()
                .filter_map(unresolved)
                .map(|r| {
                    r.detail
                        .clone()
                        .unwrap_or_else(|| format!("{} check failed", r.kind.label()))
                })
                .collect();
            let problem = (!problems.is_empty()).then(|| problems.join("; "));

            let clock_skew = mine.iter().any(|r| r.at > horizon);

            let last_backup = last(Kind::Backup);
            let last_subset = last(Kind::Subset);
            let last_canary = last(Kind::Canary);

            let fresh =
                |t: Option<u64>, window: u64| t.is_some_and(|t| now_ts.saturating_sub(t) <= window);

            let subset_fresh = fresh(last_subset, subset_max);
            let state = if problem.is_some() {
                PeerState::Bad
            } else if fresh(last_backup, liveness) && subset_fresh && fresh(last_canary, canary_max)
            {
                PeerState::Good
            } else {
                PeerState::Unknown
            };

            // Gated on the subset check still being fresh. Ungated, a peer whose
            // last successful check was three weeks ago printed `unchecked` and
            // `100%` on the same row, which reads as "all of it was read back
            // and we are unsure about something else".
            let coverage_pct = subset_fresh
                .then(|| {
                    mine.iter()
                        .rev()
                        .find(|r| r.kind == Kind::Subset && r.verdict == Verdict::Good)
                        .and_then(|r| r.coverage_pct)
                })
                .flatten();

            PeerStatus {
                name: peer.name.clone(),
                state,
                last_backup,
                last_subset,
                last_canary,
                coverage_pct,
                problem,
                clock_skew,
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
            name: PeerName::new("alice").unwrap(),
            url: "rest:http://x/".into(),
            ca_cert: None,
        });
        c
    }

    fn rec(kind: Kind, verdict: Verdict, at: u64) -> Record {
        Record {
            at,
            peer: PeerName::new("alice").unwrap(),
            kind,
            verdict,
            detail: None,
            coverage_pct: Some(1),
            snapshot: None,
        }
    }

    // A realistic unix timestamp: the tests subtract up to 60 days from it.
    const NOW: u64 = 1_800_000_000;

    fn detailed(kind: Kind, verdict: Verdict, at: u64, detail: &str) -> Record {
        Record {
            detail: Some(detail.to_owned()),
            ..rec(kind, verdict, at)
        }
    }

    #[test]
    fn corruption_of_one_kind_is_not_cleared_by_a_good_check_of_another() {
        // The critical finding. The rule this replaces looked at the single most
        // recent `Bad` of any kind and cleared it with a newer `Good` of that
        // same kind, so an unsuperseded `Bad` of a *different* kind was never
        // examined. This exact log printed `alice ok` and exited 0 with an
        // unrefuted pack-hash mismatch an hour old in it.
        let recs = vec![
            rec(Kind::Subset, Verdict::Good, NOW - 7200),
            detailed(
                Kind::Subset,
                Verdict::Bad,
                NOW - 3600,
                "pack 4f2a hash mismatch",
            ),
            detailed(
                Kind::Canary,
                Verdict::Bad,
                NOW - 1800,
                "transient restore failure",
            ),
            rec(Kind::Canary, Verdict::Good, NOW - 600),
            rec(Kind::Backup, Verdict::Good, NOW - 300),
        ];
        let r = &status(&cfg_with_peer(), &recs, NOW)[0];
        assert_eq!(r.state, PeerState::Bad, "observed damage must be reported");
        assert!(
            r.problem.as_deref().unwrap_or_default().contains("4f2a"),
            "must name the damage it found: {:?}",
            r.problem
        );
    }

    #[test]
    fn a_check_that_could_not_run_does_not_clear_observed_damage() {
        // "Could not check" says nothing about whether the damage is still
        // there. Letting an Unknown supersede a Bad turns a peer with a known
        // pack-hash mismatch into a peer we are merely unsure about, and the
        // detail stops being printed at all.
        let recs = vec![
            detailed(
                Kind::Subset,
                Verdict::Bad,
                NOW - 86400 * 2,
                "pack 4f2a hash mismatch",
            ),
            detailed(
                Kind::Canary,
                Verdict::Bad,
                NOW - 86400 * 2,
                "test file did not match",
            ),
            detailed(
                Kind::Subset,
                Verdict::Unknown,
                NOW - 3600,
                "timed out after 3600s",
            ),
            rec(Kind::Canary, Verdict::Good, NOW - 3500),
            rec(Kind::Backup, Verdict::Good, NOW - 3400),
        ];
        let r = &status(&cfg_with_peer(), &recs, NOW)[0];
        assert_eq!(r.state, PeerState::Bad);
        assert!(
            r.problem.as_deref().unwrap_or_default().contains("4f2a"),
            "{:?}",
            r.problem
        );
    }

    #[test]
    fn every_kind_that_currently_stands_bad_is_reported_not_just_one() {
        let recs = vec![
            detailed(
                Kind::Subset,
                Verdict::Bad,
                NOW - 3600,
                "pack 4f2a hash mismatch",
            ),
            detailed(
                Kind::Canary,
                Verdict::Bad,
                NOW - 1800,
                "test file did not match",
            ),
        ];
        let p = status(&cfg_with_peer(), &recs, NOW)[0]
            .problem
            .clone()
            .unwrap();
        assert!(p.contains("4f2a"), "{p}");
        assert!(p.contains("test file"), "{p}");
    }

    #[test]
    fn a_good_check_of_the_same_kind_does_still_clear_it() {
        // The other direction: supersession has to keep working, or a peer that
        // was repaired reads FAILED forever.
        let recs = vec![
            detailed(
                Kind::Subset,
                Verdict::Bad,
                NOW - 7200,
                "pack 4f2a hash mismatch",
            ),
            rec(Kind::Subset, Verdict::Good, NOW - 3600),
            rec(Kind::Backup, Verdict::Good, NOW - 300),
            rec(Kind::Canary, Verdict::Good, NOW - 300),
        ];
        let r = &status(&cfg_with_peer(), &recs, NOW)[0];
        assert_eq!(r.state, PeerState::Good, "{:?}", r.problem);
    }

    #[test]
    fn supersession_is_decided_by_log_order_not_by_timestamp() {
        // `verify` writes its subset and canary results inside the same second,
        // and `now()` has one-second resolution. Comparing timestamps strictly
        // left a same-second Bad-then-Good peer FAILED until the next run; the
        // reverse order must not be resolved the reassuring way either.
        let bad_then_good = vec![
            detailed(
                Kind::Subset,
                Verdict::Bad,
                NOW - 60,
                "pack 4f2a hash mismatch",
            ),
            rec(Kind::Subset, Verdict::Good, NOW - 60),
            rec(Kind::Backup, Verdict::Good, NOW - 60),
            rec(Kind::Canary, Verdict::Good, NOW - 60),
        ];
        assert_eq!(
            status(&cfg_with_peer(), &bad_then_good, NOW)[0].state,
            PeerState::Good
        );

        let good_then_bad = vec![
            rec(Kind::Subset, Verdict::Good, NOW - 60),
            detailed(
                Kind::Subset,
                Verdict::Bad,
                NOW - 60,
                "pack 4f2a hash mismatch",
            ),
        ];
        assert_eq!(
            status(&cfg_with_peer(), &good_then_bad, NOW)[0].state,
            PeerState::Bad
        );
    }

    #[test]
    fn a_future_dated_record_does_not_pin_a_peer_green() {
        // The second critical finding. `now - at` saturates to zero for a record
        // dated ahead, so it satisfied every window forever. One backup taken
        // while the clock was ahead greened a peer permanently, even though
        // every subsequent backup failed.
        let recs = vec![
            rec(Kind::Backup, Verdict::Good, NOW + 86400 * 365),
            rec(Kind::Subset, Verdict::Good, NOW + 86400 * 365),
            rec(Kind::Canary, Verdict::Good, NOW + 86400 * 365),
        ];
        let r = &status(&cfg_with_peer(), &recs, NOW)[0];
        assert_eq!(r.state, PeerState::Unknown);
        assert!(r.clock_skew, "the reason must be reportable");
        assert_eq!(r.last_backup, None, "a future date is not a backup time");
    }

    #[test]
    fn a_clock_set_backwards_does_not_green_every_peer() {
        // The mirror case: correct records, a `now` behind them.
        let recs = vec![
            rec(Kind::Backup, Verdict::Good, NOW),
            rec(Kind::Subset, Verdict::Good, NOW),
            rec(Kind::Canary, Verdict::Good, NOW),
        ];
        let long_ago = NOW - 86400 * 365 * 5;
        assert_eq!(
            status(&cfg_with_peer(), &recs, long_ago)[0].state,
            PeerState::Unknown
        );
    }

    #[test]
    fn a_future_dated_good_cannot_bury_a_real_bad() {
        // Supersession by log order is what makes this safe: the Bad is written
        // after the Good, so it stands, whatever the two timestamps say.
        let recs = vec![
            rec(Kind::Subset, Verdict::Good, NOW + 86400 * 365),
            detailed(
                Kind::Subset,
                Verdict::Bad,
                NOW - 60,
                "pack 4f2a hash mismatch",
            ),
        ];
        assert_eq!(
            status(&cfg_with_peer(), &recs, NOW)[0].state,
            PeerState::Bad
        );
    }

    #[test]
    fn a_small_clock_wobble_is_tolerated() {
        // NTP correcting a few seconds must not read as a broken clock.
        let recs = vec![
            rec(Kind::Backup, Verdict::Good, NOW + 30),
            rec(Kind::Subset, Verdict::Good, NOW + 30),
            rec(Kind::Canary, Verdict::Good, NOW + 30),
        ];
        let r = &status(&cfg_with_peer(), &recs, NOW)[0];
        assert_eq!(r.state, PeerState::Good);
        assert!(!r.clock_skew);
    }

    #[test]
    fn coverage_is_not_reported_once_the_check_behind_it_has_gone_stale() {
        // `alice unchecked ... 100%` on one row reads as "all of it was read
        // back and we are unsure about something else".
        let recs = vec![rec(Kind::Subset, Verdict::Good, NOW - 86400 * 30)];
        let r = &status(&cfg_with_peer(), &recs, NOW)[0];
        assert_eq!(r.state, PeerState::Unknown);
        assert_eq!(r.coverage_pct, None);
    }

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
        //
        // This used to point the whole process at a temp directory with
        // `std::env::set_var`, which races every other test in this binary that
        // reads the environment, and never put it back. The path is an argument
        // now, so the test touches nothing outside its own directory.
        let dir = std::env::temp_dir().join(format!("pb-ev-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("evidence.jsonl");

        Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, 1)).unwrap();
        Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, 2)).unwrap();
        let mut raw = fs::read_to_string(&path).unwrap();
        raw.push_str("{\"at\": 3, \"peer\": \"alic");
        fs::write(&path, raw).unwrap();

        assert_eq!(Evidence::read_since(&path, 0).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reading_a_window_stops_at_the_cutoff() {
        // status only consults a bounded window, but used to parse every record
        // ever written. This walks backwards and stops, so the cost is the
        // window rather than the history.
        let dir = std::env::temp_dir().join(format!("pb-win-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("evidence.jsonl");

        for at in [100u64, 200, 300, 400] {
            Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, at)).unwrap();
        }
        let recent = Evidence::read_since(&path, 250);
        assert_eq!(recent.len(), 2, "only records at or after the cutoff");
        assert_eq!(recent.first().unwrap().at, 300, "oldest first, as written");
        assert_eq!(recent.last().unwrap().at, 400);
        assert_eq!(
            Evidence::read_since(&path, 0).len(),
            4,
            "0 means everything"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_spanning_a_block_boundary_are_not_lost_or_duplicated() {
        // read_since walks the file backwards in 64KiB blocks, so a record that
        // straddles a boundary is only correct if the leftover bytes are carried
        // into the next block. At roughly 130 bytes a record, 2000 records is
        // several blocks.
        let dir = std::env::temp_dir().join(format!("pb-blocks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("evidence.jsonl");

        let total = 2000u64;
        for at in 1..=total {
            Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, at)).unwrap();
        }
        assert!(
            fs::metadata(&path).unwrap().len() > 64 * 1024,
            "the fixture must be larger than one block or this proves nothing"
        );

        let all = Evidence::read_since(&path, 0);
        assert_eq!(all.len(), total as usize, "every record must survive");
        assert_eq!(all.first().unwrap().at, 1, "oldest first, as written");
        assert_eq!(all.last().unwrap().at, total);
        let ats: Vec<u64> = all.iter().map(|r| r.at).collect();
        assert!(ats.windows(2).all(|w| w[1] == w[0] + 1), "order must hold");

        // And the window still stops early across blocks.
        let recent = Evidence::read_since(&path, total - 99);
        assert_eq!(recent.len(), 100);
        assert_eq!(recent.first().unwrap().at, total - 99);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_evidence_file_reads_as_no_history_not_an_error() {
        assert!(Evidence::read_since(Path::new("/nope/evidence.jsonl"), 0).is_empty());
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

#[cfg(test)]
mod scenario_tests {
    use super::*;
    use crate::config::Peer;

    #[test]
    fn a_peer_that_is_full_must_not_look_ok() {
        // A peer with no room fails every backup. Those failures are recorded
        // as Unknown, because a failed upload says nothing about the data
        // already stored. But they must not make the peer look freshly backed
        // up: that is a green light for a peer receiving nothing.
        let mut cfg = Config::default();
        cfg.peers.push(Peer {
            name: PeerName::new("full").unwrap(),
            url: "rest:http://x/".into(),
            ca_cert: None,
        });
        let now_ts = 1_800_000_000u64;
        let r = |kind, verdict, at| Record {
            at,
            peer: PeerName::new("full").unwrap(),
            kind,
            verdict,
            detail: None,
            coverage_pct: Some(1),
            snapshot: None,
        };
        let recs = vec![
            // Everything was fine a month ago.
            r(Kind::Backup, Verdict::Good, now_ts - 86400 * 30),
            r(Kind::Subset, Verdict::Good, now_ts - 86400 * 2),
            r(Kind::Canary, Verdict::Good, now_ts - 86400 * 2),
            // Since then every backup has failed for lack of space.
            r(Kind::Backup, Verdict::Unknown, now_ts - 3600),
        ];
        let st = &status(&cfg, &recs, now_ts)[0];
        assert_ne!(
            st.state,
            PeerState::Good,
            "a peer whose backups all fail must not report ok"
        );
    }
}
