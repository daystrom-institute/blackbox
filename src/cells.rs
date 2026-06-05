use anyhow::{Result, anyhow, bail};
use bro_capabilities::{CellLoadOutput, CellLoadRequest, CellRegisterOutput, CellRegisterRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::artifacts::{ArtifactCatalog, ArtifactKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CellArtifact {
    pub kind: String,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub handle: String,
    pub source: String,
    pub contract: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
}

pub(crate) fn register_cell(
    catalog: &ArtifactCatalog,
    request: CellRegisterRequest,
) -> Result<CellRegisterOutput> {
    let handle = cell_handle(&request.name, &request.version);
    let artifact = CellArtifact {
        kind: "cell".to_string(),
        name: request.name.clone(),
        version: request.version.clone(),
        description: request.description,
        handle: handle.clone(),
        source: request.source,
        contract: request.contract_json,
        supersedes: request.supersedes.clone(),
    };
    let value = serde_json::to_value(&artifact)?;
    let meta = catalog.install_value(
        ArtifactKind::Cell,
        "narf_register".to_string(),
        &value,
        Some(request.name),
        Some(request.version),
        request.supersedes,
    )?;
    Ok(CellRegisterOutput {
        handle,
        artifact_ref: format!("cell:{}@{}", meta.name, meta.version),
        name: meta.name,
        version: meta.version,
    })
}

pub(crate) fn load_cell(
    catalog: &ArtifactCatalog,
    request: CellLoadRequest,
) -> Result<CellLoadOutput> {
    let parsed = parse_cell_handle(&request.handle)?;
    let value = match parsed.version.as_deref() {
        Some(version) => {
            catalog.load_artifact_value_version(ArtifactKind::Cell, &parsed.name, version)?
        }
        None => catalog.load_artifact_value(ArtifactKind::Cell, &parsed.name)?,
    }
    .ok_or_else(|| anyhow!("registered cell `{}` not found", request.handle))?;
    let artifact: CellArtifact = serde_json::from_value(value)?;
    if artifact.kind != "cell" {
        bail!("artifact `{}` is not a cell", request.handle);
    }
    Ok(CellLoadOutput {
        handle: cell_handle(&artifact.name, &artifact.version),
        artifact_ref: format!("cell:{}@{}", artifact.name, artifact.version),
        name: artifact.name,
        version: artifact.version,
        source: artifact.source,
        contract_json: artifact.contract,
    })
}

fn cell_handle(name: &str, version: &str) -> String {
    format!("atom:{name}@{version}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedCellHandle {
    name: String,
    version: Option<String>,
}

fn parse_cell_handle(handle: &str) -> Result<ParsedCellHandle> {
    let rest = handle
        .strip_prefix("atom:")
        .or_else(|| handle.strip_prefix("cell:"))
        .ok_or_else(|| anyhow!("cell handle must start with `atom:` or `cell:`"))?;
    let (name, version) = match rest.rsplit_once('@') {
        Some((name, version)) if !name.trim().is_empty() && !version.trim().is_empty() => {
            (name.to_string(), Some(version.to_string()))
        }
        None if !rest.trim().is_empty() => (rest.to_string(), None),
        _ => bail!("invalid cell handle `{handle}`"),
    };
    Ok(ParsedCellHandle { name, version })
}
