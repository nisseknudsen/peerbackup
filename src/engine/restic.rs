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

/// The operations peerbackup runs, for the one purpose of saying what deadline
/// each gets.
///
/// Gathered into one function because the policy is not uniform and the reasons
/// are not obvious: two operations are deliberately unbounded, and which two is
/// the difference between a restore that finishes and a restore that is killed
/// half way. Scattered across four call sites, one of them was wrong for as long
/// as it existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Is this peer answering at all?
    Probe,
    /// Listing snapshots, and creating a repository.
    List,
    /// Reading a percentage of stored data back and checking it.
    Verify,
    /// Fetching the canary. A few kilobytes.
    CanaryRestore,
    /// Sending a backup.
    Backup,
    /// Getting everything back. The disaster operation.
    RestoreAll,
}

/// Extra restic tuning options from `PEERBACKUP_RESTIC_OPTS`.
///
/// Exists because the remedy for a long link is `-o rest.connections=N` and
/// there is no good default: more connections trade memory and server load for
/// throughput, and the right number depends on the round trip. restic's own
/// default of five stays the default here.
///
/// Only `-o key=value` pairs are accepted. The alternative -- passing whatever
/// is in the variable straight through -- would make this a general flag
/// injector, and `--insecure-tls` arriving that way would turn off certificate
/// verification with nothing said about it. Refusing anything else keeps the
/// knob to the thing it is for.
fn extra_opts() -> Result<Vec<String>, std::io::Error> {
    let Ok(raw) = std::env::var("PEERBACKUP_RESTIC_OPTS") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut tokens = raw.split_whitespace();
    while let Some(tok) = tokens.next() {
        match tok {
            "-o" => match tokens.next() {
                Some(v) if v.contains('=') && !v.starts_with('-') => {
                    out.push("-o".to_owned());
                    out.push(v.to_owned());
                }
                other => {
                    return Err(std::io::Error::other(format!(
                        "PEERBACKUP_RESTIC_OPTS: -o needs a key=value, got {:?}",
                        other.unwrap_or("nothing")
                    )));
                }
            },
            other => {
                return Err(std::io::Error::other(format!(
                    "PEERBACKUP_RESTIC_OPTS only accepts `-o key=value` pairs, \
                     and got {other:?}. It is for tuning, e.g. \
                     `-o rest.connections=10`, not for passing restic flags."
                )));
            }
        }
    }
    Ok(out)
}

/// Turn off HTTP/2 in restic's Go HTTP client.
///
/// restic calls `http2.ConfigureTransports` unconditionally
/// (`internal/backend/http_transport.go`) and offers no flag to undo it, so
/// over TLS it always negotiates HTTP/2. HTTP/2 then multiplexes every parallel
/// request onto **one** TCP connection, and one TCP connection to a distant
/// peer is a throughput ceiling no amount of upload concurrency can lift:
/// measured at about 58 Mbit/s over a 170ms link, against roughly 1 Gbit of
/// available bandwidth on both ends.
///
/// That is the wrong trade for this program specifically. peerbackup exists to
/// push large amounts of data to a friend's server which is, by construction,
/// somewhere else. restic's own default suits a nearby or local backend, where
/// multiplexing costs nothing; ours is the case where it costs almost
/// everything. Under HTTP/1.1 restic opens up to `rest.connections` sockets and
/// each gets its own congestion window.
///
/// A peerbase tunnel is unaffected either way, since restic then talks plain
/// HTTP to loopback and Go does not use HTTP/2 without TLS.
///
/// `GODEBUG` is a comma-separated list, so an inherited value is extended
/// rather than replaced -- and a caller who has already said something about
/// `http2client` has their choice left alone, which is the escape hatch.
///
/// Known publicly: <https://forum.restic.net/t/restic-rest-server-and-tcp-multiplexing/10803>
fn godebug() -> String {
    const OFF: &str = "http2client=0";
    match std::env::var("GODEBUG") {
        Ok(existing) if existing.contains("http2client") => existing,
        Ok(existing) if existing.trim().is_empty() => OFF.to_owned(),
        Ok(existing) => format!("{existing},{OFF}"),
        Err(_) => OFF.to_owned(),
    }
}

