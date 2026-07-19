#![allow(clippy::disallowed_methods)]

use std::process::Command;

#[test]
fn harness_binary_starts_without_daemon_linkage() {
    let output = Command::new(env!("CARGO_BIN_EXE_bro-harness"))
        .arg("--help")
        .output()
        .expect("start standalone bro-harness binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--input-format"));
    assert!(stdout.contains("--exit-when-idle"));
}

#[test]
fn isolate_binary_executes_v8_cell_as_its_own_process() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_isolate"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--cell",
            r#"text("ISOLATE_PROCESS_OK");"#,
        ])
        .output()
        .expect("start standalone isolate binary");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ISOLATE_PROCESS_OK"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
