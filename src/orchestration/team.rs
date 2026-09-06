use parking_lot::{Mutex, MutexGuard};
use rmcp::schemars;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Global mutex for team file read-modify-write operations.
/// Prevents concurrent updates from clobbering each other.
static TEAM_FILE_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Teamplate (blueprint)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamplateMember {
    pub brofile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default = "default_one")]
    pub count: u32,
}

/// Bound expansion before allocating members or persisting a new template/team.
/// The optional advisor is separate from this member-slot budget.
pub const MAX_TEAM_MEMBERS: usize = 256;

pub fn validate_teamplate_member_count(template: &Teamplate) -> anyhow::Result<usize> {
    anyhow::ensure!(
        !template.members.is_empty(),
        "team template requires at least one member"
    );
    let mut total = 0usize;
    for member in &template.members {
        anyhow::ensure!(member.count > 0, "team member count must be at least 1");
        total = total
            .checked_add(usize::try_from(member.count)?)
            .ok_or_else(|| anyhow::anyhow!("expanded team member count overflow"))?;
        anyhow::ensure!(
            total <= MAX_TEAM_MEMBERS,
            "expanded team member count exceeds maximum {MAX_TEAM_MEMBERS}; reduce slot counts or split the team"
        );
    }
    Ok(total)
}

fn default_one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Teamplate {
    pub name: String,
    pub members: Vec<TeamplateMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor: Option<TeamAdvisorConfig>,
    /// Cohort-diversity floor: the minimum number of distinct providers to
    /// spread this team's members across when dispatched together as an
    /// ensemble. `None`/`0` disables it (current behavior). The ensemble
    /// dispatch loop honors it at first dispatch; the lease pins each lane
    /// thereafter. Soft floor — never fails when the tier offers fewer
    /// distinct providers than the floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diversity_floor: Option<usize>,
}

// ---------------------------------------------------------------------------
// Team (live instance)
// ---------------------------------------------------------------------------

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    strum::EnumString,
    strum::AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[derive(Default)]
