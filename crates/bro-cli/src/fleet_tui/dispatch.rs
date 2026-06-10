use super::*;

pub(super) fn dispatch_current_input_for_mode(app: &mut App, mode: DispatchMode) {
    let prompt = app.input.trim().to_string();
    if prompt.is_empty() {
        return;
    }
    let name = truncate(&prompt, NAME_LEN);

    match mode {
        DispatchMode::Fleet => dispatch_fleet_prompt(app, prompt, name),
        DispatchMode::Standalone => dispatch_standalone_prompt(app, prompt, name),
    }
}

pub(super) fn launch_standalone_current_input(app: &mut App) {
    dispatch_current_input_for_mode(app, DispatchMode::Standalone);
}

pub(super) fn dispatch_fleet_prompt(app: &mut App, prompt: String, name: String) {
    let cfg = FleetConfig::load();
    let project = match resolve_project_directive(&prompt, &cfg.projects) {
        Ok(project) => project,
        Err(e) => {
            app.set_status(e, Duration::from_secs(6));
            return;
        }
    };
    let prompt = project.prompt;
    let name = if project.alias.is_some() {
        truncate(&prompt, NAME_LEN)
    } else {
        name
    };
    let launch_cwd = project.cwd.clone().or_else(|| app.launch_cwd.clone());
    let provider = app.next_provider;
    let model = app.next_model.clone();
    let effort = app.next_effort.clone();
    let code_mode = app.config.code_mode.clone();
    let service_tier = service_tier_for_next_dispatch(app, provider);
    let alias = project.alias.clone();
    let classifier_cfg = app.orch.classifier();
    let orch = app.orch.clone();
    let tx = app.dispatch_tx.clone();

    // The worktree git work and the `/control/exec` round-trip run on a worker
    // task so the render loop never stalls; the finished agent is installed from
    // `dispatch_rx` on the next tick (`install_dispatch`). The input clears and
    // a "dispatching…" flash shows immediately, so a dispatch reads as instant.
    app.pending_dispatches += 1;
    app.set_status(
        format!("dispatching {provider} agent…"),
        Duration::from_secs(4),
    );
    app.clear_input();
    app.rt.spawn(async move {
        let outcome = build_fleet_dispatch(
            orch,
            launch_cwd,
            prompt,
            name,
            provider,
            model,
            effort,
            code_mode,
            service_tier,
            classifier_cfg,
            alias,
        )
        .await;
        let _ = tx.send(outcome);
    });
}

/// Worker half of a fleet dispatch (runs off the render thread): create the
/// isolated worktree, frame the first turn, and register the task with the
/// daemon. The blocking git + HTTP live here, never on the UI loop.
#[allow(clippy::too_many_arguments)]
pub(super) async fn build_fleet_dispatch(
    orch: Arc<FleetOrchestrator>,
    launch_cwd: Option<String>,
    prompt: String,
    name: String,
    provider: Provider,
    model: Option<String>,
    effort: Option<String>,
    code_mode: Option<String>,
    service_tier: Option<String>,
    classifier_cfg: Option<ClassifierConfig>,
    alias: Option<String>,
) -> DispatchOutcome {
    let worktree = match prepare_dispatch_worktree(&orch, launch_cwd.as_deref(), &prompt) {
        Ok(worktree) => worktree,
        Err(e) => return DispatchOutcome::Failed(format!("worktree isolation failed: {e}")),
    };

    // Classifier companion: prepend the intern rider to the executor's first
    // turn ONLY when suggestions will actually be relayed (classifier present
    // AND auto_send). Gating on both keeps observe-only runs from telling the
    // executor about an intern whose voice never appears.
    let frame_executor = classifier_cfg
        .as_ref()
        .is_some_and(|c| c.auto_send_resolved());
    let mut dispatch_prompt = String::new();
    dispatch_prompt.push_str(&worktree.grounding);
    dispatch_prompt.push_str("\n\n");
    if frame_executor {
        dispatch_prompt.push_str(&intern_rider());
        dispatch_prompt.push_str("\n\n");
    }
    dispatch_prompt.push_str(&prompt);

    let mut spec = DispatchSpec::new(provider, dispatch_prompt);
    spec.cwd = Some(worktree.cwd.clone());
    spec.model = model.clone();
    spec.effort = effort.clone();
    spec.env_overrides = worktree.env_overrides.clone();
    spec.name = Some(name.clone());
    // Fleet-wide code-mode default from /config carries to NEW roster sessions.
    spec.code_mode = code_mode;
    spec.service_tier = service_tier.clone();
    let task = orch.dispatch_async(spec).await;

    DispatchOutcome::Ready(Box::new(DispatchedAgent {
        task,
        provider,
        model,
        effort,
        service_tier,
        project_cwd: worktree.project_cwd.clone(),
        name,
        prompt,
        classifier_cfg,
        alias,
        worktree_tail: path_tail(&worktree.cwd),
    }))
}

