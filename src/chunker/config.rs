use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;

use super::{placeholder_chunk, Chunk, Edge, SourceFormatChunker};

pub struct JsonChunker;
pub struct TomlChunker;
pub struct YamlChunker;

impl SourceFormatChunker for JsonChunker {
    fn format_id(&self) -> &str {
        "config/json"
    }

    fn claims(&self, path: &Path, _sniff: &[u8]) -> bool {
        path.extension().and_then(|ext| ext.to_str()) == Some("json")
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        let value: JsonValue = serde_json::from_slice(bytes)?;
        let chunks = json_top_level_chunks(path, &value)?;
        Ok((chunks, Vec::new()))
    }
}

impl SourceFormatChunker for TomlChunker {
    fn format_id(&self) -> &str {
        "config/toml"
    }

    fn claims(&self, path: &Path, _sniff: &[u8]) -> bool {
        path.extension().and_then(|ext| ext.to_str()) == Some("toml")
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        let text = std::str::from_utf8(bytes)?;
        let value: toml::Value = text.parse()?;
        let rendered = toml::to_string_pretty(&value).context("rendering toml value")?;
        Ok((top_level_text_chunks(path, "toml", &rendered), Vec::new()))
    }
}

impl SourceFormatChunker for YamlChunker {
    fn format_id(&self) -> &str {
        "config/yaml"
    }

    fn claims(&self, path: &Path, _sniff: &[u8]) -> bool {
        matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yaml" | "yml")
        )
    }

    fn chunk(&self, path: &Path, bytes: &[u8]) -> Result<(Vec<Chunk>, Vec<Edge>)> {
        let value: serde_yaml::Value = serde_yaml::from_slice(bytes)?;
        let rendered = serde_yaml::to_string(&value)?;
        Ok((top_level_text_chunks(path, "yaml", &rendered), Vec::new()))
    }
}

fn json_top_level_chunks(path: &Path, value: &JsonValue) -> Result<Vec<Chunk>> {
    let mut chunks = Vec::new();
    match value {
        JsonValue::Object(map) => {
            for (key, value) in map {
                let content = serde_json::to_string_pretty(&serde_json::json!({ key: value }))?;
                chunks.push(placeholder_chunk(
                    path,
                    "config_block",
                    Some("json"),
                    content,
                    0,
                    0,
                    chunks.len() as u32,
                ));
            }
        }
        _ => chunks.push(placeholder_chunk(
            path,
            "config_block",
            Some("json"),
            serde_json::to_string_pretty(value)?,
            0,
            0,
            0,
        )),
    }
    Ok(chunks)
}

fn top_level_text_chunks(path: &Path, language: &str, rendered: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in rendered.lines() {
        let starts_top_level = !line.starts_with(char::is_whitespace) && !line.trim().is_empty();
        if starts_top_level && !current.trim().is_empty() {
            chunks.push(placeholder_chunk(
                path,
                "config_block",
                Some(language),
                current.trim().to_string(),
                0,
                0,
                chunks.len() as u32,
            ));
            current.clear();
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        chunks.push(placeholder_chunk(
            path,
            "config_block",
            Some(language),
            current.trim().to_string(),
            0,
            0,
            chunks.len() as u32,
        ));
    }
    chunks
}
