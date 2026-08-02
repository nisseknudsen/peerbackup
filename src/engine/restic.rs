//! [`BackupEngine`] over the stock `restic` binary.
//!
//! The happy paths are thin. The value is in the failure handling, which is
//! where the three-state model either holds or collapses back into two.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::outcome::{Cause, VerifyOutcome};
use super::restic_error::{Classified, classify, strip_go_trace};
use super::{
    BackupEngine, EngineError, RestoredFile, Snapshot, SnapshotId, SnapshotMeta, SnapshotOpts,
};

/// A peer's repository.
#[derive(Debug, Clone)]
pub struct ResticEngine {
    /// e.g. `rest:https://user:pw@peer.example.org:8000/nisse/`
    pub repo_url: String,
    pub binary: PathBuf,
    /// A file, not an env var, so the secret stays out of process environments.
    pub password_file: PathBuf,
    /// PEM bundle for a peer with a self-signed certificate.
    pub ca_cert: Option<PathBuf>,
    pub verify_timeout: Duration,
    pub restore_timeout: Duration,
    pub list_timeout: Duration,
    /// How long to spend deciding whether a peer is reachable at all.
    pub probe_timeout: Duration,
}

impl ResticEngine {
    /// restic retries transport failures with exponential backoff and no overall
    /// deadline. Measured: a 1% check against an unreachable peer ran 631s
    /// before being killed. Unbounded waits stall the scheduler and every status
    /// read behind it.
    pub const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_secs(3600);
    pub const DEFAULT_RESTORE_TIMEOUT: Duration = Duration::from_secs(1800);
    pub const DEFAULT_LIST_TIMEOUT: Duration = Duration::from_secs(120);
    pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

    pub fn new(repo_url: impl Into<String>, password_file: impl Into<PathBuf>) -> Self {
        Self {
            repo_url: repo_url.into(),
            binary: PathBuf::from("restic"),
            password_file: password_file.into(),
            ca_cert: None,
            verify_timeout: Self::DEFAULT_VERIFY_TIMEOUT,
            restore_timeout: Self::DEFAULT_RESTORE_TIMEOUT,
            list_timeout: Self::DEFAULT_LIST_TIMEOUT,
            probe_timeout: Self::DEFAULT_PROBE_TIMEOUT,
        }
    }

    /// Quick check that the peer answers, before starting anything expensive.
    ///
    /// Without this, verifying an unreachable peer waits out the full
    /// verification timeout, because restic keeps retrying. An hour of nothing
    /// happening is not something anyone will sit through, and the answer is
    /// known within seconds anyway.
    ///
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.binary);
        c.arg("-r").arg(&self.repo_url);
        c.arg("--password-file").arg(&self.password_file);
        if let Some(ca) = &self.ca_cert {
            c.arg("--cacert").arg(ca);
        }
        c.args(args);
        c
    }

    /// `None` timeout means unbounded, which is correct for `snapshot`: a 300GB
    /// seed at 40Mbps legitimately takes 17 hours. Telling a stalled transfer
    /// from a slow one needs progress monitoring, which is separate work.
    fn run(&self, args: &[&str], timeout: Option<Duration>) -> Result<Output, EngineError> {
        match run_bounded(self.command(args), timeout) {
            Err(e) => Err(EngineError {
                message: format!("could not execute {}: {e}", self.binary.display()),
                exit_code: None,
                cause: Cause::Unclassified {
                    detail: e.to_string(),
                },
            }),
            Ok(None) => {
                let secs = timeout.map(|d| d.as_secs()).unwrap_or(0);
                Err(EngineError {
                    message: format!("restic did not finish within {secs}s and was killed"),
                    exit_code: None,
                    cause: Cause::TimedOut { after_secs: secs },
                })
            }
            Ok(Some(out)) if out.status.success() => Ok(out),
            Ok(Some(out)) => Err(self.to_engine_error(&out)),
        }
    }

    /// Create the repository. Not on the trait: it is setup, not a backup
    /// operation, and only `peer add` ever calls it.
    pub fn init_repo(&self) -> Result<(), EngineError> {
        self.run(&["init"], Some(self.list_timeout)).map(|_| ())
    }

    fn to_engine_error(&self, out: &Output) -> EngineError {
        let combined = combined_output(out);
        let code = out.status.code();
        let cause = match classify(code.unwrap_or(-1), &combined) {
            // Damage found during a backup or restore is still an operational
            // failure here; the damage verdict belongs to verification.
            Classified::Damage(d) => Cause::Unclassified {
                detail: d.to_string(),
            },
            Classified::NoVerdict(c) => c,
        };
        EngineError {
            message: strip_go_trace(&combined),
            exit_code: code,
            cause,
        }
    }
}

