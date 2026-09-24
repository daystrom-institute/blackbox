//! Checkout IO for project renders.
//!
//! One render applies a fixed output set inside one already verified
//! checkout root: provider entrypoints named by [`project_target_file`] and
//! content-addressed satellites under `.bbox/guidance/`.
//!
//! - Every target is preflighted before anything is written: symlinked
//!   parents or targets and special files refuse the whole render, and a
//!   provider file without the generated marker is preserved.
//! - Renders of one checkout are serialized by an advisory lock on its root
//!   directory, which the bound harness, the checkout-owner collector, and the
//!   daemon compatibility adapter all take.
//! - Under that lock a freshness fence refuses a plan issued before the plan
//!   that last wrote this checkout, so a delayed applier never replaces newer
//!   output with an older projection.
//! - Satellites publish before entrypoints. An entrypoint is staged in a unique
//!   sibling; the current target is moved aside and published over only if the
//!   moved bytes are exactly the bytes preflight observed. Otherwise the
//!   owner's bytes are restored (or retained beside the target) and the output
//!   is reported as a conflict.
//! - A failure after any output was published is reported in the receipt as
//!   an incomplete render, never as a render that wrote nothing.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_util::guidance::GuidanceFile;
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::projection::{GENERATED_MARKER, project_doc_nonempty, project_target_file};
use crate::transport::{
    ExpectedRenderAuthority, PROJECT_RENDER_TRANSPORT_VERSION, ProjectRenderDispositionV1,
    ProjectRenderExecutionV1, ProjectRenderPlanV1, ProjectRenderReceiptV1,
};

/// Default bound on waiting for another render of the same checkout.
pub const DEFAULT_RENDER_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const RENDER_LOCK_POLL: Duration = Duration::from_millis(10);
const FENCE_DIR: &str = ".bbox/local";
const FENCE_FILE: &str = "render-fence.json";
const FENCE_VERSION: u32 = 1;
const MAX_FENCE_BYTES: u64 = 4096;

/// Current Unix time in milliseconds. Daemons stamp plans with it; every
/// fence comparison uses daemon-issued values only.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

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
    /// Local failure detail for partial or incomplete outcomes.
    pub errors: Vec<String>,
    /// An effect may have occurred but the application could not confirm
    /// its completion (for example the directory sync failed after a
    /// rename).
    pub incomplete: bool,
}

/// The state preflight observed for one output, recorded before any write
/// so an interrupted application can be reconciled instead of repeated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservedOutputState {
    Absent,
    Generated { sha256: String },
    Handwritten { sha256: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputObservation {
    pub file_name: String,
    pub state: ObservedOutputState,
}

/// Everything an applier must persist before its first write to reconcile
/// an interrupted application of the same plan later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightRecord {
    pub project_doc_nonempty: bool,
    #[serde(default)]
    pub issued_at_ms: Option<u64>,
    pub outputs: Vec<OutputObservation>,
}

/// Controls one application.
#[derive(Default)]
pub struct ApplyOptions<'a> {
    pub dry_run: bool,
    /// Daemon-clock issuance of the plan being applied. When present, a plan
    /// issued before the newest one already applied to this checkout is
    /// refused before any write, and the fence advances after writes.
    pub issued_at_ms: Option<u64>,
    /// Runs after preflight and before the first write. An error aborts the
    /// application with nothing written.
    pub before_publish: Option<&'a mut dyn FnMut(&[OutputObservation]) -> Result<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetState {
    Absent,
    Generated([u8; 32]),
    Handwritten([u8; 32]),
}

