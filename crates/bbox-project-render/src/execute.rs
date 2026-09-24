//! Checkout IO for project renders.
//!
//! One render applies a fixed output set inside one already verified
//! checkout root: provider entrypoints named by [`project_target_file`] and
//! content-addressed satellites under `.bbox/guidance/`. Every target is
//! preflighted before anything is written: symlinked parents or targets and
//! special files refuse the whole render, a provider file without the
//! generated marker is preserved, satellites publish before entrypoints, and
//! each entrypoint stages in a unique sibling that is renamed into place only
//! if the target still holds the bytes preflight observed. Renders of one
//! checkout are serialized by an advisory lock on its root directory, which
//! the bound harness and the checkout-owner collector both take.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_util::guidance::GuidanceFile;
use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};

use crate::projection::{GENERATED_MARKER, project_doc_nonempty, project_target_file};
use crate::transport::{
    ExpectedRenderAuthority, PROJECT_RENDER_TRANSPORT_VERSION, ProjectRenderDispositionV1,
    ProjectRenderExecutionV1, ProjectRenderPlanV1, ProjectRenderReceiptV1,
};

/// Default bound on waiting for another render of the same checkout.
pub const DEFAULT_RENDER_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const RENDER_LOCK_POLL: Duration = Duration::from_millis(10);

/// Holds the exclusive render lock of one checkout root until dropped.
#[derive(Debug)]
pub struct CheckoutRenderLock {
    _root: File,
}

/// Serialize project renders of one checkout. The lock is an advisory
/// `flock` on the root directory itself, so it creates no file in the
/// checkout and every applier on the host contends on the same inode.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
pub fn lock_checkout_for_render(root: &Path, timeout: Duration) -> Result<CheckoutRenderLock> {
    let file = File::open(root).with_context(|| format!("opening {}", root.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(CheckoutRenderLock { _root: file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!(
                        "error.render_busy: another project render is applying to this checkout; retry after it finishes"
                    );
                }
                std::thread::sleep(RENDER_LOCK_POLL);
            }
            Err(error) => {
                return Err(error).context("locking the checkout for project render");
            }
        }
    }
}

/// One provider entrypoint a render may write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntrypoint {
    pub provider: String,
    /// `None` when the provider has no body; the file is left untouched.
    pub content: Option<String>,
}

/// What happened to each output of one application, in input order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectApplyOutcome {
    pub satellites: Vec<ProjectRenderDispositionV1>,
    pub entrypoints: Vec<ProjectRenderDispositionV1>,
    /// Bounded, path-free failure detail for partial outcomes.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetState {
    Absent,
    Generated([u8; 32]),
    Handwritten,
}

#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
fn observe_target(target: &Path) -> Result<TargetState> {
    match fs::symlink_metadata(target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TargetState::Absent),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", target.display())),
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "error.render_target_unsafe: project render target is a symlink: {}",
            target.display()
        ),
        Ok(metadata) if !metadata.is_file() => bail!(
            "error.render_target_unsafe: project render target is not a regular file: {}",
            target.display()
        ),
        Ok(_) => {
            let bytes =
                fs::read(target).with_context(|| format!("reading {}", target.display()))?;
            if String::from_utf8_lossy(&bytes).contains(GENERATED_MARKER) {
                Ok(TargetState::Generated(Sha256::digest(&bytes).into()))
            } else {
                Ok(TargetState::Handwritten)
            }
        }
    }
}

/// Reject a root whose own path traverses a symlink or is not a directory.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
pub fn verify_render_root(root: &Path) -> Result<PathBuf> {
    let canonical = root
        .canonicalize()
        .context("canonicalizing project render root")?;
    if canonical != root || !canonical.is_dir() {
        bail!("project render root is not the stable bound directory");
    }
    Ok(canonical)
}

