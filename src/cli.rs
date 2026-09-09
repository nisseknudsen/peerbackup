//! The commands.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::{Config, Peer, PeerName, random_token, write_private};
use crate::engine::outcome::VerifyOutcome;
use crate::engine::restic::ResticEngine;
use crate::engine::{BackupEngine, Cause, SnapshotId, SnapshotMeta, SnapshotOpts};
use crate::redact;
use crate::state::{
    Canary, Evidence, Kind, PeerState, PeerStatus, Record, Verdict, ago, canary_dir, now,
    sha256_bytes, state_dir, status, utc_date,
};

use crate::Res;

/// Real backups. The recovery instructions select on this.
pub const BACKUP_TAG: &str = "peerbackup";
/// The small round-trip check `peer add` performs. Not a backup of anything.
const CHECK_TAG: &str = "peerbackup-check";

fn err<E: std::fmt::Display>(context: &str) -> impl Fn(E) -> String + '_ {
    move |e| format!("{context}: {e}")
}

/// A private scratch directory that cleans itself up.
///
/// `create_dir` rather than `create_dir_all`, and a random suffix rather than
/// the pid: `/tmp` is world-writable, and a predictable name lets another user
/// pre-create the directory so a restore lands somewhere unexpected. Failing
/// loudly when it already exists is the point.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Result<Self, String> {
        let suffix = random_token(16).map_err(err("could not generate a temp name"))?;
        let dir = std::env::temp_dir().join(format!("peerbackup-{tag}-{suffix}"));
        std::fs::create_dir(&dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A guard rather than a call at the end: the early returns below used to
        // leak the directory on every failure path.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn engine_for(peer: &Peer) -> ResticEngine {
    let mut e = ResticEngine::new(peer.url.clone(), Config::secret_path(&peer.name));
    e.ca_cert = peer.ca_cert.clone();
    if let Some(bin) = std::env::var_os("PEERBACKUP_RESTIC") {
        e.binary = PathBuf::from(bin);
    }
    // Escape hatches for a slow link, documented in the README. `restore` has
    // no deadline to override: it is the disaster operation, and the budget that
    // used to bound it belonged to the canary restore all along.
    set_from_env("PEERBACKUP_PROBE_TIMEOUT", &mut e.probe_timeout);
    set_from_env("PEERBACKUP_VERIFY_TIMEOUT", &mut e.verify_timeout);
    set_from_env("PEERBACKUP_RESTORE_TIMEOUT", &mut e.canary_restore_timeout);
    set_from_env("PEERBACKUP_LIST_TIMEOUT", &mut e.list_timeout);
    e
}

fn set_from_env(key: &str, field: &mut std::time::Duration) {
    if let Some(secs) = env_secs(key) {
        *field = secs;
    }
}

fn env_secs(key: &str) -> Option<std::time::Duration> {
    std::env::var(key)
        .ok()?
        .parse()
        .ok()
        .map(std::time::Duration::from_secs)
}

/// Where a command reads and writes state.
///
/// Paths are carried rather than looked up from the environment at each use, so
/// the commands below can be driven against a temporary directory in a test
/// without mutating the process environment. `backup` and `verify` had no tests
/// at all while they reached for `state_dir()` and `ResticEngine` directly.
pub struct Runtime {
    pub state: PathBuf,
}

impl Runtime {
    pub fn from_env() -> Self {
        Self {
            state: crate::state::state_dir(),
        }
    }

    fn evidence(&self) -> PathBuf {
        self.state.join("evidence.jsonl")
    }
    fn canary_dir(&self) -> PathBuf {
        self.state.join("canary")
    }
    fn canary_manifest(&self) -> PathBuf {
        self.state.join("canary.json")
    }

    /// Record what happened to a peer.
    ///
    /// A `Bad` verdict is observed corruption, and losing it means the next
    /// `status` calls a damaged peer healthy. That is the one verdict worth
    /// failing the command over, so it propagates; the rest warn, because
    /// failing a backup that actually succeeded would be its own lie.
    fn record(
        &self,
        peer: &Peer,
        kind: Kind,
        verdict: Verdict,
        detail: Option<String>,
        cov: Option<u8>,
    ) -> Res {
        let r = Record {
            at: now(),
            peer: peer.name.clone(),
            // Which repository this was about, so re-using a name for a
            // different friend's server cannot inherit its history.
            repo: Some(crate::state::repo_id(&peer.url)),
            kind,
            verdict,
            // Sanitised on the way in, so the log itself is clean rather than
            // relying on every reader to be careful. `detail` is restic's text,
            // which carries the repository URL with its credentials and, since
            // restic prints a server's status line and its own warnings about
            // source paths verbatim, bytes a peer chose.
            detail: detail.as_deref().map(redact::detail),
            coverage_pct: cov,
            snapshot: None,
        };
        match Evidence::append_to(&self.evidence(), &r) {
            Ok(()) => Ok(()),
            Err(e) if verdict == Verdict::Bad => Err(format!(
                "{} reported damage and it could not be recorded to {}: {e}\n  \
                 The next `status` would call this peer healthy. Fix the state \
                 directory and re-run `peerbackup verify`.",
                peer.name,
                self.evidence().display()
            )),
            Err(e) => {
                eprintln!("warning: could not record evidence: {e}");
                Ok(())
            }
        }
    }
}

// ----------------------------------------------------------------------- init

pub fn init() -> Res {
    let cfg_path = Config::path();
    if cfg_path.exists() {
        return Err(format!("{} already exists", cfg_path.display()));
    }
    let cfg = Config::default();
    cfg.save().map_err(err("could not write config"))?;
    Canary::create().map_err(err("could not create canary files"))?;

    println!("Created {}", cfg_path.display());
    println!("Created {}", canary_dir().display());
    println!();
    println!("Next:");
    println!("  1. Add the directories you want backed up to the `sources` list in");
    println!("     {}", cfg_path.display());
    println!("  2. peerbackup peer add <name> <url>");
    Ok(())
}

/// Everything needed to start backing up, in one command.
///
/// Equivalent to `init`, adding the directories to the config, then `peer add`.
/// Split out because four steps before the first backup is three too many.
pub fn connect(
    url: &str,
    sources: &[PathBuf],
    name: Option<&str>,
    ca_cert: Option<PathBuf>,
) -> Res {
    if !Config::path().exists() {
        let cfg = Config::default();
        cfg.save().map_err(err("could not write config"))?;
        Canary::create().map_err(err("could not create test files"))?;
    }

    if sources.is_empty() {
        return Err("say what to back up, e.g. --source /srv/data".into());
    }
    check_sources(sources)?;

    let mut cfg = Config::load().map_err(err("could not read config"))?;
    for s in sources {
        let abs = s
            .canonicalize()
            .map_err(|e| format!("{}: {e}", s.display()))?;
        if !cfg.settings.sources.contains(&abs) {
            cfg.settings.sources.push(abs);
        }
    }
    cfg.save().map_err(err("could not save config"))?;

    let name = match name {
        Some(n) => n.to_owned(),
        None => peer_name_from_url(url).ok_or(
            "could not work out a name for this peer from the URL; pass --name, e.g. --name alice",
        )?,
    };
    // Threaded through rather than hardcoded to `None`. It used to be the
    // latter, so a peer whose friend uses a self-signed certificate had to
    // abandon the one-command path for `init` + `peer add` -- which the README
    // never said, because its TLS section tells them to pass `--cacert` "when
    // connecting".
    peer_add(&name, url, ca_cert)?;

    println!();
    println!("Backing up:");
    for s in &Config::load().map_err(err("config"))?.settings.sources {
        println!("  {}", s.display());
    }
    println!();
    println!("Run `peerbackup backup` whenever you want to send a backup,");
    println!("and `peerbackup recovery export` to save the details you would");
    println!("need to restore without this program.");
    Ok(())
}