impl TargetState {
    fn observation(&self) -> ObservedOutputState {
        match self {
            Self::Absent => ObservedOutputState::Absent,
            Self::Generated(digest) => ObservedOutputState::Generated {
                sha256: hex(digest),
            },
            Self::Handwritten(digest) => ObservedOutputState::Handwritten {
                sha256: hex(digest),
            },
        }
    }
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn classify(bytes: &[u8]) -> TargetState {
    let digest = Sha256::digest(bytes).into();
    if String::from_utf8_lossy(bytes).contains(GENERATED_MARKER) {
        TargetState::Generated(digest)
    } else {
        TargetState::Handwritten(digest)
    }
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
            Ok(classify(&fs::read(target).with_context(|| {
                format!("reading {}", target.display())
            })?))
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderFence {
    version: u32,
    issued_at_ms: u64,
}

/// Every component of the fence directory must be a real directory.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
fn fence_directory(root: &Path, create: bool) -> Result<Option<PathBuf>> {
    let mut current = root.to_path_buf();
    for part in Path::new(FENCE_DIR).components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!(
                "error.render_target_unsafe: render fence directory is not a real directory: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                fs::create_dir(&current)
                    .with_context(|| format!("creating {}", current.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", current.display()));
            }
        }
    }
    Ok(Some(current))
}

/// The newest plan issuance already applied to this checkout.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
fn read_fence(root: &Path) -> Result<u64> {
    let Some(directory) = fence_directory(root, false)? else {
        return Ok(0);
    };
    let path = directory.join(FENCE_FILE);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("inspecting the render fence"),
        Ok(metadata) if !metadata.is_file() || metadata.len() > MAX_FENCE_BYTES => {
            bail!("error.render_target_unsafe: the render fence is not a bounded regular file")
        }
        Ok(_) => {}
    }
    let fence: RenderFence =
        serde_json::from_slice(&fs::read(&path)?).context("parsing the render fence")?;
    if fence.version != FENCE_VERSION {
        bail!("unsupported render fence version {}", fence.version);
    }
    Ok(fence.issued_at_ms)
}

#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
fn advance_fence(root: &Path, issued_at_ms: u64) -> Result<()> {
    if read_fence(root)? >= issued_at_ms {
        return Ok(());
    }
    let directory = fence_directory(root, true)?.expect("created fence directory");
    let ignore = directory.join(".gitignore");
    if fs::symlink_metadata(&ignore).is_err() {
        fs::write(&ignore, "*\n!.gitignore\n").context("writing the local state ignore file")?;
    }
    #[cfg(test)]
    tests::interleave("advance_fence", root)?;
    let mut staged = tempfile::NamedTempFile::new_in(&directory)?;
    staged.write_all(&serde_json::to_vec(&RenderFence {
        version: FENCE_VERSION,
        issued_at_ms,
    })?)?;
    staged.as_file().sync_all()?;
    staged
        .persist(directory.join(FENCE_FILE))
        .map_err(|error| error.error)
        .context("publishing the render fence")?;
    File::open(&directory)?.sync_all()?;
    Ok(())
}

/// Apply one project render's fixed outputs inside `root`. The caller holds
/// the checkout render lock.
///
/// Preflight failures (unsafe targets, differing immutable satellites, a
/// stale plan, or a failed `before_publish`) return an error before any byte
/// is written. Failures after that point are reported per output, and a
/// failure that may follow an effect marks the outcome incomplete.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
pub fn apply_project_render(
    root: &Path,
    satellites: &[GuidanceFile],
    entrypoints: &[ProjectEntrypoint],
    options: ApplyOptions<'_>,
) -> Result<ProjectApplyOutcome> {
    let bbox_root = root.join(".bbox");
    bbox_util::guidance::preflight_files(&bbox_root, satellites)?;
    let mut observed = Vec::with_capacity(entrypoints.len());
    let mut observations = Vec::new();
    for entrypoint in entrypoints {
        let file_name = project_target_file(&entrypoint.provider)?;
        observed.push(match entrypoint.content {
            Some(_) => {
                let state = observe_target(&root.join(file_name))?;
                observations.push(OutputObservation {
                    file_name: file_name.to_string(),
                    state: state.observation(),
                });
                Some(state)
            }
            None => None,
        });
    }
    for satellite in satellites {
        let exists = fs::symlink_metadata(bbox_root.join(&satellite.path)).is_ok();
        observations.push(OutputObservation {
            file_name: format!(".bbox/{}", satellite.path),
            state: if exists {
                ObservedOutputState::Generated {
                    sha256: hex(&Sha256::digest(satellite.body.as_bytes())),
                }
            } else {
                ObservedOutputState::Absent
            },
        });
    }

    let nominal = if options.dry_run {
        ProjectRenderDispositionV1::DryRun
    } else {
        ProjectRenderDispositionV1::Written
    };
    let mut outcome = ProjectApplyOutcome {
        satellites: vec![nominal; satellites.len()],
        entrypoints: Vec::with_capacity(entrypoints.len()),
        errors: Vec::new(),
        incomplete: false,
    };
    if options.dry_run {
        for state in &observed {
            outcome.entrypoints.push(match state {
                None => ProjectRenderDispositionV1::Skipped,
                Some(TargetState::Handwritten(_)) => ProjectRenderDispositionV1::DryRunRefused,
                Some(_) => ProjectRenderDispositionV1::DryRun,
            });
        }
        return Ok(outcome);
    }

    if let Some(issued_at_ms) = options.issued_at_ms {
        let applied = read_fence(root)?;
        if issued_at_ms < applied {
            bail!(
                "error.render_superseded: a project render issued at {applied} ms already applied to this checkout; this plan, issued at {issued_at_ms} ms, was not applied"
            );
        }
    }
    if let Some(before_publish) = options.before_publish {
        before_publish(&observations).context("recording the render preflight")?;
    }

    if let Err(error) = bbox_util::guidance::publish_files(&bbox_root, satellites, false) {
        outcome.satellites = vec![ProjectRenderDispositionV1::Failed; satellites.len()];
        outcome
            .errors
            .push(format!("guidance satellites were not published: {error:#}"));
        // Satellites are content-addressed and immutable; a partial
        // satellite publication changes no existing file.
        for state in &observed {
            outcome.entrypoints.push(match state {
                None => ProjectRenderDispositionV1::Skipped,
                Some(TargetState::Handwritten(_)) => ProjectRenderDispositionV1::Refused,
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
        if matches!(state, TargetState::Handwritten(_)) {
            outcome
                .entrypoints
                .push(ProjectRenderDispositionV1::Refused);
            continue;
        }
        let file_name = project_target_file(&entrypoint.provider)?;
        match publish_entrypoint(root, file_name, content, &state) {
            Ok(published_output) => {
                published |= published_output.disposition == ProjectRenderDispositionV1::Written;
                if let Some(retained) = published_output.retained {
                    outcome.errors.push(format!(
                        "{file_name}: the owner's concurrent bytes were preserved at {}",
                        retained.display()
                    ));
                }
                outcome.entrypoints.push(published_output.disposition);
            }
            Err(error) => {
                // A failure inside publication may follow the rename that
                // moved the previous bytes aside.
                outcome.incomplete = true;
                outcome.errors.push(format!("{file_name}: {error:#}"));
                outcome.entrypoints.push(ProjectRenderDispositionV1::Failed);
            }
        }
    }
    if published {
        let synced = sync_root(root);
        if let Err(error) = synced {
            outcome.incomplete = true;
            outcome
                .errors
                .push(format!("published outputs may not be durable: {error:#}"));
        }
    }
    if published && let Some(issued_at_ms) = options.issued_at_ms {
        if let Err(error) = advance_fence(root, issued_at_ms) {
            outcome.incomplete = true;
            outcome
                .errors
                .push(format!("the render fence did not advance: {error:#}"));
        }
    }
    Ok(outcome)
}

#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
fn sync_root(root: &Path) -> Result<()> {
    #[cfg(test)]
    tests::interleave("sync_root", root)?;
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .context("syncing the project render root")
}

struct PublishedOutput {
    disposition: ProjectRenderDispositionV1,
    /// A sibling that holds bytes the owner wrote concurrently.
    retained: Option<PathBuf>,
}

impl PublishedOutput {
    fn plain(disposition: ProjectRenderDispositionV1) -> Self {
        Self {
            disposition,
            retained: None,
        }
    }
}

/// Publish one entrypoint over exactly the state preflight observed.
///
/// A target that preflight saw as generated is first moved aside to a unique
/// sibling by rename. Only when the moved bytes are the observed bytes is the
/// staged output published, without clobbering anything created meanwhile.
/// If the owner changed the file at any point, their bytes are put back (or,
/// when the path was re-created, kept beside it) and the output is a
/// conflict.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
fn publish_entrypoint(
    root: &Path,
    file_name: &str,
    content: &str,
    observed: &TargetState,
) -> Result<PublishedOutput> {
    #[cfg(test)]
    tests::interleave(&format!("publish:{file_name}"), root)?;
    let target = root.join(file_name);
    let planned: [u8; 32] = Sha256::digest(content.as_bytes()).into();
    if *observed == TargetState::Generated(planned) {
        // Already at the projected bytes. Confirm the target still holds
        // them rather than trusting preflight.
        #[cfg(test)]
        tests::interleave("before_noop_confirm", root)?;
        return Ok(PublishedOutput::plain(
            if observe_target(&target)? == *observed {
                ProjectRenderDispositionV1::Written
            } else {
                ProjectRenderDispositionV1::Conflict
            },
        ));
    }
    let mut staged = tempfile::Builder::new()
        .prefix(&format!(".{file_name}."))
        .suffix(".render-tmp")
        .tempfile_in(root)
        .context("staging the project render output")?;
    staged.write_all(content.as_bytes())?;
    staged.as_file().sync_all()?;
    #[cfg(test)]
    tests::interleave("before_replace", root)?;

    let aside = match observed {
        TargetState::Absent => None,
        TargetState::Generated(_) => {
            let aside = tempfile::Builder::new()
                .prefix(&format!(".{file_name}."))
                .suffix(".render-prev")
                .tempfile_in(root)
                .context("reserving the previous-output sibling")?
                .into_temp_path();
            match fs::rename(&target, &aside) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // The owner removed the file after preflight.
                    return Ok(PublishedOutput::plain(ProjectRenderDispositionV1::Conflict));
                }
                Err(error) => {
                    return Err(error).context("moving the previous output aside");
                }
            }
            let moved = classify(&fs::read(&aside).context("reading the previous output")?);
            if moved != *observed {
                return restore_aside(aside, &target);
            }
            Some(aside)
        }
        TargetState::Handwritten(_) => {
            return Ok(PublishedOutput::plain(ProjectRenderDispositionV1::Refused));
        }
    };
    #[cfg(test)]
    tests::interleave("before_publish_rename", root)?;
    match staged.persist_noclobber(&target) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Something created the path after it was vacated or observed
            // absent. Keep it; the moved-aside bytes were the old projection.
            return Ok(PublishedOutput::plain(ProjectRenderDispositionV1::Conflict));
        }
        Err(error) => {
            if let Some(aside) = aside {
                let _ = restore_aside(aside, &target);
            }
            return Err(error.error).context("publishing the project render output");
        }
    }
    if let Some(aside) = aside {
        // A writer that still held the old file open may have changed it
        // after the check; never discard such bytes.
        let moved = classify(&fs::read(&aside).context("rereading the previous output")?);
        if moved != *observed {
            let kept = aside
                .keep()
                .context("retaining the owner's concurrent bytes")?;
            return Ok(PublishedOutput {
                disposition: ProjectRenderDispositionV1::Conflict,
                retained: Some(kept),
            });
        }
    }
    Ok(PublishedOutput::plain(ProjectRenderDispositionV1::Written))
}

