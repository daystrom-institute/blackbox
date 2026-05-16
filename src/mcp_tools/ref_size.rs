use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::entity_loader;
use crate::entity_ref::{EntityRef, EntityType};
use crate::providers::ProviderContext;

const REF_CAP: usize = 500;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefSizeParams {
    /// Entity refs to measure. Successful refs are returned in canonical form;
    /// unresolved refs retain the caller-supplied string for diagnosis.
    pub refs: Vec<String>,
    /// Optional project root for resolving `file:` refs before falling back to
    /// registered project roots. This lets workflow-local worktrees measure
    /// newly-created files that have not been indexed under the canonical repo.
    pub project_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RefSizeEntry {
    #[serde(rename = "ref")]
    entity_ref: String,
    entity_type: String,
    bytes: u64,
    source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct UnresolvedRef {
    #[serde(rename = "ref")]
    entity_ref: String,
    error: String,
}

pub fn ref_size(p: &RefSizeParams, ctx: &ProviderContext<'_>) -> Result<String> {
    let omitted_refs = p.refs.len().saturating_sub(REF_CAP);
    let mut per_ref = Vec::new();
    let mut unresolved_refs = Vec::new();

    for raw in p.refs.iter().take(REF_CAP) {
        match size_one_ref(raw, ctx, p.project_dir.as_deref()) {
            Ok(entry) => per_ref.push(entry),
            Err(err) => unresolved_refs.push(UnresolvedRef {
                entity_ref: raw.clone(),
                error: err.to_string(),
            }),
        }
    }

    let total_bytes = per_ref.iter().map(|entry| entry.bytes).sum::<u64>();
    let status = if unresolved_refs.is_empty() && omitted_refs == 0 {
        "ok"
    } else {
        "degraded"
    };

    Ok(serde_json::to_string_pretty(&json!({
        "status": status,
        "total_bytes": total_bytes,
        "per_ref": per_ref,
        "degraded": {
            "unresolved_refs": unresolved_refs,
            "omitted_refs": omitted_refs,
            "omitted_ref_samples": p.refs.iter().skip(REF_CAP).take(10).collect::<Vec<_>>(),
        }
    }))?)
}

fn size_one_ref(
    raw: &str,
    ctx: &ProviderContext<'_>,
    project_dir: Option<&str>,
) -> Result<RefSizeEntry> {
    let entity_ref = EntityRef::parse(raw)?;
    let (bytes, source) = match entity_ref.entity_type() {
        EntityType::ProjectFile | EntityType::ProjectFileV2 => {
            let state = ctx
                .state()
                .ok_or_else(|| anyhow!("project_file refs require an index-backed context"))?;
            let doc = state
                .idx
                .read()
                .embedding_source_doc_for_entity_id(&entity_ref.to_string())?
                .ok_or_else(|| anyhow!("project_file entity {entity_ref} not found"))?;
            (byte_len(&doc.content), "project_file_content")
        }
        EntityType::File => {
            let EntityRef::File { path } = &entity_ref else {
                unreachable!();
            };
            size_file_ref(ctx, path, project_dir)?
        }
        _ => {
            let view = entity_loader::load(ctx, &entity_ref)?;
            let payload = json!({
                "ref_string": view.ref_string,
                "entity_type": view.entity_type.as_str(),
                "properties": view.properties,
            });
            let bytes = serde_json::to_vec(&payload)
                .map(|payload| payload.len() as u64)
                .map_err(|err| anyhow!("serializing entity properties: {err}"))?;
            (bytes, "entity_properties_json")
        }
    };

    Ok(RefSizeEntry {
        entity_ref: entity_ref.to_string(),
        entity_type: entity_ref.entity_type().as_str().to_string(),
        bytes,
        source,
    })
}

fn size_file_ref(
    ctx: &ProviderContext<'_>,
    path: &str,
    project_dir: Option<&str>,
) -> Result<(u64, &'static str)> {
    if let Some(project_dir) = project_dir {
        if let Ok(entry) = size_file_from_project_dir(path, project_dir) {
            return Ok(entry);
        }
    }

    let resolved = crate::providers::file::resolve_file(ctx, path)?;
    Ok((resolved.content.len() as u64, "file_content"))
}

fn size_file_from_project_dir(path: &str, project_dir: &str) -> Result<(u64, &'static str)> {
    let project_root = fs::canonicalize(project_dir)
        .with_context(|| format!("canonicalizing project_dir {project_dir}"))?;
    if !project_root.is_dir() {
        bail!(
            "project_dir `{}` is not a directory",
            project_root.display()
        );
    }

    let raw = Path::new(path);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        project_root.join(raw)
    };
    let canonical = fs::canonicalize(&candidate)
        .with_context(|| format!("canonicalizing file {}", candidate.display()))?;
    if !canonical.starts_with(&project_root) {
        bail!(
            "file ref `{}` is outside project_dir {}",
            path,
            project_root.display()
        );
    }
    if !canonical.is_file() {
        bail!("file ref `{}` is not a file", path);
    }

    let content =
        fs::read(&canonical).with_context(|| format!("reading file {}", canonical.display()))?;
    Ok((content.len() as u64, "file_content"))
}

fn byte_len(value: &str) -> u64 {
    value.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn byte_len_counts_utf8_bytes_not_chars() {
        assert_eq!(byte_len("aé"), 3);
    }

    #[test]
    fn empty_input_returns_zero_total() {
        let ctx = ProviderContext::empty_for_tests();
        let out = ref_size(
            &RefSizeParams {
                refs: Vec::new(),
                project_dir: None,
            },
            &ctx,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["total_bytes"], 0);
        assert_eq!(value["per_ref"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn unresolved_refs_degrade_the_response() {
        let ctx = ProviderContext::empty_for_tests();
        let out = ref_size(
            &RefSizeParams {
                refs: vec!["not-an-entity-ref".into()],
                project_dir: None,
            },
            &ctx,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "degraded");
        assert_eq!(value["total_bytes"], 0);
        assert_eq!(
            value["degraded"]["unresolved_refs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn virtual_entity_ref_measures_provider_properties_json() {
        let ctx = ProviderContext::empty_for_tests();
        let out = ref_size(
            &RefSizeParams {
                refs: vec!["task:task-123".into()],
                project_dir: None,
            },
            &ctx,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "ok");
        assert!(value["total_bytes"].as_u64().unwrap() > 0);
        assert_eq!(value["per_ref"][0]["ref"], "task:task-123");
        assert_eq!(value["per_ref"][0]["entity_type"], "task");
        assert_eq!(value["per_ref"][0]["source"], "entity_properties_json");
    }

    #[test]
    fn packet_and_artifact_refs_measure_provider_properties_json() {
        let ctx = ProviderContext::empty_for_tests();
        let out = ref_size(
            &RefSizeParams {
                refs: vec![
                    "packet:domain:phase-decompose/triage".into(),
                    "artifact:packet/phase-decompose/triage@1".into(),
                ],
                project_dir: None,
            },
            &ctx,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["per_ref"].as_array().unwrap().len(), 2);
        assert_eq!(
            value["per_ref"][0]["ref"],
            "packet:domain:phase-decompose/triage"
        );
        assert_eq!(value["per_ref"][0]["entity_type"], "packet");
        assert_eq!(
            value["per_ref"][1]["ref"],
            "artifact:packet/phase-decompose/triage@1"
        );
        assert_eq!(value["per_ref"][1]["entity_type"], "artifact");
    }

    #[test]
    fn file_ref_measures_registered_project_file_content() {
        let store = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("docs")).unwrap();
        let file_path = project.path().join("docs/design.md");
        fs::write(&file_path, "hello\nworld\n").unwrap();

        let state = crate::server::state::SharedState::for_test(store.path());
        state
            .projects
            .write()
            .register_path(project.path())
            .unwrap();
        let ctx = ProviderContext::new(&state);
        let out = ref_size(
            &RefSizeParams {
                refs: vec!["file:docs/design.md".into()],
                project_dir: None,
            },
            &ctx,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["total_bytes"], 12);
        assert_eq!(value["per_ref"][0]["ref"], "file:docs/design.md");
        assert_eq!(value["per_ref"][0]["entity_type"], "file");
        assert_eq!(value["per_ref"][0]["source"], "file_content");
    }

    #[test]
    fn file_ref_project_dir_resolves_worktree_only_file() {
        let store = tempfile::tempdir().unwrap();
        let registered = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        fs::create_dir_all(registered.path().join("scripts")).unwrap();
        fs::create_dir_all(worktree.path().join("scripts")).unwrap();
        fs::write(registered.path().join("scripts/guard.py"), "old").unwrap();
        fs::write(worktree.path().join("scripts/guard.py"), "new guard\n").unwrap();

        let state = crate::server::state::SharedState::for_test(store.path());
        state
            .projects
            .write()
            .register_path(registered.path())
            .unwrap();
        let ctx = ProviderContext::new(&state);
        let out = ref_size(
            &RefSizeParams {
                refs: vec!["file:scripts/guard.py".into()],
                project_dir: Some(worktree.path().to_string_lossy().into_owned()),
            },
            &ctx,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["total_bytes"], 10);
        assert_eq!(value["per_ref"][0]["ref"], "file:scripts/guard.py");
        assert_eq!(value["per_ref"][0]["source"], "file_content");
    }

    #[test]
    fn caps_refs_and_reports_omitted_samples() {
        let ctx = ProviderContext::empty_for_tests();
        let refs = (0..(REF_CAP + 2))
            .map(|idx| format!("invalid-ref-{idx}"))
            .collect::<Vec<_>>();
        let out = ref_size(
            &RefSizeParams {
                refs,
                project_dir: None,
            },
            &ctx,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "degraded");
        assert_eq!(value["degraded"]["omitted_refs"], 2);
        assert_eq!(
            value["degraded"]["unresolved_refs"]
                .as_array()
                .unwrap()
                .len(),
            REF_CAP
        );
        let expected = format!("invalid-ref-{REF_CAP}");
        assert_eq!(
            value["degraded"]["omitted_ref_samples"][0].as_str(),
            Some(expected.as_str())
        );
    }
}
