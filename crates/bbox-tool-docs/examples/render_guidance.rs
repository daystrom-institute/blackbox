//! Render project guidance from checked-in entries without a daemon or live store.
//! Usage: cargo run -p bbox-tool-docs --example render_guidance -- <source-root> <output-root>
use anyhow::{Context, Result};
use bbox_knowledge::knowledge::{Knowledge, KnowledgeEntry, RenderParams, Scope};
use std::{collections::BTreeMap, fs, path::PathBuf};

#[allow(
    clippy::disallowed_methods,
    reason = "Standalone synchronous offline renderer; no async runtime or daemon state"
)]
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    anyhow::ensure!(
        args.len() == 2,
        "usage: render_guidance <source-root> <output-root>"
    );
    let source = PathBuf::from(&args[0]).canonicalize()?;
    let output = PathBuf::from(&args[1]);
    fs::create_dir_all(&output)?;
    let output = output.canonicalize()?;
    let mut entries = Vec::new();
    for item in fs::read_dir(source.join(".bbox/knowledge"))? {
        let path = item?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let mut entry: KnowledgeEntry = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("reading {}", path.display()))?;
        anyhow::ensure!(
            entry.scope == Scope::Project,
            "non-project entry in project source"
        );
        entry.project = Some(output.display().to_string());
        entries.push(entry);
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    if source != output && source.join("PROJECT.md").is_file() {
        let target = output.join("PROJECT.md");
        anyhow::ensure!(
            !target.exists() || fs::read(&target)? == fs::read(source.join("PROJECT.md"))?,
            "output PROJECT.md differs"
        );
        fs::copy(source.join("PROJECT.md"), target)?;
    }
    let view = Knowledge::detached_view(entries, BTreeMap::new());
    println!(
        "{}",
        view.render(&RenderParams {
            project: Some(output.display().to_string()),
            scope: Some("project".into()),
            ..Default::default()
        })?
    );
    let check = view.check_project_render(&output)?;
    anyhow::ensure!(
        check.mismatches.is_empty(),
        "rendered files failed drift check: {:?}",
        check.mismatches
    );
    println!("Verified {} generated files", check.checked);
    Ok(())
}