/// Put the owner's moved-aside bytes back at `target`. If the path was
/// re-created meanwhile, keep both and report where the moved bytes are.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
fn restore_aside(aside: tempfile::TempPath, target: &Path) -> Result<PublishedOutput> {
    match fs::hard_link(&aside, target) {
        Ok(()) => Ok(PublishedOutput::plain(ProjectRenderDispositionV1::Conflict)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let kept = aside
                .keep()
                .context("retaining the owner's concurrent bytes")?;
            Ok(PublishedOutput {
                disposition: ProjectRenderDispositionV1::Conflict,
                retained: Some(kept),
            })
        }
        Err(error) => {
            let kept = aside
                .keep()
                .context("retaining the owner's concurrent bytes")?;
            Err(error).context(format!(
                "restoring the owner's bytes; they remain at {}",
                kept.display()
            ))
        }
    }
}

/// Options for one plan execution.
pub struct ExecuteOptions<'a> {
    pub lock_timeout: Duration,
    /// Issuance of a workspace plan; producer plans carry their own.
    pub issued_at_ms: Option<u64>,
    /// Persist the preflight record before the first write.
    pub before_publish: Option<&'a mut dyn FnMut(&PreflightRecord) -> Result<()>>,
}

impl Default for ExecuteOptions<'_> {
    fn default() -> Self {
        Self {
            lock_timeout: DEFAULT_RENDER_LOCK_TIMEOUT,
            issued_at_ms: None,
            before_publish: None,
        }
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
    execute_workspace_render_plan(
        plan,
        project_root,
        expected_scope,
        expected_workspace_id,
        None,
    )
}

