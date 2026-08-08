//! Config and per-peer secrets.
//!
//! Config lives at `~/.config/peerbackup/config.toml`, secrets in
//! `~/.config/peerbackup/secrets/<peer>` at mode 0600. They are kept apart so
//! the config can be read, diffed or pasted into a bug report without leaking
//! the thing that decrypts your backups.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "peer")]
    pub peers: Vec<Peer>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Settings {
    /// Directories to back up. The canary is added automatically.
    pub sources: Vec<PathBuf>,
    /// Upload ceiling in KiB/s. 0 means unlimited.
    ///
    /// Worth setting. A first backup of 300GB saturates a 40Mbit uplink for
    /// about seventeen hours, and the fastest way to get this uninstalled is to
    /// ruin someone's video call.
    pub upload_limit_kib: u32,
    /// How much of each repository to read back during a check.
    pub verify_subset_pct: u8,
    /// A peer is stale if it has not been checked within these windows.
    pub liveness_hours: u64,
    pub subset_days: u64,
    pub canary_days: u64,
}

impl Settings {
    /// Refuse settings that cannot mean what they say.
    ///
    /// Two reasons, both of which showed up as silent behaviour further down.
    ///
    /// `verify_subset_pct = 0` was accepted here and clamped to 1 three layers
    /// away, in the restic engine. Someone who writes 0 means "do not read
    /// anything back", and getting 1% instead is a small lie about the one
    /// number on the dashboard that says how much was checked. Refusing says
    /// which value is wrong and where.
    ///
    /// The windows are multiplied into seconds (`liveness_hours * 3600`, and
    /// days * 86400) to decide whether a peer is stale. Release builds do not
    /// check arithmetic overflow, so an absurd value silently wrapped to a tiny
    /// window and every peer read `unchecked` forever. The ceilings below are
    /// far past any real schedule and leave the multiplications nowhere near
    /// `u64`.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=100).contains(&self.verify_subset_pct) {
            return Err(format!(
                "verify_subset_pct must be between 1 and 100, not {}. \
                 It is the share of stored data `verify` reads back.",
                self.verify_subset_pct
            ));
        }
        // A century, in each unit.
        for (name, value, max) in [
            ("liveness_hours", self.liveness_hours, 24 * 365 * 100),
            ("subset_days", self.subset_days, 365 * 100),
            ("canary_days", self.canary_days, 365 * 100),
        ] {
            if value == 0 {
                return Err(format!(
                    "{name} is 0, so no check could ever be recent enough and every peer \
                     would read `unchecked` forever"
                ));
            }
            if value > max {
                return Err(format!("{name} of {value} is out of range (max {max})"));
            }
        }
        Ok(())
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            upload_limit_kib: 0,
            verify_subset_pct: 1,
            liveness_hours: 48,
            subset_days: 10,
            canary_days: 35,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub name: PeerName,
    /// restic repository URL, e.g. `rest:https://me:pw@alice.example.org:8000/me/`
    pub url: String,
    /// PEM bundle, if your friend uses a self-signed certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
}

/// A peer name that is safe to use as a path component and a systemd unit name.
///
/// Constructing one is the only way to get a name into [`Config::secret_path`]
/// or a grant directory, so a traversal cannot reach either. The host side
/// validated names from the start; the client side did not, which is how
/// `peer add ../../../tmp/x` came to write a password file outside the config
/// directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerName(String);

