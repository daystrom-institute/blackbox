//! fleetd's default state dir must be the daemon's BRO home for the same
//! environment. fleetd cannot link `bbox-util` (dependency ceiling), so it
//! re-implements `bbox_util::util::bro_home_dir`; this test fails the moment the
//! two rules diverge.

use std::path::Path;

use bbox_util::util::{bro_home_dir, test_env_lock};

const KEYS: [&str; 4] = ["HOME", "BRO_HOME", "BLACKBOX_STATE_DIR", "XDG_STATE_HOME"];

fn assert_parity(home: &Path, case: &str, set: &[(&str, String)]) {
    for key in KEYS {
        unsafe { std::env::remove_var(key) };
    }
    unsafe { std::env::set_var("HOME", home) };
    for (key, value) in set {
        unsafe { std::env::set_var(key, value) };
    }
    let fleetd = fleetd::paths::default_state_dir().expect("fleetd default resolves");
    let daemon = bro_home_dir(home);
    assert_eq!(fleetd, daemon, "fleetd and the daemon diverge for {case}");
}

#[test]
fn fleetd_default_matches_the_daemon_bro_home() {
    let _guard = test_env_lock();
    let saved: Vec<_> = KEYS
        .iter()
        .map(|key| (*key, std::env::var_os(key)))
        .collect();

    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().canonicalize().unwrap();
    let elsewhere = home.join("elsewhere");
    let path = |p: &Path| p.to_string_lossy().into_owned();

    assert_parity(&home, "only HOME", &[]);
    assert_parity(&home, "BRO_HOME", &[("BRO_HOME", path(&elsewhere))]);
    assert_parity(
        &home,
        "~-prefixed BRO_HOME",
        &[("BRO_HOME", "~/bro".into())],
    );
    assert_parity(
        &home,
        "BRO_HOME over BLACKBOX_STATE_DIR",
        &[
            ("BRO_HOME", path(&elsewhere)),
            ("BLACKBOX_STATE_DIR", path(&home.join("state"))),
        ],
    );
    assert_parity(
        &home,
        "BLACKBOX_STATE_DIR",
        &[("BLACKBOX_STATE_DIR", path(&home.join("state")))],
    );
    assert_parity(
        &home,
        "~-prefixed BLACKBOX_STATE_DIR",
        &[("BLACKBOX_STATE_DIR", "~/.local/state/blackbox-dev".into())],
    );
    assert_parity(
        &home,
        "BLACKBOX_STATE_DIR of ~",
        &[("BLACKBOX_STATE_DIR", "~".into())],
    );
    // Honored on Linux, ignored on macOS: whichever host runs this checks its
    // own platform's rule.
    assert_parity(
        &home,
        "absolute XDG_STATE_HOME",
        &[("XDG_STATE_HOME", path(&home.join("xdg")))],
    );
    assert_parity(
        &home,
        "relative XDG_STATE_HOME",
        &[("XDG_STATE_HOME", "xdg".into())],
    );
    assert_parity(
        &home,
        "BLACKBOX_STATE_DIR over XDG_STATE_HOME",
        &[
            ("BLACKBOX_STATE_DIR", path(&home.join("state"))),
            ("XDG_STATE_HOME", path(&home.join("xdg"))),
        ],
    );

    for (key, value) in saved {
        match value {
            Some(value) => unsafe { std::env::set_var(key, value) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