/// Short peer name from a URL host, when one can be derived sensibly.
///
/// Returns `None` for bare IP addresses rather than naming a peer "192".
///
/// Bounded to the authority segment, the same way [`redact`] is and for the same
/// reason: the credential separator is the last `@` *before the path*, and an
/// `@` inside the path is not one. Taking the last `@` in the whole URL made
/// `rest:http://alice.example.org/me@home/` suggest "home".
fn peer_name_from_url(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or("peer");
    let first = host.split('.').next().unwrap_or("peer");
    let cleaned: String = first
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() || cleaned.chars().all(|c| c.is_ascii_digit()) {
        None
    } else {
        Some(cleaned)
    }
}

// ----------------------------------------------------------------------- peer

/// Add a peer and prove the whole path works before trusting it.
///
/// The first backup is the canary, which is small. If the URL, credentials,
/// certificate or the friend's disk space are wrong, that surfaces in seconds
/// rather than several hours into a first real backup.
pub fn peer_add(name: &str, url: &str, ca_cert: Option<PathBuf>) -> Res {
    // Validate before the name reaches a path. Everything downstream takes a
    // PeerName, so this is the only place the raw argument exists.
    let name = PeerName::new(name)?;
    let mut cfg = Config::load().map_err(err("could not read config"))?;
    if cfg.peer(&name).is_some() {
        return Err(format!("peer '{name}' already exists"));
    }

    let secret = Config::secret_path(&name);
    if !secret.exists() {
        let pw = random_token(32).map_err(err("could not generate a password"))?;
        write_private(&secret, pw.as_bytes()).map_err(err("could not write the password"))?;
    }

    let peer = Peer {
        name: name.clone(),
        url: url.to_owned(),
        ca_cert,
    };
    let engine = engine_for(&peer);
    let canary = Canary::load_or_create().map_err(err("could not read the canary"))?;

    println!("Setting up repository...");
    match engine.init_repo() {
        Ok(()) => println!("  repository created"),
        Err(e) if e.cause == Cause::AlreadyInitialized => {
            println!("  repository already exists, checking the password");
            // A repository that exists but will not open is what you hit after
            // losing your config: peerbackup generates a fresh password and
            // restic answers "wrong password or no key found", which does not
            // tell you what to do about it.
            if let Some(cause) = engine.probe() {
                if cause == Cause::WrongPassword {
                    return Err(format!(
                        "A backup repository already exists at this address, but the password \
                         peerbackup generated does not open it.\n\n\
                         If you are setting this peer up again after losing your configuration, \
                         copy the password for this peer out of your recovery file into\n  {}\n\
                         and run this command again.\n\n\
                         If that password is gone, the data stored there cannot be decrypted; \
                         ask your friend to release the space so you can start fresh.",
                        secret.display()
                    ));
                }
                return Err(format!("could not open the existing repository: {cause}"));
            }
            println!("  password accepted");
        }
        Err(e) => return Err(format!("could not create the repository: {e}")),
    }

    println!("Uploading a test file...");
    let snap = engine
        .snapshot(
            &[canary_dir()],
            &SnapshotOpts {
                // Tagged apart from real backups. This snapshot holds only the
                // test file, so `restore latest` must never land on it.
                tags: vec![CHECK_TAG.into()],
                ..Default::default()
            },
        )
        .map_err(|e| format!("upload failed: {e}"))?;
    println!("  uploaded ({})", snap.id);

    println!("Downloading it again to check...");
    let file = canary
        .first()
        .ok_or("the canary is empty; run `peerbackup init`")?;
    let tmp = Scratch::new("check")?;
    let got = engine
        .restore_path(&snap.id, &file.path, tmp.path())
        .map_err(|e| format!("download failed: {e}"))?;
    let ok = got.sha256 == file.sha256;

    if !ok {
        return Err("the file that came back does not match the one sent".into());
    }
    println!("  matches");

    cfg.peers.push(peer.clone());
    cfg.save().map_err(err("could not save config"))?;
    // Only the canary. This round trip proves the peer is reachable, writable
    // and readable back byte-for-byte -- it does not prove any of your data is
    // there, because none of it was sent. Recording a Backup here made `status`
    // report a brand new peer as "backed up just now", which is the most
    // reassuring possible lie for this program to tell.
    Runtime::from_env().record(&peer, Kind::Canary, Verdict::Good, None, None)?;

    println!();
    println!("Peer '{name}' added and working.");
    println!("No data has been sent yet. Run `peerbackup backup` to send your first backup.");
    Ok(())
}

pub fn peer_list() -> Res {
    let cfg = Config::load().map_err(err("could not read config"))?;
    if cfg.peers.is_empty() {
        println!("No peers yet. Add one with `peerbackup peer add <name> <url>`.");
        return Ok(());
    }
    let w = name_width(cfg.peers.iter().map(|p| &p.name));
    for p in &cfg.peers {
        println!("{:<w$} {}", p.name, redact::url(&p.url), w = w);
    }
    Ok(())
}

pub fn peer_remove(name: &str) -> Res {
    let name = PeerName::new(name)?;
    let mut cfg = Config::load().map_err(err("could not read config"))?;
    let before = cfg.peers.len();
    cfg.peers.retain(|p| p.name != name);
    if cfg.peers.len() == before {
        return Err(format!("no peer called '{name}'"));
    }
    cfg.save().map_err(err("could not save config"))?;
    println!("Removed '{name}' from your config.");
    println!();
    println!("Your data is still on their machine. Ask them to run:");
    println!("  sudo peerbackup host release <your-name>");
    println!("Then re-export your recovery file: peerbackup recovery export");

    // The password is deliberately kept: it is the only thing that decrypts
    // what is still sitting on their disk, and deleting it here would make that
    // data unrecoverable the moment someone changed their mind. But it is a
    // plaintext credential that the recovery file no longer covers, so say so
    // rather than leaving it to be found later.
    let secret = Config::secret_path(&name);
    if secret.exists() {
        println!();
        println!("The password for '{name}' is still at");
        println!("  {}", secret.display());
        println!("It is kept because it is the only thing that decrypts what they");
        println!("still hold. Delete it once they have released the space.");
    }
    Ok(())
}