/// A restic invocation, and the short-lived files it needs on disk.
///
/// The files must outlive the child process and not one moment longer, which is
/// exactly a value's lifetime, so they ride along with the `Command` rather than
/// being cleaned up by whoever remembers.
struct Invocation {
    command: Command,
    _repo_file: SecretFile,
}

/// A 0600 file holding one secret, deleted when it goes out of scope.
///
/// `create_new` rather than `create`: the temp directory is world-writable, and
/// O_EXCL is what stops someone pre-creating the path as a symlink to something
/// they would like peerbackup to overwrite. The name is random for the same
/// reason, not for uniqueness -- a pid would do for that.
struct SecretFile(PathBuf);

impl SecretFile {
    fn new(tag: &str, contents: &[u8]) -> std::io::Result<Self> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let suffix = crate::config::random_token(16)?;
        let path = std::env::temp_dir().join(format!("peerbackup-{tag}-{suffix}"));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(contents)?;
        f.sync_all()?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A peer's repository.
#[derive(Debug, Clone)]
pub struct ResticEngine {
    /// e.g. `rest:https://user:pw@peer.example.org:8000/nisse/`.
    ///
    /// Carries the peer's HTTP credentials, which is why it reaches restic
    /// through `--repository-file` and never as an argument.
    pub repo_url: String,
    pub binary: PathBuf,
    /// A file, not an env var, so the secret stays out of process environments.
    pub password_file: PathBuf,
    /// PEM bundle for a peer with a self-signed certificate.
    pub ca_cert: Option<PathBuf>,
    pub verify_timeout: Duration,
    /// Bounds the canary restore, which fetches a few kilobytes.
    ///
    /// Deliberately *not* used for `restore_all`. See [`BackupEngine::restore_all`].
    pub canary_restore_timeout: Duration,
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
    pub const DEFAULT_CANARY_RESTORE_TIMEOUT: Duration = Duration::from_secs(1800);
    pub const DEFAULT_LIST_TIMEOUT: Duration = Duration::from_secs(120);
    pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(20);

    pub fn new(repo_url: impl Into<String>, password_file: impl Into<PathBuf>) -> Self {
        Self {
            repo_url: repo_url.into(),
            binary: PathBuf::from("restic"),
            password_file: password_file.into(),
            ca_cert: None,
            verify_timeout: Self::DEFAULT_VERIFY_TIMEOUT,
            canary_restore_timeout: Self::DEFAULT_CANARY_RESTORE_TIMEOUT,
            list_timeout: Self::DEFAULT_LIST_TIMEOUT,
            probe_timeout: Self::DEFAULT_PROBE_TIMEOUT,
        }
    }

    /// Build a restic invocation against this peer's repository.
    ///
    /// Every call gets the repository, the password file and the certificate, so
    /// no caller has to remember them.
    ///
    /// **Neither secret goes on the command line.** The repository password has
    /// always used `--password-file`; the doc here used to explain why and then,
    /// three lines up, pass `-r rest:https://user:pw@peer/` as an argument. That
    /// URL carries the peer's HTTP credentials, and on Linux with the default
    /// `hidepid=0` any local user can read `/proc/<pid>/cmdline` -- for the whole
    /// seventeen hours of the 300GB first seed this project is designed around.
    /// Whoever reads it can write to and read the repository at that peer.
    ///
    /// So the URL goes into a 0600 file too, and the file lives exactly as long
    /// as the invocation: [`Invocation`] owns it and deletes it on drop, which
    /// is why this returns a struct rather than a bare `Command`.
    fn command(&self, args: &[&str]) -> std::io::Result<Invocation> {
        let repo_file = SecretFile::new("repo", self.repo_url.as_bytes())?;
        let mut c = Command::new(&self.binary);
        c.arg("--repository-file").arg(repo_file.path());
        c.arg("--password-file").arg(&self.password_file);
        if let Some(ca) = &self.ca_cert {
            c.arg("--cacert").arg(ca);
        }
        // Anything restic would read from the environment that peerbackup has
        // not decided on itself. `--repository-file` and `--password-file` are
        // mutually exclusive with some of these, so an inherited value does not
        // quietly change behaviour -- it makes every operation fail with a
        // restic message about flags the user never passed. `RESTIC_CACERT` is
        // the quiet one: it would silently supply the trust store while
        // peerbackup believed it was using the system's.
        for k in [
            "RESTIC_REPOSITORY",
            "RESTIC_REPOSITORY_FILE",
            "RESTIC_PASSWORD",
            "RESTIC_PASSWORD_FILE",
            "RESTIC_PASSWORD_COMMAND",
            "RESTIC_KEY_HINT",
            "RESTIC_CACERT",
            "RESTIC_TLS_CLIENT_CERT",
        ] {
            c.env_remove(k);
        }
        c.env("GODEBUG", godebug());
        for opt in extra_opts()? {
            c.arg(opt);
        }
        c.args(args);
        Ok(Invocation {
            command: c,
            _repo_file: repo_file,
        })
    }

