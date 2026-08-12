//! One-shot provisional capture driver for a bound checkout.
//!
//! `WorkspaceCaptureClient` is the checkout-owner half of the knowledge-source
//! transport. In production the managed harness constructs it at session start
//! from the three environment variables the daemon mints with a workspace
//! binding, and calls `sync_once()` before the first turn. This example is the
//! same construction with no agent loop around it, so a live exercise or a
//! manual dev loop can drive a real provisional capture without an LLM turn.
//!
//! Environment (exactly what `bro workspace-binding mint` writes into
//! `.bbox/local/workspace-binding.env`):
//!
//! - `BRO_WORKSPACE_BINDING_TOKEN` - the minted capability
//! - `BRO_KNOWLEDGE_SOURCE_URL` - the daemon that minted it
//! - `BRO_WORKSPACE_PUBLISHED_SCOPE` - the scope the binding authorizes
//!
//! Usage:
//!
//! ```text
//! set -a; . .bbox/local/workspace-binding.env; set +a
//! cargo run -p bbox-knowledge-source-client --example workspace-capture -- <project-root>
//! ```
//!
//! The project root defaults to the current directory. The captured outcome is
//! printed as one JSON object on stdout.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_knowledge_source_client::WorkspaceCaptureClient;

#[tokio::main]
async fn main() -> Result<()> {
    let project_root = match std::env::args().nth(1) {
        Some(raw) => PathBuf::from(raw),
        None => std::env::current_dir().context("reading current directory")?,
    }
    .canonicalize()
    .context("canonicalizing project root")?;

    let token = env_var(bro_protocol::WORKSPACE_BINDING_ENV)?;
    let source_url = env_var(bro_protocol::KNOWLEDGE_SOURCE_URL_ENV)?;
    let raw_scope = env_var(bro_protocol::WORKSPACE_SCOPE_ENV)?;

    let scope: PublishedScope =
        serde_json::from_str(&raw_scope).context("decoding the bound published scope")?;
    scope.validate()?;

    let workspace_root = bbox_corpus_core::git::git_root_for_path(&project_root)
        .context("bound project root is not inside a Git checkout")?
        .canonicalize()
        .context("canonicalizing the bound workspace root")?;
    if !project_root.starts_with(&workspace_root) {
        bail!("bound project root is outside its workspace");
    }

    let workspace_id = bbox_corpus_core::identity::read_checkout_id(
        &workspace_root.join(".bbox/local/checkout-id"),
    )?
    .context("bound checkout has no workspace identity marker")?;
    let workspace_id = bro_core::WorkspaceId::parse(workspace_id)?;

    let client = WorkspaceCaptureClient::new(
        &source_url,
        bro_protocol::WorkspaceBindingToken::parse(token)?,
        workspace_root,
        project_root,
        workspace_id,
        scope,
    )?;
    let outcome = client.sync_once().await?;
    println!(
        "{}",
        serde_json::json!({
            "source_generation_id": outcome.source_generation_id,
            "sequence": outcome.sequence,
            "reused": outcome.reused,
        })
    );
    Ok(())
}

fn env_var(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("reading {name}"))?;
    if value.trim().is_empty() {
        bail!("{name} is empty");
    }
    Ok(value)
}
