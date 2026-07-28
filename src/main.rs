//! peerbackup — friend-to-friend homeserver backup with proof of restorability.
//!
//! Only the restic wrapper exists so far. This is a placeholder so `cargo run`
//! says something true rather than pretending to be finished.

// The seam is built ahead of its callers on purpose: it is the spine the client
// hangs off, and the eng review put it first so the three-state model is
// enforced by the compiler before anything depends on it. Dead-code warnings
// here are expected until the client lands.
#[allow(dead_code)]
mod engine;

fn main() {
    println!("peerbackup {}", env!("CARGO_PKG_VERSION"));
    println!("engine seam: restic subprocess, three-state verification");
    println!();
    println!("Not yet implemented: peer config, canary corpus, evidence store,");
    println!("recovery bundle, status dashboard. See TODOS.md and the design doc.");
}
