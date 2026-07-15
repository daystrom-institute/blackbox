use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bro_core::Provider;
use fleet_core::{
    FleetError, FleetResult, ProviderAllocationRecord, ProviderCredentialStatus,
    ProviderEnvironmentResolver, ProviderLaneRecord, ProviderQuotaStatus, ProviderSpawnEnvironment,
};
use serde::Deserialize;

const INLINE_CODEX_AUTH_ENV: &str = "BRO_HARNESS_CODEX_AUTH_JSON";

#[derive(Debug, Clone, Default, Deserialize)]
struct ProviderFileConfig {
    #[serde(default)]
    accounts: BTreeMap<String, ProviderAccountConfig>,
    #[serde(default)]
    provider_defaults: BTreeMap<Provider, ProviderDefaultConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProviderAccountConfig {
    #[serde(default)]
    provider: Option<Provider>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    max_concurrent: Option<usize>,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderDefaultConfig {
    account: String,
}

#[derive(Clone)]
pub(crate) struct HostProviderRegistry {
    lanes: Vec<ProviderLaneRecord>,
    environments: Arc<BTreeMap<String, ProviderSpawnEnvironment>>,
}

impl HostProviderRegistry {
    pub(crate) fn load(
        config_path: Option<&Path>,
        home: &Path,
        now_unix_ms: u64,
    ) -> FleetResult<Self> {
        let file = match config_path {
            Some(path) => load_provider_file(path)?,
            None => ProviderFileConfig::default(),
        };
        let provider_config_source = config_path
            .map(canonical_existing_credential_source)
            .transpose()?
            .flatten();
        let mut lanes = Vec::new();
        let mut environments = BTreeMap::new();
        for provider in dispatch_providers() {
            let default_account = file
                .provider_defaults
                .get(&provider)
                .map(|entry| entry.account.clone());
            let mut accounts = file
                .accounts
                .iter()
                .filter(|(name, account)| {
                    account.provider == Some(provider)
                        || default_account.as_deref() == Some(name.as_str())
                })
                .map(|(name, account)| (Some(name.clone()), account.clone()))
                .collect::<Vec<_>>();
            if accounts.is_empty() {
                accounts.push((None, ProviderAccountConfig::default()));
            }
            accounts.sort_by(|left, right| left.0.cmp(&right.0));

            for (account_name, account) in accounts {
                let lane_id = lane_id(provider, account_name.as_deref());
                let (mut env, mut protected_paths) = base_provider_environment(provider, home)?;
                protected_paths.extend(provider_config_source.iter().cloned());
                env.extend(account.env.clone());
                let available = !account.disabled && credentials_available(provider, &env);
                let lane = ProviderLaneRecord {
                    lane_id: lane_id.clone(),
                    provider,
                    account: account_name.clone(),
                    max_concurrent: account.max_concurrent.unwrap_or(1).max(1),
                    credential_status: if available {
                        ProviderCredentialStatus::Available
                    } else {
                        ProviderCredentialStatus::Missing
                    },
                    quota_status: ProviderQuotaStatus::Unknown,
                    cooldown_until_unix_ms: None,
                    default_for_provider: account_name == default_account,
                    updated_at_unix_ms: now_unix_ms,
                };
                lanes.push(lane);
                if available {
                    environments.insert(
                        lane_id,
                        ProviderSpawnEnvironment::with_protected_paths(env, protected_paths),
                    );
                }
            }
        }
        Ok(Self {
            lanes,
            environments: Arc::new(environments),
        })
    }

    pub(crate) fn testing(now_unix_ms: u64) -> Self {
        let mut lanes = Vec::new();
        let mut environments = BTreeMap::new();
        let home = Path::new("/");
        for provider in dispatch_providers() {
            let lane_id = lane_id(provider, None);
            lanes.push(ProviderLaneRecord {
                lane_id: lane_id.clone(),
                provider,
                account: None,
                max_concurrent: usize::MAX,
                credential_status: ProviderCredentialStatus::Available,
                quota_status: ProviderQuotaStatus::Available,
                cooldown_until_unix_ms: None,
                default_for_provider: true,
                updated_at_unix_ms: now_unix_ms,
            });
            let (environment, protected_paths) =
                base_provider_environment(provider, home).expect("test provider environment");
            environments.insert(
                lane_id,
                ProviderSpawnEnvironment::with_protected_paths(environment, protected_paths),
            );
        }
        Self {
            lanes,
            environments: Arc::new(environments),
        }
    }

    pub(crate) fn lanes(&self) -> &[ProviderLaneRecord] {
        &self.lanes
    }
}

impl ProviderEnvironmentResolver for HostProviderRegistry {
    fn resolve(
        &self,
        allocation: &ProviderAllocationRecord,
    ) -> FleetResult<ProviderSpawnEnvironment> {
        self.environments
            .get(&allocation.lane_id)
            .cloned()
            .ok_or_else(|| {
                FleetError::ProviderUnavailable(format!(
                    "credentials for lane {} are unavailable",
                    allocation.lane_id
                ))
            })
    }
}

// Called only from Fleetd::open's spawn_blocking provider-config lane.
#[allow(clippy::disallowed_methods)]
fn load_provider_file(path: &Path) -> FleetResult<ProviderFileConfig> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProviderFileConfig::default());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(FleetError::InvalidState(format!(
            "provider config {} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(FleetError::InvalidState(format!(
                "provider config {} must not be readable by group or other users",
                path.display()
            )));
        }
    }
    let body = fs::read(path)?;
    Ok(serde_json::from_slice(&body)?)
}

