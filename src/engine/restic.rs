//! [`BackupEngine`] over the stock `restic` binary.
//!
//! Everything interesting here is error handling. The happy paths are thin
//! wrappers around a subprocess; the value is in what happens when restic
//! fails, because that is where the three-state model either holds or quietly
//! collapses into two.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::outcome::{Cause, VerifyOutcome};
use super::restic_error::{Classified, classify, strip_go_trace};
use super::{BackupEngine, EngineError, RestoredFile, SnapshotId, SnapshotMeta, SnapshotOpts};

/// A peer's repository, addressed by a restic URL.
#[derive(Debug, Clone)]
pub struct ResticEngine {
    /// e.g. `rest:https://user:pw@peer.example.org:8000/nisse/`
    pub repo_url: String,
    /// Path to the restic binary. Not assumed to be on `PATH`.
    pub binary: PathBuf,
    /// File holding the repository password. A file rather than an env var so
    /// the secret does not show up in another process's environment.
    pub password_file: PathBuf,
    /// PEM bundle for a peer using a self-signed certificate.
    pub ca_cert: Option<PathBuf>,
    /// Deadline for verification. See [`ResticEngine::DEFAULT_VERIFY_TIMEOUT`].
    pub verify_timeout: Duration,
    /// Deadline for a canary restore. Canaries are small; a slow one is a sick
    /// peer, not a big transfer.
    pub restore_timeout: Duration,
    /// Deadline for listing snapshots. Metadata only, so this should be fast or
    /// something is wrong.
    pub list_timeout: Duration,
}

impl ResticEngine {
    /// restic retries transport failures with exponential backoff and no overall
    /// deadline. Measured: `check --read-data-subset 1%` against an unreachable
    /// peer ran for more than ten minutes before being killed. Unbounded waits
    /// like that stall the verification scheduler and every status read behind
    /// it, so bounded operations get a deadline.
    pub const DEFAULT_VERIFY_TIMEOUT: Duration = Duration::from_secs(60 * 60);
    pub const DEFAULT_RESTORE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
    pub const DEFAULT_LIST_TIMEOUT: Duration = Duration::from_secs(120);

    pub fn new(repo_url: impl Into<String>, password_file: impl Into<PathBuf>) -> Self {
        Self {
            repo_url: repo_url.into(),
            binary: PathBuf::from("restic"),
            password_file: password_file.into(),
            ca_cert: None,
            verify_timeout: Self::DEFAULT_VERIFY_TIMEOUT,
            restore_timeout: Self::DEFAULT_RESTORE_TIMEOUT,
            list_timeout: Self::DEFAULT_LIST_TIMEOUT,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.binary);
        c.arg("-r").arg(&self.repo_url);
        c.arg("--password-file").arg(&self.password_file);
        if let Some(ca) = &self.ca_cert {
            c.arg("--cacert").arg(ca);
        }
        // Machine-readable where restic supports it. Human output is for humans;
        // parsing it is how you get a classifier that breaks on a version bump.
        c.args(args);
        c
    }

    /// Run restic with an optional deadline.
    ///
    /// `None` means unbounded, which is correct for `snapshot`: a 300GB seed at
    /// 40Mbps legitimately takes seventeen hours. Distinguishing a *stalled*
    /// transfer from a merely slow one needs progress monitoring and is separate
    /// work; a blanket timeout here would kill legitimate seeds.
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
            Ok(Some(out)) => {
                if out.status.success() {
                    Ok(out)
                } else {
                    Err(self.to_engine_error(&out))
                }
            }
        }
    }

    /// Build an [`EngineError`] from a failed invocation, stripping the Go trace
    /// and reusing the verifier's classifier so a caller can tell "peer is full"
    /// from "peer is gone" without touching strings.
    fn to_engine_error(&self, out: &Output) -> EngineError {
        let combined = combined_output(out);
        let message = strip_go_trace(&combined);
        let code = out.status.code();
        let cause = match classify(code.unwrap_or(-1), &combined) {
            // A backup or restore that hit damage is still an operational
            // failure to the caller; the damage verdict belongs to verification.
            Classified::Damage(d) => Cause::Unclassified {
                detail: d.to_string(),
            },
            Classified::NoVerdict(c) => c,
        };
        EngineError {
            message,
            exit_code: code,
            cause,
        }
    }
}

