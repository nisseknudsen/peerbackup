//! The commands.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::config::{Config, Peer, random_token, write_private};
use crate::engine::outcome::VerifyOutcome;
use crate::engine::restic::ResticEngine;
use crate::engine::{BackupEngine, SnapshotId, SnapshotOpts};
use crate::state::{
    Canary, Evidence, Kind, PeerState, Record, Verdict, ago, canary_dir, now, sha256_bytes,
    state_dir, status,
};

type Res = Result<(), String>;

/// Real backups. The recovery instructions select on this.
pub const BACKUP_TAG: &str = "peerbackup";
/// The small round-trip check `peer add` performs. Not a backup of anything.
const CHECK_TAG: &str = "peerbackup-check";

fn err<E: std::fmt::Display>(context: &str) -> impl Fn(E) -> String + '_ {
    move |e| format!("{context}: {e}")
}

fn engine_for(peer: &Peer) -> ResticEngine {
    let mut e = ResticEngine::new(peer.url.clone(), Config::secret_path(&peer.name));
    e.ca_cert = peer.ca_cert.clone();
    if let Some(bin) = std::env::var_os("PEERBACKUP_RESTIC") {
        e.binary = PathBuf::from(bin);
    }
    if let Some(secs) = env_secs("PEERBACKUP_PROBE_TIMEOUT") {
        e.probe_timeout = secs;
    }
    if let Some(secs) = env_secs("PEERBACKUP_VERIFY_TIMEOUT") {
        e.verify_timeout = secs;
    }
    e
}

fn env_secs(key: &str) -> Option<std::time::Duration> {
    std::env::var(key)
        .ok()?
        .parse()
        .ok()
        .map(std::time::Duration::from_secs)
}

