use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;

use super::{Chunk, Edge, SourceFormatChunker, placeholder_chunk};

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
    let mut byte_offset = 0usize;
    match value {
        JsonValue::Object(map) => {
            for (key, value) in map {
                let content = serde_json::to_string_pretty(&serde_json::json!({ key: value }))?;
                let byte_start = byte_offset;
                let byte_end = byte_start + content.len();
                chunks.push(placeholder_chunk(
                    path,
                    "config_block",
                    Some("json"),
                    content,
                    byte_start as u64,
                    byte_end as u64,
                    chunks.len() as u32,
                ));
                byte_offset = byte_end + 1;
            }
        }
        _ => {
            let content = serde_json::to_string_pretty(value)?;
            chunks.push(placeholder_chunk(
                path,
                "config_block",
                Some("json"),
                content.clone(),
                0,
                content.len() as u64,
                0,
            ));
        }
    }
    Ok(chunks)
}

fn top_level_text_chunks(path: &Path, language: &str, rendered: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_start = 0usize;
    let mut current_end = 0usize;
    let mut offset = 0usize;
    for line in rendered.split_inclusive('\n') {
        let starts_top_level = !line.starts_with(char::is_whitespace) && !line.trim().is_empty();
        if starts_top_level && !current.trim().is_empty() {
            chunks.push(placeholder_chunk(
                path,
                "config_block",
                Some(language),
                current.trim().to_string(),
                current_start as u64,
                current_end as u64,
                chunks.len() as u32,
            ));
            current.clear();
            current_start = offset;
        }
        current.push_str(line);
        current_end = offset + line.len();
        offset = current_end;
    }
    if !current.trim().is_empty() {
        chunks.push(placeholder_chunk(
            path,
            "config_block",
            Some(language),
            current.trim().to_string(),
            current_start as u64,
            current_end as u64,
            chunks.len() as u32,
        ));
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_chunkers_populate_byte_ranges() {
        let (json_chunks, _) = JsonChunker
            .chunk(Path::new("config.json"), br#"{ "b": 2, "a": 1 }"#)
            .unwrap();
        assert!(
            json_chunks
                .iter()
                .all(|chunk| chunk.byte_end > chunk.byte_start)
        );

        let (toml_chunks, _) = TomlChunker
            .chunk(
                Path::new("Cargo.toml"),
                b"[package]\nname = \"x\"\n[dependencies]\n",
            )
            .unwrap();
        assert!(
            toml_chunks
                .iter()
                .all(|chunk| chunk.byte_end > chunk.byte_start)
        );

        let (yaml_chunks, _) = YamlChunker
            .chunk(Path::new("config.yaml"), b"a: 1\nb:\n  c: 2\n")
            .unwrap();
        assert!(
            yaml_chunks
                .iter()
                .all(|chunk| chunk.byte_end > chunk.byte_start)
        );
    }
}
