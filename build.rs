//! D27 build-identity emit. Captures a Unix-seconds timestamp at the
//! time the daemon binary is linked and exposes it as
//! `env!("BLACKBOX_BUILD_ID")`. The fleet cockpit compares this against
//! its own `env!("BRO_CLI_BUILD_ID")` so a long-lived cockpit binary
//! running against a freshly rebuilt daemon surfaces a "restart
//! cockpit" banner — additive display, no behavior change on
//! version match (see unit-N4/thread-c3f7c7e3 D27, AGENTS.md).
//!
//! Determinism note: this script intentionally does NOT set
//! `cargo:rerun-if-changed` directives. With no rerun guard, cargo
//! re-runs the script on every rebuild of the daemon, so the
//! emitted `BLACKBOX_BUILD_ID` reflects the wall-clock time of
//! that particular link step. The build is a developer-machine
//! operation, not a production cache; deterministic across
//! rebuilds is the *opposite* of what we want here.
//!
//! No external deps: `std::time::SystemTime` is enough. No
//! filesystem probing of the installed binary, no hashing of
//! linked artifacts.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let build_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=BLACKBOX_BUILD_ID={build_id}");
}