/// Width for the peer-name column: the longest name, never narrower than the
/// header.
///
/// Rust's padding never truncates, so a fixed `{:<12}` and a peer name longer
/// than twelve characters -- `PeerName` allows sixty-four -- pushed every
/// following column right on that row alone. Sizing the column to the content
/// keeps the table square whatever the names are, and costs one pass over a list
/// that is single digits long.
fn name_width<'a>(names: impl Iterator<Item = &'a PeerName>) -> usize {
    names
        .map(|n| n.as_str().chars().count())
        .max()
        .unwrap_or(0)
        .max(12)
}

// --------------------------------------------------------------------- backup

pub fn backup(only: Option<&str>) -> Res {
    let cfg = Config::load().map_err(err("could not read config"))?;
    backup_in(&Runtime::from_env(), &cfg, only, engine_for)
}

fn backup_in<E: BackupEngine>(
    rt: &Runtime,
    cfg: &Config,
    only: Option<&str>,
    make: impl Fn(&Peer) -> E,
) -> Res {
    let peers = select(cfg, only)?;
    if cfg.settings.sources.is_empty() {
        return Err(format!(
            "no directories to back up. Add them to `sources` in {}",
            Config::path().display()
        ));
    }

    // The canary is part of every backup, so it is checked like every other
    // source. It used to be appended after `check_sources` ran, so deleting the
    // canary directory left backups reporting success while quietly shipping
    // nothing to verify against, and `status` stuck on unchecked forever.
    Canary::load_or_create_at(&rt.canary_dir(), &rt.canary_manifest())
        .map_err(err("could not read the test files"))?;
    let sources = cfg.backup_sources(&rt.canary_dir());
    check_sources(&sources)?;

    let opts = SnapshotOpts {
        upload_limit_kib: (cfg.settings.upload_limit_kib > 0)
            .then_some(cfg.settings.upload_limit_kib),
        tags: vec![BACKUP_TAG.into()],
    };

    let mut failed = 0;
    // Sequential on purpose: two uploads at once just split the same uplink and
    // make both slower.
    for peer in &peers {
        print!("{}: backing up... ", peer.name);
        io::stdout().flush().ok();
        match make(peer).snapshot(&sources, &opts) {
            Ok(snap) if snap.incomplete => {
                // restic can finish having failed to read some of what it was
                // asked for. Treating that as success would report a backup
                // that is missing data.
                println!("INCOMPLETE ({})", snap.id);
                println!("  restic could not read everything it was asked to back up.");
                println!("  Check the paths in `sources` and their permissions.");
                rt.record(
                    peer,
                    Kind::Backup,
                    Verdict::Unknown,
                    Some("restic could not read all sources".into()),
                    None,
                )?;
                failed += 1;
            }
            Ok(snap) => {
                println!("done ({})", snap.id);
                rt.record(peer, Kind::Backup, Verdict::Good, None, None)?;
            }
            Err(e) => {
                println!("FAILED");
                println!("  {e}");
                rt.record(
                    peer,
                    Kind::Backup,
                    Verdict::Unknown,
                    Some(e.to_string()),
                    None,
                )?;
                failed += 1;
            }
        }
    }
    if failed > 0 {
        return Err(format!("{failed} of {} peers failed", peers.len()));
    }
    Ok(())
}

/// Refuse to back up when a configured source is missing or unreadable.
///
/// restic would carry on, save a snapshot, exit 0 and print a single warning,
/// so without this a forgotten bind mount or an unmounted disk produces a
/// backup that silently lacks the data you care about.
fn check_sources(sources: &[PathBuf]) -> Res {
    let mut bad = Vec::new();
    for s in sources {
        match std::fs::metadata(s) {
            Err(e) => bad.push(format!("{}: {e}", s.display())),
            Ok(_) if std::fs::read_dir(s).is_err() && std::fs::File::open(s).is_err() => {
                bad.push(format!("{}: not readable", s.display()))
            }
            Ok(_) => {}
        }
    }
    if bad.is_empty() {
        return Ok(());
    }
    Err(format!(
        "these directories are missing or unreadable, so a backup would silently \
         leave them out:\n  {}\n\nIf you are running in a container, check that each \
         one is bind-mounted at the same path.",
        bad.join("\n  ")
    ))
}

// --------------------------------------------------------------------- verify

pub fn verify(only: Option<&str>) -> Res {
    let cfg = Config::load().map_err(err("could not read config"))?;
    verify_in(&Runtime::from_env(), &cfg, only, engine_for)
}

fn verify_in<E: BackupEngine>(
    rt: &Runtime,
    cfg: &Config,
    only: Option<&str>,
    make: impl Fn(&Peer) -> E,
) -> Res {
    let peers = select(cfg, only)?;
    let canary =
        Canary::load_at(&rt.canary_manifest()).map_err(err("could not read the canary"))?;
    let pct = cfg.settings.verify_subset_pct;
    let mut bad = 0;

    for peer in &peers {
        let engine = make(peer);
        println!("{}:", peer.name);

        // Ask a cheap question first. Checking a peer that is not answering
        // otherwise burns the whole verification timeout on retries.
        if let Some(cause) = engine.probe() {
            println!("  not reachable: {cause}");
            rt.record(
                peer,
                Kind::Subset,
                Verdict::Unknown,
                Some(cause.to_string()),
                None,
            )?;
            continue;
        }

        print!("  checking {pct}% of the stored data... ");
        io::stdout().flush().ok();
        match engine.verify_subset(pct) {
            VerifyOutcome::Good { coverage_pct } => {
                println!("ok");
                rt.record(peer, Kind::Subset, Verdict::Good, None, Some(coverage_pct))?;
            }
            VerifyOutcome::Bad(c) => {
                println!("FAILED");
                println!("    {c}");
                rt.record(peer, Kind::Subset, Verdict::Bad, Some(c.to_string()), None)?;
                bad += 1;
            }
            VerifyOutcome::Indeterminate(c) => {
                println!("could not check");
                println!("    {c}");
                rt.record(
                    peer,
                    Kind::Subset,
                    Verdict::Unknown,
                    Some(c.to_string()),
                    None,
                )?;
            }
        }

        print!("  restoring a test file... ");
        io::stdout().flush().ok();
        match restore_canary(&engine, &canary) {
            CanaryCheck::Matches => {
                println!("matches");
                rt.record(peer, Kind::Canary, Verdict::Good, None, None)?;
            }
            CanaryCheck::DoesNotMatch => {
                println!("DOES NOT MATCH");
                rt.record(
                    peer,
                    Kind::Canary,
                    Verdict::Bad,
                    Some("restored test file did not match what was sent".into()),
                    None,
                )?;
                bad += 1;
            }
            CanaryCheck::Damaged(d) => {
                println!("DAMAGED");
                println!("    {d}");
                rt.record(peer, Kind::Canary, Verdict::Bad, Some(d), None)?;
                bad += 1;
            }
            CanaryCheck::CouldNotCheck(e) => {
                println!("could not restore");
                println!("    {e}");
                rt.record(peer, Kind::Canary, Verdict::Unknown, Some(e), None)?;
            }
        }
    }

    if bad > 0 {
        return Err(format!(
            "{bad} check(s) failed. Run `peerbackup status` for details."
        ));
    }
    Ok(())
}

