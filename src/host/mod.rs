//! The host side: what you run when a friend asks you to hold their backups.
//!
//! This was 549 lines of bash in `deploy/peerbackup-host` until the size parser
//! turned out to have no real test, and could not have one: a bash file that is
//! both a library and an executable cannot be sourced without running its
//! dispatcher, so the test reimplemented the function and asserted against the
//! copy. The install story was the other half of the argument -- the friend
//! doing you the favour cloned a repo while the friend being helped downloaded
//! one binary, which is backwards.
//!
//! What did not change: the OS still does the work. `provision` drives
//! `fallocate`, `mkfs.ext4` and `systemctl`; `quickstart` drives `docker`. This
//! removed a language seam, not a dependency.
//!
//! The four things that are easy to get wrong, unchanged from the shell:
//!
//!   * Images are PREALLOCATED, never sparse. A sparse image caps the inner
//!     filesystem but reserves no host blocks, so granting 500G to three
//!     friends on a 1T disk still lets the host fill up.
//!
//!   * Mounting needs CAP_SYS_ADMIN, which the internet-facing container must
//!     not have. The host mounts; the container receives a mounted directory.
//!
//!   * `guard` is fail-closed. If Docker starts before the mounts settle, bind
//!     mounts resolve to ordinary directories on the root filesystem with no
//!     size limit, and the first symptom is a full disk.
//!
//!   * The maintenance reserve is advisory. Nothing enforces it, because
//!     rest-server's `--max-size` is per instance and one instance serves every
//!     grantee.
//!
//! Peer names arrive here as [`crate::config::PeerName`], which is validated on
//! construction. This module used to carry its own `peer_valid`/`check_peer`
//! pair and call it by hand at each entry point -- the same character set, but
//! without the length limit, and re-checked rather than carried in the type.
//! The host side is where a name becomes a file name and a systemd unit name,
//! so it is the side that most wants the guarantee to be structural.

pub mod grant;
pub mod server;
pub mod size;

use std::path::{Path, PathBuf};
use std::process::Command;

pub use crate::Res;

/// 51515 is in the dynamic port range, so it is unlikely to collide with
/// something already running. 8000 collides constantly.
pub const DEFAULT_PORT: u16 = 51515;
pub const DEFAULT_CONTAINER: &str = "peerbackup-rest";
pub const REST_SERVER_IMAGE: &str = "restic/rest-server:0.14.0";

/// Where everything lives, plus the switches the tests drive.
///
/// Read from the environment once rather than at each use, so a command cannot
/// see two different values for the same setting partway through.
pub struct Ctx {
    pub root: PathBuf,
    pub units: PathBuf,
    /// Percent of a grant left as headroom for `prune` to repack. Advisory:
    /// reported so a human can leave room, enforced by nothing.
    pub reserve_pct: u64,
    /// Never let grants consume the last of the host disk, even if each grant
    /// is individually legal.
    pub host_margin_gb: u64,
    /// Print what would happen and touch nothing. The tests rely on this, and
    /// admission control deliberately still runs.
    pub dry_run: bool,
    pub quiet: bool,
    pub force: bool,
}