/// Apply one project render's fixed outputs inside `root`.
///
/// Preflight failures (unsafe targets, differing immutable satellites) return
/// an error before any byte is written. Publication failures after preflight
/// are reported per output so the caller can describe a partial render
/// truthfully.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
pub fn apply_project_render(
    root: &Path,
    satellites: &[GuidanceFile],
    entrypoints: &[ProjectEntrypoint],
    dry_run: bool,
) -> Result<ProjectApplyOutcome> {
    let bbox_root = root.join(".bbox");
    bbox_util::guidance::preflight_files(&bbox_root, satellites)?;
    let mut observed = Vec::with_capacity(entrypoints.len());
    for entrypoint in entrypoints {
        let target = root.join(project_target_file(&entrypoint.provider)?);
        observed.push(match entrypoint.content {
            Some(_) => Some(observe_target(&target)?),
            None => None,
        });
    }

    let nominal = if dry_run {
        ProjectRenderDispositionV1::DryRun
    } else {
        ProjectRenderDispositionV1::Written
    };
    let mut outcome = ProjectApplyOutcome {
        satellites: vec![nominal; satellites.len()],
        entrypoints: Vec::with_capacity(entrypoints.len()),
        errors: Vec::new(),
    };
    if dry_run {
        for state in &observed {
            outcome.entrypoints.push(match state {
                None => ProjectRenderDispositionV1::Skipped,
                Some(TargetState::Handwritten) => ProjectRenderDispositionV1::DryRunRefused,
                Some(_) => ProjectRenderDispositionV1::DryRun,
            });
        }
        return Ok(outcome);
    }

    if let Err(error) = bbox_util::guidance::publish_files(&bbox_root, satellites, false) {
        outcome.satellites = vec![ProjectRenderDispositionV1::Failed; satellites.len()];
        outcome
            .errors
            .push(format!("guidance satellites were not published: {error:#}"));
        for state in &observed {
            outcome.entrypoints.push(match state {
                None => ProjectRenderDispositionV1::Skipped,
                Some(TargetState::Handwritten) => ProjectRenderDispositionV1::Refused,
                Some(_) => ProjectRenderDispositionV1::Failed,
            });
        }
        return Ok(outcome);
    }

    let mut published = false;
    for (entrypoint, state) in entrypoints.iter().zip(observed) {
        let (Some(content), Some(state)) = (entrypoint.content.as_deref(), state) else {
            outcome
                .entrypoints
                .push(ProjectRenderDispositionV1::Skipped);
            continue;
        };
        if state == TargetState::Handwritten {
            outcome
                .entrypoints
                .push(ProjectRenderDispositionV1::Refused);
            continue;
        }
        let file_name = project_target_file(&entrypoint.provider)?;
        match publish_entrypoint(root, file_name, content, &state) {
            Ok(disposition) => {
                published |= disposition == ProjectRenderDispositionV1::Written;
                outcome.entrypoints.push(disposition);
            }
            Err(error) => {
                outcome.errors.push(format!("{file_name}: {error:#}"));
                outcome.entrypoints.push(ProjectRenderDispositionV1::Failed);
            }
        }
    }
    if published {
        File::open(root)
            .and_then(|directory| directory.sync_all())
            .context("syncing the project render root")?;
    }
    Ok(outcome)
}

#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
fn publish_entrypoint(
    root: &Path,
    file_name: &str,
    content: &str,
    observed: &TargetState,
) -> Result<ProjectRenderDispositionV1> {
    #[cfg(test)]
    if tests::fail_publication_of(file_name) {
        bail!("injected publication failure");
    }
    let target = root.join(file_name);
    if let TargetState::Generated(digest) = observed
        && digest.as_slice() == Sha256::digest(content.as_bytes()).as_slice()
    {
        // Already at the projected bytes; nothing to publish.
        return Ok(ProjectRenderDispositionV1::Written);
    }
    let mut staged = tempfile::Builder::new()
        .prefix(&format!(".{file_name}."))
        .suffix(".render-tmp")
        .tempfile_in(root)
        .context("staging the project render output")?;
    staged.write_all(content.as_bytes())?;
    staged.as_file().sync_all()?;
    // The owner may have edited the file after preflight. Publication
    // proceeds only over the exact state preflight approved.
    if observe_target(&target)? != *observed {
        return Ok(ProjectRenderDispositionV1::Conflict);
    }
    match observed {
        TargetState::Absent => match staged.persist_noclobber(&target) {
            Ok(_) => Ok(ProjectRenderDispositionV1::Written),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                Ok(ProjectRenderDispositionV1::Conflict)
            }
            Err(error) => Err(error.error).context("publishing the project render output"),
        },
        TargetState::Generated(_) => {
            staged
                .persist(&target)
                .map_err(|error| error.error)
                .context("publishing the project render output")?;
            Ok(ProjectRenderDispositionV1::Written)
        }
        TargetState::Handwritten => Ok(ProjectRenderDispositionV1::Refused),
    }
}

/// Execute an authorized workspace-bound project render inside the checkout
/// owner's already verified root.
pub fn execute_project_render_plan(
    plan: &ProjectRenderPlanV1,
    project_root: &Path,
    expected_scope: &PublishedScope,
    expected_workspace_id: &str,
) -> Result<ProjectRenderExecutionV1> {
    execute_project_render_plan_as(
        plan,
        project_root,
        expected_scope,
        ExpectedRenderAuthority::Workspace {
            workspace_id: expected_workspace_id,
        },
        DEFAULT_RENDER_LOCK_TIMEOUT,
    )
}

