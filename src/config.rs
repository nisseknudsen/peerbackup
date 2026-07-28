//! Config and per-peer secrets.
//!
//! Config lives at `~/.config/peerbackup/config.toml`, secrets in
//! `~/.config/peerbackup/secrets/<peer>` at mode 0600. They are kept apart so
//! the config can be read, diffed or pasted into a bug report without leaking
//! the thing that decrypts your backups.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
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

/// Write a file only the owner can read. Used for anything secret-adjacent.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
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
    fn random_tokens_differ_and_are_the_right_length() {
        let a = random_token(24).unwrap();
        let b = random_token(24).unwrap();
        assert_eq!(a.len(), 24);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
