use std::path::PathBuf;

use anyhow::{Context, bail};
use bbox_provenance::{ApplyExportPageResult, ProvenanceExportPage, apply_export_page};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};

use crate::mcp_call::{McpClient, default_base_url, tool_error_has_code};

const PLAN_TOOL: &str = "bbox_provenance_export_plan";
const MAX_STALE_RESTARTS: usize = 3;

#[derive(Debug, Args)]
#[command(
    after_help = "Subcommands:\n  export    plan centrally and write provenance notes in this checkout"
)]
pub(crate) struct ProvenanceArgs {
    #[command(subcommand)]
    command: ProvenanceCommand,
}

#[derive(Debug, Subcommand)]
enum ProvenanceCommand {
    /// Export tracked provenance to Git notes in one local checkout
    Export(ProvenanceExportArgs),
}

#[derive(Debug, Args)]
struct ProvenanceExportArgs {
    /// Checkout project root. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    project_root: Option<PathBuf>,
    /// Daemon base URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
}

#[derive(Debug, Default)]
struct ExportProgress {
    generation: Option<String>,
    cursor: Option<String>,
    scope: Option<Value>,
    project_id: Option<String>,
    notes_ref: Option<String>,
    stale_restarts: usize,
    pages: u64,
    applied: ApplyExportPageResult,
}

impl ExportProgress {
    fn plan_arguments(&self) -> Value {
        let mut arguments = Map::new();
        if let Some(cursor) = &self.cursor {
            arguments.insert("cursor".into(), Value::String(cursor.clone()));
        }
        if let Some(generation) = &self.generation {
            arguments.insert("generation".into(), Value::String(generation.clone()));
        }
        Value::Object(arguments)
    }

    fn accept_page(
        &mut self,
        page: &ProvenanceExportPage,
        result: ApplyExportPageResult,
    ) -> anyhow::Result<bool> {
        self.validate_page(page)?;
        if let Some(generation) = &self.generation {
            debug_assert_eq!(generation, &page.generation);
        } else {
            self.generation = Some(page.generation.clone());
            self.scope = Some(serde_json::to_value(&page.scope)?);
            self.project_id = Some(page.project_id.clone());
            self.notes_ref = Some(page.notes_ref.clone());
        }
        self.cursor.clone_from(&page.next_cursor);
        self.pages += 1;
        self.applied.written += result.written;
        self.applied.unchanged += result.unchanged;
        self.applied.rejected += result.rejected;
        Ok(self.cursor.is_none())
    }

    fn validate_page(&self, page: &ProvenanceExportPage) -> anyhow::Result<()> {
        let page_scope = serde_json::to_value(&page.scope)
            .context("serializing provenance page scope for comparison")?;
        if let Some(generation) = &self.generation
            && generation != &page.generation
        {
            bail!("provenance plan generation changed without a stale-generation error");
        }
        if self
            .scope
            .as_ref()
            .is_some_and(|scope| scope != &page_scope)
            || self
                .project_id
                .as_ref()
                .is_some_and(|project_id| project_id != &page.project_id)
            || self
                .notes_ref
                .as_ref()
                .is_some_and(|notes_ref| notes_ref != &page.notes_ref)
        {
            bail!("provenance plan identity changed between pages");
        }
        if page.next_cursor.is_some() && page.next_cursor == self.cursor {
            bail!("provenance plan returned a cursor that made no progress");
        }
        Ok(())
    }

    fn restart_after_stale(&mut self) -> anyhow::Result<()> {
        if self.stale_restarts >= MAX_STALE_RESTARTS {
            bail!("provenance export stayed stale after {MAX_STALE_RESTARTS} generation restarts");
        }
        self.stale_restarts += 1;
        self.generation = None;
        self.cursor = None;
        self.scope = None;
        self.project_id = None;
        self.notes_ref = None;
        Ok(())
    }
}

pub(crate) async fn run(args: ProvenanceArgs) -> anyhow::Result<()> {
    match args.command {
        ProvenanceCommand::Export(args) => export(args).await,
    }
}

async fn export(args: ProvenanceExportArgs) -> anyhow::Result<()> {
    let requested_root = match args.project_root {
        Some(root) => root,
        None => std::env::current_dir().context("reading current directory")?,
    };
    let project_root = requested_root.canonicalize().with_context(|| {
        format!(
            "canonicalizing provenance project root {}",
            requested_root.display()
        )
    })?;
    if !project_root.is_dir() {
        bail!("provenance project root is not a directory");
    }

    let base_url = args.daemon_url.unwrap_or_else(default_base_url);
    let mut client = McpClient::connect(&base_url, Some(&project_root)).await?;
    let mut progress = ExportProgress::default();

    loop {
        let page = match client
            .call_tool_json::<ProvenanceExportPage>(PLAN_TOOL, progress.plan_arguments())
            .await
        {
            Ok(page) => page,
            Err(error) if tool_error_has_code(&error, "error.stale_generation") => {
                progress.restart_after_stale()?;
                continue;
            }
            Err(error) => return Err(error),
        };
        progress.validate_page(&page)?;
        let result = apply_export_page(&project_root, &page)
            .with_context(|| format!("applying provenance export page {}", progress.pages + 1))?;
        if progress.accept_page(&page, result)? {
            break;
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "ok",
            "project_root": project_root,
            "generation": progress.generation,
            "pages": progress.pages,
            "stale_restarts": progress.stale_restarts,
            "written": progress.applied.written,
            "unchanged": progress.applied.unchanged,
            "rejected": progress.applied.rejected,
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(generation: &str, next_cursor: Option<&str>) -> ProvenanceExportPage {
        serde_json::from_value(json!({
            "scope": {"repo_id": "repo", "bbox_root_relpath": "."},
            "project_id": "project",
            "notes_ref": "refs/notes/bbox/provenance",
            "generation": generation,
            "documents": [],
            "next_cursor": next_cursor,
        }))
        .unwrap()
    }

    #[test]
    fn pages_keep_one_generation_and_advance_cursor() {
        let mut progress = ExportProgress::default();
        assert!(
            !progress
                .accept_page(&page("one", Some("next")), ApplyExportPageResult::default())
                .unwrap()
        );
        assert_eq!(progress.plan_arguments()["cursor"], "next");
        assert_eq!(progress.plan_arguments()["generation"], "one");
        assert!(
            progress
                .accept_page(&page("one", None), ApplyExportPageResult::default())
                .unwrap()
        );
        assert_eq!(progress.pages, 2);
    }

    #[test]
    fn changed_generation_without_stale_error_fails_closed() {
        let mut progress = ExportProgress::default();
        progress
            .accept_page(&page("one", Some("next")), ApplyExportPageResult::default())
            .unwrap();
        let error = progress
            .accept_page(&page("two", None), ApplyExportPageResult::default())
            .unwrap_err();
        assert!(error.to_string().contains("generation changed"));
    }

    #[test]
    fn stale_generation_restarts_are_bounded() {
        let mut progress = ExportProgress::default();
        for _ in 0..MAX_STALE_RESTARTS {
            progress.restart_after_stale().unwrap();
        }
        assert!(progress.restart_after_stale().is_err());
    }
}
