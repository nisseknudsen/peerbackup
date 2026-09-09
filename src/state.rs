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
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Config, PeerName, create_dir_private, home, write_private};

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
        create_dir_private(dir)?;
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

    /// Atomically, through the same helper the config uses.
    ///
    /// This was `fs::write`, which truncates and then writes, so a crash or a
    /// full disk part-way through left a half-written manifest. The next
    /// `backup` then could not parse it, silently regenerated the canary with
    /// fresh contents and fresh digests, and if *that* backup failed for any
    /// reason -- peer unreachable, out of space, upload aborted -- the peer's
    /// newest snapshot still held the old bytes while the manifest held the new
    /// digest. The next `verify` restored the old bytes, compared them against
    /// the new digest, and reported `FAILED -- restored test file did not match
    /// what was sent` for a purely local cause.
    ///
    /// A false red costs the same trust as a false green, one iteration later.
    pub fn save_at(&self, p: &Path) -> std::io::Result<()> {
        write_private(
            p,
            &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )
    }

    pub fn load_or_create() -> std::io::Result<Self> {
        Self::load_or_create_at(&canary_dir(), &Self::manifest_path())
    }

    /// Load the manifest, check it still describes the files on disk, and only
    /// create a new canary when there is genuinely nothing usable.
    ///
    /// The manifest was trusted without ever being reconciled against the files
    /// it describes, which fails in both directions:
    ///
    /// * A canary file that changed locally -- an errant edit, a bad block, a
    ///   state directory restored from another machine -- meant `backup`
    ///   uploaded the new bytes while the manifest kept the old digest, so
    ///   `verify` reported the *peer* as damaged for a problem on this disk.
    /// * Canary files deleted while the directory and manifest survived passed
    ///   `check_sources`, produced backups carrying no canary at all, and left
    ///   every `verify` recording `Unknown` with no explanation.
    ///
    /// Re-hashing three 64KiB files costs nothing next to a backup.
    pub fn load_or_create_at(dir: &Path, manifest: &Path) -> std::io::Result<Self> {
        match Self::load_at(manifest) {
            Ok(c) => match c.disagreement_with_disk() {
                None => Ok(c),
                Some(why) => {
                    // Said out loud. Regenerating silently is what turned a
                    // local problem into a report about the peer.
                    eprintln!(
                        "warning: the test files no longer match what was recorded ({why}).\n                           Making a new set. Until the next backup reaches a peer and is \
                         verified,\n  that peer's test-file check will read `unchecked`."
                    );
                    Self::create_at(dir, manifest)
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::create_at(dir, manifest),
            Err(e) => {
                eprintln!(
                    "warning: could not read the test-file manifest ({e}). Making a new set."
                );
                Self::create_at(dir, manifest)
            }
        }
    }

    /// Why the recorded digests do not describe what is on disk, if they do not.
    fn disagreement_with_disk(&self) -> Option<String> {
        if self.files.is_empty() {
            return Some("it lists no files".into());
        }
        for f in &self.files {
            match fs::read(&f.path) {
                Err(e) => return Some(format!("{}: {e}", f.path.display())),
                Ok(bytes) if sha256_bytes(&bytes) != f.sha256 => {
                    return Some(format!("{} has changed", f.path.display()));
                }
                Ok(_) => {}
            }
        }
        None
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

/// Identify the repository a record is about, from its URL.
///
/// Truncated SHA-256, not the URL itself: the URL carries the peer's HTTP
/// credentials and this value is written to a file and printed in diagnostics.
/// Sixteen hex characters is 64 bits, which is far more than enough to tell one
/// friend's server from another's.
#[must_use]
pub fn repo_id(url: &str) -> String {
    sha256_bytes(url.as_bytes()).chars().take(16).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub at: u64,
    pub peer: PeerName,
    /// Which repository this is about; see [`repo_id`].
    ///
    /// Records were keyed on the peer name alone, and a name is not an identity.
    /// `peer remove alice` deliberately keeps the evidence, so `peer add alice
    /// <a-different-friend>` adopted it: a brand-new empty server reported
    /// `alice ok / 60m ago / 58m ago / 56m ago / 1%` and exited 0, one command
    /// after `peer add` had printed "No data has been sent yet".
    ///
    /// `None` on records written before this existed. Those are still honoured
    /// for damage -- an unrefuted `Bad` does not stop being true -- but not for
    /// freshness, because "we cannot tell which repository this was about" must
    /// not read as "recently confirmed good".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub kind: Kind,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage_pct: Option<u8>,
    /// The snapshot this record is about, when there is one.
    ///
    /// Written by `backup`, read by `verify`: a peer that no longer lists a
    /// snapshot it accepted has dropped data it acknowledged, and nothing else
    /// in this program would notice. Storage is append-only precisely so that
    /// cannot happen, so it happening is worth reporting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

/// What a peer's history says, plus whether we managed to read all of it.
///
/// `incomplete` exists because the old signature was `Vec<Record>` and every
/// I/O failure -- a bad sector, a root-owned file left by one `sudo` run --
/// returned a silently *partial* history that the caller could not tell from a
/// complete one. A read error on the evidence log is itself evidence that
/// something is wrong, and it must not resolve to a green dashboard.
#[derive(Debug, Default)]
pub struct History {
    pub records: Vec<Record>,
    pub incomplete: Option<String>,
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
    ///
    /// The write is synced before returning. `Runtime::record` goes to some
    /// trouble over write *errors* for a `Bad` verdict -- "the next `status`
    /// would call this peer healthy" -- but a successful `write_all` only
    /// reaches the page cache, which on ext4 defaults is up to thirty seconds
    /// of exposure. `verify` observing damage, printing `DOES NOT MATCH`, and
    /// then losing the record to a power cut is the same failure by a slower
    /// route, on machines that are by definition the ones people lose.
    pub fn append_to(path: &Path, r: &Record) -> std::io::Result<()> {
        let mut line = serde_json::to_string(r).map_err(std::io::Error::other)?;
        line.push('\n');

        let parent = path.parent();
        let fresh = !path.exists();
        if let Some(p) = parent {
            create_dir_private(p)?;
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            // 0600 from the moment it exists, matching the config beside it.
            // The log carries peer names, the exact backup schedule, and
            // restic's error text, and under a umask of 002 -- common in
            // containers and on Debian with per-user groups -- the default
            // would be group-*writable*, which makes records forgeable.
            .mode(0o600)
            .open(path)?;
        f.write_all(line.as_bytes())?;
        f.sync_data()?;
        // A rename is not involved, but the file's first appearance in the
        // directory is a directory operation and is journalled separately.
        if fresh && let Some(p) = parent {
            fs::File::open(p)?.sync_all()?;
        }
        Ok(())
    }

    /// Records at or after `oldest`, newest last. Pass `0` for everything.
    ///
    /// A thin wrapper over [`Evidence::read`] with nothing to resolve, for
    /// callers that only want a time window.
    #[cfg(test)]
    pub fn read_since(path: &Path, oldest: u64) -> Vec<Record> {
        Self::read(path, oldest, &[]).records
    }

    /// The history `status` needs: a time window, plus however much further back
    /// it takes to learn where each of `resolve_for` currently stands.
    ///
    /// `status` needs at most a window of history to answer "how long ago", but
    /// reading only a window is not enough to answer "is anything wrong". A
    /// `Bad` record that nothing has refuted does not stop being true because it
    /// is old -- and with a window alone, an observed pack-hash mismatch quietly
    /// downgraded from `FAILED` to `unchecked` the day it aged out. So the walk
    /// also continues until, for every peer named, it has seen a `Good` or a
    /// `Bad` of every kind. Walking backwards, the first of those it meets is
    /// the one that decides the kind, because `Unknown` neither raises nor
    /// clears anything.
    ///
    /// Cost: the file is append-only and written in time order, so the walk
    /// seeks to the end and reads backwards a block at a time. The common case
    /// -- a peer checked within its windows -- resolves in the first block. The
    /// walk past the cutoff is bounded by [`MAX_RESOLVE_BYTES`] so that a peer
    /// which has never had a check of some kind cannot turn every `status` into
    /// a full scan of a log that grows forever.
    pub fn read(path: &Path, oldest: u64, resolve_for: &[PeerName]) -> History {
        use std::io::{Seek, SeekFrom};

        // Comfortably more than a window's worth of records for a normal
        // schedule, so the common case is one read.
        const BLOCK: usize = 64 * 1024;

        let mut h = History::default();
        let mut f = match fs::File::open(path) {
            Ok(f) => f,
            // A log that does not exist yet is a complete history of nothing.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return h,
            Err(e) => {
                h.incomplete = Some(format!("{} could not be opened: {e}", path.display()));
                return h;
            }
        };
        let len = match f.seek(SeekFrom::End(0)) {
            Ok(n) => n,
            Err(e) => {
                h.incomplete = Some(format!("{} could not be read: {e}", path.display()));
                return h;
            }
        };

        let mut pending = Unresolved::new(resolve_for);
        let mut pos = len;
        // Bytes already read that belong to a line beginning further left.
        let mut carry: Vec<u8> = Vec::new();

        // A run of records older than the cutoff, rather than the first one.
        // The walk used to stop at the first, on the assumption that an
        // append-only file is in time order -- so one record written while the
        // clock was wrong discarded everything to its left, permanently. A
        // handful of stragglers is a bad clock; a real cutoff is followed by the
        // entire rest of the file.
        const OLD_RUN: usize = 8;
        let mut consecutive_old = 0usize;

        'blocks: while pos > 0 {
            // The reach-back past the cutoff is bounded so that a peer which has
            // never had a check of some kind cannot turn every `status` into a
            // full scan.
            if len - pos > MAX_RESOLVE_BYTES {
                break;
            }
            let take = BLOCK.min(pos as usize);
            pos -= take as u64;
            if let Err(e) = f.seek(SeekFrom::Start(pos)) {
                h.incomplete = Some(format!("{} could not be read: {e}", path.display()));
                break;
            }
            let mut buf = vec![0u8; take];
            if let Err(e) = f.read_exact(&mut buf) {
                h.incomplete = Some(format!("{} could not be read: {e}", path.display()));
                break;
            }
            buf.extend_from_slice(&carry);

            // While there is still file to the left, the bytes before the first
            // newline are the tail of a line that starts in the next block.
            // At pos == 0 there is nothing to the left, so they are a whole line.
            //
            // A block with no newline in it at all is entirely the middle of one
            // very long line, so *everything* here has to be carried. Resetting
            // the carry to empty in that case -- which is what this did -- threw
            // away the right-hand half of the record, so when the left-hand half
            // was finally reached the two could never be rejoined and the record
            // was lost. `detail` holds restic's whole combined output, which is
            // megabytes for a backup that failed over a large tree, so the
            // record this dropped was reliably the most broken peer's.
            let split = (pos > 0)
                .then(|| buf.iter().position(|b| *b == b'\n'))
                .flatten();
            let (keep, lines): (Vec<u8>, &[u8]) = match split {
                Some(i) => (buf[..i].to_vec(), &buf[i + 1..]),
                None if pos > 0 => (buf.clone(), &[]),
                None => (Vec::new(), &buf[..]),
            };

            for line in lines.split(|b| *b == b'\n').rev() {
                let Ok(r) = serde_json::from_slice::<Record>(line) else {
                    // A truncated tail, a blank line, or a record from a future
                    // version. None of them should cost the history.
                    continue;
                };
                if r.at < oldest {
                    consecutive_old += 1;
                } else {
                    consecutive_old = 0;
                }
                // Asked before `saw`, or the very record that resolves a kind is
                // the one dropped for being outside the window.
                let keep = r.at >= oldest || pending.wanted(&r);
                pending.saw(&r);
                if keep {
                    h.records.push(r);
                }
                if pending.done() && consecutive_old >= OLD_RUN {
                    break 'blocks;
                }
            }
            carry = keep;
        }

        h.records.reverse();
        h
    }
}

/// Hard ceiling on how far back the walk will go, in bytes.
///
/// Bounds the case where a peer has never had a check of some kind, which would
/// otherwise never resolve and would turn every `status` into a full scan. At
/// roughly 150 bytes a record this is some tens of thousands of records, which
/// is well over a year of an hourly schedule. A `Bad` older than that with no
/// intervening `Good` means the peer has gone unverified for longer than the log
/// covers, and it reads `unchecked` on freshness alone.
const MAX_RESOLVE_BYTES: u64 = 8 * 1024 * 1024;

/// Tracks which `(peer, kind)` pairs still have no `Good` or `Bad` behind them.
struct Unresolved(Vec<(PeerName, Kind)>);

impl Unresolved {
    fn new(peers: &[PeerName]) -> Self {
        Self(
            peers
                .iter()
                .flat_map(|p| [Kind::Backup, Kind::Subset, Kind::Canary].map(|k| (p.clone(), k)))
                .collect(),
        )
    }

    /// True while this record is one the walk is still reaching back for, so it
    /// is kept even though it sits outside the time window.
    fn wanted(&self, r: &Record) -> bool {
        r.verdict != Verdict::Unknown && self.0.iter().any(|(p, k)| *k == r.kind && r.peer == *p)
    }

    fn saw(&mut self, r: &Record) {
        if r.verdict != Verdict::Unknown {
            self.0.retain(|(p, k)| !(*k == r.kind && r.peer == *p));
        }
    }

    fn done(&self) -> bool {
        self.0.is_empty()
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
            let id = repo_id(&peer.url);
            let mine: Vec<&Record> = records
                .iter()
                .filter(|r| r.peer == peer.name && r.repo.as_ref().is_none_or(|r| *r == id))
                .collect();
            // Freshness needs to know the record was about *this* repository.
            // Damage does not: an unrefuted `Bad` under this name is worth
            // reporting even if we cannot prove which server it came from.
            let confirmed: Vec<&Record> = mine
                .iter()
                .copied()
                .filter(|r| r.repo.as_deref() == Some(id.as_str()))
                .collect();

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
                confirmed
                    .iter()
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

            let clock_skew = confirmed.iter().any(|r| r.at > horizon);

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
                    confirmed
                        .iter()
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

/// A Unix timestamp as `YYYY-MM-DD HH:MM:SS UTC`.
///
/// Hand-rolled rather than pulling in a date library for one line of one file.
/// The civil-from-days conversion is Howard Hinnant's, which is exact for every
/// date the Gregorian calendar covers; the only assumption is that the input is
/// seconds since the epoch, which is what [`now`] produces.
///
/// UTC, deliberately. The recovery file may be read on another machine in
/// another place, and a local time with no zone on it is worse than no time.
#[must_use]
pub fn utc_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Days since 1970-01-01 -> civil date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
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
    use std::os::unix::fs::PermissionsExt;

    fn cfg_with_peer() -> Config {
        let mut c = Config::default();
        c.peers.push(Peer {
            name: PeerName::new("alice").unwrap(),
            url: "rest:http://x/".into(),
            ca_cert: None,
        });
        c
    }

    // The URL every test peer uses, so `rec` and `cfg_with_peer` agree on which
    // repository the records are about.
    const TEST_URL: &str = "rest:http://x/";

    fn rec(kind: Kind, verdict: Verdict, at: u64) -> Record {
        Record {
            at,
            peer: PeerName::new("alice").unwrap(),
            repo: Some(repo_id(TEST_URL)),
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
    fn history_under_a_reused_name_does_not_transfer_to_a_different_repository() {
        // Records were keyed on the peer name alone, and a name is not an
        // identity. `peer remove alice` deliberately keeps the evidence, so
        // `peer add alice <a-different-friend>` adopted it: a brand-new empty
        // server reported `alice ok ... 1%` and exited 0, one command after
        // `peer add` printed "No data has been sent yet".
        let recs = vec![
            rec(Kind::Backup, Verdict::Good, NOW - 3600),
            rec(Kind::Subset, Verdict::Good, NOW - 3500),
            rec(Kind::Canary, Verdict::Good, NOW - 3400),
        ];
        // Same records, same name, pointed at somewhere else.
        let mut moved = Config::default();
        moved.peers.push(Peer {
            name: PeerName::new("alice").unwrap(),
            url: "rest:http://a-different-friend/me/".into(),
            ca_cert: None,
        });
        let r = &status(&moved, &recs, NOW)[0];
        assert_eq!(r.state, PeerState::Unknown, "{:?}", r.problem);
        assert_eq!(r.last_backup, None, "this server holds nothing");

        // And the original peer is unaffected.
        assert_eq!(
            status(&cfg_with_peer(), &recs, NOW)[0].state,
            PeerState::Good
        );
    }

    #[test]
    fn damage_recorded_before_repository_ids_existed_is_still_reported() {
        // Legacy records carry no id. Ignoring them for freshness is the safe
        // direction -- "we cannot tell which repository this was" must not read
        // as "recently confirmed good" -- but an unrefuted Bad does not stop
        // being true because the record predates the field.
        let legacy = Record {
            repo: None,
            detail: Some("pack 4f2a hash mismatch".into()),
            ..rec(Kind::Subset, Verdict::Bad, NOW - 3600)
        };
        let r = &status(&cfg_with_peer(), &[legacy], NOW)[0];
        assert_eq!(r.state, PeerState::Bad);
        assert!(r.problem.as_deref().unwrap_or_default().contains("4f2a"));
    }

    #[test]
    fn a_legacy_good_record_does_not_count_as_a_recent_check() {
        let legacy = Record {
            repo: None,
            ..rec(Kind::Backup, Verdict::Good, NOW - 60)
        };
        let r = &status(&cfg_with_peer(), &[legacy], NOW)[0];
        assert_eq!(r.last_backup, None);
        assert_eq!(r.state, PeerState::Unknown);
    }

    #[test]
    fn a_repository_id_is_stable_and_carries_no_credential() {
        let url = "rest:https://me:hunter2@alice.example.org:8000/me/";
        let id = repo_id(url);
        assert_eq!(id, repo_id(url), "stable");
        assert_ne!(
            id,
            repo_id("rest:https://me:hunter2@bob.example.org:8000/me/")
        );
        assert!(!id.contains("hunter2"));
        assert_eq!(id.len(), 16);
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
    fn a_timestamp_renders_as_a_date_a_person_can_read() {
        // The recovery file said `Exported: 1788934589`, in a document meant to
        // be printed and read years later by someone who has just lost a
        // machine.
        assert_eq!(utc_date(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_date(1_000_000_000), "2001-09-09 01:46:40 UTC");
        // A leap day, and the day after.
        assert_eq!(utc_date(1_709_164_800), "2024-02-29 00:00:00 UTC");
        assert_eq!(utc_date(1_709_251_200), "2024-03-01 00:00:00 UTC");
        // 2000 is a leap year, 1900 was not; the century rules have to hold.
        assert_eq!(utc_date(951_782_400), "2000-02-29 00:00:00 UTC");
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

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pb-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_locally_modified_canary_file_is_noticed_rather_than_blamed_on_the_peer() {
        // The manifest was trusted without ever being compared to the files it
        // describes. A canary file that changed on this disk meant `backup`
        // uploaded the new bytes while the manifest kept the old digest, so
        // `verify` restored exactly what it had just sent, saw a mismatch, and
        // reported the *peer* as damaged.
        let dir = scratch("canarymod");
        let manifest = dir.join("canary.json");
        let cdir = dir.join("canary");
        let c = Canary::create_at(&cdir, &manifest).unwrap();
        assert!(
            c.disagreement_with_disk().is_none(),
            "a fresh canary agrees"
        );

        fs::write(&c.files[0].path, b"tampered").unwrap();
        let reloaded = Canary::load_at(&manifest).unwrap();
        let why = reloaded
            .disagreement_with_disk()
            .expect("a changed file must be noticed");
        assert!(why.contains("canary-0.bin"), "must name the file: {why}");

        // And loading regenerates rather than carrying on with a lie.
        let fresh = Canary::load_or_create_at(&cdir, &manifest).unwrap();
        assert!(fresh.disagreement_with_disk().is_none());
        assert_ne!(fresh.files[0].sha256, c.files[0].sha256, "new contents");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleted_canary_files_are_noticed_even_though_the_manifest_survived() {
        // `check_sources` passes on the directory, so backups carried no canary
        // and every verify recorded Unknown with no explanation.
        let dir = scratch("canarygone");
        let manifest = dir.join("canary.json");
        let cdir = dir.join("canary");
        let c = Canary::create_at(&cdir, &manifest).unwrap();
        for f in &c.files {
            fs::remove_file(&f.path).unwrap();
        }
        let reloaded = Canary::load_at(&manifest).unwrap();
        assert!(reloaded.disagreement_with_disk().is_some());

        let fresh = Canary::load_or_create_at(&cdir, &manifest).unwrap();
        assert!(fresh.files.iter().all(|f| f.path.exists()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_intact_canary_is_never_regenerated() {
        // The other direction, and the one that matters: rotating the canary
        // when nothing is wrong invalidates the digest the peer's snapshots were
        // made against, which is the false-FAILED this is meant to prevent.
        let dir = scratch("canarykeep");
        let manifest = dir.join("canary.json");
        let cdir = dir.join("canary");
        let first = Canary::create_at(&cdir, &manifest).unwrap();
        for _ in 0..3 {
            let again = Canary::load_or_create_at(&cdir, &manifest).unwrap();
            assert_eq!(again.files[0].sha256, first.files[0].sha256);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_manifest_is_written_atomically_and_privately() {
        // It used `fs::write`, which truncates first, so a crash part-way left a
        // half-file -- and the recovery from that was a silent regeneration.
        let dir = scratch("canaryperm");
        let manifest = dir.join("canary.json");
        Canary::create_at(&dir.join("canary"), &manifest).unwrap();
        let mode = fs::metadata(&manifest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "was {mode:o}");
        let strays: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files behind: {strays:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_longer_than_a_block_survives_the_backwards_walk() {
        // A block containing no newline at all is entirely the middle of one
        // very long line. Resetting the carry to empty there threw away the
        // right-hand half, so the record could never be rejoined and was lost.
        // `detail` carries restic's whole combined output, which is megabytes
        // for a backup that failed over a large tree, so the record this
        // dropped was reliably the most broken peer's.
        let dir = scratch("longline");
        let path = dir.join("evidence.jsonl");

        let huge = Record {
            detail: Some("x".repeat(200_000)),
            ..rec(Kind::Backup, Verdict::Unknown, 200)
        };
        Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, 100)).unwrap();
        Evidence::append_to(&path, &huge).unwrap();
        Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, 300)).unwrap();

        let all = Evidence::read_since(&path, 0);
        assert_eq!(
            all.iter().map(|r| r.at).collect::<Vec<_>>(),
            vec![100, 200, 300],
            "a record spanning several blocks must not be dropped"
        );
        assert_eq!(all[1].detail.as_ref().unwrap().len(), 200_000);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_record_written_with_a_wrong_clock_does_not_truncate_the_history() {
        // The walk used to stop at the first record older than the cutoff, on
        // the assumption that the file is in time order. One record written
        // while the RTC read 2001 broke that assumption permanently and
        // discarded everything to its left, not just that record.
        let dir = scratch("clockstep");
        let path = dir.join("evidence.jsonl");

        for at in [
            NOW - 500,
            NOW - 400,
            NOW - 300,
            1_000_000_000,
            NOW - 200,
            NOW - 100,
        ] {
            Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, at)).unwrap();
        }
        let got = Evidence::read(&path, NOW - 600, &[PeerName::new("alice").unwrap()]);
        let ats: Vec<u64> = got.records.iter().map(|r| r.at).collect();
        assert_eq!(
            ats,
            vec![NOW - 500, NOW - 400, NOW - 300, NOW - 200, NOW - 100],
            "the misdated record falls outside the window, but everything to its \
             left must survive it"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn damage_older_than_the_window_is_still_reported() {
        // A `Bad` nothing has refuted does not stop being true because it is
        // old. With a window alone, an observed pack-hash mismatch downgraded
        // from FAILED to unchecked the day it aged out, and the detail stopped
        // being printed at all.
        let dir = scratch("oldbad");
        let path = dir.join("evidence.jsonl");
        let alice = PeerName::new("alice").unwrap();

        Evidence::append_to(
            &path,
            &detailed(
                Kind::Subset,
                Verdict::Bad,
                NOW - 86400 * 80,
                "pack 4f2a hash mismatch",
            ),
        )
        .unwrap();
        Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, NOW - 60)).unwrap();
        Evidence::append_to(&path, &rec(Kind::Canary, Verdict::Good, NOW - 60)).unwrap();

        let window = NOW - 86400 * 70;
        assert!(
            Evidence::read_since(&path, window)
                .iter()
                .all(|r| r.verdict != Verdict::Bad),
            "the fixture must place the Bad outside the plain window"
        );

        let got = Evidence::read(&path, window, &[alice]);
        let r = &status(&cfg_with_peer(), &got.records, NOW)[0];
        assert_eq!(r.state, PeerState::Bad);
        assert!(r.problem.as_deref().unwrap_or_default().contains("4f2a"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_good_check_outside_the_window_ends_the_reach_back() {
        // The other direction: reaching back must stop at the first Good, or a
        // peer that was repaired years ago would still be read as damaged.
        let dir = scratch("oldgood");
        let path = dir.join("evidence.jsonl");
        let alice = PeerName::new("alice").unwrap();

        Evidence::append_to(
            &path,
            &detailed(
                Kind::Subset,
                Verdict::Bad,
                NOW - 86400 * 90,
                "pack 4f2a hash mismatch",
            ),
        )
        .unwrap();
        Evidence::append_to(&path, &rec(Kind::Subset, Verdict::Good, NOW - 86400 * 80)).unwrap();

        let got = Evidence::read(&path, NOW - 86400 * 70, &[alice]);
        let r = &status(&cfg_with_peer(), &got.records, NOW)[0];
        assert_eq!(r.state, PeerState::Unknown, "{:?}", r.problem);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_log_is_reported_rather_than_read_as_an_empty_one() {
        // The old signature was a bare Vec, so every I/O failure returned a
        // silently partial history the caller could not tell from a complete
        // one, and a partial history of fresh Goods reports `ok`.
        let dir = scratch("unreadable");
        let path = dir.join("evidence.jsonl");
        Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, NOW)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let got = Evidence::read(&path, 0, &[]);
        // Running as root defeats the permission, so only assert when it bit.
        if fs::File::open(&path).is_err() {
            assert!(got.incomplete.is_some(), "an unreadable log must say so");
            assert!(got.records.is_empty());
        }
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_log_is_a_complete_history_of_nothing() {
        let dir = scratch("nolog");
        let got = Evidence::read(&dir.join("nope.jsonl"), 0, &[]);
        assert!(got.incomplete.is_none(), "absent is not unreadable");
        assert!(got.records.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_evidence_log_is_not_readable_by_anyone_else() {
        // It carries peer names, the exact schedule, and restic's error text.
        // Under a umask of 002 the default would be group-writable, at which
        // point records are forgeable.
        let dir = scratch("evperm");
        let path = dir.join("evidence.jsonl");
        Evidence::append_to(&path, &rec(Kind::Backup, Verdict::Good, 1)).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "evidence must be 0600, was {mode:o}");
        let _ = fs::remove_dir_all(&dir);
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

    const TEST_URL: &str = "rest:http://x/";

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
            repo: Some(repo_id(TEST_URL)),
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
