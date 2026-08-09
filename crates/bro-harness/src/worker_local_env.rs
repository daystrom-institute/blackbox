//! Materialize provider state that belongs to the worker machine.
//!
//! An off-host daemon composes policy, model, account selection, and the
//! credential profile to use. It cannot read the worker's HOME. The standalone
//! harness performs this final, tightly bounded materialization before any
//! transport starts: lift allowlisted provider variables from an explicitly
//! named local settings file, read one explicitly named dotenv credential, and
//! build the Codex instruction-suppressed home overlay beside worker state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const LOCAL_SETTINGS_FILE: &str = "BRO_HARNESS_LOCAL_SETTINGS_FILE";
const LOCAL_DOTENV_FILE: &str = "BRO_HARNESS_LOCAL_DOTENV_FILE";
const SUPPRESS_CODEX_INSTRUCTIONS: &str = "BRO_HARNESS_SUPPRESS_CODEX_INSTRUCTIONS";
const SPAWN_SCRUB: &str = "BRO_HARNESS_SPAWN_SCRUB";
const SETTINGS_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
];

/// Resolve the worker-local markers in the current process environment.
///
/// This runs once, at standalone process startup, before the agent loop or any
/// provider transport is constructed. Environment mutation is therefore
/// single-threaded process initialization, not live-session reconfiguration.
#[allow(clippy::disallowed_methods)]
pub fn materialize_process_env() -> Result<()> {
    let before = std::env::vars().collect::<BTreeMap<_, _>>();
    let after = materialize(&before)?;
    for (key, value) in after {
        if before.get(&key) == Some(&value) {
            continue;
        }
        // SAFETY: the standalone harness calls this once at the first line of
        // main, before it spawns any application task or transport thread.
        unsafe { std::env::set_var(key, value) };
    }
    Ok(())
}

#[allow(clippy::disallowed_methods)]
fn materialize(input: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut output = input.clone();
    let mut materialized_secret_keys = Vec::new();

    if let Some(path) = non_empty(input, LOCAL_SETTINGS_FILE) {
        materialized_secret_keys.extend_from_slice(SETTINGS_KEYS);
        if !anthropic_transport_ready(&output) {
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("read worker-local provider settings {path}"))?;
            let value: serde_json::Value = serde_json::from_str(&body)
                .with_context(|| format!("parse worker-local provider settings {path}"))?;
            let env = value
                .get("env")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "worker-local provider settings {path} has no object-valued `env`"
                    )
                })?;
            for key in SETTINGS_KEYS {
                if output.get(*key).is_some_and(|value| !value.is_empty()) {
                    continue;
                }
                if let Some(value) = env.get(*key).and_then(serde_json::Value::as_str)
                    && !value.is_empty()
                {
                    output.insert((*key).to_string(), value.to_string());
                }
            }
        }
    }

    if let Some(path) = non_empty(input, LOCAL_DOTENV_FILE) {
        materialized_secret_keys.push("OPENAI_API_KEY");
        if output.get("OPENAI_API_KEY").is_none_or(String::is_empty) {
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("read worker-local provider dotenv {path}"))?;
            let key = dotenv_value(&body, "MISTRAL_API_KEY").ok_or_else(|| {
                anyhow::anyhow!("worker-local provider dotenv {path} has no MISTRAL_API_KEY")
            })?;
            output.insert("OPENAI_API_KEY".to_string(), key);
        }
    }

    extend_spawn_scrub(&mut output, materialized_secret_keys);

    if non_empty(input, SUPPRESS_CODEX_INSTRUCTIONS).is_some_and(truthy) {
        let base = non_empty(&output, "CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| non_empty(&output, "HOME").map(|home| Path::new(home).join(".codex")))
            .ok_or_else(|| {
                anyhow::anyhow!("Codex instruction suppression requires CODEX_HOME or HOME")
            })?;
        let bro_home = non_empty(&output, "BRO_HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("Codex instruction suppression requires BRO_HOME"))?;
        let overlay = prepare_codex_suppressed_home(&base, &bro_home)?;
        output.insert(
            "CODEX_HOME".to_string(),
            overlay.to_string_lossy().into_owned(),
        );
    }

    Ok(output)
}

fn extend_spawn_scrub<'a>(
    env: &mut BTreeMap<String, String>,
    keys: impl IntoIterator<Item = &'a str>,
) {
    let keys = keys.into_iter().collect::<Vec<_>>();
    if keys.is_empty() {
        return;
    }
    let mut scrub = env
        .get(SPAWN_SCRUB)
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    scrub.extend(keys.into_iter().map(str::to_string));
    env.insert(
        SPAWN_SCRUB.to_string(),
        scrub.into_iter().collect::<Vec<_>>().join(","),
    );
}

fn non_empty<'a>(env: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn anthropic_transport_ready(env: &BTreeMap<String, String>) -> bool {
    non_empty(env, "ANTHROPIC_BASE_URL").is_some()
        && (non_empty(env, "ANTHROPIC_AUTH_TOKEN").is_some()
            || non_empty(env, "ANTHROPIC_API_KEY").is_some())
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