/// UI-thread half: install a finished dispatch into the roster (or flash the
/// error). Runs from the `dispatch_rx` drain so all `&mut App` mutation — agent
/// push, classifier spawn, cursor anchor, persist — stays on the render thread.
pub(super) fn install_dispatch(app: &mut App, outcome: DispatchOutcome) {
    app.pending_dispatches = app.pending_dispatches.saturating_sub(1);
    let d = match outcome {
        DispatchOutcome::Ready(d) => *d,
        DispatchOutcome::Failed(e) => {
            app.set_status(e, Duration::from_secs(8));
            return;
        }
    };
    let id = d.task.id();
    // Spawn the watching intern for this executor.
    let classifier = d.classifier_cfg.map(|cfg| {
        spawn_monitor(
            &app.rt,
            app.orch.clone(),
            d.task.clone(),
            d.name.clone(),
            cfg,
            app.classifier_tx.clone(),
        )
    });
    app.agents.push(Agent {
        task: d.task,
        classifier,
        provider: d.provider,
        selected_model: d.model,
        selected_effort: d.effort,
        selected_service_tier: d.service_tier,
        selected_cwd: Some(d.project_cwd),
        name: d.name,
        name_overridden: false,
        initial_prompt: Some(d.prompt.clone()),
        pending_inputs: VecDeque::new(),
        seen_user_steers: 0,
    });
    // Append the dispatch prompt to the shared composer histfile.
    let _ = composer_history::append_history(&app.composer_history_path, &d.prompt);
    // Pin the roster cursor to the freshly dispatched agent so it stays selected
    // as it surfaces in (and later moves between) buckets.
    app.roster_anchor_id = Some(id.clone());
    app.set_status(
        format!(
            "dispatched {} agent {}{} in {}",
            d.provider,
            &id[..8.min(id.len())],
            d.alias
                .as_deref()
                .map(|alias| format!(" @{alias}"))
                .unwrap_or_default(),
            d.worktree_tail
        ),
        Duration::from_secs(3),
    );
}

