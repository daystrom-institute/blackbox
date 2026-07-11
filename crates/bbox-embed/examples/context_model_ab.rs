//! Offline retrieval A/B for the eval-gated route migrations
//! (design/corpus/agentic-corpus/multimodal-embedding-routing.md Layers 1-2):
//! voyage-code-3 (current `code` route) vs voyage-4-large/lite (current
//! prose route) vs voyage-context-4 (candidate contextualized route),
//! using PRODUCTION chunking (bbox_chunker::default_registry) over a real
//! corpus sample so the comparison measures the models, not a toy chunker.
//!
//! Ground truth comes from the eval suite: every manifest with a
//! project_file/symbol path_hint contributes (query, expected file).
//! Distractors are sampled deterministically from the repo. Scoring is
//! file-level MRR / recall@5-files: rank of the first chunk belonging to
//! the expected file in each arm's cosine ranking.
//!
//! Live Voyage API usage (a few thousand embeddings). Run:
//!   VOYAGE_API_KEY=... cargo run -p bbox-embed --example context_model_ab -- \
//!       /path/to/repo [max_files]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bbox_embed::embed::voyage::VoyageConfig;
use bbox_embed::embed::voyage_context::{VoyageContextConfig, VoyageContextProvider};
use bbox_embed::embed::{EmbedInput, EmbedInputType, EmbeddingProvider, voyage::VoyageProvider};

const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_CHUNKS_PER_FILE: usize = 256;
const FLAT_BATCH: usize = 96;
const CONTEXT_DOCS_PER_CALL: usize = 6;
/// Keep each contextualized request comfortably inside the 120K-token
/// aggregate cap (bytes as a conservative token proxy).
const CONTEXT_BYTES_PER_CALL: usize = 240 * 1024;
/// Per-document window: one contextualized document must fit the model's
/// 32K-token context (no truncation); 64KB of text stays safely inside
/// even for dense code.
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

struct QueryCase {
    id: String,
    query: String,
    expected_file: String,
}

struct FileChunks {
    rel_path: String,
    chunks: Vec<String>,
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run())
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().unwrap_or_else(|| ".".into())).canonicalize()?;
    let max_files: usize = args.next().and_then(|raw| raw.parse().ok()).unwrap_or(72);

    let cases = load_cases(&root)?;
    if cases.is_empty() {
        bail!(
            "no eval manifests with path_hint ground truth under {}",
            root.display()
        );
    }
    eprintln!("{} query cases", cases.len());

    let corpus = build_corpus(&root, &cases, max_files)?;
    let total_chunks: usize = corpus.iter().map(|file| file.chunks.len()).sum();
    eprintln!(
        "{} files, {} chunks in the sample corpus",
        corpus.len(),
        total_chunks
    );

    let code3 = VoyageProvider::from_config(
        "ab_code3".into(),
        &VoyageConfig {
            model: Some("voyage-code-3".into()),
            ..VoyageConfig::default()
        },
    )?;
    let v4 = VoyageProvider::from_config(
        "ab_v4".into(),
        &VoyageConfig {
            model: None,
            document_model: Some("voyage-4-large".into()),
            query_model: Some("voyage-4-lite".into()),
            ..VoyageConfig::default()
        },
    )?;
    let context4 =
        VoyageContextProvider::from_config("ab_context4".into(), &VoyageContextConfig::default())?;

    let mut reports = BTreeMap::new();
    let mut per_query: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
    for (arm, provider, contextual) in [
        ("voyage-code-3", &code3 as &dyn EmbeddingProvider, false),
        ("voyage-4-large/lite", &v4 as &dyn EmbeddingProvider, false),
        (
            "voyage-context-4",
            &context4 as &dyn EmbeddingProvider,
            true,
        ),
    ] {
        eprintln!("arm {arm}: embedding corpus...");
        let doc_vectors = embed_corpus(provider, &corpus, contextual).await?;
        let mut rrs = Vec::new();
        let mut recall5 = Vec::new();
        for case in &cases {
            let query_vector = provider
                .embed_batch(
                    &[EmbedInput::Text(case.query.clone())],
                    EmbedInputType::Query,
                )
                .await?
                .into_iter()
                .next()
                .context("no query vector")?
                .into_single()?;
            let (rr, hit5) = score(&doc_vectors, &query_vector, &case.expected_file);
            per_query
                .entry(case.id.clone())
                .or_default()
                .insert(arm.to_string(), rr);
            rrs.push(rr);
            recall5.push(if hit5 { 1.0 } else { 0.0 });
        }
        let n = rrs.len() as f64;
        reports.insert(
            arm.to_string(),
            (rrs.iter().sum::<f64>() / n, recall5.iter().sum::<f64>() / n),
        );
        eprintln!("arm {arm} done");
    }

    println!(
        "\narm                   file-MRR   file-recall@5   ({} queries)",
        cases.len()
    );
    for (arm, (mrr, recall)) in &reports {
        println!("{arm:<21} {mrr:.4}     {recall:.4}");
    }
    println!("\nper-query file-RR:");
    for (id, arms) in &per_query {
        let row = arms
            .iter()
            .map(|(arm, rr)| format!("{arm}={rr:.3}"))
            .collect::<Vec<_>>()
            .join("  ");
        println!("  {id}: {row}");
    }
    Ok(())
}

