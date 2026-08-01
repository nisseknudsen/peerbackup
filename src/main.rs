mod cli;
mod config;
mod engine;
mod host;
mod state;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "peerbackup",
    about = "Back up your server to your friends' servers",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Set up everything and connect to a peer, in one step
    Connect {
        /// Repository URL your peer gave you
        url: String,
        /// A directory to back up. Repeat for more than one.
        #[arg(long = "source", required = true)]
        sources: Vec<PathBuf>,
        /// Short name for this peer (default: taken from the URL)
        #[arg(long)]
        name: Option<String>,
    },

    /// Create the config file and test files
    Init,

    /// Add, list and remove the friends you back up to
    #[command(subcommand)]
    Peer(PeerCmd),

    /// Send a backup to every peer
    Backup {
        /// Only back up to this peer
        #[arg(long)]
        peer: Option<String>,
    },

    /// Check that what is stored can still be read back
    Verify {
        /// Only check this peer
        #[arg(long)]
        peer: Option<String>,
    },

    /// Show whether your backups are in good shape
    Status,

    /// List the backups stored on a peer
    Snapshots {
        /// Peer name
        peer: String,
    },

    /// Get your data back from a peer
    Restore {
        /// Peer name
        peer: String,
        /// Where to put the restored files
        target: PathBuf,
        /// Which backup to restore (default: the most recent)
        #[arg(long)]
        snapshot: Option<String>,
    },

    /// Manage the file that lets you recover without this program
    #[command(subcommand)]
    Recovery(RecoveryCmd),

    /// Host backups for a friend: grants, the server, and its logins
    #[command(subcommand)]
    Host(HostCmd),
}

#[derive(Subcommand)]
enum PeerCmd {
    /// Add a peer and check that backups to it work
    Add {
        /// Short name, e.g. alice
        name: String,
        /// Repository URL, e.g. rest:https://me:pw@alice.example.org:8000/me/
        url: String,
        /// Certificate file, if they use a self-signed one
        #[arg(long)]
        cacert: Option<PathBuf>,
    },
    /// List your peers
    List,
    /// Stop backing up to a peer
    Remove {
        /// Peer name
        name: String,
    },
}

#[derive(Subcommand)]
enum RecoveryCmd {
    /// Write out repository details and passwords
    Export {
        /// Where to write it (default: alongside your other peerbackup files)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Check the exported file still matches your peers
    Check,
}

/// Hosting for a friend: grants, the server, and the logins it serves.
///
/// This replaced `deploy/peerbackup-host`, which was 549 lines of bash. The OS
/// still does the work -- fallocate, mkfs.ext4, systemctl, docker -- but the
/// person hosting now installs the same one binary as the person backing up.
#[derive(Subcommand)]
enum HostCmd {
    /// Start a server and print an invite, in one command and without root
    Quickstart {
        /// Short name for the friend you are hosting for
        peer: String,
    },
    /// Create a size-limited grant the kernel enforces (needs root)
    Provision {
        peer: String,
        /// e.g. 500G, 1T, 512M
        size: String,
    },
    /// Create a login, restart the server, and verify it works
    Adduser {
        peer: String,
        /// Leave empty to generate one
        password: Option<String>,
    },
    /// Destroy a grant and give the capacity back (needs root)
    Release { peer: String },
    /// Grants, sizes and usage
    List {
        /// Only this peer
        peer: Option<String>,
    },
    /// Refuse to start unless every grant is really mounted
    Guard,
    /// Check this machine is set up to host
    Doctor,
    /// docker compose up -d
    Up,
    /// docker compose down
    Down,
}

fn run_host(cmd: HostCmd) -> Result<(), String> {
    let ctx = host::Ctx::from_env();
    let opts = host::server::ServerOpts::from_env()?;
    match cmd {
        HostCmd::Quickstart { peer } => host::server::quickstart(&ctx, &peer, &opts),
        HostCmd::Provision { peer, size } => host::grant::provision(&ctx, &peer, &size),
        HostCmd::Adduser { peer, password } => {
            host::server::adduser(&ctx, &peer, password.as_deref(), &opts).map(|_| ())
        }
        HostCmd::Release { peer } => host::grant::release(&ctx, &peer),
        HostCmd::List { peer } => host::grant::list(&ctx, peer.as_deref()),
        HostCmd::Guard => host::grant::guard(&ctx),
        HostCmd::Doctor => host::grant::doctor(&ctx),
        HostCmd::Up => host::server::compose(&ctx, true),
        HostCmd::Down => host::server::compose(&ctx, false),
    }
}

fn main() {
    let cli = Cli::parse();

    let nag_after = matches!(
        cli.command,
        Command::Status | Command::Backup { .. } | Command::Peer(_) | Command::Connect { .. }
    );

    let result = match cli.command {
        Command::Connect { url, sources, name } => cli::connect(&url, &sources, name.as_deref()),
        Command::Init => cli::init(),
        Command::Peer(PeerCmd::Add { name, url, cacert }) => cli::peer_add(&name, &url, cacert),
        Command::Peer(PeerCmd::List) => cli::peer_list(),
        Command::Peer(PeerCmd::Remove { name }) => cli::peer_remove(&name),
        Command::Backup { peer } => cli::backup(peer.as_deref()),
        Command::Verify { peer } => cli::verify(peer.as_deref()),
        Command::Status => cli::status_cmd(),
        Command::Snapshots { peer } => cli::snapshots(&peer),
        Command::Restore {
            peer,
            target,
            snapshot,
        } => cli::restore(&peer, &target, snapshot.as_deref()),
        Command::Recovery(RecoveryCmd::Export { out }) => cli::recovery_export(out),
        Command::Recovery(RecoveryCmd::Check) => cli::recovery_check(),
        Command::Host(h) => run_host(h),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    if nag_after {
        cli::warn_if_recovery_stale();
    }
}
