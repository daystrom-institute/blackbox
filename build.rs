//! D27 build-identity emit. Derives a per-commit stamp from git
//! (`git rev-parse --short=12 HEAD`) and exposes it as
//! `env!("BLACKBOX_BUILD_ID")`. The fleet cockpit compares this against
//! its own `env!("BRO_CLI_BUILD_ID")` so a long-lived cockpit binary
//! running against a freshly rebuilt daemon surfaces a "restart
//! cockpit" banner — additive display, no behavior change on
//! version match (see unit-N4/thread-c3f7c7e3 D27, AGENTS.md).
//!
//! Same-commit identities: because the stamp is derived from the
//! current HEAD, two binaries built from the same commit produce
//! identical build ids — separate cargo invocations no longer
//! falsely report a mismatch.
//!
//! Fallback: when `git rev-parse` is unavailable (not in a repo,
//! no git binary), we fall back to a Unix-seconds timestamp so
//! the stamp is never empty.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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
    // rerun when HEAD moves so rebuilds after commits get a fresh stamp.
    // .git/HEAD only changes on branch switches; commits move the branch ref
    // and append to the HEAD reflog — watch the reflog too, or build ids go
    // stale across commits (version-banner false positives, D30).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/logs/HEAD");

    let build_id = git_head_short(12)
        .unwrap_or_else(|| fallback_timestamp().to_string());
    println!("cargo:rustc-env=BLACKBOX_BUILD_ID={build_id}");
}