fn load_cases(root: &Path) -> Result<Vec<QueryCase>> {
    let mut cases = Vec::new();
    for entry in std::fs::read_dir(root.join("eval/queries"))? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let Some(locators) = manifest["target_locators"].as_array() else {
            continue;
        };
        let Some(hint) = locators
            .iter()
            .filter_map(|locator| locator["path_hint"].as_str())
            .find(|hint| root.join(hint).is_file())
        else {
            continue;
        };
        cases.push(QueryCase {
            id: manifest["id"].as_str().unwrap_or("?").to_string(),
            query: manifest["query"].as_str().context("query")?.to_string(),
            expected_file: hint.to_string(),
        });
    }
    cases.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(cases)
}

fn build_corpus(root: &Path, cases: &[QueryCase], max_files: usize) -> Result<Vec<FileChunks>> {
    let mut paths: Vec<PathBuf> = cases
        .iter()
        .map(|case| root.join(&case.expected_file))
        .collect();
    let mut candidates = Vec::new();
    for top in ["src", "crates", "design", "docs"] {
        walk(&root.join(top), &mut candidates);
    }
    candidates.sort();
    let needed = max_files.saturating_sub(paths.len());
    let step = (candidates.len() / needed.max(1)).max(1);
    for path in candidates.iter().step_by(step) {
        if paths.len() >= max_files {
            break;
        }
        if !paths.contains(path) {
            paths.push(path.clone());
        }
    }

    let registry = bbox_chunker::default_registry();
    let mut corpus = Vec::new();
    for path in paths {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.len() > MAX_FILE_BYTES || bytes.is_empty() {
            continue;
        }
        let sniff = &bytes[..bytes.len().min(4096)];
        let Some(chunker) = registry.iter().find(|c| c.claims(&path, sniff)) else {
            continue;
        };
        let Ok((chunks, _)) = chunker.chunk(&path, &bytes) else {
            continue;
        };
        let texts: Vec<String> = chunks
            .into_iter()
            .take(MAX_CHUNKS_PER_FILE)
            .map(|chunk| chunk.content)
            .filter(|text| !text.trim().is_empty())
            .collect();
        if texts.is_empty() {
            continue;
        }
        corpus.push(FileChunks {
            rel_path: path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string(),
            chunks: texts,
        });
    }
    Ok(corpus)
}

fn walk(dir: &Path, acc: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "target" || name.starts_with('.') {
                continue;
            }
            walk(&path, acc);
        } else if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("rs") | Some("md")
        ) {
            acc.push(path);
        }
    }
}