/// What restoring the test file told us. Three states, like every other check
/// here, because "could not restore it" and "restored it and it was wrong" are
/// different facts about the peer's data.
enum CanaryCheck {
    Matches,
    DoesNotMatch,
    /// restic reported damage while fetching it: a pack that did not hash to its
    /// id, a blob that failed authentication, data the repository references and
    /// does not have. Evidence about the bytes, not about the connection.
    Damaged(String),
    CouldNotCheck(String),
}

/// Restore the test file and compare it against the digest recorded when it was
/// created.
///
/// This is the only place peerbackup reads real bytes back out of a peer and
/// checks them, which makes it half of verification -- and it is why an
/// `EngineError` arriving here with `damage` set has to become a `Bad` verdict
/// rather than an "we could not check". Reported as `Unknown`, a repository with
/// a corrupt pack holding the canary produced `could not restore`, exit 0, and a
/// peer that read `unchecked` forever while restic had already said the data was
/// wrong.
fn restore_canary(engine: &impl BackupEngine, canary: &Canary) -> CanaryCheck {
    let Some(file) = canary.first() else {
        return CanaryCheck::CouldNotCheck("the canary is empty".into());
    };
    let latest = match engine.list_snapshots() {
        Ok(s) => match s.into_iter().next() {
            Some(s) => s,
            None => {
                return CanaryCheck::CouldNotCheck("no snapshots on this peer yet".into());
            }
        },
        Err(e) => return from_engine_error(&e),
    };
    let tmp = match Scratch::new("verify") {
        Ok(t) => t,
        Err(e) => return CanaryCheck::CouldNotCheck(e),
    };
    match engine.restore_path(&latest.id, &file.path, tmp.path()) {
        Ok(got) if got.sha256 == file.sha256 => CanaryCheck::Matches,
        Ok(_) => CanaryCheck::DoesNotMatch,
        Err(e) => from_engine_error(&e),
    }
}

fn from_engine_error(e: &crate::engine::EngineError) -> CanaryCheck {
    match &e.damage {
        Some(d) => CanaryCheck::Damaged(d.to_string()),
        None => CanaryCheck::CouldNotCheck(e.to_string()),
    }
}

// --------------------------------------------------------------------- status

pub fn status_cmd() -> Res {
    let rt = Runtime::from_env();
    let cfg = Config::load().map_err(err("could not read config"))?;
    if cfg.peers.is_empty() {
        println!("No peers yet. Add one with `peerbackup peer add <name> <url>`.");
        return Ok(());
    }
    let t = now();
    // As far back as the widest window `status` actually consults -- all three
    // of them. `liveness_hours` was left out of this max while `status` went on
    // judging `last_backup` against it, so a schedule with a long backup
    // interval and short check windows dropped a perfectly good backup out of
    // the read and then reported "no backup has reached this peer".
    //
    // Doubled so a boundary record is never the reason a peer looks unchecked.
    // The evidence log is append-only and never rotated, so reading all of it
    // meant re-parsing every record ever written, on the command people run
    // most. `Evidence::read` extends the walk past this cutoff only as far as it
    // takes to find where each peer currently stands.
    let window = (cfg.settings.liveness_hours * 3600)
        .max(cfg.settings.subset_days * 86400)
        .max(cfg.settings.canary_days * 86400)
        * 2;
    let names: Vec<_> = cfg.peers.iter().map(|p| p.name.clone()).collect();
    let history = Evidence::read(&rt.evidence(), t.saturating_sub(window), &names);
    let records = &history.records;
    let rows = status(&cfg, records, t);

    let w = name_width(rows.iter().map(|r| &r.name));
    println!(
        "{:<w$} {:<11} {:<12} {:<12} {:<12} READ BACK",
        "PEER",
        "STATE",
        "BACKED UP",
        "CHECKED",
        "TEST FILE",
        w = w
    );
    for r in &rows {
        println!(
            "{:<w$} {:<11} {:<12} {:<12} {:<12} {}",
            r.name,
            r.state.label(),
            ago(r.last_backup, t),
            ago(r.last_subset, t),
            ago(r.last_canary, t),
            r.coverage_pct
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "-".into()),
            w = w
        );
    }

    // Sanitised again on the way out: records written before this was fixed are
    // still in the log, and they are the ones most likely to hold something odd.
    for r in rows.iter().filter(|r| r.problem.is_some()) {
        println!();
        println!(
            "{}: {}",
            r.name,
            redact::detail(r.problem.as_ref().unwrap())
        );
    }

    // Said out loud rather than silently absorbed. The records are excluded from
    // freshness, so the peer reads `unchecked` instead of green -- but a peer
    // that reads `unchecked` for a reason that has nothing to do with the peer
    // is exactly the sort of thing that gets ignored for a month.
    for r in rows.iter().filter(|r| r.clock_skew) {
        println!();
        println!("{}: some results are dated in the future", r.name);
        println!("  This machine's clock has been wrong. Those results are ignored,");
        println!("  so run `peerbackup verify` once the clock is right.");
    }

    // An unchecked peer usually just needs `verify`. But if its last attempt
    // failed, say so: "unchecked" alone reads as "nothing happened yet" when
    // the truth may be that every backup is being rejected.
    for r in rows.iter().filter(|r| r.state == PeerState::Unknown) {
        if let Some(reason) = last_failure(records, &r.name) {
            println!();
            println!("{}: last attempt did not succeed", r.name);
            println!("  {}", redact::detail(&reason));
        }
    }

    // A peer that has never received a backup and a peer whose checks have gone
    // stale are both `unchecked`, but the thing to do about them is different.
    // Telling someone to run `verify` against a peer holding none of their data
    // sends them to check something that was never there.
    let (never, stale) = partition_unknown(&rows);

    if !never.is_empty() {
        println!();
        println!(
            "No backup has reached: {}. Run `peerbackup backup`.",
            join_names(&never)
        );
    }
    if !stale.is_empty() {
        println!();
        println!(
            "Not checked recently: {}. Run `peerbackup verify`.",
            join_names(&stale)
        );
    }

    if let Some(why) = &history.incomplete {
        println!();
        println!("Some of the history could not be read:");
        println!("  {why}");
        println!("  What is shown may be missing results, including failures.");
    }

    verdict(&rows, history.incomplete.is_some())
}

/// Split the `unknown` peers into "never received a backup" and "checks have
/// gone stale". Both print differently and both must reach the exit code.
fn partition_unknown(rows: &[PeerStatus]) -> (Vec<&PeerStatus>, Vec<&PeerStatus>) {
    rows.iter()
        .filter(|r| r.state == PeerState::Unknown)
        .partition(|r| r.last_backup.is_none())
}