pub(super) fn dispatch_standalone_prompt(app: &mut App, prompt: String, name: String) {
    let cwd = app.launch_cwd.clone().or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
    });
    let pending_resume = app.mode.pending_resume().map(str::to_string);
    forget_standalone_agents(app, true);

    let provider = app.next_provider;
    let model = app.next_model.clone();
    let effort = app.next_effort.clone();
    let code_mode = app.config.code_mode.clone();
    let orch = app.orch.clone();
    let tx = app.standalone_tx.clone();
    let is_resume = pending_resume.is_some();

    // The single agent appears in the SingleAgent view; clear the prior one and
    // run start/resume off-thread so the launch (and any /resume) never blocks.
    app.mode.set_pending_resume(None);
    app.agents.clear();
    app.focused_agent_id = None;
    app.zone = Zone::SingleAgent;
    app.clear_input();
    app.history_cursor = None;
    app.scroll_from_bottom = 0;
    app.set_status(
        if is_resume { "resuming…" } else { "starting…" },
        Duration::from_secs(4),
    );
    app.rt.spawn(async move {
        let task = if let Some(session_id) = pending_resume {
            let mut spec = ResumeSpec::new(provider, session_id, prompt.clone());
            spec.cwd = cwd.clone();
            spec.model = model.clone();
            spec.effort = effort.clone();
            spec.name = Some(name.clone());
            orch.resume_async(spec).await
        } else {
            let mut spec = DispatchSpec::new(provider, prompt.clone());
            spec.cwd = cwd.clone();
            spec.model = model.clone();
            spec.effort = effort.clone();
            spec.name = Some(name.clone());
            // New session carries the fleet-wide code-mode; resume does not.
            spec.code_mode = code_mode.clone();
            orch.dispatch_async(spec).await
        };
        let _ = tx.send(StandaloneOutcome {
            task,
            provider,
            model,
            effort,
            cwd,
            name,
            prompt,
            is_resume,
        });
    });
}

/// UI-thread half: install the single standalone agent once its off-thread
/// start/resume lands.
pub(super) fn install_standalone(app: &mut App, o: StandaloneOutcome) {
    let id = o.task.id();
    app.agents.clear();
    app.agents.push(Agent {
        task: o.task,
        classifier: None,
        provider: o.provider,
        selected_model: o.model,
        selected_effort: o.effort,
        selected_service_tier: None,
        selected_cwd: o.cwd,
        name: o.name,
        name_overridden: false,
        initial_prompt: (!o.is_resume).then(|| o.prompt.clone()),
        pending_inputs: VecDeque::new(),
        seen_user_steers: 0,
    });
    // Append the prompt to the shared composer histfile.
    let _ = composer_history::append_history(&app.composer_history_path, &o.prompt);
    app.focused_agent_id = Some(id.clone());
    app.zone = Zone::SingleAgent;
    let verb = if o.is_resume { "resumed" } else { "started" };
    app.set_status(
        format!("{verb} {} agent {}", o.provider, &id[..8.min(id.len())]),
        Duration::from_secs(3),
    );
}

pub(super) fn resolve_project_directive(
    input: &str,
    projects: &BTreeMap<String, String>,
) -> Result<ProjectDirective, String> {
    let trimmed = input.trim_start();
    let Some(rest) = trimmed.strip_prefix('@') else {
        return Ok(ProjectDirective {
            alias: None,
            cwd: None,
            prompt: input.to_string(),
        });
    };
    let key_len = rest.find(char::is_whitespace).ok_or_else(|| {
        "usage: @project <prompt> (Tab completes configured projects)".to_string()
    })?;
    let key = &rest[..key_len];
    if key.is_empty() {
        return Err("usage: @project <prompt>".to_string());
    }
    let prompt = rest[key_len..].trim_start();
    if prompt.is_empty() {
        return Err(format!("usage: @{key} <prompt>"));
    }
    let raw = projects
        .get(key)
        .ok_or_else(|| format!("unknown @project `{key}`"))?;
    let cwd = PathBuf::from(raw)
        .canonicalize()
        .map_err(|e| format!("@{key}: cannot resolve {raw}: {e}"))?;
    if !cwd.is_dir() {
        return Err(format!("@{key}: {} is not a directory", cwd.display()));
    }
    Ok(ProjectDirective {
        alias: Some(key.to_string()),
        cwd: Some(cwd.display().to_string()),
        prompt: prompt.to_string(),
    })
}