impl BackupEngine for ResticEngine {
    fn snapshot(&self, sources: &[PathBuf], opts: &SnapshotOpts) -> Result<Snapshot, EngineError> {
        let mut args: Vec<String> = vec!["backup".into(), "--json".into()];
        if let Some(kib) = opts.upload_limit_kib {
            args.extend(["--limit-upload".into(), kib.to_string()]);
        }
        for t in &opts.tags {
            args.extend(["--tag".into(), t.clone()]);
        }
        args.extend(sources.iter().map(|s| s.display().to_string()));

        let refs: Vec<&str> = args.iter().map(String::as_str).collect();

        // Exit 3 is restic saying "I finished, but I could not read some of
        // what you asked for". There is a real snapshot behind it, and it is
        // missing data, which is precisely the case worth telling the user
        // about. `run` rejects every non-zero exit, so this is unwrapped here
        // rather than treated as a failure with raw restic text attached.
        let out = match self.run(&refs, None) {
            Ok(out) => out,
            Err(e) if e.exit_code == Some(EXIT_INCOMPLETE) => {
                return match parse_snapshot_id(&e.message) {
                    Some(id) => Ok(Snapshot {
                        id,
                        incomplete: true,
                    }),
                    // Exit 3 with no snapshot id means nothing was stored.
                    None => Err(e),
                };
            }
            Err(e) => return Err(e),
        };

        let id = parse_snapshot_id(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| {
            EngineError {
                message: "backup reported success but emitted no snapshot_id".into(),
                exit_code: out.status.code(),
                cause: Cause::Unclassified {
                    detail: "no snapshot_id in the --json summary".into(),
                },
            }
        })?;
        Ok(Snapshot {
            id,
            incomplete: is_incomplete(&combined_output(&out)),
        })
    }

    fn restore_path(
        &self,
        snapshot: &SnapshotId,
        path: &Path,
        target: &Path,
    ) -> Result<RestoredFile, EngineError> {
        self.run(
            &[
                "restore",
                &snapshot.0,
                "--target",
                &target.display().to_string(),
                "--include",
                &path.display().to_string(),
            ],
            Some(self.restore_timeout),
        )?;

        // restic reconstructs the full source path under target.
        let landed = target.join(path.strip_prefix("/").unwrap_or(path));
        let io_err = |e: std::io::Error| EngineError {
            message: format!("restore reported success but {}: {e}", landed.display()),
            exit_code: None,
            cause: Cause::Unclassified {
                detail: e.to_string(),
            },
        };
        let bytes = std::fs::metadata(&landed).map_err(io_err)?.len();
        let sha256 = sha256_file(&landed).map_err(io_err)?;
        Ok(RestoredFile {
            path: landed,
            sha256,
            bytes,
        })
    }

    fn restore_all(&self, snapshot: &SnapshotId, target: &Path) -> Result<(), EngineError> {
        self.run(
            &[
                "restore",
                &snapshot.0,
                "--target",
                &target.display().to_string(),
            ],
            Some(self.restore_timeout),
        )?;
        Ok(())
    }

