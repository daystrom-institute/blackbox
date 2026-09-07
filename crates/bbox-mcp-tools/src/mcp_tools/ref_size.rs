use std::collections::HashMap;

use anyhow::{Result, anyhow};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;

use bbox_corpus_core::entity_ref::{EntityRef, EntityType};
use bbox_providers::entity_loader;
use bbox_providers::providers::ProviderContext;

const REF_CAP: usize = 500;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RefSizeParams {
    /// Entity refs to measure. Successful refs are returned in canonical form;
    /// unresolved refs retain the caller-supplied string for diagnosis.
    pub refs: Vec<String>,
    /// Optional exact project or checkout root used by the daemon adapter when
    /// selecting checkout authority for relative `file:` refs. This lower
    /// module never opens the directory directly.
    pub project_dir: Option<String>,
    /// Continue exact result JSON with body.next_cursor and the same refs/project_dir.
    /// Changed measurements or selection refuse continuation.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Exact response body bytes, 4..=4096. Oversized replies start paging automatically.
    #[serde(default)]
    pub body_limit: Option<usize>,
}

/// Validate paging syntax before the adapter acquires checkout data.
pub fn validate_response_params(p: &RefSizeParams) -> Result<()> {
    anyhow::ensure!(
        p.body_limit.is_none_or(|limit| (4..=4096).contains(&limit)),
        "body_limit must be between 4 and 4096"
    );
    if let Some(cursor) = p.cursor.as_deref() {
        let valid = cursor.split_once(':').is_some_and(|(hash, offset)| {
            hash.len() == 64
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
                && offset.parse::<usize>().is_ok()
        });
        anyhow::ensure!(valid, "invalid cursor; use body.next_cursor");
    }
    Ok(())
}

/// Apply only after all measurement leases have been revalidated. Paging must
/// never turn a refused checkout read into a successful partial measurement.
pub fn page_response(p: &RefSizeParams, output: &str) -> Result<String> {
    validate_response_params(p)?;
    let scope = json!(["ref_size", p.refs, p.project_dir]).to_string();
    let value = bbox_corpus_core::response_page::bounded_json_response(
        &scope,
        serde_json::from_str(output)?,
        p.cursor.as_deref(),
        p.body_limit,
    )?;
    Ok(value.to_string())
}

/// Caller-resolved filesystem input for one `file:` ref. The daemon must keep
/// the lease that produced this path alive for the complete `ref_size` call.
/// This lower layer deliberately has no project registry or checkout authority.
#[derive(Debug, Clone)]
pub struct ValidatedFileInput {
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub enum FileInputResolution {
    Validated(ValidatedFileInput),
    Rejected(String),
}

pub type ValidatedFileInputs = HashMap<String, FileInputResolution>;

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
    ref_size_with_validated_files(p, ctx, &ValidatedFileInputs::new())
}

pub fn ref_size_with_validated_files(
    p: &RefSizeParams,
    ctx: &ProviderContext<'_>,
    validated_files: &ValidatedFileInputs,
) -> Result<String> {
    let omitted_refs = p.refs.len().saturating_sub(REF_CAP);
    let mut per_ref = Vec::new();
    let mut unresolved_refs = Vec::new();

    for raw in p.refs.iter().take(REF_CAP) {
        match size_one_ref(raw, ctx, validated_files) {
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
    validated_files: &ValidatedFileInputs,
) -> Result<RefSizeEntry> {
    let entity_ref = EntityRef::parse(raw)?;
    let (bytes, source) = match entity_ref.entity_type() {
        EntityType::ProjectFile | EntityType::ProjectFileV2 => {
            let stores = ctx
                .stores()
                .ok_or_else(|| anyhow!("project_file refs require an index-backed context"))?;
            let doc = stores
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
            size_file_ref(path, validated_files)?
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

fn size_file_ref(path: &str, validated_files: &ValidatedFileInputs) -> Result<(u64, &'static str)> {
    let input = validated_files.get(path).ok_or_else(|| {
        anyhow!("error.checkout_access_required: file ref has no validated checkout attachment")
    })?;
    let input = match input {
        FileInputResolution::Validated(input) => input,
        FileInputResolution::Rejected(error) => return Err(anyhow!(error.clone())),
    };
    Ok((input.bytes, "file_content"))
}

fn byte_len(value: &str) -> u64 {
    value.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_pages_preserve_large_unresolved_refs_and_refuse_stale_selection() {
        let ctx = ProviderContext::empty_for_tests();
        let mut params = RefSizeParams {
            refs: vec![format!("invalid-{}", "界\n\"".repeat(12000))],
            ..Default::default()
        };
        let output = ref_size(&params, &ctx).unwrap();
        let expected: serde_json::Value = serde_json::from_str(&output).unwrap();
        let first: serde_json::Value =
            serde_json::from_str(&page_response(&params, &output).unwrap()).unwrap();
        assert_eq!(first["detail_limited"], true);
        let next = first["body"]["next_cursor"].as_str().unwrap().to_owned();
        params.cursor = Some(next);
        assert!(page_response(&params, "{}").is_err());
        params.project_dir = Some("/synthetic/other-checkout".into());
        assert!(page_response(&params, &output).is_err());
        params.project_dir = None;
        params.cursor = None;
        let mut recovered = String::new();
        loop {
            let page: serde_json::Value =
                serde_json::from_str(&page_response(&params, &output).unwrap()).unwrap();
            let envelope = json!({"content":[{"type":"text","text":page.to_string()}],"structuredContent":page});
            assert!(
                serde_json::to_vec(&envelope).unwrap().len()
                    <= bbox_corpus_core::response_page::PAGE_BUDGET_BYTES
            );
            recovered.push_str(page["body"]["text"].as_str().unwrap());
            params.cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if params.cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recovered).unwrap(),
            expected
        );
        assert_eq!(
            expected["degraded"]["unresolved_refs"][0]["ref"],
            params.refs[0]
        );
        params.body_limit = Some(0);
        assert!(validate_response_params(&params).is_err());
        params.body_limit = None;
        params.cursor = Some("bad-cursor".into());
        assert!(validate_response_params(&params).is_err());
    }

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
                ..Default::default()
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
                ..Default::default()
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
    fn raw_project_dir_never_grants_lower_layer_file_access() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret.txt");
        std::fs::write(&file, "must not be read").unwrap();
        let ctx = ProviderContext::empty_for_tests();
        let out = ref_size(
            &RefSizeParams {
                refs: vec![format!("file:{}", file.display())],
                project_dir: Some(dir.path().to_string_lossy().into_owned()),
                ..Default::default()
            },
            &ctx,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["status"], "degraded");
        assert!(
            value["degraded"]["unresolved_refs"][0]["error"]
                .as_str()
                .unwrap()
                .contains("error.checkout_access_required")
        );
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
                ..Default::default()
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
    fn caps_refs_and_reports_omitted_samples() {
        let ctx = ProviderContext::empty_for_tests();
        let refs = (0..(REF_CAP + 2))
            .map(|idx| format!("invalid-ref-{idx}"))
            .collect::<Vec<_>>();
        let out = ref_size(
            &RefSizeParams {
                refs,
                project_dir: None,
                ..Default::default()
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
