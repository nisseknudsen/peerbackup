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
    pub name: String,
    /// restic repository URL, e.g. `rest:https://me:pw@alice.example.org:8000/me/`
    pub url: String,
    /// PEM bundle, if your friend uses a self-signed certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
}

impl Config {
    pub fn dir() -> PathBuf {
        std::env::var_os("PEERBACKUP_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config/peerbackup"))
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.toml")
    }

    pub fn secret_path(peer: &str) -> PathBuf {
        Self::dir().join("secrets").join(peer)
    }

    pub fn load() -> io::Result<Self> {
        let p = Self::path();
        let text = fs::read_to_string(&p).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{}: {e}  (run `peerbackup init` first)", p.display()),
            )
        })?;
        toml::from_str(&text).map_err(|e| io::Error::other(format!("{}: {e}", p.display())))
    }

    pub fn save(&self) -> io::Result<()> {
        let dir = Self::dir();
        fs::create_dir_all(&dir)?;
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        write_private(&Self::path(), text.as_bytes())
    }

    pub fn peer(&self, name: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.name == name)
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

    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));

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

/// Random alphanumeric string from the kernel. Avoids pulling in a crate for
/// something `/dev/urandom` already does, and this is Linux-only anyway.
pub fn random_token(len: usize) -> io::Result<String> {
    use std::io::Read;
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut buf = vec![0u8; len];
    fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn round_trips_through_toml() {
        let mut c = Config::default();
        c.settings.sources = vec![PathBuf::from("/srv/data")];
        c.peers.push(Peer {
            name: "alice".into(),
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