fn dispatch_providers() -> [Provider; 5] {
    [
        Provider::Glm,
        Provider::Deepseek,
        Provider::Minimax,
        Provider::Brodex,
        Provider::VibeBh,
    ]
}

fn lane_id(provider: Provider, account: Option<&str>) -> String {
    match account {
        Some(account) => format!("{}:{account}", provider.as_str()),
        None => format!("{}:default", provider.as_str()),
    }
}

// Called only from Fleetd::open's spawn_blocking provider-config lane.
#[allow(clippy::disallowed_methods)]
fn base_provider_environment(
    provider: Provider,
    home: &Path,
) -> FleetResult<(BTreeMap<String, String>, Vec<PathBuf>)> {
    let mut protected_paths = Vec::new();
    let environment = match provider {
        Provider::Glm | Provider::Deepseek | Provider::Minimax => {
            let config_dir = match provider {
                Provider::Glm => ".claude-zai",
                Provider::Deepseek => ".claude-ds",
                Provider::Minimax => ".claude-mm",
                _ => unreachable!(),
            };
            let mut env = BTreeMap::from([("BRO_HARNESS_TRANSPORT".into(), "anthropic".into())]);
            let config_dir = home.join(config_dir);
            let settings_path = config_dir.join("settings.json");
            if let Ok(body) = fs::read(&settings_path)
                && let Ok(settings) = serde_json::from_slice::<serde_json::Value>(&body)
            {
                for key in [
                    "ANTHROPIC_BASE_URL",
                    "ANTHROPIC_AUTH_TOKEN",
                    "ANTHROPIC_API_KEY",
                ] {
                    if let Some(value) = settings["env"][key].as_str() {
                        env.insert(key.into(), value.into());
                    }
                }
                if let Some(path) = canonical_existing_credential_source(&settings_path)? {
                    protected_paths.push(path);
                }
                if let Some(path) = canonical_existing_credential_source(&config_dir)? {
                    protected_paths.push(path);
                }
            }
            env
        }
        Provider::Brodex => {
            let mut env =
                BTreeMap::from([("BRO_HARNESS_TRANSPORT".into(), "openai-responses".into())]);
            let codex_home = home.join(".codex");
            let auth_path = codex_home.join("auth.json");
            if let Ok(raw) = fs::read_to_string(&auth_path) {
                // Parse before admission so a malformed host credential never
                // becomes a nominally available provider lane.
                if serde_json::from_str::<serde_json::Value>(&raw).is_ok() {
                    env.insert(INLINE_CODEX_AUTH_ENV.into(), raw);
                    if let Some(path) = canonical_existing_credential_source(&auth_path)? {
                        protected_paths.push(path);
                    }
                    if let Some(path) = canonical_existing_credential_source(&codex_home)? {
                        protected_paths.push(path);
                    }
                }
            }
            env
        }
        Provider::VibeBh => {
            let mut env = BTreeMap::from([
                ("BRO_HARNESS_TRANSPORT".into(), "openai-chat".into()),
                ("BRO_HARNESS_CHAT_REASONING".into(), "mistral".into()),
                ("OPENAI_BASE_URL".into(), "https://api.mistral.ai/v1".into()),
            ]);
            let dotenv = home.join(".vibe/.env");
            let process_key = std::env::var("MISTRAL_API_KEY")
                .ok()
                .filter(|value| !value.is_empty());
            let key = process_key
                .clone()
                .or_else(|| read_dotenv_key(&dotenv, "MISTRAL_API_KEY"));
            if let Some(key) = key {
                env.insert("OPENAI_API_KEY".into(), key);
                if process_key.is_none() {
                    if let Some(path) = canonical_existing_credential_source(&dotenv)? {
                        protected_paths.push(path);
                    }
                    if let Some(path) = canonical_existing_credential_source(
                        dotenv.parent().unwrap_or_else(|| Path::new(".")),
                    )? {
                        protected_paths.push(path);
                    }
                }
            }
            env
        }
        Provider::Workflow => BTreeMap::new(),
    };
    Ok((environment, protected_paths))
}