pub enum AdvisorMode {
    #[default]
    Blocking,
    Background,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamAdvisorConfig {
    pub brofile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub charter: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub halt_conditions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exit_conditions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<f64>,
    #[serde(default)]
    pub mode: AdvisorMode,
}

impl TeamAdvisorConfig {
    pub fn display_name(&self) -> String {
        self.alias.clone().unwrap_or_else(|| self.brofile.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamAdvisor {
    pub name: String,
    pub config: TeamAdvisorConfig,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub task_history: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    pub name: String,
    pub brofile: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub task_history: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub name: String,
    pub teamplate: String,
    pub members: Vec<TeamMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor: Option<TeamAdvisor>,
    pub project_dir: Option<String>,
    pub created_at: u64,
    /// Copied from the teamplate at instantiation. See `Teamplate::diversity_floor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diversity_floor: Option<usize>,
}

// ---------------------------------------------------------------------------
// Teamplate disk ops
// ---------------------------------------------------------------------------

fn teamplates_dir(store_dir: &Path) -> PathBuf {
    store_dir.join("teamplates")
}

fn project_teamplates_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".bro").join("teamplates")
}

pub fn save_teamplate(
    tp: &Teamplate,
    scope: &str,
    store_dir: &Path,
    project_dir: Option<&str>,
) -> anyhow::Result<()> {
    validate_teamplate_member_count(tp)?;
    let dir = if scope == "project" {
        project_teamplates_dir(Path::new(project_dir.unwrap_or(".")))
    } else {
        teamplates_dir(store_dir)
    };
    crate::json_store::atomic_write_json_locked(&dir.join(format!("{}.json", tp.name)), tp)
}

pub fn resolve_teamplate(
    name: &str,
    store_dir: &Path,
    project_dir: Option<&str>,
) -> Option<Teamplate> {
    if let Some(pd) = project_dir {
        if let Some(tp) = load_json(&project_teamplates_dir(Path::new(pd)), name) {
            return Some(tp);
        }
    }
    load_json(&teamplates_dir(store_dir), name)
}

pub fn list_teamplates(scope: &str, store_dir: &Path, project_dir: Option<&str>) -> Vec<Teamplate> {
    let dir = if scope == "project" {
        project_teamplates_dir(Path::new(project_dir.unwrap_or(".")))
    } else {
        teamplates_dir(store_dir)
    };
    list_json_files(&dir)
}

pub fn delete_teamplate(
    name: &str,
    scope: &str,
    store_dir: &Path,
    project_dir: Option<&str>,
) -> bool {
    let dir = if scope == "project" {
        project_teamplates_dir(Path::new(project_dir.unwrap_or(".")))
    } else {
        teamplates_dir(store_dir)
    };
    fs::remove_file(dir.join(format!("{name}.json"))).is_ok()
}

/// Scoped discovery never falls back from project templates to global ones.
/// Keep the legacy resolution helper for dispatch compatibility.
pub fn get_teamplate_checked(
    name: &str,
    scope: &str,
    store_dir: &Path,
    project_dir: Option<&str>,
) -> anyhow::Result<Option<Teamplate>> {
    let dir = checked_teamplates_dir(scope, store_dir, project_dir)?;
    load_json_checked(&dir, name)
}

pub fn list_teamplates_checked(
    scope: &str,
    store_dir: &Path,
    project_dir: Option<&str>,
) -> anyhow::Result<Vec<Teamplate>> {
    list_json_files_checked(&checked_teamplates_dir(scope, store_dir, project_dir)?)
}

fn checked_teamplates_dir(
    scope: &str,
    store_dir: &Path,
    project_dir: Option<&str>,
) -> anyhow::Result<PathBuf> {
    match scope {
        "global" if project_dir.is_none() => Ok(teamplates_dir(store_dir)),
        "project" => {
            let project = project_dir
                .ok_or_else(|| anyhow::anyhow!("project_dir is required for project templates"))?;
            anyhow::ensure!(
                Path::new(project).is_absolute(),
                "project template directory must be absolute"
            );
            Ok(project_teamplates_dir(Path::new(project)))
        }
        _ => anyhow::bail!("invalid template scope/project_dir combination"),
    }
}

pub fn load_team_checked(name: &str, store_dir: &Path) -> anyhow::Result<Option<Team>> {
    load_json_checked(&teams_dir(store_dir), name)
}

pub fn load_all_teams_checked(store_dir: &Path) -> anyhow::Result<Vec<Team>> {
    list_json_files_checked(&teams_dir(store_dir))
}

// Caller-facing discovery must distinguish an empty catalog from an unreadable
// or malformed one. Existing dispatch/lifecycle readers keep their contract.
fn load_json_checked<T: serde::de::DeserializeOwned>(
    dir: &Path,
    name: &str,
) -> anyhow::Result<Option<T>> {
    use anyhow::Context;
    anyhow::ensure!(
        !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\\', '\0']),
        "stored team/template name is not a path"
    );
    let data = match fs::read_to_string(dir.join(format!("{name}.json"))) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("stored team/template {name} could not be read"));
        }
    };
    serde_json::from_str(&data)
        .with_context(|| format!("stored team/template {name} is invalid JSON"))
        .map(Some)
}

