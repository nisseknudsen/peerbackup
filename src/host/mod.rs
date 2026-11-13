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

/// A directory path from the environment, refused if it could not be one.
///
/// `provision` writes a systemd mount unit as root, built by interpolating these
/// paths into an INI file. A newline in one of them appends arbitrary directives
/// to that unit -- `ExecStartPre=` among them. Reaching that needs the ability to
/// set the environment of a root command, which is most of the way to root
/// already, so this is a guard rail rather than a boundary; it costs four lines
/// and removes the question.
///
/// Absolute, because everything downstream joins onto it and a relative root
/// would resolve against whatever directory systemd happened to start in.
fn dir_from_env(key: &str, default: &str) -> Result<PathBuf, String> {
    let Some(v) = std::env::var_os(key) else {
        return Ok(PathBuf::from(default));
    };
    let p = PathBuf::from(v);
    let s = p.as_os_str().as_encoded_bytes();
    if s.is_empty() {
        return Err(format!("{key} is set but empty"));
    }
    if s.iter().any(|b| *b == b'\n' || *b == b'\r' || *b == 0) {
        return Err(format!(
            "{key} contains a newline or a null byte. It is interpolated into a \
             systemd unit written as root."
        ));
    }
    if !p.is_absolute() {
        return Err(format!("{key}={} must be an absolute path", p.display()));
    }
    Ok(p)
}

/// Read a boolean environment variable, refusing anything ambiguous.
///
/// This matched the literal string `"1"` and treated everything else as false,
/// so `DRY_RUN=true peerbackup host provision alice 500G` allocated 500GB,
/// formatted it and mounted it. For a switch whose entire job is to prevent
/// that, silently reading an unrecognised value as "no" is the wrong default;
/// saying so is cheap.
fn env_flag(key: &str) -> Result<bool, String> {
    let Some(v) = std::env::var_os(key) else {
        return Ok(false);
    };
    match v.to_string_lossy().trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "" | "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!(
            "{key}={other} is not a yes or a no. Use 1/true/yes/on or 0/false/no/off."
        )),
    }
}

/// The two switches that change whether a `host` command is safe, passed on the
/// command line.
///
/// They are flags rather than only environment variables because every command
/// that needs them also needs root, and `sudo` resets the environment by
/// default. `DRY_RUN=1 sudo peerbackup host provision alice 500G` -- the ordering
/// almost everyone types -- dropped the variable and performed a real 500GB
/// provision. Nothing in the code or the docs said the safe form was
/// `sudo DRY_RUN=1 peerbackup ...`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    pub dry_run: bool,
    pub force: bool,
}