    fn verify_subset(&self, percent: u8) -> VerifyOutcome {
        let pct = percent.clamp(1, 100);
        let cmd = self.command(&["check", "--read-data-subset", &format!("{pct}%")]);

        let out = match run_bounded(cmd, Some(self.verify_timeout)) {
            Ok(Some(o)) => o,
            // Waited, got no answer. Not corruption, not health.
            Ok(None) => {
                return VerifyOutcome::Indeterminate(Cause::TimedOut {
                    after_secs: self.verify_timeout.as_secs(),
                });
            }
            Err(e) => {
                return VerifyOutcome::Indeterminate(Cause::Unclassified {
                    detail: format!("could not execute restic: {e}"),
                });
            }
        };

        if out.status.success() {
            return VerifyOutcome::Good { coverage_pct: pct };
        }

        // A failed `check` isn't automatically corruption: it also fails for full
        // disks, dead peers and append-only refusals.
        match classify(out.status.code().unwrap_or(-1), &combined_output(&out)) {
            Classified::Damage(d) => VerifyOutcome::Bad(d),
            Classified::NoVerdict(c) => VerifyOutcome::Indeterminate(c),
        }
    }

    /// Returns `None` when the peer responded.
    ///
    /// Without this, verifying an unreachable peer waits out the full
    /// verification timeout, because restic keeps retrying. An hour of nothing
    /// happening is not something anyone will sit through, and the answer is
    /// known within seconds anyway.
    fn probe(&self) -> Option<Cause> {
        match self.run(&["cat", "config"], Some(self.probe_timeout)) {
            Ok(_) => None,
            Err(e) => Some(e.cause),
        }
    }

    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, EngineError> {
        let out = self.run(&["snapshots", "--json"], Some(self.list_timeout))?;
        parse_snapshots(&String::from_utf8_lossy(&out.stdout)).map_err(|e| EngineError {
            message: format!("could not parse restic snapshot output: {e}"),
            exit_code: None,
            cause: Cause::Unclassified {
                detail: e.to_string(),
            },
        })
    }
}

/// Run a command with an optional deadline, killing it if the deadline passes.
///
/// stdout and stderr are drained on threads: polling `try_wait` while leaving
/// the pipes unread deadlocks once a buffer fills, which would reintroduce the
/// exact hang this exists to prevent.
///
/// `Ok(None)` means the deadline expired and the child was killed.
fn run_bounded(mut cmd: Command, timeout: Option<Duration>) -> std::io::Result<Option<Output>> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    let mut child_out = child.stdout.take().expect("stdout piped");
    let mut child_err = child.stderr.take().expect("stderr piped");
    let t_out = thread::spawn(move || {
        let mut v = Vec::new();
        let _ = child_out.read_to_end(&mut v);
        v
    });
    let t_err = thread::spawn(move || {
        let mut v = Vec::new();
        let _ = child_err.read_to_end(&mut v);
        v
    });

    let deadline = timeout.map(|d| Instant::now() + d);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if deadline.is_some_and(|dl| Instant::now() >= dl) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(50));
    };

    // Join on both paths. On the timeout path these used to be dropped and the
    // threads detached; they do exit once the pipes close after the kill, but
    // leaving them unjoined asserts a cleanup that was not performed. The kill
    // and wait above have already happened, so neither can block.
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();

    Ok(status.map(|status| Output {
        status,
        stdout,
        stderr,
    }))
}

fn combined_output(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    s
}

#[derive(Deserialize)]
struct BackupSummary {
    snapshot_id: Option<String>,
}

#[derive(Deserialize)]
struct SnapshotJson {
    short_id: String,
    time: String,
    #[serde(default)]
    paths: Vec<PathBuf>,
    /// restic omits the key entirely for an untagged snapshot rather than
    /// emitting an empty list.
    #[serde(default)]
    tags: Vec<String>,
}

/// restic's exit code for "completed, but could not read everything".
///
/// Since 0.17 this is how a partial backup is reported: there is a snapshot,
/// and it is missing data. Treating it as an ordinary failure loses the
/// snapshot id and shows raw restic output instead of saying what happened.
pub const EXIT_INCOMPLETE: i32 = 3;

/// Did this backup fail to read some of its sources?
///
/// Indices come from `find` and a literal length, so they land on character
/// boundaries; the lint cannot see that.
#[allow(clippy::string_slice)]
///
/// Belt and braces alongside the exit code: older restic reported some partial
/// reads on exit 0, and the summary carries an explicit count either way.
fn is_incomplete(combined: &str) -> bool {
    if combined.contains("could not be read") {
        return true;
    }
    // `"error_count":0` is the healthy case; any other value is not.
    match combined.find("\"error_count\":") {
        Some(i) => {
            let rest = &combined[i + "\"error_count\":".len()..];
            let digits: String = rest
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<u64>().is_ok_and(|n| n > 0)
        }
        None => false,
    }
}