/// (rel_path, vector) per chunk, arm-specific.
async fn embed_corpus(
    provider: &dyn EmbeddingProvider,
    corpus: &[FileChunks],
    contextual: bool,
) -> Result<Vec<(String, Vec<f32>)>> {
    let mut out = Vec::new();
    if contextual {
        // voyage-context-4 caps one DOCUMENT at its 32K-token context
        // window with no truncation (verified live 2026-07-11); window
        // oversized files into sub-documents the way the embed queue does.
        let windowed: Vec<FileChunks> = corpus
            .iter()
            .flat_map(|file| {
                let mut subs = Vec::new();
                let mut chunks = Vec::new();
                let mut bytes = 0usize;
                for chunk in &file.chunks {
                    if !chunks.is_empty() && bytes + chunk.len() > MAX_DOCUMENT_BYTES {
                        subs.push(FileChunks {
                            rel_path: file.rel_path.clone(),
                            chunks: std::mem::take(&mut chunks),
                        });
                        bytes = 0;
                    }
                    bytes += chunk.len();
                    chunks.push(chunk.clone());
                }
                if !chunks.is_empty() {
                    subs.push(FileChunks {
                        rel_path: file.rel_path.clone(),
                        chunks,
                    });
                }
                subs
            })
            .collect();
        let corpus = &windowed[..];
        let mut call: Vec<&FileChunks> = Vec::new();
        let mut call_bytes = 0usize;
        let mut flush = Vec::new();
        for file in corpus {
            let file_bytes: usize = file.chunks.iter().map(|chunk| chunk.len()).sum();
            if !call.is_empty()
                && (call.len() >= CONTEXT_DOCS_PER_CALL
                    || call_bytes + file_bytes > CONTEXT_BYTES_PER_CALL)
            {
                flush.push(std::mem::take(&mut call));
                call_bytes = 0;
            }
            call_bytes += file_bytes;
            call.push(file);
        }
        if !call.is_empty() {
            flush.push(call);
        }
        for files in flush {
            let inputs: Vec<EmbedInput> = files
                .iter()
                .map(|file| EmbedInput::DocumentChunks(file.chunks.clone()))
                .collect();
            let outputs = provider
                .embed_batch(&inputs, EmbedInputType::Document)
                .await?;
            for (file, output) in files.iter().zip(outputs) {
                for vector in output.vectors {
                    out.push((file.rel_path.clone(), vector));
                }
            }
        }
        return Ok(out);
    }
    let flat: Vec<(String, String)> = corpus
        .iter()
        .flat_map(|file| {
            file.chunks
                .iter()
                .map(|chunk| (file.rel_path.clone(), chunk.clone()))
        })
        .collect();
    for batch in flat.chunks(FLAT_BATCH) {
        let inputs: Vec<EmbedInput> = batch
            .iter()
            .map(|(_, text)| EmbedInput::Text(text.clone()))
            .collect();
        let outputs = provider
            .embed_batch(&inputs, EmbedInputType::Document)
            .await?;
        for ((rel_path, _), output) in batch.iter().zip(outputs) {
            out.push((rel_path.clone(), output.into_single()?));
        }
    }
    Ok(out)
}

fn score(doc_vectors: &[(String, Vec<f32>)], query: &[f32], expected_file: &str) -> (f64, bool) {
    let mut scored: Vec<(&str, f32)> = doc_vectors
        .iter()
        .map(|(rel_path, vector)| (rel_path.as_str(), cosine(query, vector)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut seen_files = Vec::new();
    let mut rr = 0.0;
    let mut hit5 = false;
    for (rel_path, _) in scored {
        if !seen_files.contains(&rel_path) {
            seen_files.push(rel_path);
        }
        if rel_path == expected_file {
            let rank = seen_files.len();
            rr = 1.0 / rank as f64;
            hit5 = rank <= 5;
            break;
        }
    }
    (rr, hit5)
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}
