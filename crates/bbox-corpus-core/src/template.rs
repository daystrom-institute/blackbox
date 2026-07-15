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
    let context =
        Context::from_serialize(ctx).map_err(|e| anyhow::anyhow!("template context: {e}"))?;
    tera.render("__tpl__", &context)
        .map_err(|e| anyhow::anyhow!("template render: {e}"))
}

/// Render a Tera template loaded from a file against a JSON context value.
/// File-backed rendering is a synchronous workflow/indexing boundary; async
/// callers must place it on their blocking lane.
#[allow(clippy::disallowed_methods)]
pub fn render_file(path: &Path, ctx: &serde_json::Value) -> Result<String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("template file '{}': {e}", path.display()))?;
    render(&source, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_simple_substitution() {
        let out = render("hello {{ name }}", &json!({"name": "world"})).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn render_loop_and_condition() {
        let tpl = "{% for x in items %}{% if x > 1 %}{{ x }} {% endif %}{% endfor %}";
        let out = render(tpl, &json!({"items": [1, 2, 3]})).unwrap();
        assert_eq!(out.trim(), "2 3");
    }

    #[test]
    fn render_in_operator() {
        let tpl =
            "{% set active = [\"a\", \"b\"] %}{% if val in active %}yes{% else %}no{% endif %}";
        let yes = render(tpl, &json!({"val": "a"})).unwrap();
        let no = render(tpl, &json!({"val": "c"})).unwrap();
        assert_eq!(yes, "yes");
        assert_eq!(no, "no");
    }

    #[test]
    fn render_bad_template_returns_err() {
        let err = render("{{ unclosed", &json!({}));
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("template parse"));
    }

    #[test]
    fn render_file_missing_path_returns_err() {
        let err = render_file(Path::new("/nonexistent/path.tera"), &json!({}));
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("template file"));
    }
}
