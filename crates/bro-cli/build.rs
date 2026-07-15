//! D27 build-identity emit for the `bro` cockpit. Mirrors
//! `build.rs` in the root `blackbox` crate: derives a per-commit
//! stamp from git (`git rev-parse --short=12 HEAD`) and exposes it
//! as `env!("BRO_CLI_BUILD_ID")`. The cockpit compares its own
//! value against the daemon's `BLACKBOX_BUILD_ID` (carried on
//! `RosterSnapshotV1.daemon_build_id`) and shows a "restart
//! cockpit" banner when they diverge (D27, unit-N4).
//!
//! Same-commit identities: see the root `build.rs` for the
//! determinism note — git-derived stamp means two binaries built
//! from the same commit produce identical ids, so separate cargo
//! invocations no longer falsely report a mismatch.
//!
//! Fallback: when `git rev-parse` is unavailable, falls back to a
//! Unix-seconds timestamp so the stamp is never empty.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// Cargo build scripts are synchronous processes, so deriving the build stamp
// cannot block an application Tokio worker.
#[allow(clippy::disallowed_methods)]
fn git_head_short(short_len: u32) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", &format!("--short={short_len}"), "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        let sha = String::from_utf8(output.stdout).ok()?;
        let trimmed = sha.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    None
}

fn fallback_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() {
    // Rerun when HEAD moves so rebuilds after commits get a fresh stamp.
    // Paths are relative to THIS crate dir, so reach the workspace .git two
    // levels up; and .git/HEAD only changes on branch switches — commits
    // append to the HEAD reflog, so watch that too (stale build ids were
    // version-banner false positives, D30).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");

    let build_id = git_head_short(12).unwrap_or_else(|| fallback_timestamp().to_string());
    println!("cargo:rustc-env=BRO_CLI_BUILD_ID={build_id}");
}