pub(super) fn prepare_dispatch_worktree(
    orch: &FleetOrchestrator,
    launch_cwd: Option<&str>,
    prompt: &str,
) -> Result<DispatchWorktree, String> {
    let base_cwd = launch_cwd
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| "cannot resolve launch cwd".to_string())?;
    let git_root_raw = git_capture(&base_cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|e| format!("launch cwd is not inside a git repository: {e}"))?;
    let git_root = PathBuf::from(git_root_raw.trim())
        .canonicalize()
        .map_err(|e| format!("canonicalizing git root: {e}"))?;
    let base_branch = git_capture(&git_root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .or_else(|_| git_capture(&git_root, &["rev-parse", "--abbrev-ref", "HEAD"]))
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let base_sha = git_capture(&git_root, &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string();
    let base_status = git_capture(&git_root, &["status", "--short", "--branch"])
        .unwrap_or_else(|e| format!("git status unavailable: {e}"));

    let repo_name = git_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let suffix = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect::<String>();
    let slug = prompt_slug(prompt);
    let branch = format!("bro-fleet/{slug}-{suffix}");
    let worktree_root = orch.store_dir().join("worktrees");
    let worktree_dir = worktree_root.join(sanitize_path_component(repo_name));
    fs::create_dir_all(&worktree_dir)
        .map_err(|e| format!("creating worktree parent {}: {e}", worktree_dir.display()))?;
    let worktree_path = worktree_dir.join(format!("{slug}-{suffix}"));
    let add = Command::new("git")
        .arg("-C")
        .arg(&git_root)
        .args(["worktree", "add", "-b"])
        .arg(&branch)
        .arg(&worktree_path)
        .arg("HEAD")
        .output()
        .map_err(|e| format!("starting git worktree add: {e}"))?;
    if !add.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        ));
    }
    let worktree_path = worktree_path
        .canonicalize()
        .map_err(|e| format!("canonicalizing worktree path: {e}"))?;
    // Persist the fork-point base branch (captured above) keyed to THIS
    // worktree's branch, so closeout can default `target` to "the branch this
    // work diverged from" instead of reading the base repo's live HEAD (which
    // is multi-tenant — a peer agent or the operator may switch/advance the
    // base checkout between dispatch and closeout). Branch-scoped config is
    // unique per worktree-branch and is removed by `git branch -D` at closeout.
    // Best-effort: a failed write just falls back to the endpoint's resolver.
    if base_branch != "unknown" && base_branch != "HEAD" && !base_branch.is_empty() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&worktree_path)
            .args([
                "config",
                &format!("branch.{branch}.broFleetBase"),
                &base_branch,
            ])
            .output();
    }
    // Opt-in build-cache seeding (fleet.json project_dispatch.seed_dirs):
    // CoW-clone warm dirs (e.g. target/) from the base repo so the bro's
    // first build is incremental, not cold. Best-effort; outcomes logged.
    let seed_dirs = FleetConfig::load()
        .project_dispatch_for(&git_root)
        .map(|d| d.seed_dirs.clone())
        .unwrap_or_default();
    if !seed_dirs.is_empty() {
        for outcome in
            bro_fleet_client::seed_worktree_dirs(&git_root, &worktree_path, &seed_dirs)
        {
            tracing::info!(worktree = %worktree_path.display(), "{outcome}");
        }
    }

    let worktree_status = git_capture(&worktree_path, &["status", "--short", "--branch"])
        .unwrap_or_else(|e| format!("git status unavailable: {e}"));

    let mut env = HashMap::new();
    env.insert(
        "BRO_FLEET_BASE_REPO".to_string(),
        git_root.display().to_string(),
    );
    env.insert(
        "BRO_FLEET_WORKTREE_ROOT".to_string(),
        worktree_root.display().to_string(),
    );
    env.insert(
        "BRO_FLEET_PARENT_WORKTREE".to_string(),
        git_root.display().to_string(),
    );
    env.insert("BRO_FLEET_WORKTREE_BRANCH".to_string(), branch.clone());
    // Per-worktree build isolation: the cockpit no longer injects CARGO_TARGET_DIR
    // (or any language-specific build env). Each worktree gets its own target dir
    // (the cargo default), so concurrent worktree builds never serialize on a
    // shared build lock, and the cockpit stays project-agnostic. Project-specific
    // dispatch env (fleet.json `project_dispatch`) is resolved DAEMON-SIDE at
    // task spawn — keyed by the task cwd, worktree→base-repo aware — so every
    // dispatch path (cockpit, bro_exec, agent dispatch, workflows) gets it and
    // it lands in the harness shell-env lane, not the transport session env.
    let env_overrides = Some(env);

    let grounding = format!(
        "[fleet worktree grounding]\n\
You are running in an isolated git worktree created for this fleet dispatch.\n\
Worktree path: {}\n\
Worktree branch: {branch}\n\
Base repository: {}\n\
Base branch/ref: {base_branch} @ {base_sha}\n\
Make code changes only inside the worktree path above unless the operator explicitly redirects you.\n\
For project-scoped bbox calls (bbox_thread/_list, bbox_code_*, bbox_learn/decide/remember, \
bbox_render, slice tools), pass THIS worktree path as project/project_dir — committed artifacts \
(thread records, knowledge entries, rendered memory) then land in the worktree and travel with this \
branch instead of the base checkout; the daemon keys durable scope to the registered base.\n\
\n\
Initial worktree git status:\n```text\n{}\n```\n\
Source worktree status at dispatch time:\n```text\n{}\n```",
        worktree_path.display(),
        git_root.display(),
        worktree_status.trim(),
        base_status.trim(),
    );

    Ok(DispatchWorktree {
        cwd: worktree_path.display().to_string(),
        project_cwd: git_root.display().to_string(),
        grounding,
        env_overrides,
    })
}