fn record(peer: &str, kind: Kind, verdict: Verdict, detail: Option<String>, cov: Option<u8>) {
    let r = Record {
        at: now(),
        peer: peer.to_string(),
        kind,
        verdict,
        detail,
        coverage_pct: cov,
        snapshot: None,
    };
    if let Err(e) = Evidence::append(&r) {
        eprintln!("warning: could not record evidence: {e}");
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
pub fn connect(url: &str, sources: &[PathBuf], name: Option<&str>) -> Res {
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
        Some(n) => n.to_string(),
        None => peer_name_from_url(url).ok_or(
            "could not work out a name for this peer from the URL; pass --name, e.g. --name alice",
        )?,
    };
    peer_add(&name, url, None)?;

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
fn peer_name_from_url(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme
        .rsplit('@')
        .next()
        .unwrap_or(after_scheme)
        .split(['/', ':'])
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
    let mut cfg = Config::load().map_err(err("could not read config"))?;
    if cfg.peer(name).is_some() {
        return Err(format!("peer '{name}' already exists"));
    }

    let secret = Config::secret_path(name);
    if !secret.exists() {
        let pw = random_token(32).map_err(err("could not generate a password"))?;
        write_private(&secret, pw.as_bytes()).map_err(err("could not write the password"))?;
    }

    let peer = Peer {
        name: name.to_string(),
        url: url.to_string(),
        ca_cert,
    };
    let engine = engine_for(&peer);
    let canary = Canary::load_or_create().map_err(err("could not read the canary"))?;

    println!("Setting up repository...");
    match engine.init_repo() {
        Ok(()) => println!("  repository created"),
        Err(e)
            if e.message.contains("already initialized")
                || e.message.contains("already exists") =>
        {
            println!("  repository already exists, checking the password");
            // A repository that exists but will not open is what you hit after
            // losing your config: peerbackup generates a fresh password and
            // restic answers "wrong password or no key found", which does not
            // tell you what to do about it.
            if let Some(cause) = engine.probe() {
                let c = cause.to_string();
                if c.contains("wrong password") || c.contains("no key found") {
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
    let tmp = std::env::temp_dir().join(format!("peerbackup-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let got = engine
        .restore_path(&snap.id, &file.path, &tmp)
        .map_err(|e| format!("download failed: {e}"))?;
    let ok = got.sha256 == file.sha256;
    let _ = std::fs::remove_dir_all(&tmp);

    if !ok {
        return Err("the file that came back does not match the one sent".into());
    }
    println!("  matches");

    cfg.peers.push(peer);
    cfg.save().map_err(err("could not save config"))?;
    record(name, Kind::Backup, Verdict::Good, None, None);
    record(name, Kind::Canary, Verdict::Good, None, None);

    println!();
    println!("Peer '{name}' added and working.");
    println!("Run `peerbackup backup` to send your first real backup.");
    Ok(())
}

pub fn peer_list() -> Res {
    let cfg = Config::load().map_err(err("could not read config"))?;
    if cfg.peers.is_empty() {
        println!("No peers yet. Add one with `peerbackup peer add <name> <url>`.");
        return Ok(());
    }
    for p in &cfg.peers {
        println!("{:<12} {}", p.name, redact(&p.url));
    }
    Ok(())
}

pub fn peer_remove(name: &str) -> Res {
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
    println!("  sudo peerbackup-host release <your-name>");
    println!("Then re-export your recovery file: peerbackup recovery export");
    Ok(())
}

/// Hide the password in a repository URL before printing it.
fn redact(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(s), Some(at)) if at > s => {
            let scheme = &url[..s + 3];
            let rest = &url[at..];
            let user = url[s + 3..at].split(':').next().unwrap_or("");
            format!("{scheme}{user}:***{rest}")
        }
        _ => url.to_string(),
    }
}

// --------------------------------------------------------------------- backup

pub fn backup(only: Option<&str>) -> Res {
    let cfg = Config::load().map_err(err("could not read config"))?;
    let peers = select(&cfg, only)?;
    if cfg.settings.sources.is_empty() {
        return Err(format!(
            "no directories to back up. Add them to `sources` in {}",
            Config::path().display()
        ));
    }

    check_sources(&cfg.settings.sources)?;

    let sources = cfg.backup_sources(&canary_dir());
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
        match engine_for(peer).snapshot(&sources, &opts) {
            Ok(snap) if snap.incomplete => {
                // restic exits 0 and saves a snapshot even when it could not
                // read a source, printing one warning line. Treating that as
                // success would mean reporting a backup that is missing data.
                println!("INCOMPLETE ({})", snap.id);
                println!("  restic could not read everything it was asked to back up.");
                println!("  Check the paths in `sources` and their permissions.");
                record(
                    &peer.name,
                    Kind::Backup,
                    Verdict::Unknown,
                    Some("restic could not read all sources".into()),
                    None,
                );
                failed += 1;
            }
            Ok(snap) => {
                println!("done ({})", snap.id);
                record(&peer.name, Kind::Backup, Verdict::Good, None, None);
            }
            Err(e) => {
                println!("FAILED");
                println!("  {e}");
                record(
                    &peer.name,
                    Kind::Backup,
                    Verdict::Unknown,
                    Some(e.to_string()),
                    None,
                );
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
    let peers = select(&cfg, only)?;
    let canary = Canary::load().map_err(err("could not read the canary"))?;
    let pct = cfg.settings.verify_subset_pct;
    let mut bad = 0;

    for peer in &peers {
        let engine = engine_for(peer);
        println!("{}:", peer.name);

        // Ask a cheap question first. Checking a peer that is not answering
        // otherwise burns the whole verification timeout on retries.
        if let Some(cause) = engine.probe() {
            println!("  not reachable: {cause}");
            record(
                &peer.name,
                Kind::Subset,
                Verdict::Unknown,
                Some(cause.to_string()),
                None,
            );
            continue;
        }

        print!("  checking {pct}% of the stored data... ");
        io::stdout().flush().ok();
        match engine.verify_subset(pct) {
            VerifyOutcome::Good { coverage_pct } => {
                println!("ok");
                record(
                    &peer.name,
                    Kind::Subset,
                    Verdict::Good,
                    None,
                    Some(coverage_pct),
                );
            }
            VerifyOutcome::Bad(c) => {
                println!("FAILED");
                println!("    {c}");
                record(
                    &peer.name,
                    Kind::Subset,
                    Verdict::Bad,
                    Some(c.to_string()),
                    None,
                );
                bad += 1;
            }
            VerifyOutcome::Indeterminate(c) => {
                println!("could not check");
                println!("    {c}");
                record(
                    &peer.name,
                    Kind::Subset,
                    Verdict::Unknown,
                    Some(c.to_string()),
                    None,
                );
            }
        }

        print!("  restoring a test file... ");
        io::stdout().flush().ok();
        match restore_canary(&engine, &canary) {
            Ok(true) => {
                println!("matches");
                record(&peer.name, Kind::Canary, Verdict::Good, None, None);
            }
            Ok(false) => {
                println!("DOES NOT MATCH");
                record(
                    &peer.name,
                    Kind::Canary,
                    Verdict::Bad,
                    Some("restored test file did not match what was sent".into()),
                    None,
                );
                bad += 1;
            }
            Err(e) => {
                println!("could not restore");
                println!("    {e}");
                record(&peer.name, Kind::Canary, Verdict::Unknown, Some(e), None);
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

fn restore_canary(engine: &ResticEngine, canary: &Canary) -> Result<bool, String> {
    let file = canary.first().ok_or("the canary is empty")?;
    let latest = engine
        .list_snapshots()
        .map_err(|e| e.to_string())?
        .into_iter()
        .next()
        .ok_or("no snapshots on this peer yet")?;
    let tmp = std::env::temp_dir().join(format!("peerbackup-verify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let got = engine
        .restore_path(&latest.id, &file.path, &tmp)
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(got.sha256 == file.sha256)
}

// --------------------------------------------------------------------- status

pub fn status_cmd() -> Res {
    let cfg = Config::load().map_err(err("could not read config"))?;
    if cfg.peers.is_empty() {
        println!("No peers yet. Add one with `peerbackup peer add <name> <url>`.");
        return Ok(());
    }
    let records = Evidence::read_all();
    let t = now();
    let rows = status(&cfg, &records, t);

    println!(
        "{:<12} {:<11} {:<12} {:<12} {:<12} READ BACK",
        "PEER", "STATE", "BACKED UP", "CHECKED", "TEST FILE"
    );
    for r in &rows {
        println!(
            "{:<12} {:<11} {:<12} {:<12} {:<12} {}",
            r.name,
            r.state.label(),
            ago(r.last_backup, t),
            ago(r.last_subset, t),
            ago(r.last_canary, t),
            r.coverage_pct
                .map(|p| format!("{p}%"))
                .unwrap_or_else(|| "-".into()),
        );
    }

    for r in rows.iter().filter(|r| r.problem.is_some()) {
        println!();
        println!("{}: {}", r.name, r.problem.as_ref().unwrap());
    }

    // An unchecked peer usually just needs `verify`. But if its last attempt
    // failed, say so: "unchecked" alone reads as "nothing happened yet" when
    // the truth may be that every backup is being rejected.
    for r in rows.iter().filter(|r| r.state == PeerState::Unknown) {
        if let Some(reason) = last_failure(&records, &r.name) {
            println!();
            println!("{}: last attempt did not succeed", r.name);
            println!("  {reason}");
        }
    }

    let unchecked: Vec<&str> = rows
        .iter()
        .filter(|r| r.state == PeerState::Unknown)
        .map(|r| r.name.as_str())
        .collect();
    if !unchecked.is_empty() {
        println!();
        println!(
            "Not checked recently: {}. Run `peerbackup verify`.",
            unchecked.join(", ")
        );
    }

    if rows.iter().any(|r| r.state == PeerState::Bad) {
        return Err("one or more peers reported a problem".into());
    }
    Ok(())
}

/// Why a peer's most recent attempt did not succeed, if it did not.
fn last_failure(records: &[crate::state::Record], peer: &str) -> Option<String> {
    let last = records.iter().rfind(|r| r.peer == peer)?;
    (last.verdict != Verdict::Good)
        .then(|| last.detail.clone())
        .flatten()
}

// -------------------------------------------------------------------- restore

pub fn restore(peer_name: &str, target: &Path, snapshot: Option<&str>) -> Res {
    let cfg = Config::load().map_err(err("could not read config"))?;
    let peer = cfg
        .peer(peer_name)
        .ok_or_else(|| format!("no peer called '{peer_name}'"))?;
    let engine = engine_for(peer);

    let id = match snapshot {
        Some(s) => SnapshotId(s.to_string()),
        None => {
            let snaps = engine.list_snapshots().map_err(|e| e.to_string())?;
            // Skip the round-trip check `peer add` uploads: it contains the test
            // file and nothing else, so restoring it would look like success and
            // give you none of your data.
            snaps
                .iter()
                .find(|s| s.paths.iter().any(|p| cfg.settings.sources.contains(p)))
                .or_else(|| snaps.first())
                .ok_or("no backups on this peer")?
                .id
                .clone()
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
    let cfg = Config::load().map_err(err("could not read config"))?;
    let peer = cfg
        .peer(peer_name)
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
    let path = out.unwrap_or_else(|| state_dir().join("recovery.txt"));

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
        if let Some(ca) = &peer.ca_cert {
            s.push_str(&format!(
                "Certificate: {} (copy this file too)\n",
                ca.display()
            ));
        }
        s.push_str("\nTo see what is stored:\n");
        s.push_str(&format!("  restic -r '{}' snapshots\n", peer.url));
        s.push_str("\n(The --tag below matters: it skips the small connection test\n");
        s.push_str(" that peerbackup uploads when a peer is first set up.)\n");
        s.push_str("\nTo get everything back:\n");
        s.push_str(&format!(
            "  restic -r '{}' restore latest --tag {BACKUP_TAG} --target /where/to/put/it\n\n",
            peer.url
        ));
    }

    s.push_str(&format!("Exported: {}\n", now()));
    s.push_str(&format!("Fingerprint: {}\n", fingerprint(&cfg)));

    write_private(&path, s.as_bytes()).map_err(err("could not write the recovery file"))?;
    std::fs::write(state_dir().join("recovery.fingerprint"), fingerprint(&cfg))
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
        material.push_str(&p.name);
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

// -------------------------------------------------------------------- helpers

fn select<'a>(cfg: &'a Config, only: Option<&str>) -> Result<Vec<&'a Peer>, String> {
    match only {
        Some(name) => cfg
            .peer(name)
            .map(|p| vec![p])
            .ok_or_else(|| format!("no peer called '{name}'")),
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
    fn passwords_are_hidden_when_urls_are_printed() {
        let out = redact("rest:https://me:hunter2@alice.example.org:8000/me/");
        assert!(!out.contains("hunter2"), "password leaked: {out}");
        assert!(out.contains("alice.example.org"));
        assert!(out.contains("me"));
    }

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

    #[test]
    fn redacting_leaves_urls_without_credentials_alone() {
        let plain = "rest:https://alice.example.org:8000/me/";
        assert_eq!(redact(plain), plain);
    }
}
