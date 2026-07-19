//! Build-identity stamp for the handshake's build-compatibility gate.
//! Mirrors `crates/bro-cli/build.rs`: a git-derived per-commit stamp so two
//! binaries built from the same commit produce identical ids, with a
//! Unix-seconds fallback so the stamp is never empty (`bro_rpc::BuildIdentity`
//! rejects an empty `build_id`).

// The crate's `disallowed_methods` deny exists to keep blocking process calls
// off tokio worker threads. A build script runs at compile time, on cargo's
// own thread, with no runtime anywhere in sight, so the rule does not apply
// here (same shape as `crates/bro-cli/build.rs`, which predates the lint).
#![allow(clippy::disallowed_methods)]

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git_head_short(short_len: u32) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", &format!("--short={short_len}"), "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let trimmed = sha.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn fallback_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    let build_id = git_head_short(12).unwrap_or_else(|| fallback_timestamp().to_string());
    println!("cargo:rustc-env=FLEETD_BUILD_ID={build_id}");
}