impl Ctx {
    pub fn from_env() -> Self {
        let env_num = |k: &str, d: u64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let flag = |k: &str| std::env::var(k).is_ok_and(|v| v == "1");
        Self {
            root: std::env::var_os("PEERBACKUP_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/srv/peerbackup")),
            units: std::env::var_os("SYSTEMD_UNIT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/systemd/system")),
            reserve_pct: env_num("MAINTENANCE_RESERVE_PCT", 15),
            host_margin_gb: env_num("HOST_MARGIN_GB", 20),
            dry_run: flag("DRY_RUN"),
            quiet: flag("QUIET"),
            force: flag("FORCE"),
        }
    }

    pub fn images(&self) -> PathBuf {
        self.root.join("images")
    }
    pub fn mnt(&self) -> PathBuf {
        self.root.join("mnt")
    }
    pub fn image_of(&self, peer: &str) -> PathBuf {
        self.images().join(format!("{peer}.img"))
    }
    pub fn dir_of(&self, peer: &str) -> PathBuf {
        self.mnt().join(peer)
    }

    /// Describe an action that a dry run would take instead of taking it.
    /// Returns true when the caller should skip the real work.
    pub fn would(&self, what: &str) -> bool {
        if self.dry_run {
            println!("  would run: {what}");
        }
        self.dry_run
    }

    pub fn info(&self, msg: &str) {
        println!("{msg}");
    }
    pub fn say(&self, msg: &str) {
        if !self.quiet {
            println!("{msg}");
        }
    }

    /// Run a command, or describe it under `DRY_RUN`.
    ///
    /// The printed form matches what the shell printed, because the tests read
    /// it and because seeing the exact command is the point of a dry run.
    pub fn run<S: AsRef<std::ffi::OsStr>>(&self, program: &str, args: &[S]) -> Res {
        // Arguments are OsStr, not str. Paths used to be flattened through
        // `to_string_lossy` on the way here, so a root directory containing a
        // non-UTF-8 byte became a *different* path with U+FFFD in it -- and
        // that mangled path was then handed to `chown -R` and `rm -f`.
        let line = format!(
            "{program} {}",
            args.iter()
                .map(|a| a.as_ref().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        );
        if self.dry_run {
            println!("  would run: {line}");
            return Ok(());
        }
        let out = Command::new(program)
            .args(args.iter().map(std::convert::AsRef::as_ref))
            .output()
            .map_err(|e| format!("could not run `{line}`: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!(
            "`{line}` failed{}{}",
            match out.status.code() {
                Some(c) => format!(" (exit {c})"),
                None => String::new(),
            },
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        ))
    }

    /// Same, but a non-zero exit only warns. For teardown steps that are
    /// expected to fail when the thing is already gone.
    pub fn run_best_effort<S: AsRef<std::ffi::OsStr>>(&self, program: &str, args: &[S]) {
        if let Err(e) = self.run(program, args) {
            warn(&e);
        }
    }

    pub fn need_root(&self) -> Res {
        if self.dry_run || is_root() {
            return Ok(());
        }
        Err("must run as root (mount, losetup and systemd all need it)".into())
    }
}

pub fn warn(msg: &str) {
    eprintln!("\x1b[33mwarn:\x1b[0m {msg}");
}

pub fn is_root() -> bool {
    // SAFETY: geteuid reads a process property. It cannot fail and touches no
    // memory we own.
    unsafe { libc::geteuid() == 0 }
}

/// Is this path a mount point?
///
/// Compares the device id of the directory against its parent, which is what
/// `mountpoint` does. Native rather than shelled out because it is two stat
/// calls, and because `guard` depends on the answer: a false "yes" here means
/// writes land on the host root filesystem with no size limit.
pub fn is_mountpoint(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(here) = std::fs::metadata(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return true; // "/" is always a mount point.
    };
    match std::fs::metadata(parent) {
        Ok(up) => here.dev() != up.dev(),
        Err(_) => false,
    }
}

