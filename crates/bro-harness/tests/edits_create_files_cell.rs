//! Integration: `edits.createFiles` as a cell namespace global, and the
//! published Java/Rust transform recipes that feed it, run in a V8 cell over
//! synthetic transform results. No language server or real HOME state.

// Test-only target: writes fixture trees in tempdirs.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bro_capabilities::ToolCapability;
use bro_harness::code_mode::{CodeMode, code_mode_tools};
use bro_harness::mcp::ToolFilter;
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{Value, json};

const EXISTING: &str = "alpha beta\n";
const OLD: &str = "old body\n";

fn cx_in(root: &Path) -> ToolCx {
    ToolCx {
        tool_observations: Default::default(),
        instruction_generation: 0,
        instruction_policy: None,
        root: root.to_path_buf(),
        safety: Arc::new(bro_tools::SafetyPolicy::new()),
        http: reqwest::Client::new(),
        todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
        shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
        edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
        cancellation: Default::default(),
        output_budget: 16 * 1024,
        child_env: Arc::new(Default::default()),
        session_env: Arc::new(BTreeMap::new()),
        tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
        shell_env: Arc::new(Default::default()),
    }
}

fn exec_over(callable: Vec<Arc<dyn Tool>>, cx: &ToolCx) -> Vec<Arc<dyn Tool>> {
    let seam: Arc<dyn ToolCapability> = Arc::new(bro_harness::capabilities::HostTools::new(
        callable.clone(),
        cx.clone(),
    ));
    code_mode_tools(
        &callable,
        seam,
        CodeMode::Only,
        &bro_harness::bindings::namespace_descriptions(),
    )
}

async fn run_cell(exec: &Arc<dyn Tool>, cx: &ToolCx, source: &str) -> String {
    match exec.call(json!({ "source": source }), cx).await {
        ToolResult::Text(text) => text,
        other => panic!("cell failed: {other:?}\n--- source ---\n{source}"),
    }
}

#[tokio::test]
async fn create_files_is_a_filtered_cell_only_namespace_global() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let cx = cx_in(&root);
    let callable = bro_harness::bindings::binding_tools();
    assert!(callable.iter().any(|t| t.name() == "edits.createFiles"
        && t.namespace_binding() == Some(("edits".to_string(), "createFiles".to_string()))));
    let flat = exec_over(callable.clone(), &cx);
    assert!(
        flat.iter().all(|t| t.name() != "edits.createFiles"),
        "binding leaked onto the flat tool surface"
    );
    let text = run_cell(
        &flat[0],
        &cx,
        r#"
const entry = ALL_TOOLS.find(t => t.canonical_name === "edits.createFiles");
text(`${typeof edits.createFiles}:${typeof tools["edits.createFiles"]}:${entry.declaration.includes("createFiles(args:")}:${entry.input_schema.properties.files.maxItems}`);
"#,
    )
    .await;
    assert!(text.contains("function:undefined:true:1024"), "{text}");

    // The canonical-name filter removes it without touching its siblings.
    let filter = ToolFilter::from_csv(Some("edits.createFiles"), None);
    let filtered: Vec<Arc<dyn Tool>> = bro_harness::bindings::binding_tools()
        .into_iter()
        .filter(|t| filter.permits(t.name()))
        .collect();
    let exec = exec_over(filtered, &cx).remove(0);
    let text = run_cell(
        &exec,
        &cx,
        "text(`${typeof edits.createFiles}:${typeof edits.createFile}`);",
    )
    .await;
    assert!(text.contains("undefined:function"), "{text}");
}

#[tokio::test]
async fn cell_batch_refusal_queues_nothing_and_the_set_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let cx = cx_in(&root);
    let exec = exec_over(bro_harness::bindings::binding_tools(), &cx).remove(0);
    let text = run_cell(
        &exec,
        &cx,
        r#"
const es = await edits.begin();
const first = await edits.createFiles({ es, files: [
  { path: "gen/One.txt", content: "one\n" },
  { path: "gen/Two.txt", content: "two\n" },
] });
let refusal = "";
try {
  await edits.createFiles({ es, files: [
    { path: "gen/Three.txt", content: "three\n" },
    { path: "gen/One.txt", content: "again\n" },
  ] });
} catch (e) {
  refusal = String(e.message ?? e);
}
const empty = await edits.createFiles({ es, files: [] });
const applied = await edits.apply({ es });
text(JSON.stringify({ first: first.creates, empty: empty.creates, refusal, applied: applied.applied, lineage: applied.lineage.syntax_only }));
"#,
    )
    .await;
    assert!(text.contains("\"first\":2"), "{text}");
    assert!(text.contains("\"empty\":2"), "{text}");
    assert!(text.contains("nothing queued"), "{text}");
    assert!(text.contains("gen/One.txt (already created)"), "{text}");
    assert!(text.contains("\"applied\":true"), "{text}");
    assert!(text.contains("\"lineage\":2"), "{text}");
    assert!(root.join("gen/One.txt").exists() && root.join("gen/Two.txt").exists());
    assert!(!root.join("gen/Three.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("gen/One.txt")).unwrap(),
        "one\n"
    );
}