/// [`execute_project_render_plan`] with the daemon's issuance of the plan,
/// which the checkout freshness fence compares.
pub fn execute_workspace_render_plan(
    plan: &ProjectRenderPlanV1,
    project_root: &Path,
    expected_scope: &PublishedScope,
    expected_workspace_id: &str,
    issued_at_ms: Option<u64>,
) -> Result<ProjectRenderExecutionV1> {
    execute_project_render_plan_with(
        plan,
        project_root,
        expected_scope,
        ExpectedRenderAuthority::Workspace {
            workspace_id: expected_workspace_id,
        },
        ExecuteOptions {
            issued_at_ms,
            ..Default::default()
        },
    )
}

pub fn execute_project_render_plan_as(
    plan: &ProjectRenderPlanV1,
    project_root: &Path,
    expected_scope: &PublishedScope,
    expected: ExpectedRenderAuthority<'_>,
    lock_timeout: Duration,
) -> Result<ProjectRenderExecutionV1> {
    execute_project_render_plan_with(
        plan,
        project_root,
        expected_scope,
        expected,
        ExecuteOptions {
            lock_timeout,
            ..Default::default()
        },
    )
}

fn split_outputs(
    outputs: &[(
        crate::transport::ProjectRenderProjectionReceiptV1,
        Option<String>,
    )],
) -> (Vec<ProjectEntrypoint>, Vec<GuidanceFile>) {
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
        .collect();
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
        .collect();
    (entrypoints, satellites)
}