fn dotenv_value(body: &str, key: &str) -> Option<String> {
    for raw in body.lines() {
        let line = raw.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some(rest) = line
            .strip_prefix(key)
            .and_then(|rest| rest.trim_start().strip_prefix('='))
        else {
            continue;
        };
        let value = rest.trim().trim_matches('"').trim_matches('\'');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[allow(clippy::disallowed_methods)]
fn prepare_codex_suppressed_home(base_home: &Path, bro_home: &Path) -> Result<PathBuf> {
    let overlay = bro_home.join("generated").join(format!(
        "codex-home-suppressed-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&overlay)
        .with_context(|| format!("create Codex suppressed home {}", overlay.display()))?;
    let entries = std::fs::read_dir(base_home)
        .with_context(|| format!("read worker Codex home {}", base_home.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name == "AGENTS.md" || name == "AGENTS.override.md" {
            continue;
        }
        symlink_entry(&entry.path(), &overlay.join(name))?;
    }
    Ok(overlay)
}

#[cfg(unix)]
fn symlink_entry(source: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(not(unix))]
fn symlink_entry(_source: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Codex suppressed home overlays require symlink support",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_settings_lift_only_allowlisted_missing_values() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let settings = root.join("settings.json");
        std::fs::write(
            &settings,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://worker.invalid","ANTHROPIC_AUTH_TOKEN":"worker-token","UNRELATED_SECRET":"never"}}"#,
        )
        .unwrap();
        let input = BTreeMap::from([
            (
                LOCAL_SETTINGS_FILE.to_string(),
                settings.to_string_lossy().into_owned(),
            ),
            (
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "explicit-token".to_string(),
            ),
            (SPAWN_SCRUB.to_string(), "BRO_HOME,EXISTING".to_string()),
        ]);
        let output = materialize(&input).unwrap();
        assert_eq!(
            output.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("https://worker.invalid")
        );
        assert_eq!(
            output.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
            Some("explicit-token")
        );
        assert!(!output.contains_key("UNRELATED_SECRET"));
        let scrub = output.get(SPAWN_SCRUB).unwrap();
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "BRO_HOME",
            "EXISTING",
        ] {
            assert!(
                scrub.split(',').any(|candidate| candidate == key),
                "{scrub}"
            );
        }
    }

    #[test]
    fn worker_dotenv_materializes_mistral_key() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let dotenv = root.join("provider.env");
        std::fs::write(&dotenv, "export MISTRAL_API_KEY='worker-key'\nOTHER=nope\n").unwrap();
        let input = BTreeMap::from([(
            LOCAL_DOTENV_FILE.to_string(),
            dotenv.to_string_lossy().into_owned(),
        )]);
        assert_eq!(
            materialize(&input)
                .unwrap()
                .get("OPENAI_API_KEY")
                .map(String::as_str),
            Some("worker-key")
        );
        assert!(
            materialize(&input)
                .unwrap()
                .get(SPAWN_SCRUB)
                .unwrap()
                .split(',')
                .any(|candidate| candidate == "OPENAI_API_KEY")
        );
    }

    #[test]
    fn explicit_worker_credentials_do_not_require_fallback_files_but_are_scrubbed() {
        let input = BTreeMap::from([
            (
                LOCAL_SETTINGS_FILE.to_string(),
                "/missing/settings.json".to_string(),
            ),
            (
                LOCAL_DOTENV_FILE.to_string(),
                "/missing/provider.env".to_string(),
            ),
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "https://explicit.invalid".to_string(),
            ),
            (
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "explicit-token".to_string(),
            ),
            ("OPENAI_API_KEY".to_string(), "explicit-key".to_string()),
        ]);

        let output = materialize(&input).unwrap();
        let scrub = output.get(SPAWN_SCRUB).unwrap();
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "OPENAI_API_KEY",
        ] {
            assert!(
                scrub.split(',').any(|candidate| candidate == key),
                "{scrub}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn codex_suppression_is_built_from_worker_home_and_skips_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let codex = root.join(".codex");
        let bro = root.join("bro");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(codex.join("auth.json"), "{}\n").unwrap();
        std::fs::write(codex.join("AGENTS.md"), "do not inherit\n").unwrap();
        let input = BTreeMap::from([
            (
                "CODEX_HOME".to_string(),
                codex.to_string_lossy().into_owned(),
            ),
            ("BRO_HOME".to_string(), bro.to_string_lossy().into_owned()),
            (SUPPRESS_CODEX_INSTRUCTIONS.to_string(), "1".to_string()),
        ]);
        let output = materialize(&input).unwrap();
        let overlay = PathBuf::from(output.get("CODEX_HOME").unwrap());
        assert_eq!(
            std::fs::read_link(overlay.join("auth.json")).unwrap(),
            codex.join("auth.json")
        );
        assert!(!overlay.join("AGENTS.md").exists());
    }
}
