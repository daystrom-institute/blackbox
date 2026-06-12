//! Integration: a V8 cell composes `code.query` → `lsp.rename` →
//! `edits.merge` → `edits.apply` against a real rust-analyzer — the full
//! lsp_verified-authority path of the refactor cell DSL, cross-file.
//!
//! Skips (like bro-lsp's rust_analyzer test) when rust-analyzer is absent.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use bro_capabilities::ToolCapability;
use bro_harness::code_mode::{CodeMode, code_mode_tools};
use bro_tools::{ToolCx, ToolResult};
use serde_json::json;

fn rust_analyzer_available() -> bool {
    for var in [
        "BRO_LSP_RUST_ANALYZER_BIN",
        "BRO_RUST_ANALYZER_BIN",
        "BLACKBOX_RUST_ANALYZER_BIN",
    ] {
        if let Some(path) = std::env::var_os(var)
            && command_runs(&PathBuf::from(path))
        {
            return true;
        }
    }
    if command_runs(&PathBuf::from("rust-analyzer")) {
        return true;
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| command_runs(&home.join(".cargo/bin/rust-analyzer")))
        .unwrap_or(false)
}

fn command_runs(path: &PathBuf) -> bool {
    std::process::Command::new(path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cell_renames_cross_file_via_lsp_authority() -> anyhow::Result<()> {
    if !rust_analyzer_available() {
        eprintln!("skipping lsp rename cell test: rust-analyzer not found");
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    std::fs::create_dir(root.join("src"))?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"lsp_rename_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::write(
        root.join("src/lib.rs"),
        "pub mod util;\n\npub fn run() -> u32 {\n    util::compute(3)\n}\n",
    )?;
    std::fs::write(
        root.join("src/util.rs"),
        "pub fn compute(x: u32) -> u32 {\n    x * 2\n}\n",
    )?;

    let cx = ToolCx {
        root: root.clone(),
        safety: Arc::new(bro_tools::SafetyPolicy::new()),
        http: reqwest::Client::new(),
        todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
        shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
        edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
        session_env: Arc::new(BTreeMap::new()),
        tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
        shell_env: Arc::new(Default::default()),
    };
    let callable = bro_harness::bindings::binding_tools();
    let seam: Arc<dyn ToolCapability> = Arc::new(bro_harness::capabilities::HostTools::new(
        callable.clone(),
        cx.clone(),
    ));
    let exec = code_mode_tools(
        &callable,
        seam,
        CodeMode::Only,
        &bro_harness::bindings::namespace_descriptions(),
    )
    .remove(0);

    let source = r#"// @exec: {"yield_time_ms": 120000}
const hits = await code.query({ file: "src/util.rs", query: "(function_item name: (identifier) @n)" });
const at = hits.captures.find(c => c.text === "compute");
const r = await lsp.rename({ span: at.span, newName: "double" });
const es = await edits.begin();
await edits.merge({ es, changes: r.changes });
const applied = await edits.apply({ es });
text(`${r.files.length}:${applied.applied}`);
"#;
    let result = exec.call(json!({ "source": source }), &cx).await;
    match result {
        ToolResult::Text(t) => assert!(t.contains("2:true"), "got: {t}"),
        other => panic!("expected text, got {other:?}"),
    }

    let util = std::fs::read_to_string(root.join("src/util.rs"))?;
    let lib = std::fs::read_to_string(root.join("src/lib.rs"))?;
    assert!(util.contains("pub fn double"), "{util}");
    assert!(!util.contains("compute"), "{util}");
    assert!(lib.contains("util::double(3)"), "{lib}");
    Ok(())
}