fn list_json_files_checked<T: serde::de::DeserializeOwned>(dir: &Path) -> anyhow::Result<Vec<T>> {
    use anyhow::Context;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("team catalog could not be read"),
    };
    let mut names = Vec::new();
    for entry in entries {
        let path = entry
            .context("team catalog entry could not be read")?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            names.push(
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .context("team catalog has an invalid record name")?
                    .to_owned(),
            );
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|name| {
            load_json_checked(dir, &name)?
                .context("team catalog changed during discovery; retry the read")
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Team disk ops
// ---------------------------------------------------------------------------

fn teams_dir(store_dir: &Path) -> PathBuf {
    store_dir.join("teams")
}

pub fn save_team(team: &Team, store_dir: &Path) {
    atomic_write_json(&teams_dir(store_dir), &team.name, team);
}

/// Acquire the team file lock for read-modify-write operations.
pub fn lock_teams() -> MutexGuard<'static, ()> {
    TEAM_FILE_LOCK.lock()
}

pub fn load_team(name: &str, store_dir: &Path) -> Option<Team> {
    load_json(&teams_dir(store_dir), name)
}

pub fn load_all_teams(store_dir: &Path) -> Vec<Team> {
    list_json_files(&teams_dir(store_dir))
}

pub fn remove_team(name: &str, store_dir: &Path) -> bool {
    fs::remove_file(teams_dir(store_dir).join(format!("{name}.json"))).is_ok()
}

pub fn rename_project_refs(store_dir: &Path, old_project: &str, new_project: &str) -> usize {
    let mut updated = 0usize;
    for mut team in load_all_teams(store_dir) {
        if team.project_dir.as_deref() == Some(old_project) {
            team.project_dir = Some(new_project.to_string());
            save_team(&team, store_dir);
            updated += 1;
        }
    }
    updated
}

// ---------------------------------------------------------------------------
// Instantiation
// ---------------------------------------------------------------------------

pub fn instantiate_team(
    tp: &Teamplate,
    team_name: &str,
    project_dir: Option<&str>,
    store_dir: &Path,
) -> anyhow::Result<Team> {
    let member_count = validate_teamplate_member_count(tp)?;
    let mut members = Vec::with_capacity(member_count);
    for slot in &tp.members {
        let count = slot.count;
        for i in 0..count {
            let name = if let Some(ref alias) = slot.alias {
                if count > 1 {
                    format!("{alias}-{}", i + 1)
                } else {
                    alias.clone()
                }
            } else if count > 1 {
                format!("{}-{}", slot.brofile, i + 1)
            } else {
                slot.brofile.clone()
            };
            members.push(TeamMember {
                name,
                brofile: slot.brofile.clone(),
                session_id: None,
                task_history: vec![],
            });
        }
    }

    let team = Team {
        name: team_name.to_string(),
        teamplate: tp.name.clone(),
        members,
        advisor: tp.advisor.clone().map(|config| TeamAdvisor {
            name: config.display_name(),
            config,
            session_id: None,
            task_history: vec![],
        }),
        project_dir: project_dir.map(String::from),
        created_at: super::now_ms(),
        diversity_floor: tp.diversity_floor,
    };
    crate::json_store::atomic_write_json_locked(
        &teams_dir(store_dir).join(format!("{team_name}.json")),
        &team,
    )?;
    Ok(team)
}

// ---------------------------------------------------------------------------
// Bro resolution — find a named bro across all teams
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct BroMatch<'a> {
    pub team: &'a Team,
    pub member_idx: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroRef {
    pub team_name: String,
    pub member_name: String,
}

pub fn find_bro_matches<'a>(name: &str, teams: &'a [Team]) -> Vec<BroMatch<'a>> {
    let mut matches = Vec::new();
    for team in teams {
        for (i, member) in team.members.iter().enumerate() {
            if member.name == name {
                matches.push(BroMatch {
                    team,
                    member_idx: i,
                });
            }
        }
    }
    matches
}

#[allow(dead_code)] // used by tests in same file
pub fn find_bro<'a>(name: &str, teams: &'a [Team]) -> Option<BroMatch<'a>> {
    find_bro_matches(name, teams).into_iter().next()
}

pub fn resolve_bro_selector<'a>(
    selector: &str,
    teams: &'a [Team],
) -> Result<Option<BroMatch<'a>>, String> {
    if let Some((team_name, bro_name)) = selector.split_once("::") {
        let team = teams
            .iter()
            .find(|team| team.name == team_name)
            .ok_or_else(|| format!("Unknown team in bro selector: {team_name}"))?;
        let member_idx = team
            .members
            .iter()
            .position(|member| member.name == bro_name)
            .ok_or_else(|| format!("Unknown bro selector: {selector}"))?;
        return Ok(Some(BroMatch { team, member_idx }));
    }

    let matches = find_bro_matches(selector, teams);
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => {
            let mut team_names: Vec<&str> = matches.iter().map(|m| m.team.name.as_str()).collect();
            team_names.sort_unstable();
            team_names.dedup();
            Err(format!(
                "Ambiguous bro name: {selector}. Matches live team members in [{}]. Use team::bro to disambiguate.",
                team_names.join(", ")
            ))
        }
    }
}