fn credentials_available(provider: Provider, env: &BTreeMap<String, String>) -> bool {
    match provider {
        Provider::Glm | Provider::Deepseek | Provider::Minimax => env
            .get("ANTHROPIC_AUTH_TOKEN")
            .or_else(|| env.get("ANTHROPIC_API_KEY"))
            .is_some_and(|value| !value.is_empty()),
        Provider::Brodex => env
            .get("OPENAI_API_KEY")
            .or_else(|| env.get(INLINE_CODEX_AUTH_ENV))
            .is_some_and(|value| !value.is_empty()),
        Provider::VibeBh => env
            .get("OPENAI_API_KEY")
            .is_some_and(|value| !value.is_empty()),
        Provider::Workflow => false,
    }
}

// Provider credential discovery runs once on fleetd's blocking startup lane.
#[allow(clippy::disallowed_methods)]
fn canonical_existing_credential_source(path: &Path) -> FleetResult<Option<PathBuf>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
        return Err(FleetError::InvalidState(format!(
            "credential source {} must be a regular file or real directory",
            path.display()
        )));
    }
    let canonical = path.canonicalize()?;
    #[cfg(unix)]
    if metadata.is_file() {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(FleetError::InvalidState(format!(
                "credential source {} must have exactly one filesystem link",
                path.display()
            )));
        }
    }
    Ok(Some(canonical))
}

// Called only from Fleetd::open's spawn_blocking provider-config lane.
#[allow(clippy::disallowed_methods)]
fn read_dotenv_key(path: &Path, key: &str) -> Option<String> {
    let body = fs::read_to_string(path).ok()?;
    body.lines().find_map(|line| {
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        let (found, value) = line.split_once('=')?;
        (found.trim() == key)
            .then(|| value.trim().trim_matches(['\'', '"']).to_string())
            .filter(|value| !value.is_empty())
    })
}

#[cfg(test)]
// Fixture setup is outside the async service runtime.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn secret_values_are_redacted_and_missing_credentials_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let registry = HostProviderRegistry::load(None, &root, 1).unwrap();
        assert!(
            registry
                .lanes()
                .iter()
                .all(|lane| lane.credential_status == ProviderCredentialStatus::Missing)
        );

        let environment = ProviderSpawnEnvironment::new(BTreeMap::from([(
            "API_TOKEN".into(),
            "super-secret".into(),
        )]));
        let rendered = format!("{environment:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("API_TOKEN"));
    }

    #[test]
    fn brodex_auth_is_inline_and_its_sources_are_worker_protected() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let codex_home = root.join(".codex");
        fs::create_dir(&codex_home).unwrap();
        let auth_path = codex_home.join("auth.json");
        let auth = r#"{"tokens":{"access_token":"access-secret","refresh_token":"refresh-secret","account_id":"acct"}}"#;
        fs::write(&auth_path, auth).unwrap();

        let registry = HostProviderRegistry::load(None, &root, 1).unwrap();
        let environment = registry.environments.get("brodex:default").unwrap();

        assert_eq!(
            environment.expose().get(INLINE_CODEX_AUTH_ENV),
            Some(&auth.to_string())
        );
        assert!(!environment.expose().contains_key("CODEX_HOME"));
        let protected = environment.protected_paths().collect::<Vec<_>>();
        assert!(protected.contains(&auth_path.canonicalize().unwrap().as_path()));
        assert!(protected.contains(&codex_home.canonicalize().unwrap().as_path()));
        let rendered = format!("{environment:?}");
        assert!(!rendered.contains("access-secret"));
        assert!(!rendered.contains(root.to_str().unwrap()));
        assert!(rendered.contains("protected_path_count"));
    }

    #[cfg(unix)]
    #[test]
    fn brodex_symlinked_auth_source_fails_closed() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let codex_home = root.join(".codex");
        fs::create_dir(&codex_home).unwrap();
        let actual = root.join("actual-auth.json");
        fs::write(
            &actual,
            r#"{"tokens":{"access_token":"access","account_id":"acct"}}"#,
        )
        .unwrap();
        symlink(&actual, codex_home.join("auth.json")).unwrap();

        assert!(matches!(
            HostProviderRegistry::load(None, &root, 1),
            Err(FleetError::InvalidState(message)) if message.contains("regular file or real directory")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn provider_config_must_be_private_and_is_never_returned_as_state() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("providers.json");
        fs::write(
            &path,
            br#"{"accounts":{"primary":{"provider":"glm","env":{"ANTHROPIC_AUTH_TOKEN":"secret"}}},"provider_defaults":{"glm":{"account":"primary"}}}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let registry = HostProviderRegistry::load(Some(&path), &root, 1).unwrap();
        let lane = registry
            .lanes()
            .iter()
            .find(|lane| lane.provider == Provider::Glm)
            .unwrap();
        assert_eq!(lane.account.as_deref(), Some("primary"));
        let serialized = serde_json::to_string(lane).unwrap();
        assert!(!serialized.contains("secret"));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(HostProviderRegistry::load(Some(&path), &root, 1).is_err());
    }
}