    /// `None` timeout means unbounded, which is correct for `snapshot`: a 300GB
    /// seed at 40Mbps legitimately takes 17 hours. Telling a stalled transfer
    /// from a slow one needs progress monitoring, which is separate work.
    fn run(&self, args: &[&str], timeout: Option<Duration>) -> Result<Output, EngineError> {
        let inv = match self.command(args) {
            Ok(i) => i,
            Err(e) => return Err(self.spawn_error(&e)),
        };
        match run_bounded(inv.command, timeout) {
            Err(e) => Err(EngineError {
                message: format!("could not execute {}: {e}", self.binary.display()),
                exit_code: None,
                cause: Cause::Unclassified {
                    detail: e.to_string(),
                },
                damage: None,
            }),
            Ok(None) => {
                let secs = timeout.map(|d| d.as_secs()).unwrap_or(0);
                Err(EngineError {
                    message: format!("restic did not finish within {secs}s and was killed"),
                    exit_code: None,
                    cause: Cause::TimedOut { after_secs: secs },
                    damage: None,
                })
            }
            Ok(Some(out)) if out.status.success() => Ok(out),
            Ok(Some(out)) => Err(self.to_engine_error(&out)),
        }
    }

    /// How long each operation may take before it is killed.
    ///
    /// `None` means unbounded, and exactly two operations get it. A 300GB first
    /// seed at 40Mbps legitimately takes seventeen hours, so `Backup` cannot
    /// have a wall-clock budget that is not either useless or lethal --  and
    /// `RestoreAll` is the same transfer in the other direction, at the one
    /// moment the user has already lost a disk. It used to share the canary
    /// restore's 1800s, which is the right size for a few kilobytes and killed a
    /// real restore after thirty minutes with a partial tree on disk and a
    /// message about a deadline rather than about their data.
    ///
    /// Telling a stalled transfer from a slow one needs progress monitoring, for
    /// both of them. Until that exists the honest answer is to let them run.
    #[must_use]
    pub fn timeout_for(&self, op: Op) -> Option<Duration> {
        match op {
            Op::Probe => Some(self.probe_timeout),
            Op::List => Some(self.list_timeout),
            Op::Verify => Some(self.verify_timeout),
            Op::CanaryRestore => Some(self.canary_restore_timeout),
            Op::Backup | Op::RestoreAll => None,
        }
    }

    /// Could not even get as far as running restic.
    fn spawn_error(&self, e: &std::io::Error) -> EngineError {
        EngineError {
            message: format!("could not prepare the restic invocation: {e}"),
            exit_code: None,
            cause: Cause::Unclassified {
                detail: e.to_string(),
            },
            damage: None,
        }
    }