/// Run a command with an optional deadline, killing it if the deadline passes.
///
/// stdout and stderr are drained on separate threads. Polling `try_wait` while
/// leaving the pipes unread deadlocks as soon as a child fills a pipe buffer,
/// which is a classic way to reintroduce the exact hang this function exists to
/// prevent.
///
/// Returns `Ok(None)` when the deadline expired and the child was killed.
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

impl BackupEngine for ResticEngine {
    fn snapshot(
        &self,
        sources: &[PathBuf],
        opts: &SnapshotOpts,
    ) -> Result<SnapshotId, EngineError> {
        let mut args: Vec<String> = vec!["backup".into(), "--json".into()];
        if let Some(kib) = opts.upload_limit_kib {
            args.push("--limit-upload".into());
            args.push(kib.to_string());
        }
        for t in &opts.tags {
            args.push("--tag".into());
            args.push(t.clone());
        }
        for s in sources {
            args.push(s.display().to_string());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run(&refs, None)?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        parse_snapshot_id(&stdout).ok_or_else(|| EngineError {
            message: "backup reported success but emitted no snapshot_id".into(),
            exit_code: out.status.code(),
            cause: Cause::Unclassified {
                detail: "missing snapshot_id in --json summary".into(),
            },
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
        let bytes = std::fs::metadata(&landed)
            .map_err(|e| EngineError {
                message: format!(
                    "restore reported success but {} is not there: {e}",
                    landed.display()
                ),
                exit_code: None,
                cause: Cause::Unclassified {
                    detail: e.to_string(),
                },
            })?
            .len();
        let sha256 = sha256_file(&landed).map_err(|e| EngineError {
            message: format!("could not hash the restored file: {e}"),
            exit_code: None,
            cause: Cause::Unclassified {
                detail: e.to_string(),
            },
        })?;
        Ok(RestoredFile {
            path: landed,
            sha256,
            bytes,
        })
    }

    fn verify_subset(&self, percent: u8) -> VerifyOutcome {
        let pct = percent.clamp(1, 100);
        let subset = format!("{pct}%");
        let cmd = self.command(&["check", "--read-data-subset", &subset]);
        let out = match run_bounded(cmd, Some(self.verify_timeout)) {
            Ok(Some(o)) => o,
            Ok(None) => {
                // We waited and got no answer. That is not corruption, and it is
                // not health either.
                return VerifyOutcome::Indeterminate(Cause::TimedOut {
                    after_secs: self.verify_timeout.as_secs(),
                });
            }
            Err(e) => {
                // Could not even start restic. We learned nothing about the data.
                return VerifyOutcome::Indeterminate(Cause::Unclassified {
                    detail: format!("could not execute restic: {e}"),
                });
            }
        };

        if out.status.success() {
            return VerifyOutcome::Good { coverage_pct: pct };
        }

        // The load-bearing line in this file. `check` failing is NOT
        // automatically corruption: it fails for full disks, dead peers and
        // append-only refusals too. Only the classifier decides.
        match classify(out.status.code().unwrap_or(-1), &combined_output(&out)) {
            Classified::Damage(d) => VerifyOutcome::Bad(d),
            Classified::NoVerdict(c) => VerifyOutcome::Indeterminate(c),
        }
    }

    fn list_snapshots(&self) -> Result<Vec<SnapshotMeta>, EngineError> {
        let out = self.run(&["snapshots", "--json"], Some(self.list_timeout))?;
        Ok(parse_snapshots(&String::from_utf8_lossy(&out.stdout)))
    }
}

// --------------------------------------------------------------------------
// Minimal parsing.
//
// restic's --json output is line-delimited for backup and a single array for
// snapshots. Only a few fields are needed, so these hand-rolled extractors keep
// a serde dependency out of the tree for now. If the shape grows, swap them for
// serde_json behind the same signatures.
// --------------------------------------------------------------------------

fn parse_snapshot_id(stdout: &str) -> Option<SnapshotId> {
    // The final summary line carries "snapshot_id".
    stdout
        .lines()
        .rev()
        .find_map(|l| json_str_field(l, "snapshot_id"))
        .map(SnapshotId)
}

fn parse_snapshots(stdout: &str) -> Vec<SnapshotMeta> {
    let mut out = Vec::new();
    // Split the array into objects without a full parser: adequate because the
    // fields we want never contain braces.
    for chunk in stdout.split('{').skip(1) {
        let obj = chunk.split('}').next().unwrap_or("");
        let (Some(id), Some(time)) = (json_str_field(obj, "short_id"), json_str_field(obj, "time"))
        else {
            continue;
        };
        let paths = json_str_array(obj, "paths")
            .into_iter()
            .map(PathBuf::from)
            .collect();
        out.push(SnapshotMeta {
            id: SnapshotId(id),
            time,
            paths,
        });
    }
    out.reverse(); // restic lists oldest first; callers want newest first.
    out
}

fn json_str_field(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let start = s.find(&pat)? + pat.len();
    let rest = s[start..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn json_str_array(s: &str, key: &str) -> Vec<String> {
    let pat = format!("\"{key}\":");
    let Some(start) = s.find(&pat) else {
        return Vec::new();
    };
    let rest = &s[start + pat.len()..];
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let Some(close) = rest[open..].find(']') else {
        return Vec::new();
    };
    rest[open + 1..open + close]
        .split(',')
        .filter_map(|p| {
            let p = p.trim().trim_matches('"');
            (!p.is_empty()).then(|| p.to_string())
        })
        .collect()
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.hex())
}

// A small SHA-256 so the seam has no dependencies yet. Swap for the `sha2`
// crate when the client grows one; the signature above does not change.
struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buflen: usize,
    len: u64,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            buflen: 0,
            len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        while !data.is_empty() {
            let take = (64 - self.buflen).min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buflen = 0;
            }
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        // Indexes two arrays in lockstep (w[i] and K[i]); enumerate() over one
        // of them would obscure that rather than clarify it.
        #[allow(clippy::needless_range_loop)]
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }

    fn hex(mut self) -> String {
        let bitlen = self.len.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buflen != 56 {
            self.update(&[0]);
        }
        // update() advanced len; write the original bit length directly.
        let block_tail = bitlen.to_be_bytes();
        self.buf[56..64].copy_from_slice(&block_tail);
        let block = self.buf;
        self.compress(&block);
        self.state.iter().map(|w| format!("{w:08x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        // If this is wrong, every canary comparison is wrong, so pin it against
        // the standard vectors rather than trusting the implementation.
        let mut h = Sha256::new();
        h.update(b"");
        assert_eq!(
            h.hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            h.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let mut h = Sha256::new();
        h.update(b"The quick brown fox jumps over the lazy dog");
        assert_eq!(
            h.hex(),
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
        );
    }

    #[test]
    fn sha256_handles_multi_block_input() {
        // Exercises the buffering path, which is where hand-rolled hashes break.
        let data = vec![0xABu8; 1000];
        let mut h = Sha256::new();
        h.update(&data);
        let one_shot = h.hex();

        let mut h = Sha256::new();
        for chunk in data.chunks(7) {
            h.update(chunk);
        }
        assert_eq!(one_shot, h.hex(), "chunked and one-shot must agree");
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
        let stdout = r#"{"message_type":"status","percent_done":1.0}"#;
        assert!(parse_snapshot_id(stdout).is_none());
    }

    #[test]
    fn parses_snapshots_newest_first() {
        let stdout = r#"[
          {"time":"2026-07-01T10:00:00Z","short_id":"aaaa1111","paths":["/srv/data"]},
          {"time":"2026-07-02T10:00:00Z","short_id":"bbbb2222","paths":["/srv/data","/etc"]}
        ]"#;
        let snaps = parse_snapshots(stdout);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].id.0, "bbbb2222", "newest snapshot must come first");
        assert_eq!(snaps[1].id.0, "aaaa1111");
        assert_eq!(snaps[0].paths.len(), 2);
    }

    #[test]
    fn engine_error_message_has_no_go_trace() {
        // An EngineError is what a user eventually reads. It must never contain
        // something that looks like a crash.
        let engine = ResticEngine::new("rest:http://127.0.0.1:1/x/", "/dev/null");
        let out = Output {
            status: exit_status(3),
            stdout: Vec::new(),
            stderr: b"failed to remove one or more snapshots\nmain.init\n\t/restic/cmd/restic/cmd_forget.go:67\nruntime.goexit\n\t/usr/local/go/src/runtime/asm_amd64.s:1771".to_vec(),
        };
        let err = engine.to_engine_error(&out);
        assert!(err.message.contains("failed to remove"));
        assert!(!err.message.contains("runtime.goexit"));
        assert!(!err.message.contains("/usr/local/go"));
        assert_eq!(err.exit_code, Some(3));
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn a_missing_restic_binary_yields_indeterminate_not_bad() {
        // The engine cannot even start. That says nothing about the data, so it
        // must not redden a peer.
        let mut engine = ResticEngine::new("rest:http://127.0.0.1:1/x/", "/dev/null");
        engine.binary = PathBuf::from("/nonexistent/restic-does-not-exist");
        let outcome = engine.verify_subset(5);
        assert!(
            !outcome.is_bad(),
            "a missing binary must not look like corruption"
        );
        assert_eq!(outcome.label(), "unknown");
    }

    #[test]
    fn an_unreachable_peer_times_out_into_indeterminate_not_bad() {
        // Port 1 on localhost refuses connections. Real subprocess, real restic.
        //
        // Without a deadline this test ran for 631 SECONDS: restic retries
        // transport failures with exponential backoff and no overall limit.
        // That is exactly what the timeout exists to contain, because a nightly
        // verification against a dead peer would otherwise still be running the
        // next night, with the scheduler and every status read stuck behind it.
        // Two seconds here; one hour in production.
        let restic = std::env::var("RESTIC_BIN").unwrap_or_else(|_| "restic".into());
        if std::process::Command::new(&restic)
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping: no restic binary available");
            return;
        }
        let pw = std::env::temp_dir().join("pb-test-pw");
        std::fs::write(&pw, "irrelevant").unwrap();
        let mut engine = ResticEngine::new("rest:http://127.0.0.1:1/nope/", &pw);
        engine.binary = PathBuf::from(restic);
        engine.verify_timeout = Duration::from_secs(2);

        let started = Instant::now();
        let outcome = engine.verify_subset(1);
        let elapsed = started.elapsed();

        assert!(
            !outcome.is_bad(),
            "an unreachable peer must never be reported as corruption, got {outcome}"
        );
        assert_eq!(outcome.label(), "unknown");
        assert!(
            elapsed < Duration::from_secs(20),
            "verification must respect its deadline; took {elapsed:?}"
        );
    }

    #[test]
    fn run_bounded_kills_a_process_that_overruns() {
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        let started = Instant::now();
        let got = run_bounded(cmd, Some(Duration::from_millis(300))).unwrap();
        assert!(
            got.is_none(),
            "an overrunning process must report as timed out"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the kill must be prompt"
        );
    }

    #[test]
    fn run_bounded_returns_output_when_the_process_finishes_in_time() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf out; printf err >&2; exit 7");
        let got = run_bounded(cmd, Some(Duration::from_secs(10)))
            .unwrap()
            .expect("should not time out");
        assert_eq!(got.status.code(), Some(7));
        assert_eq!(String::from_utf8_lossy(&got.stdout), "out");
        assert_eq!(String::from_utf8_lossy(&got.stderr), "err");
    }

    #[test]
    fn run_bounded_does_not_deadlock_on_a_large_output() {
        // Draining the pipes on threads is what makes this safe. Polling
        // try_wait() without reading them deadlocks once a pipe buffer fills,
        // reintroducing the very hang the timeout is meant to prevent.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("yes hello | head -c 2000000");
        let got = run_bounded(cmd, Some(Duration::from_secs(30)))
            .unwrap()
            .expect("should not time out");
        assert_eq!(got.stdout.len(), 2_000_000);
    }
}