impl Ctx {
    pub fn from_env(flags: Flags) -> Result<Self, String> {
        // Bounded, not just parsed. `reserve_pct` is multiplied by a filesystem
        // size and `host_margin_gb` by 1024^3, and release builds have
        // overflow-checks on -- so an absurd value panicked rather than being
        // refused. A percentage over 100 is also not a percentage.
        let env_num = |k: &str, d: u64, max: u64| -> Result<u64, String> {
            match std::env::var(k) {
                Err(_) => Ok(d),
                Ok(v) => match v.trim().parse::<u64>() {
                    Ok(n) if n <= max => Ok(n),
                    Ok(n) => Err(format!("{k}={n} is out of range (max {max})")),
                    Err(_) => Err(format!("{k}='{v}' is not a whole number")),
                },
            }
        };
        Ok(Self {
            root: dir_from_env("PEERBACKUP_ROOT", "/srv/peerbackup")?,
            units: dir_from_env("SYSTEMD_UNIT_DIR", "/etc/systemd/system")?,
            reserve_pct: env_num("MAINTENANCE_RESERVE_PCT", 15, 100)?,
            // A yottabyte of headroom is past any real disk and leaves the
            // multiplication by 1024^3 nowhere near u64.
            host_margin_gb: env_num("HOST_MARGIN_GB", 20, 1 << 30)?,
            // `DRY_RUN` keeps working: it fails in the safe direction, so an
            // ambient one costs someone a command that did not happen.
            dry_run: flags.dry_run || env_flag("DRY_RUN")?,
            quiet: env_flag("QUIET")?,
            // The bare `FORCE` is deliberately *not* read any more. It was the
            // only way to skip the confirmation on the command that permanently
            // destroys a friend's backups, it is undocumented, and it is a
            // common shell habit exported by plenty of build and deploy scripts
            // -- so `sudo -E peerbackup host release alice` from the wrong shell
            // destroyed 500GB with no prompt. A switch that fails in the unsafe
            // direction has to be asked for by name.
            force: flags.force || env_flag("PB_FORCE")?,
        })
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
                // A tool's stderr is not ours to trust: `docker exec` relays
                // text from inside the container. Folded before it can reach
                // the terminal through the error path.
                format!(": {}", crate::redact::message(stderr.trim()))
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

/// Print a warning, with anything that could move the cursor folded out.
///
/// Subprocess output reaches this: the stderr of `docker exec ... htpasswd`,
/// which runs inside the internet-facing container as the container's own uid,
/// so a compromised server can choose its bytes. Printed raw, `ESC[1A ESC[2K`
/// erased the real `generated password` line and painted a forged `verified`
/// line in its place while the command exited 1. The client side folds every
/// piece of peer-influenced text before printing it (`redact`); this is the
/// host side's one shared sink, so the fold lives here. Newlines survive, as
/// they do for restic's multi-line errors.
pub fn warn(msg: &str) {
    eprintln!("\x1b[33mwarn:\x1b[0m {}", crate::redact::message(msg));
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
    let differs = match std::fs::metadata(parent) {
        Ok(up) => here.dev() != up.dev(),
        Err(_) => false,
    };
    if !differs {
        return false;
    }
    // btrfs gives every subvolume its own st_dev, so the comparison above says
    // "mounted" for an ordinary subvolume that is nothing of the kind. That
    // matters here more than anywhere: `guard` runs as ExecStartPre precisely to
    // refuse a boot where a grant is not really mounted, and btrfs is one of the
    // three filesystems this tool's own error messages recommend.
    //
    // /proc/self/mountinfo is the authority. It is missing in some containers,
    // so its absence falls back to the st_dev answer rather than failing open
    // or closed on a technicality.
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(info) => info.lines().any(|l| {
            // Field 5 is the mount point, space-escaped as \040 etc.
            l.split_whitespace()
                .nth(4)
                .is_some_and(|m| unescape_mountinfo(m) == path.as_os_str())
        }),
        Err(_) => true,
    }
}

/// mountinfo escapes space, tab, newline and backslash as octal.
fn unescape_mountinfo(s: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            let oct = std::str::from_utf8(&b[i + 1..i + 4])
                .ok()
                .and_then(|d| u8::from_str_radix(d, 8).ok());
            if let Some(byte) = oct {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    std::ffi::OsString::from_vec(out)
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
        // systemd-escape's own complaint, which it writes to stderr and which
        // this discarded -- leaving "produced an empty unit name" as the entire
        // explanation for a failure it had already described.
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        return Err(format!(
            "systemd-escape produced no unit name for {}{}",
            dir.display(),
            if why.is_empty() {
                String::new()
            } else {
                format!(": {why}")
            }
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
    fn a_failing_commands_stderr_cannot_carry_control_sequences_into_the_error() {
        // `docker exec` relays stderr from inside the container, and the error
        // string ends up on the operator's terminal. A cursor-up plus line-erase
        // in it rewrote the lines above.
        let e = ctx(false)
            .run("sh", &["-c", "printf 'nope\\033[2K\\rforged' >&2; exit 3"])
            .unwrap_err();
        assert!(
            !e.chars().any(|c| c.is_control() && c != '\n'),
            "control characters survived: {e:?}"
        );
        assert!(e.contains("nope") && e.contains("forged"), "{e}");
        assert!(e.contains("exit 3"), "{e}");
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

    #[test]
    fn a_boolean_env_var_that_is_not_a_yes_or_a_no_is_refused() {
        // This matched the literal string "1", so `DRY_RUN=true peerbackup host
        // provision alice 500G` really allocated, formatted and mounted 500GB.
        // For a switch whose entire job is to prevent that, reading an
        // unrecognised value as "no" is the wrong default.
        //
        // Uses a variable name no other test touches: `std::env::set_var` is
        // unsafe in edition 2024 because it races every concurrent reader in
        // this binary.
        let key = "PB_TEST_FLAG_PARSE";
        for (v, want) in [
            ("1", Some(true)),
            ("true", Some(true)),
            ("TRUE", Some(true)),
            ("yes", Some(true)),
            ("on", Some(true)),
            ("0", Some(false)),
            ("false", Some(false)),
            ("no", Some(false)),
            ("", Some(false)),
            ("maybe", None),
            ("2", None),
        ] {
            // SAFETY: this key is used by no other test and by no other thread.
            unsafe { std::env::set_var(key, v) };
            match want {
                Some(b) => assert_eq!(env_flag(key).unwrap(), b, "{v:?}"),
                None => assert!(env_flag(key).is_err(), "{v:?} must be refused"),
            }
        }
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
        assert!(!env_flag(key).unwrap(), "unset is a no");
    }

    #[test]
    fn the_command_line_can_ask_for_a_dry_run_and_for_force() {
        // Every command that needs these also needs root, and sudo resets the
        // environment, so `DRY_RUN=1 sudo peerbackup host provision alice 500G`
        // -- the ordering people type -- did a real provision.
        let c = Ctx::from_env(Flags {
            dry_run: true,
            force: true,
        })
        .unwrap();
        assert!(c.dry_run);
        assert!(c.force);
    }

    #[test]
    fn a_bare_force_in_the_environment_no_longer_skips_the_confirmation() {
        // `FORCE=1` is undocumented, unnamespaced and exported by plenty of
        // build scripts, and it was the only way to skip the prompt on the
        // command that permanently destroys a friend's backups.
        //
        // SAFETY: `FORCE` is read by nothing else in this binary.
        unsafe { std::env::set_var("FORCE", "1") };
        let c = Ctx::from_env(Flags::default()).unwrap();
        // SAFETY: as above.
        unsafe { std::env::remove_var("FORCE") };
        assert!(!c.force, "the bare FORCE must not be honoured");
    }

    #[test]
    fn a_path_from_the_environment_cannot_carry_a_newline_into_a_systemd_unit() {
        // `provision` writes a mount unit as root by interpolating these paths
        // into an INI file, so a newline appends arbitrary directives --
        // `ExecStartPre=` among them. Reaching it needs the ability to set the
        // environment of a root command, so this is a guard rail rather than a
        // boundary, but it costs four lines.
        let key = "PB_TEST_DIR_FROM_ENV";
        let cases = [
            ("/srv/peerbackup", true),
            ("/srv/x\nExecStartPre=/bin/sh -c evil", false),
            ("relative/path", false),
            ("", false),
        ];
        for (v, ok) in cases {
            // SAFETY: this key is used by no other test and by no other thread.
            unsafe { std::env::set_var(key, v) };
            assert_eq!(
                dir_from_env(key, "/default").is_ok(),
                ok,
                "{v:?} should {} be accepted",
                if ok { "" } else { "not" }
            );
        }
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
        assert_eq!(
            dir_from_env(key, "/default").unwrap(),
            std::path::PathBuf::from("/default")
        );
    }

    #[test]
    fn an_out_of_range_tuning_value_is_refused_rather_than_panicking() {
        // `reserve_pct` is multiplied by a filesystem size and `host_margin_gb`
        // by 1024^3, and release builds have overflow-checks on, so an absurd
        // value panicked. A percentage over 100 is also not a percentage.
        //
        // SAFETY: these keys are read by nothing else in this binary.
        unsafe { std::env::set_var("MAINTENANCE_RESERVE_PCT", "500") };
        assert!(Ctx::from_env(Flags::default()).is_err());
        unsafe { std::env::set_var("MAINTENANCE_RESERVE_PCT", "15") };
        assert!(Ctx::from_env(Flags::default()).is_ok());
        unsafe { std::env::set_var("HOST_MARGIN_GB", "99999999999999999999") };
        assert!(Ctx::from_env(Flags::default()).is_err());
        unsafe {
            std::env::remove_var("MAINTENANCE_RESERVE_PCT");
            std::env::remove_var("HOST_MARGIN_GB");
        }
    }

    #[test]
    fn mountinfo_escapes_are_decoded() {
        assert_eq!(
            unescape_mountinfo("/srv/a"),
            std::ffi::OsString::from("/srv/a")
        );
        assert_eq!(
            unescape_mountinfo(r"/srv/a\040b"),
            std::ffi::OsString::from("/srv/a b")
        );
    }
}