/// What `status` should exit with.
///
/// Separated from the printing so it can be tested. It was inline, and untested,
/// and that is how the `never` case below came to be silent.
///
/// `Bad` is observed damage. But a peer stuck on `unknown` past its own windows
/// -- unreachable for weeks, out of space, rejecting credentials -- used to exit
/// 0, which meant this program was silent from cron in exactly the situation it
/// exists for. The three-state model is right; collapsing it to two at the exit
/// code was not, and it collapsed in the reassuring direction.
///
/// Peers that have never received a backup at all were the remaining hole: they
/// were partitioned out for printing and then left out of the exit code, so a
/// peer holding none of your data exited 0 as long as some other peer was fine.
/// That is the most alarming state of the three, and it was the only silent one.
fn verdict(rows: &[PeerStatus], history_incomplete: bool) -> Res {
    if rows.iter().any(|r| r.state == PeerState::Bad) {
        return Err("one or more peers reported a problem".into());
    }
    // A read error on the evidence log is itself evidence that something is
    // wrong. The old signature returned a bare `Vec`, so a bad sector partway
    // through the file produced a shorter history that looked exactly like a
    // shorter history, and the newest block's fresh `Good` records reported `ok`
    // over an unrefuted `Bad` that was never reached.
    if history_incomplete {
        return Err(
            "the evidence log could not be read in full, so nothing here can be trusted".into(),
        );
    }
    if !rows.is_empty() && rows.iter().all(|r| r.state == PeerState::Unknown) {
        return Err(
            "no peer has been confirmed good. Run `peerbackup backup` and `peerbackup verify`"
                .into(),
        );
    }

    let (never, stale) = partition_unknown(rows);
    let mut problems = Vec::new();
    if !never.is_empty() {
        problems.push(format!(
            "{} peer(s) hold no backup at all ({})",
            never.len(),
            join_names(&never)
        ));
    }
    if !stale.is_empty() {
        problems.push(format!(
            "{} peer(s) have not been checked within their windows ({})",
            stale.len(),
            join_names(&stale)
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// Why a peer's most recent attempt did not succeed, if it did not.
fn last_failure(records: &[crate::state::Record], peer: &PeerName) -> Option<String> {
    let last = records.iter().rfind(|r| peer == r.peer.as_str())?;
    (last.verdict != Verdict::Good)
        .then(|| last.detail.clone())
        .flatten()
}

// -------------------------------------------------------------------- restore

/// Pick the snapshot a bare `restore` should use: the newest one tagged as a
/// real backup, and nothing else.
///
/// `peer add` uploads the canary under its own tag to prove the peer works. That
/// snapshot holds the test file and none of your data, so selecting it would
/// restore almost nothing and report success. This function previously ended in
/// `.or_else(|| snaps.first())` — three lines under a comment saying the test
/// snapshot must never be chosen — which did exactly that whenever no real
/// backup existed. Refusing is the only correct answer: there is nothing to
/// restore, and saying so is the entire job.
///
/// Selection is by tag, not by matching snapshot paths against `sources`.
/// Paths stop matching the moment someone reorganises their folders, and a
/// restore that refuses because you renamed a directory is its own failure.
fn newest_real_backup(snaps: &[SnapshotMeta], peer_name: &PeerName) -> Result<SnapshotId, String> {
    if let Some(s) = snaps
        .iter()
        .find(|s| s.tags.iter().any(|t| t == BACKUP_TAG))
    {
        return Ok(s.id.clone());
    }
    if snaps.is_empty() {
        return Err(format!(
            "{peer_name} holds no backups at all.\n  Run `peerbackup backup` to send one."
        ));
    }
    Err(format!(
        "{peer_name} holds no backup of your data.\n  \
         The {} snapshot(s) there are from `peer add`, which uploads a test file and \
         nothing else.\n  \
         Run `peerbackup backup` first, or name one explicitly with --snapshot if you \
         know what is in it.",
        snaps.len()
    ))
}

pub fn restore(peer_name: &str, target: &Path, snapshot: Option<&str>) -> Res {
    let peer_name = PeerName::new(peer_name)?;
    let cfg = Config::load().map_err(err("could not read config"))?;
    let peer = cfg
        .peer(&peer_name)
        .ok_or_else(|| format!("no peer called '{peer_name}'"))?;
    let engine = engine_for(peer);

    let id = match snapshot {
        Some(s) => SnapshotId(s.to_owned()),
        None => {
            let snaps = engine.list_snapshots().map_err(|e| e.to_string())?;
            newest_real_backup(&snaps, &peer_name)?
        }
    };

    println!("Restoring {id} from {peer_name} into {}", target.display());
    engine
        .restore_all(&id, target)
        .map_err(|e| format!("restore failed: {e}"))?;
    println!("Done.");
    Ok(())
}

pub fn snapshots(peer_name: &str) -> Res {
    let peer_name = PeerName::new(peer_name)?;
    let cfg = Config::load().map_err(err("could not read config"))?;
    let peer = cfg
        .peer(&peer_name)
        .ok_or_else(|| format!("no peer called '{peer_name}'"))?;
    for s in engine_for(peer)
        .list_snapshots()
        .map_err(|e| e.to_string())?
    {
        println!("{}  {}", s.id, s.time);
    }
    Ok(())
}

// ------------------------------------------------------------------- recovery

/// Write down everything needed to get the data back without this program.
pub fn recovery_export(out: Option<PathBuf>) -> Res {
    let cfg = Config::load().map_err(err("could not read config"))?;
    if cfg.peers.is_empty() {
        return Err("no peers to export".into());
    }
    // An explicit --out must land where the user said, not somewhere near it.
    // `write_private` creates parent directories, so
    // `recovery export --out /mnt/usb/recovery.txt` with the stick not mounted
    // created /mnt/usb on the root filesystem, wrote the passwords that decrypt
    // every backup into it, recorded the fingerprint as current, and printed
    // "Written to /mnt/usb/recovery.txt". The user then believes the only copy
    // is on removable media. It is on the disk they are backing up.
    let path = match out {
        Some(p) => {
            let parent = p.parent().filter(|d| !d.as_os_str().is_empty());
            if let Some(dir) = parent
                && !dir.is_dir()
            {
                return Err(format!(
                    "{} does not exist.\n  \
                     Create it first, or check the drive is mounted. This file holds the \
                     passwords\n  that decrypt every backup, so it is not written \
                     somewhere approximate.",
                    dir.display()
                ));
            }
            p
        }
        None => state_dir().join("recovery.txt"),
    };

    let mut s = String::new();
    s.push_str("PEERBACKUP RECOVERY DETAILS\n");
    s.push_str("===========================\n\n");
    s.push_str("This file contains the passwords to your backups. Print it or put it\n");
    s.push_str("on a USB stick, and keep it somewhere other than the machine you are\n");
    s.push_str("backing up. If that machine dies and this file dies with it, your\n");
    s.push_str("backups cannot be read by anyone, including you.\n\n");
    s.push_str("You do not need peerbackup to use these. Install restic and run the\n");
    s.push_str("commands below.\n\n");

    for peer in &cfg.peers {
        let pw = std::fs::read_to_string(Config::secret_path(&peer.name))
            .map_err(|e| format!("could not read the password for {}: {e}", peer.name))?;
        s.push_str(&format!("--- {} ---\n\n", peer.name));
        s.push_str(&format!("Repository: {}\n", peer.url));
        s.push_str(&format!("Password:   {}\n", pw.trim()));
        // The flag has to appear in the commands, not just the certificate in a
        // note above them. Someone recovering onto a fresh machine from a host
        // with a self-signed certificate would otherwise paste a command that
        // fails on TLS, with nothing here telling them what to add -- and this
        // file exists precisely to work without peerbackup, on a machine that
        // may have just been installed.
        let cacert = match &peer.ca_cert {
            Some(ca) => {
                s.push_str(&format!(
                    "Certificate: {}\n            Copy this file too. Without it the \
                     commands below fail on TLS.\n",
                    ca.display()
                ));
                format!(" --cacert '{}'", ca.display())
            }
            None => String::new(),
        };
        s.push_str("\nTo see what is stored:\n");
        s.push_str(&format!("  restic -r '{}'{cacert} snapshots\n", peer.url));
        s.push_str("\n(The --tag below matters: it skips the small connection test\n");
        s.push_str(" that peerbackup uploads when a peer is first set up.)\n");
        s.push_str("\nTo get everything back:\n");
        s.push_str(&format!(
            "  restic -r '{}'{cacert} restore latest --tag {BACKUP_TAG} \
             --target /where/to/put/it\n\n",
            peer.url
        ));
    }

    // A date, not a Unix timestamp. This document is meant to be printed and
    // read years later, by someone who has just lost a machine.
    let stamp = now();
    s.push_str(&format!("Exported: {} ({stamp})\n", utc_date(stamp)));
    let fp = fingerprint(&cfg);
    s.push_str(&format!("Fingerprint: {fp}\n"));

    write_private(&path, s.as_bytes()).map_err(err("could not write the recovery file"))?;
    // The fingerprint is a digest over the peer names, URLs and passwords, so it
    // is derived from secrets and gets the same handling as the file beside it
    // rather than the process umask.
    write_private(&state_dir().join("recovery.fingerprint"), fp.as_bytes())
        .map_err(err("could not record the fingerprint"))?;

    println!("Written to {}", path.display());
    println!();
    println!("It contains your backup passwords in plain text. Move it off this");
    println!("machine, then delete the copy here.");
    Ok(())
}

/// Identifies the peer set and credentials an export was made from, so we can
/// tell the user when their printed copy no longer matches reality.
fn fingerprint(cfg: &Config) -> String {
    let mut material = String::new();
    for p in &cfg.peers {
        material.push_str(p.name.as_str());
        material.push_str(&p.url);
        if let Ok(pw) = std::fs::read_to_string(Config::secret_path(&p.name)) {
            material.push_str(pw.trim());
        }
    }
    sha256_bytes(material.as_bytes())
}

/// True when the exported recovery file no longer matches the current config.
pub fn recovery_is_stale(cfg: &Config) -> bool {
    match std::fs::read_to_string(state_dir().join("recovery.fingerprint")) {
        Ok(saved) => saved.trim() != fingerprint(cfg),
        Err(_) => !cfg.peers.is_empty(),
    }
}

pub fn recovery_check() -> Res {
    let cfg = Config::load().map_err(err("could not read config"))?;
    if recovery_is_stale(&cfg) {
        println!("Your recovery file is out of date. Run `peerbackup recovery export`.");
    } else {
        println!("Recovery file matches your current peers.");
    }
    Ok(())
}

fn join_names(rows: &[&PeerStatus]) -> String {
    rows.iter()
        .map(|r| r.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

// -------------------------------------------------------------------- helpers

fn select<'a>(cfg: &'a Config, only: Option<&str>) -> Result<Vec<&'a Peer>, String> {
    match only {
        Some(name) => {
            let name = PeerName::new(name)?;
            cfg.peer(&name)
                .map(|p| vec![p])
                .ok_or_else(|| format!("no peer called '{name}'"))
        }
        None if cfg.peers.is_empty() => {
            Err("no peers yet. Add one with `peerbackup peer add <name> <url>`".into())
        }
        None => Ok(cfg.peers.iter().collect()),
    }
}

/// Shown after commands that change what a recovery file would need to contain.
pub fn warn_if_recovery_stale() {
    if Config::load().is_ok_and(|cfg| recovery_is_stale(&cfg)) {
        eprintln!("Your recovery file is out of date: run `peerbackup recovery export`.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_names_come_from_the_url_host() {
        assert_eq!(
            peer_name_from_url("rest:https://me:pw@alice.example.org:8000/me/").as_deref(),
            Some("alice")
        );
        assert_eq!(
            peer_name_from_url("rest:http://homeserver:8000/me/").as_deref(),
            Some("homeserver")
        );
        // A bare IP yields no useful name, so ask instead of calling it "192".
        assert_eq!(peer_name_from_url("rest:http://192.168.1.5:8000/me/"), None);
        // An `@` in the path is not a credential separator. Taking the last one
        // in the whole URL suggested "home" for this.
        assert_eq!(
            peer_name_from_url("rest:http://alice.example.org/me@home/").as_deref(),
            Some("alice")
        );
        assert_eq!(
            peer_name_from_url("rest:https://me:pw@alice.example.org/me@home/").as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn missing_sources_are_refused() {
        // restic would save a snapshot anyway, exit 0, and print one warning.
        let missing = PathBuf::from("/definitely/not/here");
        let e = check_sources(&[missing]).unwrap_err();
        assert!(e.contains("missing or unreadable"), "{e}");
        assert!(e.contains("bind-mounted"), "should mention mounts: {e}");
    }

    #[test]
    fn existing_sources_pass() {
        assert!(check_sources(&[std::env::temp_dir()]).is_ok());
    }

    #[test]
    fn one_bad_source_among_good_ones_still_refuses() {
        let sources = vec![std::env::temp_dir(), PathBuf::from("/nope/nope")];
        assert!(check_sources(&sources).is_err());
    }

    // ------------------------------------------------------- status exit code

    fn row(name: &str, state: PeerState, last_backup: Option<u64>) -> PeerStatus {
        PeerStatus {
            name: pn(name),
            state,
            last_backup,
            last_subset: None,
            last_canary: None,
            coverage_pct: None,
            problem: None,
            clock_skew: false,
        }
    }

    #[test]
    fn a_peer_holding_no_backup_at_all_exits_non_zero() {
        // The hole this closes: `never` peers were printed and then dropped from
        // the exit code, so a peer holding none of your data exited 0 as long as
        // some other peer looked fine. From cron the exit code is the whole
        // signal, and this is the most alarming of the three unknown states.
        let rows = [
            row("alice", PeerState::Good, Some(100)),
            row("bob", PeerState::Unknown, None),
        ];
        let e = verdict(&rows, false).unwrap_err();
        assert!(e.contains("no backup at all"), "got: {e}");
        assert!(e.contains("bob"), "must name the peer: {e}");
        assert!(!e.contains("alice"), "must not blame the healthy peer: {e}");
    }

    #[test]
    fn observed_damage_outranks_everything_else() {
        let rows = [
            row("alice", PeerState::Bad, Some(100)),
            row("bob", PeerState::Unknown, None),
        ];
        let e = verdict(&rows, false).unwrap_err();
        assert!(e.contains("reported a problem"), "got: {e}");
    }

    #[test]
    fn a_stale_peer_still_exits_non_zero_and_says_which() {
        let rows = [
            row("alice", PeerState::Good, Some(100)),
            row("bob", PeerState::Unknown, Some(50)),
        ];
        let e = verdict(&rows, false).unwrap_err();
        assert!(e.contains("not been checked"), "got: {e}");
        assert!(e.contains("bob"), "got: {e}");
    }

    #[test]
    fn never_and_stale_are_reported_together_not_one_instead_of_the_other() {
        let rows = [
            row("alice", PeerState::Good, Some(100)),
            row("bob", PeerState::Unknown, None),
            row("carol", PeerState::Unknown, Some(50)),
        ];
        let e = verdict(&rows, false).unwrap_err();
        assert!(e.contains("bob"), "the never-backed-up peer: {e}");
        assert!(e.contains("carol"), "the stale peer: {e}");
    }

    #[test]
    fn every_peer_healthy_exits_zero() {
        let rows = [
            row("alice", PeerState::Good, Some(100)),
            row("bob", PeerState::Good, Some(100)),
        ];
        assert!(verdict(&rows, false).is_ok());
    }

    #[test]
    fn no_peers_at_all_is_not_a_failure_here() {
        // `status_cmd` returns early with its own message before reaching this.
        assert!(verdict(&[], false).is_ok());
    }

    // ------------------------------------------------- backup and verify

    // These are the reason the engine seam exists. Both commands used to reach
    // for `Config::load`, `state_dir()` and `ResticEngine` directly, so neither
    // had a single test -- including the branch that decides whether a partial
    // backup counts as success, which is the distinction the whole product
    // rests on.

    use crate::engine::EngineError;
    use crate::engine::fake::FakeEngine;
    use crate::engine::outcome::Corruption;

    struct Harness {
        _dir: PathBuf,
        rt: Runtime,
        cfg: Config,
    }

    fn harness(tag: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("pb-cmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sources = dir.join("data");
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::write(sources.join("f.txt"), b"hello").unwrap();

        let mut cfg = Config::default();
        cfg.settings.sources = vec![sources];
        cfg.peers.push(Peer {
            name: pn("alice"),
            url: "rest:http://example.invalid/alice/".into(),
            ca_cert: None,
        });
        Harness {
            rt: Runtime { state: dir.clone() },
            cfg,
            _dir: dir,
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self._dir);
        }
    }

    fn records(h: &Harness) -> Vec<Record> {
        Evidence::read_since(&h.rt.evidence(), 0)
    }

    #[test]
    fn a_complete_backup_is_recorded_as_good() {
        let h = harness("ok");
        std::fs::create_dir_all(h.rt.canary_dir()).unwrap();
        backup_in(&h.rt, &h.cfg, None, |_| {
            FakeEngine::always(VerifyOutcome::Good { coverage_pct: 1 })
        })
        .unwrap();
        let r = records(&h);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].kind, Kind::Backup);
        assert_eq!(r[0].verdict, Verdict::Good);
    }

    #[test]
    fn an_incomplete_backup_is_a_failure_not_a_success() {
        // restic can finish having failed to read some of what it was asked
        // for. Recording that as Good would report a backup missing data, and
        // `status` would show the peer as fine.
        let h = harness("incomplete");
        std::fs::create_dir_all(h.rt.canary_dir()).unwrap();
        let err =
            backup_in(&h.rt, &h.cfg, None, |_| FakeEngine::incomplete_snapshot()).unwrap_err();
        assert!(err.contains("1 of 1"), "must report a failure: {err}");

        let r = records(&h);
        assert_eq!(r.len(), 1);
        assert_ne!(
            r[0].verdict,
            Verdict::Good,
            "an incomplete backup is not good"
        );
        assert_eq!(r[0].verdict, Verdict::Unknown);
    }

    #[test]
    fn a_failed_backup_records_unknown_never_bad() {
        // A refused upload says nothing about the data already stored there.
        // Recording Bad would raise a corruption alarm for a full disk.
        let h = harness("failed");
        std::fs::create_dir_all(h.rt.canary_dir()).unwrap();
        let e = EngineError {
            message: "server out of space".into(),
            exit_code: Some(1),
            cause: Cause::OutOfSpace,
            damage: None,
        };
        assert!(
            backup_in(&h.rt, &h.cfg, None, |_| FakeEngine::failing_snapshot(
                e.clone()
            ))
            .is_err()
        );
        let r = records(&h);
        assert_eq!(r[0].verdict, Verdict::Unknown);
        assert!(r[0].detail.as_ref().unwrap().contains("out of space"));
    }

    #[test]
    fn backup_refuses_when_a_source_is_missing_rather_than_shipping_less() {
        let mut h = harness("missing");
        std::fs::create_dir_all(h.rt.canary_dir()).unwrap();
        h.cfg.settings.sources.push(PathBuf::from("/nope/not/here"));
        let err = backup_in(&h.rt, &h.cfg, None, |_| {
            FakeEngine::always(VerifyOutcome::Good { coverage_pct: 1 })
        })
        .unwrap_err();
        assert!(err.contains("/nope/not/here"), "must name the path: {err}");
        assert!(records(&h).is_empty(), "nothing may be recorded");
    }

    #[test]
    fn verify_probes_before_checking_so_a_dead_peer_costs_seconds() {
        // Without the probe, an unreachable peer burns the whole verification
        // timeout while restic retries.
        //
        // Asserted on the call log rather than inferred from a record count.
        // The count only shows that one thing was recorded; what matters is
        // that the expensive calls were never made at all.
        let h = harness("probe");
        Canary::create_at(&h.rt.canary_dir(), &h.rt.canary_manifest()).unwrap();
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let handle = log.clone();
        verify_in(&h.rt, &h.cfg, None, move |_| FakeEngine {
            calls: handle.clone(),
            ..FakeEngine::unreachable("connection refused")
        })
        .unwrap();

        assert_eq!(
            &*log.borrow(),
            &["probe()"],
            "a failed probe must end the peer's turn before anything expensive"
        );
        let r = records(&h);
        assert_eq!(r.len(), 1, "a probe failure ends the peer's turn");
        assert_eq!(r[0].verdict, Verdict::Unknown, "unreachable is not damage");
    }

    #[test]
    fn verify_checks_the_data_then_the_canary_when_the_peer_answers() {
        // The other side of the ordering: a reachable peer gets the subset check
        // and then the canary restore, and the canary needs a snapshot listed
        // first. Pinning the sequence is what makes the probe test above mean
        // "stopped early" rather than "did nothing for some other reason".
        let h = harness("order");
        Canary::create_at(&h.rt.canary_dir(), &h.rt.canary_manifest()).unwrap();
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let handle = log.clone();
        verify_in(&h.rt, &h.cfg, None, move |_| FakeEngine {
            calls: handle.clone(),
            ..FakeEngine::always(VerifyOutcome::Good { coverage_pct: 1 })
        })
        .unwrap();

        let calls = log.borrow();
        assert_eq!(calls[0], "probe()");
        assert_eq!(calls[1], "verify_subset(1)");
        assert_eq!(calls[2], "list_snapshots()");
        assert!(calls[3].starts_with("restore_path("), "got {}", calls[3]);
    }

    #[test]
    fn verify_records_bad_only_for_data_it_read_and_found_wrong() {
        let h = harness("bad");
        Canary::create_at(&h.rt.canary_dir(), &h.rt.canary_manifest()).unwrap();
        let err = verify_in(&h.rt, &h.cfg, None, |_| {
            FakeEngine::always(VerifyOutcome::Bad(Corruption::PackHashMismatch {
                pack: "abc123".into(),
            }))
        })
        .unwrap_err();
        assert!(err.contains("failed"), "{err}");

        let subset = records(&h)
            .into_iter()
            .find(|r| r.kind == Kind::Subset)
            .unwrap();
        assert_eq!(subset.verdict, Verdict::Bad);
        assert!(subset.detail.unwrap().contains("abc123"));
    }

    #[test]
    fn verify_treats_an_unreadable_check_as_unknown_not_damage() {
        // The distinction the whole product rests on: could-not-check must
        // never age into "your backup is corrupt".
        let h = harness("indet");
        Canary::create_at(&h.rt.canary_dir(), &h.rt.canary_manifest()).unwrap();
        verify_in(&h.rt, &h.cfg, None, |_| {
            FakeEngine::always(VerifyOutcome::Indeterminate(Cause::TimedOut {
                after_secs: 3600,
            }))
        })
        .unwrap();
        let subset = records(&h)
            .into_iter()
            .find(|r| r.kind == Kind::Subset)
            .unwrap();
        assert_eq!(subset.verdict, Verdict::Unknown);
    }

    #[test]
    fn a_canary_that_comes_back_wrong_is_bad() {
        let h = harness("canary");
        Canary::create_at(&h.rt.canary_dir(), &h.rt.canary_manifest()).unwrap();
        let err = verify_in(&h.rt, &h.cfg, None, |_| {
            // Digest that cannot match what was sent.
            FakeEngine {
                restored_digest: Some("f".repeat(64)),
                ..FakeEngine::always(VerifyOutcome::Good { coverage_pct: 1 })
            }
        })
        .unwrap_err();
        assert!(err.contains("failed"), "{err}");
        let canary = records(&h)
            .into_iter()
            .find(|r| r.kind == Kind::Canary)
            .unwrap();
        assert_eq!(canary.verdict, Verdict::Bad);
    }

    #[test]
    fn corruption_found_while_restoring_the_test_file_is_damage_not_a_missed_check() {
        // The canary restore is the only place peerbackup reads real bytes back
        // and compares them, so it is half of verification -- but damage found
        // there was relabelled `Unclassified` at the engine seam, recorded as
        // `Unknown`, and `verify` exited 0. A repository with a corrupt pack
        // holding the canary read `unchecked` forever while restic had already
        // said the data was wrong.
        let h = harness("canarydamage");
        Canary::create_at(&h.rt.canary_dir(), &h.rt.canary_manifest()).unwrap();
        let err = verify_in(&h.rt, &h.cfg, None, |_| FakeEngine {
            restore_damage: Some(Corruption::PackHashMismatch {
                pack: "4f2a1b3c".into(),
            }),
            ..FakeEngine::always(VerifyOutcome::Good { coverage_pct: 1 })
        })
        .unwrap_err();
        assert!(err.contains("failed"), "verify must not exit 0: {err}");

        let canary = records(&h)
            .into_iter()
            .find(|r| r.kind == Kind::Canary)
            .unwrap();
        assert_eq!(canary.verdict, Verdict::Bad, "observed damage is Bad");
        assert!(
            canary.detail.unwrap().contains("4f2a1b3c"),
            "must name what restic found"
        );
    }

    #[test]
    fn a_peer_that_merely_will_not_answer_is_still_not_damage() {
        // The other direction, and the one that matters most: a failure to
        // reach the peer must never age into "your backup is corrupt".
        let h = harness("canaryunreach");
        Canary::create_at(&h.rt.canary_dir(), &h.rt.canary_manifest()).unwrap();
        verify_in(&h.rt, &h.cfg, None, |_| {
            FakeEngine::unreachable("connection refused")
        })
        .unwrap();
        assert!(
            records(&h).iter().all(|r| r.verdict != Verdict::Bad),
            "nothing here is evidence about the data"
        );
    }

    // ---------------------------------------------------------- restore choice

    fn pn(s: &str) -> PeerName {
        PeerName::new(s).unwrap()
    }

    fn snap(id: &str, tags: &[&str]) -> SnapshotMeta {
        SnapshotMeta {
            id: SnapshotId(id.into()),
            time: "2026-07-30T12:00:00Z".into(),
            paths: vec![PathBuf::from("/srv/data")],
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn restore_refuses_when_only_the_peer_add_test_snapshot_exists() {
        // The bug this replaced fell back to `snaps.first()`, restored a canary
        // directory and printed "Done." to someone who had just lost a disk.
        let snaps = [snap("aaaa1111", &[CHECK_TAG])];
        let err = newest_real_backup(&snaps, &pn("alice")).unwrap_err();
        assert!(
            err.contains("no backup of your data"),
            "must say the data is not there, got: {err}"
        );
        assert!(
            err.contains("peerbackup backup"),
            "must say what to do about it, got: {err}"
        );
    }

    #[test]
    fn restore_refuses_when_the_peer_holds_nothing() {
        let err = newest_real_backup(&[], &pn("alice")).unwrap_err();
        assert!(err.contains("no backups at all"), "got: {err}");
    }

    #[test]
    fn restore_picks_the_newest_real_backup_over_the_test_snapshot() {
        // list_snapshots is newest first.
        let snaps = [
            snap("bbbb2222", &[BACKUP_TAG]),
            snap("cccc3333", &[BACKUP_TAG]),
            snap("aaaa1111", &[CHECK_TAG]),
        ];
        assert_eq!(
            newest_real_backup(&snaps, &pn("alice")).unwrap(),
            SnapshotId("bbbb2222".into())
        );
    }

    #[test]
    fn an_untagged_snapshot_is_not_treated_as_a_backup() {
        // Anything restic already held before peerbackup touched the repository
        // is not ours and we cannot say what is in it.
        let snaps = [snap("dddd4444", &[])];
        assert!(newest_real_backup(&snaps, &pn("alice")).is_err());
    }
}