    /// The restic version on `PATH`, checked against what this depends on.
    ///
    /// The README says "restic 0.17+" and nothing enforced it. Debian bookworm
    /// ships 0.14, and someone installing from source as the README describes
    /// would get it. That matters more than a version number usually does:
    /// `is_incomplete` reads a field out of `backup --json`, and the shape of
    /// that output is a contract with a specific restic. An older one produces a
    /// different shape, `is_incomplete` sees nothing, and a partial backup
    /// reports success -- the false-success class this project's history exists
    /// to prevent.
    ///
    /// Returns `Ok(None)` when the version cannot be determined, because
    /// refusing to run over an unparsed version string would be worse than the
    /// risk it guards against.
    pub fn version_problem(&self) -> Result<Option<String>, EngineError> {
        let out = self.run(&["version"], Some(Duration::from_secs(20)))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let Some((major, minor)) = parse_version(&text) else {
            return Ok(None);
        };
        // 0.17 is the floor the README states.
        if (major, minor) < (0, 17) {
            return Ok(Some(format!(
                "restic {major}.{minor} is older than 0.17, which peerbackup needs.\n  \
                 It reads the summary of `backup --json` to tell a complete backup \
                 from one\n  that could not read everything, and older versions do \
                 not report it the same way,\n  so a partial backup would be recorded \
                 as a success."
            )));
        }
        Ok(None)
    }

    /// Create the repository. Not on the trait: it is setup, not a backup
    /// operation, and only `peer add` ever calls it.
    pub fn init_repo(&self) -> Result<(), EngineError> {
        self.run(&["init"], self.timeout_for(Op::List)).map(|_| ())
    }

    fn to_engine_error(&self, out: &Output) -> EngineError {
        let combined = combined_output(out);
        let code = out.status.code();
        let (cause, damage) = match classify(code.unwrap_or(-1), &combined) {
            // The operation still failed, so `cause` keeps saying so -- but the
            // damage verdict travels with it now instead of being discarded.
            // The canary restore is the one place peerbackup reads bytes back
            // and compares them, and it goes through here.
            Classified::Damage(d) => (
                Cause::Unclassified {
                    detail: d.to_string(),
                },
                Some(d),
            ),
            Classified::NoVerdict(c) => (c, None),
        };
        EngineError {
            // restic writes `Fatal: create repository at
            // rest:http://me:pw@host/me/ failed: ...`, and this message is both
            // printed and persisted. `redact` exists and was unit-tested twice;
            // it was simply never applied to anything the engine produced.
            message: crate::redact::message(&strip_go_trace(&combined)),
            exit_code: code,
            cause,
            damage,
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
            // restic splits `--tag` on commas, so one containing a comma would
            // silently become two and `restore --tag peerbackup` would stop
            // matching. Both tags are constants today; this is here so that
            // stays true if one ever is not.
            debug_assert!(!t.contains(','), "a tag must not contain a comma: {t}");
            args.extend(["--tag".into(), t.clone()]);
        }
        // Everything after `--` is a path, whatever it starts with. A `sources`
        // entry beginning with a dash reached restic as a flag: `check_sources`
        // only requires `metadata()` to succeed, which a file literally named
        // `--insecure-tls` satisfies.
        args.push("--".into());
        args.extend(sources.iter().map(|s| s.display().to_string()));

        let refs: Vec<&str> = args.iter().map(String::as_str).collect();

        // Exit 3 is restic saying "I finished, but I could not read some of
        // what you asked for". There is a real snapshot behind it, and it is
        // missing data, which is precisely the case worth telling the user
        // about. `run` rejects every non-zero exit, so this is unwrapped here
        // rather than treated as a failure with raw restic text attached.
        let out = match self.run(&refs, self.timeout_for(Op::Backup)) {
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
                damage: None,
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
                "--target",
                &target.display().to_string(),
                "--include",
                &path.display().to_string(),
                // The id last, after `--`, so it cannot be read as a flag.
                "--",
                &snapshot.0,
            ],
            self.timeout_for(Op::CanaryRestore),
        )?;

        // restic reconstructs the full source path under target.
        let landed = target.join(path.strip_prefix("/").unwrap_or(path));
        let io_err = |e: std::io::Error| EngineError {
            message: format!("restore reported success but {}: {e}", landed.display()),
            exit_code: None,
            cause: Cause::Unclassified {
                detail: e.to_string(),
            },
            damage: None,
        };
        let bytes = std::fs::metadata(&landed).map_err(io_err)?.len();
        let sha256 = sha256_file(&landed).map_err(io_err)?;
        Ok(RestoredFile {
            path: landed,
            sha256,
            bytes,
        })
    }