pub(super) fn resume_env_overrides(
    app: &App,
    idx: usize,
    cwd: Option<&str>,
) -> Option<HashMap<String, String>> {
    let cwd_raw = cwd?;
    let worktree = PathBuf::from(cwd_raw)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(cwd_raw));
    let worktree_root_raw = app.orch.store_dir().join("worktrees");
    let worktree_root = worktree_root_raw
        .canonicalize()
        .unwrap_or(worktree_root_raw);
    if !worktree.starts_with(&worktree_root) {
        return None;
    }

    let base_repo = app.agents[idx]
        .selected_cwd
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| project_display_cwd(Some(cwd_raw)).map(PathBuf::from))?;
    let base_repo = base_repo.canonicalize().unwrap_or(base_repo);
    let branch = git_capture(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;

    let mut env = HashMap::new();
    env.insert(
        "BRO_FLEET_BASE_REPO".to_string(),
        base_repo.display().to_string(),
    );
    env.insert(
        "BRO_FLEET_WORKTREE_ROOT".to_string(),
        worktree_root.display().to_string(),
    );
    env.insert(
        "BRO_FLEET_PARENT_WORKTREE".to_string(),
        base_repo.display().to_string(),
    );
    env.insert("BRO_FLEET_WORKTREE_BRANCH".to_string(), branch);
    // Per-worktree build isolation on resume too — no shared CARGO_TARGET_DIR.
    // Project-specific dispatch env is merged from fleet.json `project_dispatch`.
    // project_dispatch env now resolved daemon-side at spawn (see above).
    Some(env)
}

/// Merge per-project dispatch env from `fleet.json` `project_dispatch` (keyed by
/// canonical repo path) into the worktree dispatch env. Best-effort: a missing or
/// malformed `fleet.json` must never block a dispatch, and reserved `BRO_FLEET_*`
/// vars (already inserted) are never overridden.
pub(super) fn git_capture(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub(super) fn prompt_slug(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("task");
    let slug = sanitize_path_component(first)
        .trim_matches('-')
        .chars()
        .take(36)
        .collect::<String>();
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    }
}

pub(super) fn sanitize_path_component(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if normalized == '-' {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(normalized);
            last_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}