impl PeerName {
    pub fn new(name: &str) -> Result<Self, String> {
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if ok {
            Ok(Self(name.to_owned()))
        } else {
            Err(format!(
                "peer name '{name}' must be 1-64 characters of [a-zA-Z0-9_-] only \
                 (it becomes a file name and a systemd unit name)"
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PeerName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for PeerName {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PeerName {
    /// Validates on the way in, so a hand-edited `config.toml` cannot
    /// reintroduce a name that escapes the secrets directory.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

impl PartialEq<str> for PeerName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for PeerName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl std::str::FromStr for PeerName {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Why the config could not be read. Kept apart from `io::Error` so a TOML
/// syntax error is not laundered into one, and so the "run init first" hint
/// belongs to the missing-file case rather than to every failure.
#[derive(Debug)]
pub enum ConfigError {
    NotFound(PathBuf),
    Io(PathBuf, io::Error),
    Parse(PathBuf, String),
    /// Valid TOML, but a setting that cannot mean what it says.
    Invalid(PathBuf, String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(p) => {
                write!(
                    f,
                    "no config at {}; run `peerbackup init` first",
                    p.display()
                )
            }
            Self::Io(p, e) => write!(f, "could not read {}: {e}", p.display()),
            Self::Parse(p, e) => write!(f, "{} is not valid TOML: {e}", p.display()),
            Self::Invalid(p, e) => write!(f, "{}: {e}", p.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn dir() -> PathBuf {
        std::env::var_os("PEERBACKUP_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config/peerbackup"))
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    /// Where a peer's repository password lives.
    ///
    /// Takes a validated name rather than a `&str` so the type system carries
    /// the guarantee. Before [`PeerName`] existed this took the raw argument,
    /// and `peerbackup peer add ../../../tmp/x` wrote a password file outside
    /// the config directory: `write_private` creates parent directories, so
    /// the traversal target was created on the way.
    pub fn secret_path(peer: &PeerName) -> PathBuf {
        Self::dir().join("secrets").join(peer.as_str())
    }

    pub fn load() -> Result<Self, ConfigError> {
        let p = Self::path();
        let text = fs::read_to_string(&p).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => ConfigError::NotFound(p.clone()),
            _ => ConfigError::Io(p.clone(), e),
        })?;
        let cfg: Self =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(p.clone(), e.to_string()))?;
        cfg.settings
            .validate()
            .map_err(|e| ConfigError::Invalid(p, e))?;
        Ok(cfg)
    }

    pub fn save(&self) -> io::Result<()> {
        let dir = Self::dir();
        fs::create_dir_all(&dir)?;
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        write_private(&Self::path(), text.as_bytes())
    }

    pub fn peer(&self, name: &PeerName) -> Option<&Peer> {
        self.peers.iter().find(|p| &p.name == name)
    }

    /// Everything a backup covers: what you asked for, plus the canary.
    pub fn backup_sources(&self, canary_dir: &Path) -> Vec<PathBuf> {
        let mut v = self.settings.sources.clone();
        v.push(canary_dir.to_path_buf());
        v
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

/// Write a file only the owner can read, atomically.
///
/// Two properties, both load-bearing, because this writes repository passwords
/// and the config that says which peer holds what:
///
/// **0600 from the moment the file exists.** Creating it and chmodding after
/// leaves a window at the process umask, usually 0644, with the password
/// already in it. A password read out of that window decrypts every backup on
/// every peer, forever, and nothing would record that it happened.
///
/// **Temp file, fsync, rename.** `fs::write` truncates first, so a crash
/// mid-write leaves a half-file. rename(2) is atomic within a filesystem, so a
/// reader sees either the old file or the new one. The temp file goes in the
/// same directory for that reason: rename across filesystems is not atomic and
/// fails with EXDEV.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;

    // Append to the file name rather than replacing its extension.
    // `with_extension` would turn both `al.ice` and `al.bob` into `al.tmp.PID`,
    // so two peers written concurrently would clobber each other's temp file.
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{} has no file name to write", path.display())))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);

    let write = || -> io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        // Renames are journalled separately from file contents. Without this
        // the rename itself can be lost in a crash, leaving the old file.
        fs::File::open(parent)?.sync_all()
    };

    write().inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Random alphanumeric string from the kernel.
///
/// Avoids pulling in a crate for something `/dev/urandom` already does, and
/// this is Linux-only anyway.
///
/// Rejection sampling rather than `% ALPHABET.len()`. The alphabet is 57
/// characters and 256 is not a multiple of it, so the modulo would make the
/// first 28 characters 25% more likely than the rest. That is a small bias and
/// it would not be worth fixing anywhere else, but this generates the password
/// that decrypts a backup repository, and four lines is cheap.
pub fn random_token(len: usize) -> io::Result<String> {
    use std::io::Read;
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    // The largest multiple of the alphabet that fits in a byte. Anything at or
    // above it is redrawn rather than folded.
    const LIMIT: u8 = (256 / ALPHABET.len() * ALPHABET.len()) as u8;

    let mut urandom = fs::File::open("/dev/urandom")?;
    let mut out = String::with_capacity(len);
    let mut buf = vec![0u8; len];
    while out.len() < len {
        urandom.read_exact(&mut buf)?;
        for b in &buf {
            if *b < LIMIT {
                out.push(ALPHABET[*b as usize % ALPHABET.len()] as char);
                if out.len() == len {
                    break;
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_peer_name_cannot_escape_the_secrets_directory() {
        // `peer add ../../../tmp/x` used to write a password file there:
        // secret_path joined the raw argument, and write_private creates parent
        // directories, so the traversal target was created on the way.
        for bad in [
            "../../../tmp/pwned",
            "..",
            "a/b",
            "/etc/shadow",
            "",
            "a b",
            "a.b",
            "a;rm -rf /",
            "péer",
        ] {
            assert!(PeerName::new(bad).is_err(), "{bad:?} must be refused");
        }
        let name = PeerName::new("alice").unwrap();
        let p = Config::secret_path(&name);
        assert_eq!(p.file_name().unwrap(), "alice");
        assert!(p.parent().unwrap().ends_with("secrets"));
    }

    #[test]
    fn a_hand_edited_config_cannot_reintroduce_a_bad_name() {
        // The type is only a guarantee if it also guards the deserialize path.
        let toml = r#"
[settings]
sources = []
upload_limit_kib = 0
verify_subset_pct = 1
liveness_hours = 48
subset_days = 10
canary_days = 35

[[peer]]
name = "../../../tmp/pwned"
url = "rest:http://x/"
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn two_peers_whose_names_differ_after_a_dot_get_different_temp_files() {
        // with_extension would turn both `al.ice` and `al.bob` into
        // `al.tmp.PID`. Peer names cannot contain dots any more, but
        // write_private is also used for config.toml and is worth being right.
        let dir = std::env::temp_dir().join(format!("pb-tmpname-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        write_private(&dir.join("al.ice"), b"one").unwrap();
        write_private(&dir.join("al.bob"), b"two").unwrap();
        assert_eq!(fs::read_to_string(dir.join("al.ice")).unwrap(), "one");
        assert_eq!(fs::read_to_string(dir.join("al.bob")).unwrap(), "two");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_that_cannot_mean_what_they_say_are_refused_on_load() {
        // Each of these used to be accepted and then quietly turned into
        // something else much further down.
        assert!(
            Settings::default().validate().is_ok(),
            "the defaults must be valid"
        );

        let pct = |v: u8| Settings {
            verify_subset_pct: v,
            ..Settings::default()
        };
        let e = pct(0).validate().unwrap_err();
        assert!(e.contains("verify_subset_pct"), "must name the key: {e}");
        assert!(pct(101).validate().is_err());
        assert!(pct(100).validate().is_ok());

        // Zero windows: nothing could ever be fresh, so every peer would read
        // `unchecked` forever.
        for zeroed in [
            Settings {
                liveness_hours: 0,
                ..Settings::default()
            },
            Settings {
                subset_days: 0,
                ..Settings::default()
            },
            Settings {
                canary_days: 0,
                ..Settings::default()
            },
        ] {
            assert!(zeroed.validate().is_err());
        }

        // Absurd windows: `liveness_hours * 3600` wraps in a release build,
        // which turns "never stale" into "always stale".
        assert!(
            Settings {
                liveness_hours: u64::MAX,
                ..Settings::default()
            }
            .validate()
            .is_err(),
            "an overflowing window must be refused"
        );
    }

    #[test]
    fn a_valid_window_never_overflows_when_turned_into_seconds() {
        // What the ceilings are actually for. `status` computes
        // `canary_days.max(subset_days) * 86400 * 2`, the widest of them.
        let s = Settings {
            liveness_hours: 24 * 365 * 100,
            subset_days: 365 * 100,
            canary_days: 365 * 100,
            ..Settings::default()
        };
        s.validate().unwrap();
        assert!(s.liveness_hours.checked_mul(3600).is_some());
        assert!(
            s.canary_days
                .max(s.subset_days)
                .checked_mul(86400)
                .and_then(|v| v.checked_mul(2))
                .is_some()
        );
    }

    #[test]
    fn round_trips_through_toml() {
        let mut c = Config::default();
        c.settings.sources = vec![PathBuf::from("/srv/data")];
        c.peers.push(Peer {
            name: PeerName::new("alice").unwrap(),
            url: "rest:https://me:pw@alice.example.org:8000/me/".into(),
            ca_cert: None,
        });
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.peers.len(), 1);
        assert_eq!(back.peers[0].name, "alice");
        assert_eq!(back.settings.sources, vec![PathBuf::from("/srv/data")]);
    }

    #[test]
    fn canary_is_always_backed_up() {
        // If the canary is not in a snapshot, verification has nothing to
        // restore and every peer reads unknown forever.
        let c = Config::default();
        let sources = c.backup_sources(Path::new("/var/lib/peerbackup/canary"));
        assert!(sources.iter().any(|p| p.ends_with("canary")));
    }

    #[test]
    fn a_secret_is_never_readable_by_anyone_else_even_briefly() {
        // The bug this replaced wrote the file at the process umask and chmodded
        // afterwards. The password was on disk, world-readable, in between. This
        // asserts the end state; the guarantee that no window exists comes from
        // passing the mode to open() rather than calling set_permissions after.
        let dir = std::env::temp_dir().join(format!("pb-perm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("secret");
        write_private(&p, b"hunter2").unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret must be 0600, was {mode:o}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwriting_replaces_content_and_keeps_the_mode() {
        let dir = std::env::temp_dir().join(format!("pb-over-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("secret");
        write_private(&p, b"first").unwrap();
        write_private(&p, b"second").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "second");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // No temp file may survive a successful write.
        let strays: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files behind: {strays:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_leftover_temp_file_does_not_break_reading_the_config() {
        // A crash mid-write can leave one. It must be inert, not fatal: config
        // is the record of which peer holds what.
        let dir = std::env::temp_dir().join(format!("pb-stray-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("config.toml");
        let good = toml::to_string_pretty(&Config::default()).unwrap();
        write_private(&p, good.as_bytes()).unwrap();
        fs::write(dir.join("config.tmp.999"), b"garbage not toml").unwrap();

        let text = fs::read_to_string(&p).unwrap();
        assert!(
            toml::from_str::<Config>(&text).is_ok(),
            "a stray temp file must not affect reading config.toml"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn random_tokens_differ_and_are_the_right_length() {
        let a = random_token(24).unwrap();
        let b = random_token(24).unwrap();
        assert_eq!(a.len(), 24);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
