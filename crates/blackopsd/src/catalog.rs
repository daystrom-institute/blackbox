//! Adapter from the shipped/installable artifact catalog into blackopsd's
//! immutable operational definitions.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use blackops_core::{DefinitionInstallRequest, DefinitionKind, OperationalDefinition};
use jsonschema::{Draft, JSONSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::{AuthorityActor, BlackopsdError, BlackopsdResult};

include!(concat!(env!("OUT_DIR"), "/shipped_catalog.rs"));

const MAX_CATALOG_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogImportReport {
    pub shipped_atoms: usize,
    pub installed_atoms: usize,
    pub definitions: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct AtomArtifact {
    #[serde(rename = "_contract")]
    contract: String,
    kind: String,
    name: String,
    version: Value,
    #[serde(default)]
    subcontract: Option<String>,
    manifest: AtomManifest,
}

#[derive(Debug, Clone, Deserialize)]
struct AtomManifest {
    description: String,
    #[serde(default)]
    inputs: Option<AtomInputs>,
    #[serde(default)]
    outputs: Option<AtomOutputs>,
    implementation: AtomImplementation,
    #[serde(default)]
    effects: Option<Value>,
    #[serde(default)]
    composition: Option<Value>,
    #[serde(default)]
    trace: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct AtomInputs {
    #[serde(default)]
    schema: Option<Value>,
    #[serde(default)]
    prompt_template: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AtomOutputs {
    #[serde(default)]
    schema: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AtomImplementation {
    Profile { brofile_ref: String },
    Workflow { workflow_ref: String },
    Deterministic { runner: String },
    Adapter { adapter_name: String },
    Consultant { consumer: String },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedAtomDefinition {
    pub backend: AtomBackend,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    pub prompt_template: Option<String>,
    pub description: String,
    pub subcontract: Option<String>,
    pub effects: Option<Value>,
    pub composition: Option<Value>,
    pub trace: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AtomBackend {
    Deterministic {
        runner: String,
    },
    Adapter {
        adapter_name: String,
    },
    Profile {
        brofile_ref: String,
        brofile: Value,
    },
    Workflow {
        workflow_ref: String,
        workflow: Value,
    },
    Consultant {
        consumer: String,
    },
}

#[derive(Debug, Clone)]
struct CatalogDocument {
    source: String,
    value: Value,
    shipped: bool,
}

pub async fn import_catalog(
    authority: &AuthorityActor,
    installed_catalog_root: &Path,
) -> BlackopsdResult<CatalogImportReport> {
    let mut atoms = embedded_documents(SHIPPED_ATOM_SOURCES, "system-defaults/atoms")?;
    let shipped_atoms = atoms.len();
    let mut brofiles = embedded_documents(SHIPPED_BROFILE_SOURCES, "system-defaults/brofiles")?;
    let mut workflows = embedded_documents(SHIPPED_WORKFLOW_SOURCES, "system-defaults/workflows")?;

    let installed_catalog_root = installed_catalog_root.to_path_buf();
    let (installed_atoms, installed_brofiles, installed_workflows) =
        tokio::task::spawn_blocking(move || {
            Ok::<_, BlackopsdError>((
                installed_documents(&installed_catalog_root, "atom")?,
                installed_documents(&installed_catalog_root, "brofile")?,
                installed_documents(&installed_catalog_root, "workflow")?,
            ))
        })
        .await
        .map_err(|error| {
            BlackopsdError::Configuration(format!("catalog loader task failed: {error}"))
        })??;
    atoms.extend(installed_atoms);
    brofiles.extend(installed_brofiles);
    workflows.extend(installed_workflows);

    let installed_atoms = atoms.iter().filter(|document| !document.shipped).count();
    let brofiles = reference_map("brofile", brofiles)?;
    let workflows = reference_map("workflow", workflows)?;
    let mut definitions = Vec::with_capacity(atoms.len());
    for document in atoms {
        definitions.push(to_definition(document, &brofiles, &workflows)?);
    }
    definitions.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.version.cmp(&right.version))
    });
    definitions.dedup_by(|left, right| {
        left.name == right.name
            && left.version == right.version
            && left.input_contract == right.input_contract
            && left.body == right.body
    });

    for definition in definitions.iter().cloned() {
        authority
            .call(move |authority| authority.install_definition(definition))
            .await?;
    }

    Ok(CatalogImportReport {
        shipped_atoms,
        installed_atoms,
        definitions: definitions.len(),
    })
}

pub(crate) fn resolve_atom_definition(
    definition: &OperationalDefinition,
) -> Result<ResolvedAtomDefinition, String> {
    let artifact_value = definition
        .body
        .get("artifact")
        .ok_or_else(|| "atom definition is missing its artifact envelope".to_string())?;
    let artifact: AtomArtifact = serde_json::from_value(artifact_value.clone())
        .map_err(|error| format!("atom artifact is invalid: {error}"))?;
    validate_artifact(&artifact)?;
    let backend = match &artifact.manifest.implementation {
        AtomImplementation::Deterministic { runner } => AtomBackend::Deterministic {
            runner: runner.clone(),
        },
        AtomImplementation::Adapter { adapter_name } => AtomBackend::Adapter {
            adapter_name: adapter_name.clone(),
        },
        AtomImplementation::Profile { brofile_ref } => AtomBackend::Profile {
            brofile_ref: brofile_ref.clone(),
            brofile: definition
                .body
                .get("resolved_brofile")
                .cloned()
                .ok_or_else(|| format!("profile atom cannot resolve {brofile_ref}"))?,
        },
        AtomImplementation::Workflow { workflow_ref } => AtomBackend::Workflow {
            workflow_ref: workflow_ref.clone(),
            workflow: definition
                .body
                .get("resolved_workflow")
                .cloned()
                .ok_or_else(|| format!("workflow atom cannot resolve {workflow_ref}"))?,
        },
        AtomImplementation::Consultant { consumer } => AtomBackend::Consultant {
            consumer: consumer.clone(),
        },
    };
    Ok(ResolvedAtomDefinition {
        backend,
        input_schema: artifact
            .manifest
            .inputs
            .as_ref()
            .and_then(|inputs| inputs.schema.clone()),
        output_schema: artifact
            .manifest
            .outputs
            .as_ref()
            .and_then(|outputs| outputs.schema.clone()),
        prompt_template: artifact
            .manifest
            .inputs
            .and_then(|inputs| inputs.prompt_template),
        description: artifact.manifest.description,
        subcontract: artifact.subcontract,
        effects: artifact.manifest.effects,
        composition: artifact.manifest.composition,
        trace: artifact.manifest.trace,
    })
}

pub(crate) fn validate_value(schema: Option<&Value>, value: &Value) -> Result<(), String> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let compiled = JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .compile(schema)
        .map_err(|error| format!("catalog JSON Schema is invalid: {error}"))?;
    if let Err(errors) = compiled.validate(value) {
        let messages = errors
            .take(16)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        return Err(format!(
            "JSON Schema validation failed: {}",
            messages.join("; ")
        ));
    }
    Ok(())
}

fn embedded_documents(
    sources: &[(&str, &str)],
    prefix: &str,
) -> BlackopsdResult<Vec<CatalogDocument>> {
    sources
        .iter()
        .map(|(relative, raw)| {
            let value = serde_json::from_str(raw).map_err(|error| {
                BlackopsdError::InvalidRequest(format!(
                    "embedded catalog source {prefix}/{relative} is invalid: {error}"
                ))
            })?;
            Ok(CatalogDocument {
                source: format!("{prefix}/{relative}"),
                value,
                shipped: true,
            })
        })
        .collect()
}

// This synchronous catalog snapshot runs only on import_catalog's blocking lane
// or in isolated tests, never on a Tokio runtime worker.
#[allow(clippy::disallowed_methods)]
fn installed_documents(root: &Path, kind: &str) -> BlackopsdResult<Vec<CatalogDocument>> {
    let directory = root.join(kind);
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut documents = Vec::new();
    for entry in WalkDir::new(&directory)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !entry.file_type().is_file()
            || path.extension().is_none_or(|extension| extension != "json")
            || path
                .components()
                .any(|component| component.as_os_str() == ".versions")
            || path.file_name().is_some_and(|name| name == "metadata.json")
        {
            continue;
        }
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || metadata.len() > MAX_CATALOG_DOCUMENT_BYTES {
            return Err(BlackopsdError::InvalidRequest(format!(
                "installed catalog document {} is not a bounded regular file",
                path.display()
            )));
        }
        let raw = fs::read_to_string(path)?;
        let value = serde_json::from_str(&raw).map_err(|error| {
            BlackopsdError::InvalidRequest(format!(
                "installed catalog document {} is invalid: {error}",
                path.display()
            ))
        })?;
        documents.push(CatalogDocument {
            source: path.to_string_lossy().into_owned(),
            value,
            shipped: false,
        });
    }
    documents.sort_by(|left, right| left.source.cmp(&right.source));
    Ok(documents)
}

fn reference_map(
    kind: &str,
    documents: Vec<CatalogDocument>,
) -> BlackopsdResult<BTreeMap<String, Value>> {
    let mut values = BTreeMap::new();
    for document in documents {
        let name = required_string(&document.value, "name", &document.source)?;
        let version = artifact_version(
            document.value.get("version").ok_or_else(|| {
                BlackopsdError::InvalidRequest(format!(
                    "catalog source {} has no version",
                    document.source
                ))
            })?,
            &document.source,
        )?;
        let reference = format!("{kind}:{name}@{version}");
        if let Some(previous) = values.insert(reference.clone(), document.value.clone())
            && previous != document.value
        {
            return Err(BlackopsdError::InvalidRequest(format!(
                "catalog reference {reference} has conflicting immutable definitions"
            )));
        }
    }
    Ok(values)
}

fn to_definition(
    document: CatalogDocument,
    brofiles: &BTreeMap<String, Value>,
    workflows: &BTreeMap<String, Value>,
) -> BlackopsdResult<DefinitionInstallRequest> {
    let artifact: AtomArtifact =
        serde_json::from_value(document.value.clone()).map_err(|error| {
            BlackopsdError::InvalidRequest(format!(
                "atom catalog source {} is invalid: {error}",
                document.source
            ))
        })?;
    validate_artifact(&artifact).map_err(|message| {
        BlackopsdError::InvalidRequest(format!("{}: {message}", document.source))
    })?;
    let version = artifact_version(&artifact.version, &document.source)?;
    let input_contract = artifact
        .manifest
        .inputs
        .as_ref()
        .and_then(|inputs| inputs.schema.clone())
        .unwrap_or_else(|| json!({"type": "object"}));
    validate_schema(&input_contract, "input", &document.source)?;
    if let Some(output) = artifact
        .manifest
        .outputs
        .as_ref()
        .and_then(|outputs| outputs.schema.as_ref())
    {
        validate_schema(output, "output", &document.source)?;
    }

    let mut body = serde_json::Map::from_iter([("artifact".into(), document.value)]);
    match &artifact.manifest.implementation {
        AtomImplementation::Profile { brofile_ref } => {
            let brofile = lookup_reference(brofiles, brofile_ref).ok_or_else(|| {
                BlackopsdError::InvalidRequest(format!(
                    "{} references missing {brofile_ref}",
                    document.source
                ))
            })?;
            body.insert("resolved_brofile".into(), brofile.clone());
        }
        AtomImplementation::Workflow { workflow_ref } => {
            let workflow = lookup_reference(workflows, workflow_ref).ok_or_else(|| {
                BlackopsdError::InvalidRequest(format!(
                    "{} references missing {workflow_ref}",
                    document.source
                ))
            })?;
            body.insert("resolved_workflow".into(), workflow.clone());
        }
        AtomImplementation::Deterministic { .. }
        | AtomImplementation::Adapter { .. }
        | AtomImplementation::Consultant { .. } => {}
    }
    body.insert("catalog_source".into(), Value::String(document.source));
    Ok(DefinitionInstallRequest {
        kind: DefinitionKind::Atom,
        name: artifact.name,
        version,
        input_contract,
        body: Value::Object(body),
        activate: true,
        created_at_unix_ms: 0,
    })
}

fn lookup_reference<'a>(values: &'a BTreeMap<String, Value>, reference: &str) -> Option<&'a Value> {
    values.get(reference).or_else(|| {
        let (name, _) = reference.rsplit_once('@')?;
        let prefix = format!("{name}@v");
        values
            .iter()
            .filter(|(candidate, _)| candidate.starts_with(&prefix))
            .filter_map(|(candidate, value)| {
                candidate
                    .strip_prefix(&prefix)
                    .and_then(|version| version.parse::<u64>().ok())
                    .map(|version| (version, value))
            })
            .max_by_key(|(version, _)| *version)
            .map(|(_, value)| value)
    })
}

