use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WalRecord {
    pub entity_id: String,
    pub content_hash: String,
    pub model: String,
    pub dims: usize,
    pub vector: Vec<f32>,
    pub deleted_at: Option<String>,
    pub route: String,
}

impl WalRecord {
    pub fn upsert(route: &str, entity_id: &str, content_hash: &str, vector: Vec<f32>) -> Self {
        Self {
            entity_id: entity_id.to_string(),
            content_hash: content_hash.to_string(),
            model: route.to_string(),
            dims: vector.len(),
            vector,
            deleted_at: None,
            route: route.to_string(),
        }
    }

    pub fn delete(route: &str, entity_id: &str) -> Self {
        Self {
            entity_id: entity_id.to_string(),
            content_hash: String::new(),
            model: route.to_string(),
            dims: 0,
            vector: Vec::new(),
            deleted_at: Some(chrono::Utc::now().to_rfc3339()),
            route: route.to_string(),
        }
    }
}

pub fn append(path: &Path, record: &WalRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating vector WAL dir {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening vector WAL {}", path.display()))?;
    serde_json::to_writer(&mut file, record).context("serializing vector WAL record")?;
    file.write_all(b"\n").context("writing vector WAL newline")?;
    file.sync_data().context("fsync vector WAL")?;
    Ok(())
}

pub fn read_all(path: &Path) -> Result<Vec<WalRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path).with_context(|| format!("opening vector WAL {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading vector WAL line {}", idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        records.push(
            serde_json::from_str(&line)
                .with_context(|| format!("parsing vector WAL line {}", idx + 1))?,
        );
    }
    Ok(records)
}