/// The queue-and-apply block of a published recipe: from `edits.begin()`
/// through the line that applies.
fn recipe_block(contract: &str) -> String {
    let lines: Vec<&str> = contract.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.contains("const es = await edits.begin();"))
        .unwrap_or_else(|| panic!("recipe has no begin line:\n{contract}"));
    let end = start
        + lines[start..]
            .iter()
            .position(|l| l.contains("edits.apply("))
            .unwrap_or_else(|| panic!("recipe has no apply line:\n{contract}"));
    let block = lines[start..=end]
        .iter()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        block.contains("edits.createFiles({ es, files: r.creates })")
            && !block.contains("edits.createFile("),
        "recipe does not batch its creates:\n{block}"
    );
    block
}

async fn contract(callable: &[Arc<dyn Tool>], cx: &ToolCx, tool: &str, transform: &str) -> String {
    let describe = callable.iter().find(|t| t.name() == tool).unwrap();
    match describe.call(json!({ "transform": transform }), cx).await {
        ToolResult::Json(value) => value["contract"].as_str().unwrap().to_string(),
        other => panic!("{tool}({transform}) failed: {other:?}"),
    }
}

fn sha256(bytes: &[u8]) -> String {
    bbox_refactor::sha256_hex(bytes)
}

fn creates() -> Value {
    json!([
        { "path": "gen/New1.txt", "content": "new one\n" },
        { "path": "gen/New2.txt", "content": "new two\n" }
    ])
}

fn changes() -> Value {
    json!([{
        "span": {
            "file": "src/Existing.txt",
            "byte_start": 0,
            "byte_end": 5,
            "content_sha256": sha256(EXISTING.as_bytes())
        },
        "new_text": "gamma"
    }])
}

fn deletes() -> Value {
    json!([{ "path": "src/Old.txt", "content_sha256": sha256(OLD.as_bytes()) }])
}

struct Outcome {
    created: bool,
    changed: bool,
    deleted: bool,
}

#[tokio::test]
async fn published_recipes_batch_creates_and_guard_empty_operations() {
    let recipes = [
        ("java.describe", "extractClass", false),
        ("java.describe", "moveClass", true),
        ("java.describe", "movePackage", true),
        ("java.describe", "extractInterface", false),
        ("rust.describe", "extractItems", false),
        ("rust.describe", "extractTrait", false),
        ("rust.describe", "liftToFree", false),
    ];
    for (describe, transform, moves) in recipes {
        let scenarios = [
            ("creates-only", creates(), json!([]), json!([])),
            ("changes-only", json!([]), changes(), json!([])),
            (
                "mixed",
                creates(),
                changes(),
                if moves { deletes() } else { json!([]) },
            ),
            ("no-operation", json!([]), json!([]), json!([])),
        ];
        for (scenario, creates, changes, deletes) in scenarios {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            std::fs::create_dir(root.join("src")).unwrap();
            std::fs::write(root.join("src/Existing.txt"), EXISTING).unwrap();
            std::fs::write(root.join("src/Old.txt"), OLD).unwrap();
            let cx = cx_in(&root);
            let callable = bro_harness::bindings::binding_tools();
            let block = recipe_block(&contract(&callable, &cx, describe, transform).await);
            let expected = Outcome {
                created: creates.as_array().is_some_and(|a| !a.is_empty()),
                changed: changes.as_array().is_some_and(|a| !a.is_empty()),
                deleted: deletes.as_array().is_some_and(|a| !a.is_empty()),
            };
            let r = json!({ "creates": creates, "changes": changes, "deletes": deletes, "findings": [] });
            let source = format!("{{\nconst r = {r};\n{block}\n}}\ntext(\"recipe-done\");\n");
            let exec = exec_over(callable, &cx).remove(0);
            let text = run_cell(&exec, &cx, &source).await;
            let label = format!("{transform} {scenario}");
            assert!(text.contains("recipe-done"), "{label}: {text}");

            assert_eq!(
                root.join("gen/New1.txt").exists() && root.join("gen/New2.txt").exists(),
                expected.created,
                "{label}"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("src/Existing.txt")).unwrap() == "gamma beta\n",
                expected.changed,
                "{label}"
            );
            assert_eq!(
                !root.join("src/Old.txt").exists(),
                expected.deleted,
                "{label}"
            );
            if !expected.changed {
                assert_eq!(
                    std::fs::read_to_string(root.join("src/Existing.txt")).unwrap(),
                    EXISTING,
                    "{label}"
                );
            }
        }
    }
}
