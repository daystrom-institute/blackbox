//! D27 build-identity emit for the `bro` cockpit. Mirrors
//! `build.rs` in the root `blackbox` crate: captures a Unix-seconds
//! timestamp at link time and exposes it as
//! `env!("BRO_CLI_BUILD_ID")`. The cockpit compares its own
//! value against the daemon's `BLACKBOX_BUILD_ID` (carried on
//! `RosterSnapshotV1.daemon_build_id`) and shows a "restart
//! cockpit" banner when they diverge (D27, unit-N4).
//!
//! See the root `build.rs` for the determinism note.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let build_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=BRO_CLI_BUILD_ID={build_id}");
}