/// Execute an authorized project render inside the checkout owner's already
/// verified root. The renderer never receives a daemon path. Its destinations
/// are fixed provider filenames and content-addressed satellites derived from
/// validated source entries within `project_root`.
pub fn execute_project_render_plan_as(
    plan: &ProjectRenderPlanV1,
    project_root: &Path,
    expected_scope: &PublishedScope,
    expected: ExpectedRenderAuthority<'_>,
    lock_timeout: Duration,
) -> Result<ProjectRenderExecutionV1> {
    plan.validate_expected_authority(expected_scope, expected)?;
    let root = verify_render_root(project_root)?;
    let _lock = lock_checkout_for_render(&root, lock_timeout)?;
    let project_doc_nonempty = project_doc_nonempty(&root);
    let outputs = plan.expected_outputs(project_doc_nonempty)?;
    let entrypoint_count = outputs
        .iter()
        .take_while(|(receipt, _)| !receipt.file_name.starts_with(".bbox/guidance/"))
        .count();
    let entrypoints = outputs[..entrypoint_count]
        .iter()
        .map(|(receipt, content)| ProjectEntrypoint {
            provider: receipt.provider.clone(),
            content: content.clone(),
        })
        .collect::<Vec<_>>();
    let satellites = outputs[entrypoint_count..]
        .iter()
        .map(|(receipt, content)| GuidanceFile {
            path: receipt
                .file_name
                .strip_prefix(".bbox/")
                .expect("satellite receipts are rooted in .bbox")
                .to_string(),
            body: content.clone().expect("satellites always carry a body"),
        })
        .collect::<Vec<_>>();
    let applied = apply_project_render(&root, &satellites, &entrypoints, plan.dry_run)?;

    let (mut projections, contents): (Vec<_>, Vec<_>) = outputs.into_iter().unzip();
    for (projection, disposition) in projections
        .iter_mut()
        .zip(applied.entrypoints.iter().chain(&applied.satellites))
    {
        projection.disposition = *disposition;
    }
    let receipt = ProjectRenderReceiptV1 {
        version: PROJECT_RENDER_TRANSPORT_VERSION,
        project_id: plan.project_id.clone(),
        scope: plan.scope.clone(),
        workspace_id: plan.workspace_id.clone(),
        producer: plan.producer.clone(),
        project_doc_nonempty,
        projections,
    };
    receipt.validate_against(plan)?;
    let output = render_output_text(&root, &receipt, &contents, &applied.errors, plan.dry_run);
    Ok(ProjectRenderExecutionV1 { output, receipt })
}

/// Human-readable local summary of one execution. It names local paths and
/// is shown only on the host that applied the render; receipts stay
/// path-free.
fn render_output_text(
    root: &Path,
    receipt: &ProjectRenderReceiptV1,
    contents: &[Option<String>],
    errors: &[String],
    dry_run: bool,
) -> String {
    let prefix = if dry_run { "[DRY-RUN] " } else { "" };
    let mut satellites = Vec::new();
    let mut entrypoints = Vec::new();
    for (projection, content) in receipt.projections.iter().zip(contents) {
        let path = root.join(&projection.file_name);
        let bytes = projection.projection_bytes.unwrap_or_default();
        if projection.file_name.starts_with(".bbox/") {
            satellites.push(format!(
                "{prefix}SATELLITE {} ({bytes} bytes){}",
                projection.file_name,
                match projection.disposition {
                    ProjectRenderDispositionV1::Failed => " FAILED",
                    _ => "",
                }
            ));
            continue;
        }
        let content = content.as_deref().unwrap_or_default();
        entrypoints.push(match projection.disposition {
            ProjectRenderDispositionV1::Skipped => format!(
                "Skipped {} (no project-scope entries and no PROJECT.md include)",
                path.display()
            ),
            ProjectRenderDispositionV1::DryRun => format!(
                "[DRY-RUN] PROJECT {} ({bytes} chars)\n{content}",
                path.display()
            ),
            ProjectRenderDispositionV1::DryRunRefused => format!(
                "[DRY-RUN] PROJECT REFUSED {} ({bytes} chars)\n{content}",
                path.display()
            ),
            ProjectRenderDispositionV1::Written => {
                format!("Wrote project {} ({bytes} chars)", path.display())
            }
            ProjectRenderDispositionV1::Refused => format!(
                "Refused project {}: existing file is not blackbox-generated; preserve hand-authored content in PROJECT.md on the checkout owner before requesting a managed render",
                path.display()
            ),
            ProjectRenderDispositionV1::Conflict => format!(
                "Conflict on project {}: the file changed while the render was applying; its current bytes were preserved",
                path.display()
            ),
            ProjectRenderDispositionV1::Failed => {
                format!("Failed to write project {}", path.display())
            }
        });
    }
    let mut lines = satellites;
    lines.extend(entrypoints);
    for error in errors {
        lines.push(format!("Partial render: {error}"));
    }
    lines.join("\n\n")
}

#[cfg(test)]
#[path = "execute_tests.rs"]
mod tests;