/// Execute an authorized project render inside the checkout owner's already
/// verified root. The renderer never receives a daemon path. Its destinations
/// are fixed provider filenames and content-addressed satellites derived from
/// validated source entries within `project_root`.
pub fn execute_project_render_plan_with(
    plan: &ProjectRenderPlanV1,
    project_root: &Path,
    expected_scope: &PublishedScope,
    expected: ExpectedRenderAuthority<'_>,
    options: ExecuteOptions<'_>,
) -> Result<ProjectRenderExecutionV1> {
    plan.validate_expected_authority(expected_scope, expected)?;
    let root = verify_render_root(project_root)?;
    let _lock = lock_checkout_for_render(&root, options.lock_timeout)?;
    let project_doc_nonempty = project_doc_nonempty(&root);
    let issued_at_ms = plan
        .producer
        .as_ref()
        .map(|producer| producer.issued_at_ms)
        .or(options.issued_at_ms);
    let outputs = plan.expected_outputs(project_doc_nonempty)?;
    let (entrypoints, satellites) = split_outputs(&outputs);
    let mut before_publish = options.before_publish;
    let mut record_preflight = |observations: &[OutputObservation]| -> Result<()> {
        match before_publish.as_mut() {
            Some(hook) => hook(&PreflightRecord {
                project_doc_nonempty,
                issued_at_ms,
                outputs: observations.to_vec(),
            }),
            None => Ok(()),
        }
    };
    let applied = apply_project_render(
        &root,
        &satellites,
        &entrypoints,
        ApplyOptions {
            dry_run: plan.dry_run,
            issued_at_ms,
            before_publish: Some(&mut record_preflight),
        },
    )?;

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
        incomplete: applied.incomplete,
        projections,
    };
    receipt.validate_against(plan)?;
    let output = render_output_text(&root, &receipt, &contents, &applied.errors, plan.dry_run);
    Ok(ProjectRenderExecutionV1 { output, receipt })
}

