//! Test-only installation of accepted publications.
//!
//! Production never uses this module. Installing accepted bytes is the
//! catalog transaction owner's job during migration, and the publisher
//! establish/advance path's job afterwards. Crate-external tests still need
//! catalog state to read from, and `#[cfg(test)]` does not cross a crate
//! boundary, so the `test-support` feature exposes exactly two doors: build
//! and install one generation through the real preparation path, and damage
//! one installed generation file.
//!
//! Everything here goes through `prepare_accepted_publication_v1`, so a
//! fixture cannot contain hashes, ids, or manifests that production would
//! reject.

use std::fs;
use std::path::Path;

use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{AttachmentId, ProjectId};

use crate::accepted_publication_store::{
    AcceptedGapSourceV1, AcceptedKnowledgeSourceV1, AcceptedPublicationBuildInputV1,
    AcceptedPublicationBuildSourceV1, AcceptedPublicationGenerationId, AcceptedPublicationLimits,
    AcceptedPublicationPriorPointerV1, AcceptedPublicationStorePaths, FullPublisherRef,
    GitObjectId, acquire_accepted_publication_lock, decode_pointer_v1,
    prepare_accepted_publication_v1, rebind_pointer_attachment_locked,
};

/// One committed source file, byte-exact, as the publisher would have read
/// it at the accepted commit.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct AcceptedPublicationSourceFileForTest {
    pub repository_relative_filename: String,
    pub source_bytes: Vec<u8>,
}

/// The identities an installed fixture exposes so a test can assert on
/// stamps or damage a specific generation.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct InstalledAcceptedPublicationForTest {
    pub generation_id: String,
    pub generation_hash: String,
    pub pointer_sha256: String,
}

/// Install one accepted publication for `project_id`.
///
/// Any pointer already installed becomes the prior arm, which is how a
/// fixture reaches the advance and prior-fallback states.
#[doc(hidden)]
#[allow(clippy::too_many_arguments)] // every argument is one durable pointer field
pub fn install_accepted_publication_for_test(
    projects_path: &Path,
    project_id: &ProjectId,
    attachment_id: &AttachmentId,
    scope: &PublishedScope,
    full_ref: &str,
    accepted_commit: &str,
    knowledge: Vec<AcceptedPublicationSourceFileForTest>,
    gaps: Vec<AcceptedPublicationSourceFileForTest>,
) -> anyhow::Result<InstalledAcceptedPublicationForTest> {
    let paths = AcceptedPublicationStorePaths::derive(projects_path)?;
    let limits = AcceptedPublicationLimits::default();
    let prior_pointer = match fs::read(paths.pointer(project_id)) {
        Ok(bytes) => {
            let pointer = decode_pointer_v1(&bytes, &limits)?;
            Some(AcceptedPublicationPriorPointerV1 {
                attachment_id: pointer.attachment_id,
                source_binding: pointer.source_binding,
                full_ref: pointer.full_ref,
                accepted_commit: pointer.accepted_commit,
                accepted_scope: pointer.accepted_scope,
                accepted_generation: pointer.accepted_generation,
                generation_hash: pointer.generation_hash,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let prepared = prepare_accepted_publication_v1(
        AcceptedPublicationBuildInputV1 {
            project_id: project_id.clone(),
            source_binding: AcceptedPublicationBuildSourceV1::Attachment(attachment_id.clone()),
            scope: scope.clone(),
            full_ref: FullPublisherRef::parse(full_ref)?,
            accepted_commit: GitObjectId::parse(accepted_commit)?,
            knowledge: knowledge
                .into_iter()
                .map(|file| AcceptedKnowledgeSourceV1 {
                    repository_relative_filename: file.repository_relative_filename,
                    source_bytes: file.source_bytes,
                })
                .collect(),
            gaps: gaps
                .into_iter()
                .map(|file| AcceptedGapSourceV1 {
                    repository_relative_filename: file.repository_relative_filename,
                    source_bytes: file.source_bytes,
                })
                .collect(),
            prior_pointer,
        },
        &limits,
    )?;
    fs::create_dir_all(paths.pointers())?;
    fs::create_dir_all(paths.generations().join(project_id.as_str()))?;
    // Generation before pointer, exactly like the durable install order: a
    // pointer must never name bytes that are not on disk yet.
    fs::write(
        paths.generation(project_id, &prepared.generation_id),
        prepared.generation_bytes.as_slice(),
    )?;
    fs::write(paths.pointer(project_id), prepared.pointer_bytes.as_slice())?;
    Ok(InstalledAcceptedPublicationForTest {
        generation_id: prepared.generation_id.as_str().to_string(),
        generation_hash: prepared.generation_hash.as_str().to_string(),
        pointer_sha256: prepared.pointer_hash.as_str().to_string(),
    })
}

/// Rebind one installed pointer to another attachment, changing binding
/// identity while leaving accepted content byte-identical. This is the real
/// attachment-only rebind, not a fixture shortcut.
#[doc(hidden)]
pub fn rebind_accepted_pointer_for_test(
    projects_path: &Path,
    project_id: &ProjectId,
    new_attachment: &AttachmentId,
) -> anyhow::Result<()> {
    let paths = AcceptedPublicationStorePaths::derive(projects_path)?;
    let limits = AcceptedPublicationLimits::default();
    let guard = acquire_accepted_publication_lock(&paths)?;
    rebind_pointer_attachment_locked(&paths, &guard, project_id, new_attachment, None, &limits)?;
    Ok(())
}

/// Overwrite one installed generation file so it no longer verifies against
/// the pointer that names it.
#[doc(hidden)]
pub fn corrupt_accepted_generation_for_test(
    projects_path: &Path,
    project_id: &ProjectId,
    generation_id: &str,
) -> anyhow::Result<()> {
    let paths = AcceptedPublicationStorePaths::derive(projects_path)?;
    let generation_id = AcceptedPublicationGenerationId::parse(generation_id)?;
    fs::write(paths.generation(project_id, &generation_id), b"corrupt")?;
    Ok(())
}