/// `backup --json` is line-delimited; the summary line carries `snapshot_id`.
fn parse_snapshot_id(stdout: &str) -> Option<SnapshotId> {
    stdout.lines().rev().find_map(|l| {
        serde_json::from_str::<BackupSummary>(l)
            .ok()?
            .snapshot_id
            .map(SnapshotId)
    })
}

/// Newest first. Hand-rolled extraction used to live here and silently returned
/// an empty list, because restic nests a `summary` object inside each snapshot.
///
/// Sorted on the timestamp rather than reversing restic's output. `restore` and
/// the canary check both take the first element and call it the newest, so if
/// restic's ordering ever changed a bare `restore` would hand back the *oldest*
/// backup and print "Done." RFC3339 with a fixed offset sorts correctly as
/// text, and the offset is restic's own so it is consistent within a
/// repository; ties keep restic's order, which is stable.
fn parse_snapshots(stdout: &str) -> serde_json::Result<Vec<SnapshotMeta>> {
    let v: Vec<SnapshotJson> = serde_json::from_str(stdout)?;
    let mut out: Vec<SnapshotMeta> = v
        .into_iter()
        .map(|s| SnapshotMeta {
            id: SnapshotId(s.short_id),
            time: s.time,
            paths: s.paths,
            tags: s.tags,
        })
        .collect();
    out.sort_by(|a, b| b.time.cmp(&a.time));
    Ok(out)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim `restic 0.19.1 snapshots --json`, including the nested `summary`
    /// object that broke the previous hand-rolled parser. A fixture you write
    /// yourself proves only that your parser matches your imagination.
    const REAL_SNAPSHOTS_JSON: &str = r#"[{"time":"2026-07-27T14:52:17.825129263-07:00","tree":"38903fa1","paths":["/tmp/jsontest/src"],"hostname":"cachyos-x8664","username":"nisse","uid":1000,"gid":1000,"program_version":"restic 0.19.1","summary":{"backup_start":"2026-07-27T14:52:17.8-07:00","backup_end":"2026-07-27T14:52:18.4-07:00","files_new":1,"data_added":1481,"total_bytes_processed":3},"id":"54d973943c4e9698","short_id":"54d97394"}]"#;

    #[test]
    fn parses_real_restic_output_with_a_nested_summary() {
        let snaps = parse_snapshots(REAL_SNAPSHOTS_JSON).unwrap();
        assert_eq!(
            snaps.len(),
            1,
            "nested summary must not swallow the snapshot"
        );
        assert_eq!(snaps[0].id.0, "54d97394");
        assert_eq!(snaps[0].paths, vec![PathBuf::from("/tmp/jsontest/src")]);
    }

    /// Verbatim from `restic 0.19.1 snapshots --json`, trimmed to the fields
    /// this layer reads. Captured from a real repository holding one backup and
    /// one `peer add` check, because `restore` now decides which snapshot holds
    /// your data by reading `tags`. If restic ever renamed that key, every
    /// snapshot would parse as untagged and `restore` would refuse all of them.
    const REAL_TAGGED_SNAPSHOTS_JSON: &str = r#"[{"time": "2026-07-31T22:14:14.246121119-07:00", "paths": ["/tmp/src"], "tags": ["peerbackup"], "short_id": "4d14d8df"}, {"time": "2026-07-31T22:14:14.94813273-07:00", "paths": ["/tmp/src"], "tags": ["peerbackup-check"], "short_id": "bb1a23ec"}]"#;

    #[test]
    fn parses_the_tags_restic_actually_emits() {
        let snaps = parse_snapshots(REAL_TAGGED_SNAPSHOTS_JSON).unwrap();
        // Newest first, so the check snapshot leads.
        assert_eq!(snaps[0].tags, vec!["peerbackup-check".to_owned()]);
        assert_eq!(snaps[1].tags, vec!["peerbackup".to_owned()]);
    }

    #[test]
    fn an_untagged_snapshot_parses_as_having_no_tags() {
        // restic omits the key entirely rather than emitting []. Without the
        // serde default this would fail to parse and list_snapshots would
        // report the whole repository as unreadable.
        let snaps = parse_snapshots(REAL_SNAPSHOTS_JSON).unwrap();
        assert!(snaps[0].tags.is_empty());
    }

    #[test]
    fn lists_snapshots_newest_first() {
        // Deliberately unsorted input. The version this replaced just reversed
        // restic's output, so a test feeding ascending input and asserting a
        // reversal could not fail for any reason that mattered -- it pinned the
        // implementation, not the property callers depend on.
        let json = r#"[
          {"time":"2026-07-02T10:00:00Z","short_id":"bbbb2222","paths":["/srv/data"]},
          {"time":"2026-06-01T10:00:00Z","short_id":"cccc3333","paths":["/srv/data"]},
          {"time":"2026-07-30T10:00:00Z","short_id":"dddd4444","paths":["/srv/data"]},
          {"time":"2026-07-01T10:00:00Z","short_id":"aaaa1111","paths":["/srv/data"]}
        ]"#;
        let snaps = parse_snapshots(json).unwrap();
        let ids: Vec<&str> = snaps.iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(ids, ["dddd4444", "bbbb2222", "aaaa1111", "cccc3333"]);
    }

    #[test]
    fn ordering_holds_for_the_offsets_restic_actually_emits() {
        // restic writes local time with an offset, not Z.
        let json = r#"[
          {"time":"2026-07-27T14:52:17.825129263-07:00","short_id":"older"},
          {"time":"2026-07-27T15:52:17.825129263-07:00","short_id":"newer"}
        ]"#;
        let snaps = parse_snapshots(json).unwrap();
        assert_eq!(snaps[0].id.0, "newer");
    }

    #[test]
    fn malformed_snapshot_json_is_an_error_not_an_empty_list() {
        // The bug this replaced returned an empty vec on input it could not
        // understand, which reads as "this peer holds nothing".
        assert!(parse_snapshots("not json at all").is_err());
    }

    /// Verbatim from `restic 0.19.1 backup --json` over a directory containing
    /// an unreadable subdirectory. Trimmed to the two lines that matter.
    ///
    /// Note what is NOT here: `error_count`. The heuristic this replaced looked
    /// for that field, and restic 0.19.1 does not emit it, so that half was
    /// dead. The other half looked for "could not be read", which only ever
    /// appears alongside exit code 3 -- and `run` rejects every non-zero exit,
    /// so the whole incomplete check was unreachable and the user got raw
    /// restic text instead of the message written for this case.
    const REAL_INCOMPLETE_BACKUP: &str = concat!(
        r#"{"message_type":"error","error":{"message":"openfile for readdirnames failed: open /srv/data/sub: permission denied"},"during":"scan","item":"/srv/data/sub"}"#,
        "\n",
        r#"{"message_type":"summary","files_new":1,"data_added":3030,"total_bytes_processed":3,"snapshot_id":"80aec4e00642cb0da6162c2d7bb17c6744bfa11caa0d393f7ebe7bdaf8e9bf58"}"#,
        "\n",
        r#"{"message_type":"exit_error","code":3,"message":"Warning: at least one source file could not be read"}"#,
    );

    #[test]
    fn a_partial_backup_still_yields_its_snapshot_id() {
        // Exit 3 carries a real snapshot. Losing it would mean reporting a
        // failure for a backup that did store data, just not all of it.
        let id = parse_snapshot_id(REAL_INCOMPLETE_BACKUP).expect("summary carries the id");
        assert_eq!(
            id.0,
            "80aec4e00642cb0da6162c2d7bb17c6744bfa11caa0d393f7ebe7bdaf8e9bf58"
        );
    }

    #[test]
    fn a_partial_backup_reads_as_incomplete() {
        assert!(is_incomplete(REAL_INCOMPLETE_BACKUP));
    }

    #[test]
    fn a_clean_backup_does_not_read_as_incomplete() {
        let clean = r#"{"message_type":"summary","files_new":1,"data_added":1481,"snapshot_id":"54d973943c4e9698"}"#;
        assert!(!is_incomplete(clean));
        // And the field-based form, for restic builds that do emit it.
        assert!(!is_incomplete(r#"{"error_count":0,"files_new":1}"#));
        assert!(is_incomplete(r#"{"error_count":2,"files_new":1}"#));
        assert!(is_incomplete(r#"{"error_count": 7 }"#));
    }

    #[test]
    fn extracts_snapshot_id_from_the_summary_line() {
        let stdout = concat!(
            r#"{"message_type":"status","percent_done":0.5}"#,
            "\n",
            r#"{"message_type":"summary","files_new":3,"snapshot_id":"7e68e51e"}"#,
            "\n"
        );
        assert_eq!(parse_snapshot_id(stdout).unwrap().0, "7e68e51e");
    }

    #[test]
    fn missing_snapshot_id_is_not_silently_ignored() {
        assert!(parse_snapshot_id(r#"{"message_type":"status","percent_done":1.0}"#).is_none());
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        let f = std::env::temp_dir().join("pb-sha-test");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(
            sha256_file(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn engine_error_message_has_no_go_trace() {
        let engine = ResticEngine::new("rest:http://127.0.0.1:1/x/", "/dev/null");
        let out = Output {
            status: exit_status(3),
            stdout: Vec::new(),
            stderr: b"failed to remove one or more snapshots\nmain.init\n\t/restic/cmd/restic/cmd_forget.go:67\nruntime.goexit\n\t/usr/local/go/src/runtime/asm_amd64.s:1771".to_vec(),
        };
        let err = engine.to_engine_error(&out);
        assert!(err.message.contains("failed to remove"));
        assert!(!err.message.contains("runtime.goexit"));
        assert_eq!(err.exit_code, Some(3));
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn a_missing_restic_binary_yields_indeterminate_not_bad() {
        let mut engine = ResticEngine::new("rest:http://127.0.0.1:1/x/", "/dev/null");
        engine.binary = PathBuf::from("/nonexistent/restic");
        assert_eq!(engine.verify_subset(5).label(), "unknown");
    }

    #[test]
    fn an_unreachable_peer_times_out_into_indeterminate_not_bad() {
        // Without a deadline this ran for 631s: restic retries transport
        // failures with backoff and no overall limit.
        let restic = std::env::var("RESTIC_BIN").unwrap_or_else(|_| "restic".into());
        if Command::new(&restic).arg("version").output().is_err() {
            eprintln!("skipping: no restic binary");
            return;
        }
        let pw = std::env::temp_dir().join("pb-test-pw");
        std::fs::write(&pw, "irrelevant").unwrap();
        let mut engine = ResticEngine::new("rest:http://127.0.0.1:1/nope/", &pw);
        engine.binary = PathBuf::from(restic);
        engine.verify_timeout = Duration::from_secs(2);

        let started = Instant::now();
        let outcome = engine.verify_subset(1);
        assert!(
            !outcome.is_bad(),
            "unreachable is not corruption: {outcome}"
        );
        assert_eq!(outcome.label(), "unknown");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "deadline ignored"
        );
    }

    #[test]
    fn run_bounded_kills_a_process_that_overruns() {
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        let started = Instant::now();
        assert!(
            run_bounded(cmd, Some(Duration::from_millis(300)))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn run_bounded_returns_output_when_the_process_finishes_in_time() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf out; printf err >&2; exit 7");
        let got = run_bounded(cmd, Some(Duration::from_secs(10)))
            .unwrap()
            .unwrap();
        assert_eq!(got.status.code(), Some(7));
        assert_eq!(String::from_utf8_lossy(&got.stdout), "out");
        assert_eq!(String::from_utf8_lossy(&got.stderr), "err");
    }

    #[test]
    fn run_bounded_does_not_deadlock_on_a_large_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("yes hello | head -c 2000000");
        let got = run_bounded(cmd, Some(Duration::from_secs(30)))
            .unwrap()
            .unwrap();
        assert_eq!(got.stdout.len(), 2_000_000);
    }
}