/// Reconcile an application that recorded its preflight but was interrupted
/// before recording its result. Nothing is written: each output is reported
/// as published when it holds the planned bytes, as not published when it
/// still holds the preflight bytes, and as a conflict when it holds anything
/// else, so owner edits made since the interruption are never replaced.
#[allow(
    clippy::disallowed_methods,
    reason = "Synchronous renderer IO; daemon callers run on the blocking pool and owner callers are synchronous"
)]
pub fn reconcile_interrupted_render(
    plan: &ProjectRenderPlanV1,
    project_root: &Path,
    expected_scope: &PublishedScope,
    expected: ExpectedRenderAuthority<'_>,
    record: &PreflightRecord,
    lock_timeout: Duration,
) -> Result<ProjectRenderExecutionV1> {
    plan.validate_expected_authority(expected_scope, expected)?;
    if plan.dry_run {
        bail!("a dry-run plan writes nothing and has no interrupted application");
    }
    let root = verify_render_root(project_root)?;
    let _lock = lock_checkout_for_render(&root, lock_timeout)?;
    let outputs = plan.expected_outputs(record.project_doc_nonempty)?;
    let (mut projections, contents): (Vec<_>, Vec<_>) = outputs.into_iter().unzip();
    let mut satellites_published = true;
    for projection in projections
        .iter_mut()
        .filter(|projection| projection.file_name.starts_with(".bbox/guidance/"))
    {
        let path = root.join(&projection.file_name);
        let present = fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file())
            && fs::read(&path).is_ok_and(|bytes| {
                Some(hex(&Sha256::digest(&bytes))) == projection.projection_sha256
            });
        projection.disposition = if present {
            ProjectRenderDispositionV1::Written
        } else {
            satellites_published = false;
            ProjectRenderDispositionV1::Failed
        };
    }
    let mut any_written = false;
    for projection in projections
        .iter_mut()
        .filter(|projection| !projection.file_name.starts_with(".bbox/guidance/"))
    {
        if projection.projection_sha256.is_none() {
            projection.disposition = ProjectRenderDispositionV1::Skipped;
            continue;
        }
        let preflight = record
            .outputs
            .iter()
            .find(|observation| observation.file_name == projection.file_name)
            .map(|observation| &observation.state)
            .context("the preflight record does not cover a planned output")?;
        if matches!(preflight, ObservedOutputState::Handwritten { .. }) {
            projection.disposition = ProjectRenderDispositionV1::Refused;
            continue;
        }
        let current = observe_target(&root.join(&projection.file_name))?.observation();
        let planned = matches!(&current, ObservedOutputState::Generated { sha256 }
            if Some(sha256) == projection.projection_sha256.as_ref());
        projection.disposition = if planned && satellites_published {
            any_written = true;
            ProjectRenderDispositionV1::Written
        } else if &current == preflight {
            ProjectRenderDispositionV1::Failed
        } else {
            ProjectRenderDispositionV1::Conflict
        };
    }
    let mut errors = vec![
        "the previous application was interrupted; outputs were reconciled without writing"
            .to_string(),
    ];
    let mut incomplete = false;
    if any_written && let Some(issued_at_ms) = record.issued_at_ms {
        if let Err(error) = advance_fence(&root, issued_at_ms) {
            incomplete = true;
            errors.push(format!("the render fence did not advance: {error:#}"));
        }
    }
    let receipt = ProjectRenderReceiptV1 {
        version: PROJECT_RENDER_TRANSPORT_VERSION,
        project_id: plan.project_id.clone(),
        scope: plan.scope.clone(),
        workspace_id: plan.workspace_id.clone(),
        producer: plan.producer.clone(),
        project_doc_nonempty: record.project_doc_nonempty,
        incomplete,
        projections,
    };
    receipt.validate_against(plan)?;
    let output = render_output_text(&root, &receipt, &contents, &errors, false);
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