fn validate_artifact(artifact: &AtomArtifact) -> Result<(), String> {
    if artifact.contract != "atom/v1" || artifact.kind != "atom" {
        return Err("expected an atom/v1 artifact envelope".into());
    }
    if artifact.name.is_empty()
        || artifact.name.len() > 128
        || !artifact
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("atom name must be a bounded catalog identifier".into());
    }
    if artifact.manifest.description.trim().is_empty() {
        return Err("atom description must not be empty".into());
    }
    match &artifact.manifest.implementation {
        AtomImplementation::Profile { brofile_ref } if !valid_typed_ref(brofile_ref, "brofile") => {
            Err("profile atom has an invalid brofile_ref".into())
        }
        AtomImplementation::Workflow { workflow_ref }
            if !valid_typed_ref(workflow_ref, "workflow") =>
        {
            Err("workflow atom has an invalid workflow_ref".into())
        }
        AtomImplementation::Deterministic { runner } if runner.trim().is_empty() => {
            Err("deterministic atom has no runner".into())
        }
        AtomImplementation::Adapter { adapter_name } if adapter_name.trim().is_empty() => {
            Err("adapter atom has no adapter_name".into())
        }
        AtomImplementation::Consultant { consumer } if consumer.trim().is_empty() => {
            Err("consultant atom has no consumer".into())
        }
        _ => Ok(()),
    }
}