pub fn find_bro_name_for_task(task_id: &str, store_dir: &Path) -> Option<String> {
    find_bro_ref_for_task(task_id, store_dir).map(|r| r.member_name)
}

pub fn find_bro_ref_for_task(task_id: &str, store_dir: &Path) -> Option<BroRef> {
    for team in load_all_teams(store_dir) {
        for member in &team.members {
            if member.task_history.contains(&task_id.to_string()) {
                return Some(BroRef {
                    team_name: team.name.clone(),
                    member_name: member.name.clone(),
                });
            }
        }
    }
    None
}

/// After a task completes and discovers its sessionId, propagate it back to the team member.
pub fn propagate_session_id(task_id: &str, session_id: &str, store_dir: &Path) {
    let _lock = lock_teams();
    for mut team in load_all_teams(store_dir) {
        let mut dirty = false;
        for member in &mut team.members {
            // Only update if this task is the most recent launch — a
            // late-completing older task must not clobber a newer session.
            if member.task_history.last().map(String::as_str) == Some(task_id) {
                member.session_id = Some(session_id.to_string());
                dirty = true;
            }
        }
        if let Some(advisor) = team.advisor.as_mut() {
            if advisor.task_history.last().map(String::as_str) == Some(task_id) {
                advisor.session_id = Some(session_id.to_string());
                dirty = true;
            }
        }
        if dirty {
            save_team(&team, store_dir);
        }
    }
}

// ---------------------------------------------------------------------------
// Generic JSON file helpers
// ---------------------------------------------------------------------------

fn atomic_write_json<T: Serialize>(dir: &Path, name: &str, value: &T) {
    let _ = fs::create_dir_all(dir);
    let file = dir.join(format!("{name}.json"));
    let tmp = dir.join(format!("{name}.json.tmp"));
    if let Ok(data) = serde_json::to_string_pretty(value) {
        if let Ok(mut f) = fs::File::create(&tmp) {
            let _ = f.write_all(data.as_bytes());
            let _ = f.sync_all();
            let _ = fs::rename(&tmp, &file);
        }
    }
}

fn load_json<T: serde::de::DeserializeOwned>(dir: &Path, name: &str) -> Option<T> {
    let file = dir.join(format!("{name}.json"));
    let data = fs::read_to_string(&file).ok()?;
    serde_json::from_str(&data).ok()
}