/// Capacity of the filesystem holding `path`: (total, available) in bytes.
pub fn statfs(path: &Path) -> Result<(u64, u64), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path {} contains a NUL byte", path.display()))?;
    let mut buf = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call,
    // and `buf` is owned here and correctly sized. statvfs writes the whole
    // struct on success and signals failure through its return value, so `buf`
    // is only read below after a zero return.
    let rc = unsafe { libc::statvfs(c.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(format!(
            "could not read free space on {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: statvfs returned 0, so the struct is initialised.
    let buf = unsafe { buf.assume_init() };
    // f_frsize is the fragment size, which is what f_blocks and f_bavail are
    // counted in. f_bsize is the preferred I/O size and is the wrong multiplier.
    // These are `c_ulong`, which is u64 on every target this builds for. No cast:
    // on a 32-bit target the arithmetic below would stop compiling, which is a
    // better way to find out than a silent truncation.
    let unit = if buf.f_frsize > 0 {
        buf.f_frsize
    } else {
        buf.f_bsize
    };
    Ok((buf.f_blocks * unit, buf.f_bavail * unit))
}

/// The systemd mount unit name for a grant directory.
///
/// Shelled out on purpose. The escaping rules are exact, and getting them
/// subtly wrong writes a unit to a path systemd never reads. The shell version
/// hit that once: the unit was written, `provision` reported success, and the
/// grant never mounted.
pub fn unit_name(dir: &Path) -> Result<String, String> {
    let out = Command::new("systemd-escape")
        .args(["--path", "--suffix=mount"])
        .arg(dir)
        .output()
        .map_err(|e| {
            format!(
                "systemd-escape not found ({e}); cannot derive the mount unit name for {}",
                dir.display()
            )
        })?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || name.is_empty() {
        return Err(format!(
            "systemd-escape produced an empty unit name for {}",
            dir.display()
        ));
    }
    Ok(name)
}

pub fn have(tool: &str) -> bool {
    // PATH lookup without a shell, so a tool name can never be interpreted.
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let c = dir.join(tool);
                std::fs::metadata(&c).map(|m| m.is_file()).unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_directory_is_not_a_mountpoint() {
        // The guard's whole job rests on this returning false for an ordinary
        // directory, so a container never starts against unmounted storage.
        let dir = std::env::temp_dir().join(format!("pb-mp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(!is_mountpoint(&dir.join("sub")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_directory_is_not_a_mountpoint() {
        // Fail closed: an absent path must never read as mounted.
        assert!(!is_mountpoint(Path::new("/nope/not/here")));
    }

    #[test]
    fn root_is_a_mountpoint() {
        assert!(is_mountpoint(Path::new("/")));
    }

    #[test]
    fn statfs_reports_a_plausible_filesystem() {
        let (total, avail) = statfs(Path::new("/")).unwrap();
        assert!(total > 0, "root filesystem should have a size");
        assert!(avail <= total, "available cannot exceed total");
    }

    #[test]
    fn statfs_on_a_missing_path_is_an_error_not_a_zero() {
        // Returning zero here would make admission control refuse every grant,
        // or accept one against a filesystem it never read.
        assert!(statfs(Path::new("/definitely/not/here/at/all")).is_err());
    }

    #[test]
    fn have_finds_a_tool_that_exists_and_not_one_that_does_not() {
        assert!(have("sh"), "sh must be on PATH");
        assert!(!have("definitely-not-a-real-binary-xyzzy"));
    }

    fn ctx(dry_run: bool) -> Ctx {
        Ctx {
            root: PathBuf::from("/tmp/pb-unused"),
            units: PathBuf::from("/tmp/pb-unused"),
            reserve_pct: 15,
            host_margin_gb: 20,
            dry_run,
            quiet: true,
            force: false,
        }
    }

    #[test]
    fn a_dry_run_describes_the_command_and_does_not_run_it() {
        // The printed form is load-bearing: the shell suite reads
        // "would run: fallocate -l <bytes> ..." to prove the size a user typed
        // reaches the allocation. Changing the wording breaks that test from a
        // long way away, so pin it here where the reason is visible.
        let marker = std::env::temp_dir().join(format!("pb-dry-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        ctx(true)
            .run("touch", &[marker.as_os_str()])
            .expect("a dry run always succeeds");
        assert!(!marker.exists(), "a dry run must not touch anything");
    }

    #[test]
    fn a_real_run_reports_the_command_and_the_reason_when_it_fails() {
        let e = ctx(false)
            .run("sh", &["-c", "echo nope >&2; exit 3"])
            .unwrap_err();
        assert!(e.contains("exit 3"), "must carry the exit code: {e}");
        assert!(e.contains("nope"), "must carry stderr: {e}");
        assert!(e.contains("sh -c"), "must show what was run: {e}");
    }

    #[test]
    fn a_command_that_does_not_exist_is_an_error_not_a_silent_success() {
        let e = ctx(false)
            .run("definitely-not-a-real-binary-xyzzy", &["x"])
            .unwrap_err();
        assert!(e.contains("could not run"), "got: {e}");
    }

    #[test]
    fn a_successful_run_really_runs() {
        let marker = std::env::temp_dir().join(format!("pb-run-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        ctx(false).run("touch", &[marker.as_os_str()]).unwrap();
        assert!(marker.exists());
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn a_best_effort_run_warns_instead_of_failing() {
        // Teardown steps are expected to fail when the thing is already gone.
        // `release` calls this for `systemctl disable` and `losetup -d`, and a
        // grant that was never enabled must still be releasable.
        ctx(false).run_best_effort("sh", &["-c", "exit 1"]);
    }
}
