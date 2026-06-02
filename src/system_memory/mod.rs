//! System memories — runtime-loaded markdown runbooks.
//!
//! Memory content is loaded from `system-defaults/memories/` and optional user
//! overlay directories at startup. Agents reach memory runbooks through explicit
//! lookup (`exact_query`) or query (`search`) paths.

mod catalog;
mod loader;

use std::path::Path;
use std::sync::OnceLock;

pub use crate::system_memory::catalog::{MemoryCatalog, SystemMemory};

use anyhow::Result;
use serde_json::Value;

static SYSTEM_MEMORY_CATALOG: OnceLock<MemoryCatalog> = OnceLock::new();

/// Initialize the process-wide catalog from on-disk memory artifacts.
pub fn init(defaults_dir: &Path, user_dir: Option<&Path>, ctx: &Value) -> Result<()> {
    let catalog = MemoryCatalog::load(defaults_dir, user_dir, ctx)?;
    SYSTEM_MEMORY_CATALOG
        .set(catalog)
        .map_err(|_| anyhow::anyhow!("system memory catalog already initialized"))
}

pub fn catalog() -> &'static MemoryCatalog {
    SYSTEM_MEMORY_CATALOG
        .get()
        .expect("system memory catalog not initialized")
}

#[cfg(test)]
pub fn init_for_tests() {
    let defaults = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("system-defaults")
        .join("memories");
    let _ = SYSTEM_MEMORY_CATALOG.get_or_init(|| {
        MemoryCatalog::load(&defaults, None, &serde_json::json!({}))
            .expect("load system memory defaults for tests")
    });
}

/// Lookup by exact ID. Accepts either canonical form (`sm-rule-packets`) or
/// bare slug (`rule-packets`) for ergonomics.
pub fn get(id: &str) -> Option<&'static SystemMemory> {
    catalog().get(id)
}

fn normalize_exact_query(raw: &str) -> &str {
    let trimmed = raw.trim();
    let unquoted = if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    unquoted.trim()
}

/// Lookup by exact canonical query. This is intentionally narrower than `get`:
/// bare slugs such as `refactor` remain searchable terms, while canonical
/// `sm-refactor` fetches exactly that memory.
pub fn exact_query(query: Option<&str>) -> Option<&'static SystemMemory> {
    let candidate = normalize_exact_query(query?);
    if !candidate
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sm-"))
    {
        return None;
    }
    catalog().exact_query(Some(candidate))
}

/// Smart search over all memories using the same query grammar used by bbox queries.
pub fn search(query: Option<&str>) -> Vec<&'static SystemMemory> {
    catalog().search(query)
}

/// Render one memory for an agent response: `[system] sm-…` header + title +
/// tag line + full body.
pub fn format_for_listing(memory: &SystemMemory) -> String {
    catalog::format_for_listing(memory)
}

pub fn format_catalog_summary(query: Option<&str>) -> String {
    catalog().format_catalog_summary(query)
}

/// Render one memory as a compact signpost for the broad query surface: header
/// + tags + one-line preview + a breadcrumb to pull the full body by exact id.
pub fn format_for_signpost(memory: &SystemMemory) -> String {
    catalog::format_for_signpost(memory)
}