fn list_json_files<T: serde::de::DeserializeOwned>(dir: &Path) -> Vec<T> {
    let mut results = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(data) = fs::read_to_string(&path) {
                    if let Ok(item) = serde_json::from_str::<T>(&data) {
                        results.push(item);
                    }
                }
            }
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn team_member_admission_rejects_zero_large_and_overflowing_counts_before_writes() {
        let dir = temp_store();
        let root = dir.path().canonicalize().unwrap();
        let mut template = Teamplate {
            name: "bounded".into(),
            members: vec![],
            advisor: None,
            diversity_floor: None,
        };
        for counts in [
            vec![],
            vec![0],
            vec![257],
            vec![128, 129],
            vec![1, u32::MAX],
            vec![u32::MAX, u32::MAX],
        ] {
            template.members = counts
                .into_iter()
                .map(|count| TeamplateMember {
                    brofile: "reviewer".into(),
                    alias: None,
                    count,
                })
                .collect();
            assert!(save_teamplate(&template, "global", &root, None).is_err());
            assert!(instantiate_team(&template, "rejected", None, &root).is_err());
            assert!(!root.join("teamplates").exists());
            assert!(!root.join("teams").exists());
        }
        template.members = vec![TeamplateMember {
            brofile: "reviewer".into(),
            alias: None,
            count: MAX_TEAM_MEMBERS as u32,
        }];
        save_teamplate(&template, "global", &root, None).unwrap();
        let team = instantiate_team(&template, "accepted", None, &root).unwrap();
        assert_eq!(team.members.len(), MAX_TEAM_MEMBERS);
        template.members[0].count = 0;
        assert!(save_teamplate(&template, "global", &root, None).is_err());
        assert_eq!(
            resolve_teamplate("bounded", &root, None).unwrap().members[0].count,
            MAX_TEAM_MEMBERS as u32
        );
    }

    #[test]
    fn test_save_and_load_teamplate() {
        let dir = temp_store();
        let tp = Teamplate {
            name: "review-panel".into(),
            members: vec![
                TeamplateMember {
                    brofile: "reviewer".into(),
                    alias: None,
                    count: 1,
                },
                TeamplateMember {
                    brofile: "critic".into(),
                    alias: Some("devil".into()),
                    count: 1,
                },
            ],
            advisor: None,
            diversity_floor: None,
        };
        save_teamplate(&tp, "global", dir.path(), None).unwrap();
        let loaded = resolve_teamplate("review-panel", dir.path(), None);
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.members.len(), 2);
        assert_eq!(loaded.members[1].alias.as_deref(), Some("devil"));
    }

    #[test]
    fn test_instantiate_team() {
        let dir = temp_store();
        let tp = Teamplate {
            name: "test-tp".into(),
            members: vec![
                TeamplateMember {
                    brofile: "worker".into(),
                    alias: None,
                    count: 2,
                },
                TeamplateMember {
                    brofile: "lead".into(),
                    alias: Some("boss".into()),
                    count: 1,
                },
            ],
            advisor: None,
            diversity_floor: None,
        };

        let team = instantiate_team(&tp, "test-team", Some("/tmp/proj"), dir.path()).unwrap();
        assert_eq!(team.name, "test-team");
        assert_eq!(team.members.len(), 3);
        assert_eq!(team.members[0].name, "worker-1");
        assert_eq!(team.members[1].name, "worker-2");
        assert_eq!(team.members[2].name, "boss");
        assert_eq!(team.project_dir.as_deref(), Some("/tmp/proj"));

        // Should be persisted
        let loaded = load_team("test-team", dir.path());
        assert!(loaded.is_some());
    }

    #[test]
    fn test_find_bro() {
        let teams = vec![Team {
            name: "t1".into(),
            teamplate: "tp1".into(),
            members: vec![
                TeamMember {
                    name: "alice".into(),
                    brofile: "reviewer".into(),
                    session_id: None,
                    task_history: vec![],
                },
                TeamMember {
                    name: "bob".into(),
                    brofile: "critic".into(),
                    session_id: None,
                    task_history: vec![],
                },
            ],
            advisor: None,
            project_dir: None,
            created_at: 0,
            diversity_floor: None,
        }];

        let found = find_bro("alice", &teams);
        assert!(found.is_some());
        assert_eq!(found.unwrap().member_idx, 0);

        let found = find_bro("bob", &teams);
        assert!(found.is_some());
        assert_eq!(found.unwrap().member_idx, 1);

        assert!(find_bro("charlie", &teams).is_none());
    }

    #[test]
    fn test_resolve_bro_selector_requires_disambiguation_for_duplicates() {
        let teams = vec![
            Team {
                name: "red".into(),
                teamplate: "tp1".into(),
                members: vec![TeamMember {
                    name: "reviewer".into(),
                    brofile: "reviewer".into(),
                    session_id: Some("sid-red".into()),
                    task_history: vec![],
                }],
                advisor: None,
                project_dir: None,
                created_at: 0,
                diversity_floor: None,
            },
            Team {
                name: "blue".into(),
                teamplate: "tp1".into(),
                members: vec![TeamMember {
                    name: "reviewer".into(),
                    brofile: "reviewer".into(),
                    session_id: Some("sid-blue".into()),
                    task_history: vec![],
                }],
                advisor: None,
                project_dir: None,
                created_at: 0,
                diversity_floor: None,
            },
        ];

        let err = resolve_bro_selector("reviewer", &teams).unwrap_err();
        assert!(err.contains("Ambiguous bro name: reviewer"));
        assert!(err.contains("red"));
        assert!(err.contains("blue"));

        let scoped = resolve_bro_selector("blue::reviewer", &teams)
            .unwrap()
            .unwrap();
        assert_eq!(scoped.team.name, "blue");
        assert_eq!(scoped.team.members[scoped.member_idx].name, "reviewer");
    }

    #[test]
    fn test_resolve_bro_selector_unknown_scoped_member_errors() {
        let teams = vec![Team {
            name: "red".into(),
            teamplate: "tp1".into(),
            members: vec![TeamMember {
                name: "reviewer".into(),
                brofile: "reviewer".into(),
                session_id: None,
                task_history: vec![],
            }],
            advisor: None,
            project_dir: None,
            created_at: 0,
            diversity_floor: None,
        }];

        let err = resolve_bro_selector("red::critic", &teams).unwrap_err();
        assert_eq!(err, "Unknown bro selector: red::critic");
    }

    #[test]
    fn test_find_bro_name_for_task() {
        let dir = temp_store();
        let team = Team {
            name: "t1".into(),
            teamplate: "tp1".into(),
            members: vec![TeamMember {
                name: "alice".into(),
                brofile: "reviewer".into(),
                session_id: None,
                task_history: vec!["task-123".into()],
            }],
            advisor: None,
            project_dir: None,
            created_at: 0,
            diversity_floor: None,
        };
        save_team(&team, dir.path());

        assert_eq!(
            find_bro_name_for_task("task-123", dir.path()),
            Some("alice".into())
        );
        assert_eq!(
            find_bro_ref_for_task("task-123", dir.path()),
            Some(BroRef {
                team_name: "t1".into(),
                member_name: "alice".into(),
            })
        );
        assert_eq!(find_bro_name_for_task("task-999", dir.path()), None);
    }

    #[test]
    fn test_propagate_session_id() {
        let dir = temp_store();
        let team = Team {
            name: "t1".into(),
            teamplate: "tp1".into(),
            members: vec![TeamMember {
                name: "alice".into(),
                brofile: "reviewer".into(),
                session_id: Some("pending".into()),
                task_history: vec!["task-abc".into()],
            }],
            advisor: None,
            project_dir: None,
            created_at: 0,
            diversity_floor: None,
        };
        save_team(&team, dir.path());

        propagate_session_id("task-abc", "real-session-id", dir.path());

        let loaded = load_team("t1", dir.path()).unwrap();
        assert_eq!(
            loaded.members[0].session_id.as_deref(),
            Some("real-session-id")
        );
    }

    #[test]
    fn test_propagate_session_id_updates_advisor() {
        let dir = temp_store();
        let team = Team {
            name: "t1".into(),
            teamplate: "tp1".into(),
            members: vec![],
            advisor: Some(TeamAdvisor {
                name: "lead".into(),
                config: TeamAdvisorConfig {
                    brofile: "advisor".into(),
                    alias: None,
                    charter: "watch things".into(),
                    context: None,
                    halt_conditions: vec![],
                    exit_conditions: vec![],
                    packet_id: None,
                    timeout_seconds: None,
                    mode: AdvisorMode::Blocking,
                },
                session_id: Some("pending".into()),
                task_history: vec!["task-adv".into()],
            }),
            project_dir: None,
            created_at: 0,
            diversity_floor: None,
        };
        save_team(&team, dir.path());

        propagate_session_id("task-adv", "advisor-session-id", dir.path());

        let loaded = load_team("t1", dir.path()).unwrap();
        assert_eq!(
            loaded
                .advisor
                .as_ref()
                .and_then(|advisor| advisor.session_id.as_deref()),
            Some("advisor-session-id")
        );
    }

    #[test]
    fn test_dissolve_team() {
        let dir = temp_store();
        let tp = Teamplate {
            name: "tp".into(),
            members: vec![TeamplateMember {
                brofile: "w".into(),
                alias: None,
                count: 1,
            }],
            advisor: None,
            diversity_floor: None,
        };
        let _team = instantiate_team(&tp, "to-dissolve", None, dir.path()).unwrap();
        assert!(load_team("to-dissolve", dir.path()).is_some());
        assert!(remove_team("to-dissolve", dir.path()));
        assert!(load_team("to-dissolve", dir.path()).is_none());
    }
}