fn valid_typed_ref(reference: &str, kind: &str) -> bool {
    reference
        .strip_prefix(&format!("{kind}:"))
        .and_then(|rest| rest.rsplit_once('@'))
        .is_some_and(|(name, version)| !name.is_empty() && version.starts_with('v'))
}

fn artifact_version(value: &Value, source: &str) -> BlackopsdResult<String> {
    let version = match value {
        Value::Number(number) => number.as_u64().filter(|version| *version > 0),
        Value::String(text) => text
            .strip_prefix('v')
            .unwrap_or(text)
            .parse::<u64>()
            .ok()
            .filter(|version| *version > 0),
        _ => None,
    }
    .ok_or_else(|| {
        BlackopsdError::InvalidRequest(format!(
            "catalog source {source} requires a positive integer version"
        ))
    })?;
    Ok(format!("v{version}"))
}

fn required_string(value: &Value, field: &str, source: &str) -> BlackopsdResult<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            BlackopsdError::InvalidRequest(format!(
                "catalog source {source} requires string field {field}"
            ))
        })
}

fn validate_schema(schema: &Value, lane: &str, source: &str) -> BlackopsdResult<()> {
    JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .compile(schema)
        .map(|_| ())
        .map_err(|error| {
            BlackopsdError::InvalidRequest(format!(
                "catalog source {source} has an invalid {lane} schema: {error}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_contains_exact_echo_backend_and_profiles() {
        let atoms = embedded_documents(SHIPPED_ATOM_SOURCES, "system-defaults/atoms").unwrap();
        let brofiles = reference_map(
            "brofile",
            embedded_documents(SHIPPED_BROFILE_SOURCES, "system-defaults/brofiles").unwrap(),
        )
        .unwrap();
        let workflows = reference_map(
            "workflow",
            embedded_documents(SHIPPED_WORKFLOW_SOURCES, "system-defaults/workflows").unwrap(),
        )
        .unwrap();
        let echo = atoms
            .into_iter()
            .find(|document| document.value["name"] == "echo")
            .unwrap();
        let request = to_definition(echo, &brofiles, &workflows).unwrap();
        assert_eq!(request.version, "v1");
        let definition = OperationalDefinition {
            key: blackops_core::DefinitionKey {
                kind: DefinitionKind::Atom,
                name: request.name,
                version: request.version,
            },
            input_contract: request.input_contract,
            body: request.body,
            content_digest: "test".into(),
            created_at_unix_ms: 0,
        };
        assert_eq!(
            resolve_atom_definition(&definition).unwrap().backend,
            AtomBackend::Deterministic {
                runner: "echo".into()
            }
        );
        assert!(SHIPPED_ATOM_SOURCES.len() > 100);
    }

    #[test]
    // The fixture intentionally builds a temporary installed-catalog tree.
    #[allow(clippy::disallowed_methods)]
    fn installed_catalog_rejects_symlinks_and_oversize_documents() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let atom_dir = root.join("atom");
        fs::create_dir_all(&atom_dir).unwrap();
        let target = root.join("target.json");
        fs::write(&target, "{}").unwrap();
        let oversized = atom_dir.join("oversized.json");
        let file = fs::File::create(&oversized).unwrap();
        file.set_len(MAX_CATALOG_DOCUMENT_BYTES + 1).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, atom_dir.join("bad.json")).unwrap();
        }
        let error = installed_documents(&root, "atom").unwrap_err();
        assert!(
            matches!(error, BlackopsdError::InvalidRequest(ref detail) if detail.contains("oversized.json") && detail.contains("bounded regular file")),
            "catalog import must fail closed on an oversized installed document: {error}"
        );
    }
}