    /// Unbounded. See [`ResticEngine::timeout_for`].
    fn restore_all(&self, snapshot: &SnapshotId, target: &Path) -> Result<(), EngineError> {
        self.run(
            &[
                "restore",
                "--target",
                &target.display().to_string(),
                // `restore --snapshot <s>` takes an unvalidated string straight
                // from the command line, so `peerbackup restore alice /mnt/new
                // --snapshot --insecure-tls` handed restic that flag and turned
                // off certificate verification for the run.
                "--",
                &snapshot.0,
            ],
            self.timeout_for(Op::RestoreAll),
        )?;

        // Asserted, not assumed. `restic restore` exits 0 for a selection that
        // matched nothing, and `restore` then printed "Done." to someone who had
        // just lost a disk. `restore_path` at least stats and hashes what
        // landed; this had no check at all.
        let landed = std::fs::read_dir(target)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if !landed {
            return Err(EngineError {
                message: format!(
                    "restic reported success but {} is empty. \
                     The snapshot may hold nothing, or nothing matched.",
                    target.display()
                ),
                exit_code: None,
                cause: Cause::Unclassified {
                    detail: "restore wrote no files".into(),
                },
                damage: None,
            });
        }
        Ok(())
    }

    fn verify_subset(&self, percent: u8) -> VerifyOutcome {
        // A backstop, not the validation. `Settings::validate` refuses a
        // configured value outside 1..=100 and names the key, because clamping
        // 0 to 1 here meant someone who asked for no read-back silently got
        // some, three layers from where they wrote it. This stays because the
        // trait is a public seam and restic rejects a 0% subset outright.
        let pct = percent.clamp(1, 100);
        let inv = match self.command(&["check", "--read-data-subset", &format!("{pct}%")]) {
            Ok(i) => i,
            Err(e) => {
                return VerifyOutcome::Indeterminate(Cause::Unclassified {
                    detail: format!("could not prepare the restic invocation: {e}"),
                });
            }
        };

        let out = match run_bounded(inv.command, self.timeout_for(Op::Verify)) {
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
        match self.run(&["cat", "config"], self.timeout_for(Op::Probe)) {
            Ok(_) => None,
            Err(e) => Some(e.cause),
        }
    }

    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, EngineError> {
        let out = self.run(&["snapshots", "--json"], self.timeout_for(Op::List))?;
        parse_snapshots(&String::from_utf8_lossy(&out.stdout)).map_err(|e| EngineError {
            message: format!("could not parse restic snapshot output: {e}"),
            exit_code: None,
            cause: Cause::Unclassified {
                detail: e.to_string(),
            },
            damage: None,
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
    // Nothing here is interactive, and `snapshot` runs with no deadline -- so a
    // restic that decided to prompt on an inherited terminal would hang the
    // backup with nothing to break it. Closing stdin turns any such prompt into
    // an immediate EOF and an error we can classify.
    cmd.stdin(Stdio::null());
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

    let start = Instant::now();
    let deadline = timeout.map(|d| start + d);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if deadline.is_some_and(|dl| Instant::now() >= dl) {
            // Ask once more before killing. A child that exited inside the
            // window between the `try_wait` above and this check was killed and
            // reported as `TimedOut`, so a verify that finished on the deadline
            // read as "could not check" instead of using its answer. The window
            // is microseconds and the misclassification is fail-safe, but the
            // answer is right here.
            if let Some(s) = child.try_wait()? {
                break Some(s);
            }
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        // 50ms is fine for an operation measured in seconds; a verify can run
        // for an hour, and waking 72,000 times to ask a question whose answer
        // almost never changes is pure waste. Backing off to a quarter second
        // after the first few seconds keeps a short command responsive and a
        // long one cheap.
        thread::sleep(if start.elapsed() < Duration::from_secs(5) {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(250)
        });
    };

    // Join on both paths. On the timeout path these used to be dropped and the
    // threads detached; they do exit once the pipes close after the kill, but
    // leaving them unjoined asserts a cleanup that was not performed. The kill
    // and wait above have already happened, so neither can block.
    // A reader thread only panics if the allocator gives out, and then its
    // output is empty -- which `classify` reads as `Unclassified`, i.e. a
    // verdict of "we learned nothing". Fail-safe, but silent, and a run whose
    // output vanished should say so rather than looking like a run that
    // produced none.
    let stdout = t_out
        .join()
        .unwrap_or_else(|_| b"peerbackup: the thread reading restic's stdout failed".to_vec());
    let stderr = t_err
        .join()
        .unwrap_or_else(|_| b"peerbackup: the thread reading restic's stderr failed".to_vec());

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
/// Belt and braces alongside the exit code: older restic reported some partial
/// reads on exit 0, and the summary carries an explicit count either way.
///
/// Indices come from `find` and a literal length, so they land on character
/// boundaries; the lint cannot see that.
#[allow(clippy::string_slice)]
fn is_incomplete(combined: &str) -> bool {
    // Read out of the field restic puts it in, not found anywhere in the stream.
    // `backup --json` emits a status line per file carrying paths the user
    // chose, so a directory named `could not be read` under `sources` made every
    // backup report INCOMPLETE, record `Unknown` and exit non-zero. Contrived,
    // but it is a false red on the headline command, and the fixture below shows
    // exactly which field the real signal arrives in.
    #[derive(Deserialize)]
    struct Line {
        message_type: Option<String>,
        message: Option<String>,
    }
    for line in combined.lines() {
        let said = match serde_json::from_str::<Line>(line) {
            // A JSON line only counts when it is restic's own error summary.
            Ok(l) => match l.message_type.as_deref() {
                Some("exit_error" | "error") => l.message.unwrap_or_default(),
                _ => continue,
            },
            // Not JSON, so it is plain stderr and all of it is restic's voice.
            Err(_) => line.to_owned(),
        };
        if said.contains("could not be read") {
            return true;
        }
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

/// `restic 0.19.1 compiled with go1.26.4 on linux/amd64` -> `(0, 19)`.
fn parse_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.trim().strip_prefix("restic ")?;
    let num = rest.split_whitespace().next()?;
    let mut parts = num.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
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
    // `Option`, because Go marshals a nil slice as `null` rather than `[]`. A
    // bare `Vec` errors on that, so an empty repository produced
    // "could not parse restic snapshot output: invalid type: null" instead of
    // no snapshots -- which made `newest_real_backup`'s carefully worded
    // "holds no backups at all" message unreachable and turned the empty-peer
    // case into a parser bug report.
    let v: Vec<SnapshotJson> = serde_json::from_str::<Option<_>>(stdout)?.unwrap_or_default();
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
    fn a_filename_cannot_make_a_healthy_backup_report_incomplete() {
        // `backup --json` emits a status line per file, carrying paths the user
        // chose. Substring-matching the whole stream meant a directory named
        // `could not be read` under `sources` reported INCOMPLETE on every run.
        let hostile = concat!(
            r#"{"message_type":"status","action":"scan_finished","current_files":["/srv/data/could not be read/x"]}"#,
            "\n",
            r#"{"message_type":"summary","snapshot_id":"aaaa1111","error_count":0}"#,
        );
        assert!(!is_incomplete(hostile), "a filename is not an error");
    }

    #[test]
    fn restics_own_warning_still_reads_as_incomplete() {
        // The direction that matters. Real restic 0.19.1 output; the signal
        // arrives inside the exit_error line, which is why this is parsed
        // rather than pattern-matched.
        assert!(is_incomplete(REAL_INCOMPLETE_BACKUP));
        assert!(
            is_incomplete("Warning: at least one source file could not be read"),
            "plain stderr counts too"
        );
    }

    #[test]
    fn an_empty_repository_parses_as_no_snapshots() {
        // Go marshals a nil slice as `null`. A bare `Vec` errors on that, so the
        // empty-peer case surfaced as "could not parse restic snapshot output"
        // and the carefully worded "holds no backups at all" message was
        // unreachable.
        assert!(parse_snapshots("null").unwrap().is_empty());
        assert!(parse_snapshots("[]").unwrap().is_empty());
        assert!(
            parse_snapshots("not json").is_err(),
            "garbage is still an error"
        );
    }

    #[test]
    fn positional_arguments_cannot_be_read_as_flags() {
        // `peerbackup restore alice /mnt/new --snapshot --insecure-tls` handed
        // restic that flag and turned off certificate verification for the run.
        // A `sources` entry starting with a dash did the same on the way out.
        let e = ResticEngine::new("rest:https://host/me/", "/tmp/pw");
        let inv = e
            .command(&["restore", "--target", "/tmp/x", "--", "--insecure-tls"])
            .unwrap();
        let argv: Vec<String> = inv
            .command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let dashdash = argv.iter().position(|a| a == "--").expect("a -- guard");
        let flag = argv.iter().position(|a| a == "--insecure-tls").unwrap();
        assert!(
            dashdash < flag,
            "the guard must precede the value: {argv:?}"
        );
    }

    #[test]
    fn a_restic_version_string_is_read_correctly() {
        assert_eq!(
            parse_version("restic 0.19.1 compiled with go1.26.4 on linux/amd64"),
            Some((0, 19))
        );
        assert_eq!(parse_version("restic 0.14.0\n"), Some((0, 14)));
        assert_eq!(parse_version("restic 1.0.0"), Some((1, 0)));
        // Anything unrecognised yields no opinion rather than a refusal.
        assert_eq!(parse_version("not restic at all"), None);
        assert_eq!(parse_version("restic vNext"), None);
        assert_eq!(parse_version(""), None);
    }

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
    fn a_full_restore_is_unbounded_and_the_canary_restore_is_not() {
        // They shared one 1800s budget. That is the right size for the few
        // kilobytes the canary fetches, and no budget at all for the operation
        // this program exists to make possible: a 300GB restore over the link
        // that took seventeen hours to seed was killed after thirty minutes,
        // leaving a partial tree, at the one moment the user has already lost a
        // disk.
        let e = ResticEngine::new("rest:https://host/me/", "/tmp/pw");
        assert_eq!(e.timeout_for(Op::RestoreAll), None);
        assert_eq!(
            e.timeout_for(Op::Backup),
            None,
            "unchanged, and for the same reason"
        );
        assert_eq!(
            e.timeout_for(Op::CanaryRestore),
            Some(ResticEngine::DEFAULT_CANARY_RESTORE_TIMEOUT)
        );
        for op in [Op::Probe, Op::List, Op::Verify] {
            assert!(
                e.timeout_for(op).is_some(),
                "{op:?} must stay bounded: restic retries transport failures \
                 forever, so a dead peer would stall the whole run"
            );
        }
    }

    #[test]
    fn the_restore_timeout_env_var_moves_the_canary_budget() {
        // The variable is documented and keeps its name; what it bounds is now
        // only the check, because the disaster restore has no deadline to set.
        let mut e = ResticEngine::new("rest:https://host/me/", "/tmp/pw");
        e.canary_restore_timeout = Duration::from_secs(60);
        assert_eq!(
            e.timeout_for(Op::CanaryRestore),
            Some(Duration::from_secs(60))
        );
        assert_eq!(e.timeout_for(Op::RestoreAll), None);
    }

    #[test]
    fn tuning_options_reach_restic_but_arbitrary_flags_do_not() {
        // The remedy for a long link is `-o rest.connections=N`, and there is
        // no good default for it. Passing the variable through unfiltered would
        // make this a general flag injector, and `--insecure-tls` arriving that
        // way would disable certificate verification silently.
        //
        // SAFETY: this variable is read by nothing else in this binary.
        unsafe { std::env::set_var("PEERBACKUP_RESTIC_OPTS", "-o rest.connections=10") };
        assert_eq!(
            extra_opts().unwrap(),
            vec!["-o".to_owned(), "rest.connections=10".to_owned()]
        );

        for bad in [
            "--insecure-tls",
            "-o rest.connections=10 --insecure-tls",
            "-o --insecure-tls",
            "-o noequals",
        ] {
            unsafe { std::env::set_var("PEERBACKUP_RESTIC_OPTS", bad) };
            assert!(extra_opts().is_err(), "{bad:?} must be refused");
        }

        unsafe { std::env::remove_var("PEERBACKUP_RESTIC_OPTS") };
        assert!(extra_opts().unwrap().is_empty());
    }

    #[test]
    fn http2_is_turned_off_for_the_restic_child() {
        // restic calls `http2.ConfigureTransports` unconditionally and has no
        // flag to undo it, so over TLS it negotiates HTTP/2 and multiplexes
        // every parallel upload onto one TCP connection. Measured on a 170ms
        // link that caps the whole backup at about 58 Mbit/s against a gigabit
        // on both ends.
        let e = ResticEngine::new("rest:https://host/me/", "/tmp/pw");
        let inv = e.command(&["snapshots"]).unwrap();
        let godebug = inv
            .command
            .get_envs()
            .find(|(k, _)| *k == "GODEBUG")
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned())
            .expect("GODEBUG must be set");
        assert!(godebug.contains("http2client=0"), "got {godebug}");
    }

    #[test]
    fn an_inherited_godebug_is_extended_rather_than_replaced() {
        // It is a comma-separated list, and clobbering someone's unrelated
        // setting to fix throughput would be a poor trade.
        //
        // SAFETY: GODEBUG is read by nothing else in this binary, and these
        // assertions do not run concurrently with another reader of it.
        unsafe { std::env::set_var("GODEBUG", "madvdontneed=1") };
        assert_eq!(godebug(), "madvdontneed=1,http2client=0");

        // A caller who already said something about http2client keeps it. This
        // is the escape hatch for anyone who wants HTTP/2 back.
        unsafe { std::env::set_var("GODEBUG", "http2client=1") };
        assert_eq!(godebug(), "http2client=1");

        unsafe { std::env::set_var("GODEBUG", "") };
        assert_eq!(godebug(), "http2client=0");

        unsafe { std::env::remove_var("GODEBUG") };
        assert_eq!(godebug(), "http2client=0");
    }

    #[test]
    fn no_secret_ever_reaches_the_command_line() {
        // `/proc/<pid>/cmdline` is world-readable under the default hidepid=0,
        // and a first seed runs for hours. The password was already handled;
        // the repository URL, which carries the peer's HTTP credentials, was
        // passed as `-r rest://user:pw@host/` three lines below the comment
        // explaining why arguments are unsafe.
        let e = ResticEngine::new(
            "rest:https://me:hunter2@alice.example.org:8000/me/",
            "/tmp/pw",
        );
        let inv = e.command(&["snapshots", "--json"]).unwrap();
        let argv: Vec<String> = std::iter::once(inv.command.get_program())
            .chain(inv.command.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = argv.join(" ");
        assert!(!joined.contains("hunter2"), "password on argv: {joined}");
        assert!(
            !joined.contains("alice.example.org"),
            "repository on argv: {joined}"
        );
        assert!(joined.contains("--repository-file"), "{joined}");
        assert!(joined.contains("--password-file"), "{joined}");
        assert!(joined.contains("snapshots"), "the real args must survive");
    }

    #[test]
    fn the_repository_file_holds_the_url_at_0600_and_is_removed_after() {
        use std::os::unix::fs::PermissionsExt as _;

        let e = ResticEngine::new("rest:https://me:hunter2@alice.example.org/me/", "/tmp/pw");
        let path;
        {
            let inv = e.command(&["snapshots"]).unwrap();
            path = inv._repo_file.path().to_path_buf();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "rest:https://me:hunter2@alice.example.org/me/"
            );
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "was {mode:o}");
        }
        assert!(!path.exists(), "the file must not outlive the invocation");
    }

    #[test]
    fn restic_env_vars_from_the_ambient_environment_are_not_honoured() {
        // `RESTIC_CACERT` is the quiet one: it would supply the TLS trust store
        // while peerbackup believed it was using the system's. The mutually
        // exclusive ones are merely baffling -- every operation fails citing a
        // flag the user never passed.
        let e = ResticEngine::new("rest:https://host/me/", "/tmp/pw");
        let inv = e.command(&["snapshots"]).unwrap();
        let removed: Vec<_> = inv
            .command
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for k in [
            "RESTIC_REPOSITORY",
            "RESTIC_PASSWORD_COMMAND",
            "RESTIC_CACERT",
        ] {
            assert!(
                removed.iter().any(|r| r == k),
                "{k} not cleared: {removed:?}"
            );
        }
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
