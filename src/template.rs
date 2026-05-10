//! Tera-based template rendering. Single entry point for all template
//! evaluation in the stack — roadmap render, workflow Render nodes,
//! lens/prompt assembly, Badgey Slack posts, etc.

use anyhow::Result;
use std::path::Path;
use tera::{Context, Tera};

/// Render a Tera template string against a JSON context value.
///
/// The context value must be a JSON object; its keys become top-level
/// template variables. Returns the rendered string.
pub fn render(source: &str, ctx: &serde_json::Value) -> Result<String> {
    let mut tera = Tera::default();
    tera.add_raw_template("__tpl__", source)
        .map_err(|e| anyhow::anyhow!("template parse: {e}"))?;
    let context = Context::from_serialize(ctx)
        .map_err(|e| anyhow::anyhow!("template context: {e}"))?;
    tera.render("__tpl__", &context)
        .map_err(|e| anyhow::anyhow!("template render: {e}"))
}

/// Render a Tera template loaded from a file against a JSON context value.
pub fn render_file(path: &Path, ctx: &serde_json::Value) -> Result<String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("template file '{}': {e}", path.display()))?;
    render(&source, ctx)
}
