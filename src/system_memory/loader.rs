//! Load system memories from markdown files with TOML front matter.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const TOML_DELIMITER: &str = "+++";
const NON_MEMORY_MARKDOWN_FILES: &[&str] = &["system-memory-catalog.md"];

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryFrontMatter {
    pub title: String,
    pub tags: Vec<String>,
    #[serde(default = "default_order")]
    pub order: usize,
    #[serde(default)]
    pub template: bool,
}

const fn default_order() -> usize {
    999
}

#[derive(Debug, Clone)]
pub struct RawMemory {
    pub slug: String,
    pub front_matter: MemoryFrontMatter,
    pub body: String,
}

fn strip_leading_newline(raw: &str) -> &str {
    let mut rest = raw;
    loop {
        if let Some(stripped) = rest.strip_prefix("\r\n") {
            rest = stripped;
        } else if let Some(stripped) = rest.strip_prefix('\n') {
            rest = stripped;
        } else {
            return rest;
        }
    }
}

pub fn parse_memory_file(slug: &str, raw_content: &str) -> Result<RawMemory> {
    let mut parts = raw_content.splitn(3, TOML_DELIMITER);
    let lead = parts
        .next()
        .ok_or_else(|| anyhow!("memory {slug}: missing front matter"))?;
    if !lead.is_empty() {
        return Err(anyhow!(
            "memory {slug}: missing opening TOML delimiter `+++` at file start"
        ));
    }

    let front_matter = parts
        .next()
        .ok_or_else(|| anyhow!("memory {slug}: missing opening TOML delimiter `+++`"))?;
    let raw_body = parts
        .next()
        .ok_or_else(|| anyhow!("memory {slug}: missing closing TOML delimiter `+++`"))?;

    let front_matter: MemoryFrontMatter = toml::from_str(front_matter)
        .with_context(|| format!("memory {slug}: invalid TOML front matter"))?;

    if front_matter.title.trim().is_empty() {
        return Err(anyhow!("memory {slug}: front matter title is empty"));
    }
    if front_matter.tags.is_empty() {
        return Err(anyhow!("memory {slug}: front matter tags are empty"));
    }

    Ok(RawMemory {
        slug: slug.to_string(),
        front_matter,
        body: strip_leading_newline(raw_body).to_string(),
    })
}

fn list_markdown_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("unable to list directory {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_none_or(|name| !NON_MEMORY_MARKDOWN_FILES.contains(&name))
        })
        .collect();

    entries.sort();
    Ok(entries)
}

pub fn load_dir(dir: &Path) -> Result<Vec<RawMemory>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut memories = Vec::new();
    for path in list_markdown_files(dir)? {
        let slug = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("memory file has non-utf8 stem: {}", path.display()))?;

        let content = fs::read_to_string(&path)
            .with_context(|| format!("unable to read memory file {}", path.display()))?;
        let memory = parse_memory_file(slug, &content)
            .with_context(|| format!("failed to parse memory file {}", path.display()))?;

        memories.push(memory);
    }

    memories.sort_by(|a, b| {
        a.front_matter
            .order
            .cmp(&b.front_matter.order)
            .then_with(|| a.slug.cmp(&b.slug))
    });

    Ok(memories)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn parse_memory_file_round_trips_valid_front_matter() {
        let content = "+++\ntitle = \"Alpha\"\ntags = [\"a\", \"b\"]\norder = 1\ntemplate = false\n+++\n\nalpha body\n";
        let memory = parse_memory_file("alpha", content).expect("valid memory file should parse");
        assert_eq!(memory.slug, "alpha");
        assert_eq!(memory.front_matter.title, "Alpha");
        assert_eq!(memory.front_matter.tags, vec!["a", "b"]);
        assert_eq!(memory.front_matter.order, 1);
        assert!(!memory.front_matter.template);
        assert_eq!(memory.body, "alpha body\n");
    }

    #[test]
    fn parse_memory_file_rejects_missing_opening_delimiter() {
        let content = "title = \"Alpha\"\ntags = [\"a\"]\n+++\nbody\n";
        let err = parse_memory_file("alpha", content).unwrap_err();
        assert!(err.to_string().contains("missing opening TOML delimiter"));
    }

    #[test]
    fn parse_memory_file_rejects_missing_closing_delimiter() {
        let content = "+++\ntitle = \"Alpha\"\ntags = [\"a\"]\nbody\n";
        let err = parse_memory_file("alpha", content).unwrap_err();
        assert!(err.to_string().contains("missing closing TOML delimiter"));
    }

    #[test]
    fn parse_memory_file_rejects_invalid_toml() {
        let content = "+++\ntitle = \"Alpha\"\n\ntags: [\"a\"]\n+++\nbody\n";
        let err = parse_memory_file("alpha", content).unwrap_err();
        assert!(err.to_string().contains("invalid TOML front matter"));
    }

    #[test]
    fn parse_memory_file_rejects_empty_title_or_tags() {
        let empty_title = "+++\ntitle = \"\"\ntags = [\"a\"]\n+++\nbody\n";
        assert!(parse_memory_file("alpha", empty_title).is_err());

        let empty_tags = "+++\ntitle = \"Alpha\"\ntags = []\n+++\nbody\n";
        assert!(parse_memory_file("alpha", empty_tags).is_err());
    }

    #[test]
    fn parse_memory_file_handles_crlf() {
        let content =
            "+++\r\ntitle = \"Alpha\"\r\ntags = [\"a\"]\r\norder = 9\r\n+++\r\n\r\nalpha body\r\n";
        let memory = parse_memory_file("alpha", content).expect("crlf front matter should parse");
        assert_eq!(memory.body, "alpha body\r\n");
    }

    #[test]
    fn load_dir_returns_empty_for_missing_directory() {
        let missing = Path::new("/does-not-exist-for-system-memory-tests");
        let out = load_dir(missing).expect("missing directory should be empty");
        assert!(out.is_empty());
    }

    #[test]
    fn load_dir_parses_all_files_in_dir_sorted_by_order() {
        let dir = tempdir().expect("temp dir");
        let base = dir.path();

        fs::write(
            base.join("b.md"),
            "+++\ntitle = \"b\"\ntags = [\"x\"]\norder = 9\n+++\nB\n",
        )
        .expect("write b");
        fs::write(
            base.join("a.md"),
            "+++\ntitle = \"a\"\ntags = [\"y\"]\norder = 2\n+++\nA\n",
        )
        .expect("write a");

        let loaded = load_dir(base).expect("load should parse");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].slug, "a");
        assert_eq!(loaded[1].slug, "b");
    }

    #[test]
    fn load_dir_errors_on_invalid_file() {
        let dir = tempdir().expect("temp dir");
        let base = dir.path();
        fs::write(base.join("bad.md"), "no delimiter\nbody\n").expect("write bad");
        assert!(load_dir(base).is_err());
    }

    #[test]
    fn load_dir_skips_markdown_navigation_catalog() {
        let dir = tempdir().expect("temp dir");
        let base = dir.path();
        fs::write(
            base.join("valid.md"),
            "+++\ntitle = \"valid\"\ntags = [\"x\"]\norder = 1\n+++\nValid\n",
        )
        .expect("write valid");
        fs::write(
            base.join("system-memory-catalog.md"),
            "---\ntitle: Catalog\n---\n# Catalog\n",
        )
        .expect("write catalog");

        let loaded = load_dir(base).expect("load should skip catalog");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].slug, "valid");
    }
}
