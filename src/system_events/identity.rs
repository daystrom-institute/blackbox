use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// Input fields for a pending-identity request, matching payload of `bro.identity.required`.
#[derive(Debug, Clone)]
pub struct IdentityRequest {
    pub scope: String,
    pub instance: String,
    pub bro: String,
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    pub project: Option<String>,
    pub owner: Option<String>,
    pub repo: Option<String>,
}

/// Forgejo/Gitea cap usernames at 40 characters; we stay one under for
/// headroom against the underlying validator.
pub const FORGEJO_MAX_USERNAME_LEN: usize = 39;

impl IdentityRequest {
    /// Derived Forgejo-safe username, bounded to `FORGEJO_MAX_USERNAME_LEN`.
    ///
    /// Strategy:
    /// 1. Build the readable stem `bro-<bro>-<provider>-<model_slug>` where
    ///    `model_slug` is the lowercased ASCII-alphanumeric projection of the
    ///    model field. Examples:
    ///      - `haiku-4.5`                  → `haiku45`
    ///      - `claude-sonnet-4-6`          → `claudesonnet46`
    ///      - `claude-haiku-4-5-20251001`  → `claudehaiku4520251001`
    /// 2. If the stem fits, return it verbatim — short historical model names
    ///    keep their old readable shape.
    /// 3. Otherwise truncate the trailing slug portion and append a 6-hex
    ///    suffix derived from `sha256(self.model)` so two distinct long
    ///    model IDs for the same `(bro, provider)` cannot collide.
    ///
    /// The identity KEY (`subject`, `provider`, `model`) is unaffected — those
    /// remain the exact catalog IDs.
    pub fn username(&self) -> String {
        let model_slug: String = self
            .model
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_lowercase();
        let stem = format!("bro-{}-{}-{}", self.bro, self.provider, model_slug);
        if stem.len() <= FORGEJO_MAX_USERNAME_LEN {
            return stem;
        }
        let prefix = format!("bro-{}-{}-", self.bro, self.provider);
        let suffix = model_hash_suffix(&self.model);
        // `prefix` + `<trimmed-slug>` + `-` + `suffix` <= MAX.
        let reserved = prefix.len() + 1 + suffix.len();
        if reserved >= FORGEJO_MAX_USERNAME_LEN {
            // The bro/provider stem alone won't fit. Fall back to a fully
            // hash-derived username keyed by all tuple fields so the result
            // stays deterministic and under the cap.
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(self.bro.as_bytes());
            hasher.update(b":");
            hasher.update(self.provider.as_bytes());
            hasher.update(b":");
            hasher.update(self.model.as_bytes());
            let digest = hasher.finalize();
            let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
            return format!("bro-{}", hex);
        }
        let budget = FORGEJO_MAX_USERNAME_LEN - reserved;
        let trimmed_slug: String = model_slug.chars().take(budget).collect();
        format!("{prefix}{trimmed_slug}-{suffix}")
    }

    pub fn display_name(&self) -> String {
        match &self.effort {
            Some(e) => format!("{} / {} {} {}", self.bro, self.provider, self.model, e),
            None => format!("{} / {} {}", self.bro, self.provider, self.model),
        }
    }

    pub fn email(&self) -> String {
        format!("bro-{}@blackbox.local", self.bro)
    }

    pub fn subject(&self) -> String {
        format!("bro:{}", self.bro)
    }

    pub fn correlation(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::from_iter([
            ("scope".to_string(), serde_json::json!(self.scope.clone())),
            (
                "instance".to_string(),
                serde_json::json!(self.instance.clone()),
            ),
            ("bro".to_string(), serde_json::json!(self.bro.clone())),
            (
                "provider".to_string(),
                serde_json::json!(self.provider.clone()),
            ),
            ("model".to_string(), serde_json::json!(self.model.clone())),
        ])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExternalIdentity {
    pub scope: String,
    pub instance: String,
    pub subject: String,
    pub provider: String,
    pub model: String,
    pub external_user_id: String,
    pub username: String,
    pub token_ref: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<String>,
}

pub fn validate_identity_token_ref(token_ref: &str) -> Result<()> {
    if let Some(name) = token_ref.strip_prefix("secret:") {
        if name.is_empty() {
            bail!("token_ref secret name cannot be empty after 'secret:'");
        }
        if name.contains('/') || name.contains('\\') {
            bail!("token_ref secret name cannot contain path separators: '{token_ref}'");
        }
        let re = regex::Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap();
        if !re.is_match(name) {
            bail!("token_ref secret name must match [A-Za-z0-9_.-]+, got '{name}'");
        }
        Ok(())
    } else {
        bail!("token_ref must use 'secret:' prefix, got '{token_ref}'");
    }
}

pub fn validate_external_identity(id: &ExternalIdentity) -> Result<()> {
    if id.scope.is_empty() {
        bail!("identity scope cannot be empty");
    }
    if id.instance.is_empty() {
        bail!("identity instance cannot be empty");
    }
    if id.subject.is_empty() {
        bail!("identity subject cannot be empty");
    }
    if id.provider.is_empty() {
        bail!("identity provider cannot be empty");
    }
    if id.model.is_empty() {
        bail!("identity model cannot be empty");
    }
    if id.username.is_empty() {
        bail!("identity username cannot be empty");
    }
    if id.external_user_id.is_empty() {
        bail!("identity external_user_id cannot be empty");
    }
    validate_identity_token_ref(&id.token_ref)
}

/// In-memory key for a pending provisioning request.
/// Cleared when the identity is upserted (provisioning succeeded).
type PendingKey = (String, String, String, String, String);

pub struct IdentityRegistry {
    root: PathBuf,
    cache: RwLock<HashMap<(String, String), Vec<ExternalIdentity>>>,
    /// Dedup set for in-flight provisioning: (scope, instance, subject, provider, model).
    /// Prevents emitting `bro.identity.required` repeatedly in one daemon lifetime
    /// before provisioning completes. Cleared by `upsert()`.
    pending: RwLock<HashSet<PendingKey>>,
}

impl IdentityRegistry {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)
            .with_context(|| format!("creating identity registry dir {}", root.display()))?;
        let reg = Self {
            root,
            cache: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashSet::new()),
        };
        reg.reload()?;
        Ok(reg)
    }

    /// Create a registry with a non-persistent temporary root. Used as a
    /// fallback when the real root cannot be created.
    pub fn new_empty() -> Self {
        Self {
            root: std::env::temp_dir()
                .join(format!("bbox-identity-empty-{}", uuid::Uuid::new_v4())),
            cache: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashSet::new()),
        }
    }

    /// Mark provisioning as in-flight for a (scope, instance, subject, provider, model) key.
    pub fn mark_pending(
        &self,
        scope: &str,
        instance: &str,
        subject: &str,
        provider: &str,
        model: &str,
    ) {
        self.pending.write().insert((
            scope.to_string(),
            instance.to_string(),
            subject.to_string(),
            provider.to_string(),
            model.to_string(),
        ));
    }

    /// Return true if provisioning is already in-flight for this key.
    pub fn is_pending(
        &self,
        scope: &str,
        instance: &str,
        subject: &str,
        provider: &str,
        model: &str,
    ) -> bool {
        self.pending.read().contains(&(
            scope.to_string(),
            instance.to_string(),
            subject.to_string(),
            provider.to_string(),
            model.to_string(),
        ))
    }

    fn clear_pending(
        &self,
        scope: &str,
        instance: &str,
        subject: &str,
        provider: &str,
        model: &str,
    ) {
        self.pending.write().remove(&(
            scope.to_string(),
            instance.to_string(),
            subject.to_string(),
            provider.to_string(),
            model.to_string(),
        ));
    }

    fn scope_path(&self, scope: &str, instance: &str) -> PathBuf {
        self.root.join(scope).join(format!("{instance}.json"))
    }

    pub fn reload(&self) -> Result<()> {
        let mut cache = HashMap::new();
        if !self.root.exists() {
            *self.cache.write() = cache;
            return Ok(());
        }
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(_) => {
                *self.cache.write() = cache;
                return Ok(());
            }
        };
        for scope_entry in entries {
            let scope_entry = scope_entry?;
            if !scope_entry.file_type()?.is_dir() {
                continue;
            }
            let scope = scope_entry.file_name().to_string_lossy().to_string();
            for instance_entry in fs::read_dir(scope_entry.path())
                .with_context(|| format!("reading identity scope dir"))?
            {
                let instance_entry = instance_entry?;
                if instance_entry.file_type()?.is_dir() {
                    continue;
                }
                let path = instance_entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let instance = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let content = fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let identities: Vec<ExternalIdentity> = serde_json::from_str(&content)
                    .with_context(|| format!("parsing {}", path.display()))?;
                cache.insert((scope.clone(), instance), identities);
            }
        }
        *self.cache.write() = cache;
        Ok(())
    }

    pub fn upsert(&self, identity: &ExternalIdentity) -> Result<()> {
        validate_external_identity(identity)?;
        let path = self.scope_path(&identity.scope, &identity.instance);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut identities = load_file(&path)?;
        let new_key = (
            identity.subject.clone(),
            identity.provider.clone(),
            identity.model.clone(),
        );
        if let Some(pos) = identities
            .iter()
            .position(|id| (id.subject.clone(), id.provider.clone(), id.model.clone()) == new_key)
        {
            identities[pos] = identity.clone();
        } else {
            identities.push(identity.clone());
        }
        crate::json_store::with_store_lock(&path, || {
            crate::json_store::atomic_write_json_locked(&path, &identities)
        })?;
        self.cache.write().insert(
            (identity.scope.clone(), identity.instance.clone()),
            identities,
        );
        // Clear pending marker so future require_identity calls see the provisioned identity.
        self.clear_pending(
            &identity.scope,
            &identity.instance,
            &identity.subject,
            &identity.provider,
            &identity.model,
        );
        Ok(())
    }

    pub fn lookup(
        &self,
        scope: &str,
        instance: &str,
        subject: &str,
        provider: &str,
        model: &str,
    ) -> Option<ExternalIdentity> {
        self.cache
            .read()
            .get(&(scope.to_string(), instance.to_string()))
            .and_then(|ids| {
                ids.iter()
                    .find(|id| {
                        id.subject == subject && id.provider == provider && id.model == model
                    })
                    .cloned()
            })
    }

    pub fn list_all(&self) -> Vec<ExternalIdentity> {
        self.cache.read().values().flatten().cloned().collect()
    }
}

fn load_file(path: &Path) -> Result<Vec<ExternalIdentity>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&content)?)
}

fn model_hash_suffix(model: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(model.as_bytes());
    let digest = hasher.finalize();
    digest.iter().take(3).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_identity(token_ref: &str) -> ExternalIdentity {
        ExternalIdentity {
            scope: "forgejo".to_string(),
            instance: "local-forgejo15".to_string(),
            subject: "bro:keystone-review".to_string(),
            provider: "claude".to_string(),
            model: "haiku-4.5".to_string(),
            external_user_id: "123".to_string(),
            username: "bro-keystone-review-claude-haiku45".to_string(),
            token_ref: token_ref.to_string(),
            created_at: "2026-05-13T12:00:00Z".to_string(),
            last_verified_at: None,
        }
    }

    #[test]
    fn rejects_secret_ref_with_slash() {
        let err = validate_identity_token_ref("secret:forgejo/foo").unwrap_err();
        assert!(err.to_string().contains("path separators"));
    }

    #[test]
    fn rejects_secret_ref_with_backslash() {
        let err = validate_identity_token_ref("secret:forgejo\\foo").unwrap_err();
        assert!(err.to_string().contains("path separators"));
    }

    #[test]
    fn rejects_secret_ref_empty_name() {
        let err = validate_identity_token_ref("secret:").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn rejects_token_ref_without_secret_prefix() {
        let err = validate_identity_token_ref("plain-token-value").unwrap_err();
        assert!(err.to_string().contains("secret:"));
    }

    #[test]
    fn accepts_valid_secret_ref() {
        assert!(validate_identity_token_ref("secret:forgejo-bro-token").is_ok());
        assert!(validate_identity_token_ref("secret:my_token.v2").is_ok());
    }

    #[test]
    fn validate_rejects_bad_token_ref() {
        let id = test_identity("secret:forgejo/foo");
        assert!(
            validate_external_identity(&id)
                .unwrap_err()
                .to_string()
                .contains("path separators")
        );
    }

    #[test]
    fn validate_rejects_empty_scope() {
        let mut id = test_identity("secret:valid-token");
        id.scope = String::new();
        assert!(
            validate_external_identity(&id)
                .unwrap_err()
                .to_string()
                .contains("scope")
        );
    }

    #[test]
    fn validate_accepts_valid() {
        assert!(validate_external_identity(&test_identity("secret:forgejo-bro-token")).is_ok());
    }

    #[test]
    fn identity_registry_upsert_and_lookup() {
        let dir = tempdir().unwrap();
        let reg = IdentityRegistry::new(dir.path().to_path_buf()).unwrap();
        reg.upsert(&test_identity("secret:forgejo-bro-token"))
            .unwrap();
        let found = reg
            .lookup(
                "forgejo",
                "local-forgejo15",
                "bro:keystone-review",
                "claude",
                "haiku-4.5",
            )
            .unwrap();
        assert_eq!(found.token_ref, "secret:forgejo-bro-token");
    }

    #[test]
    fn identity_registry_upsert_replaces_existing() {
        let dir = tempdir().unwrap();
        let reg = IdentityRegistry::new(dir.path().to_path_buf()).unwrap();
        reg.upsert(&test_identity("secret:token-v1")).unwrap();
        let mut id2 = test_identity("secret:token-v2");
        id2.external_user_id = "456".to_string();
        reg.upsert(&id2).unwrap();
        let found = reg
            .lookup(
                "forgejo",
                "local-forgejo15",
                "bro:keystone-review",
                "claude",
                "haiku-4.5",
            )
            .unwrap();
        assert_eq!(found.external_user_id, "456");
        assert_eq!(found.token_ref, "secret:token-v2");
        assert_eq!(reg.list_all().len(), 1);
    }

    #[test]
    fn identity_registry_lookup_missing_returns_none() {
        let dir = tempdir().unwrap();
        let reg = IdentityRegistry::new(dir.path().to_path_buf()).unwrap();
        reg.upsert(&test_identity("secret:token")).unwrap();
        assert!(
            reg.lookup(
                "forgejo",
                "local-forgejo15",
                "bro:other",
                "claude",
                "haiku-4.5"
            )
            .is_none()
        );
    }

    fn req(bro: &str, provider: &str, model: &str) -> IdentityRequest {
        IdentityRequest {
            scope: "forgejo".to_string(),
            instance: "local-forgejo15".to_string(),
            bro: bro.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            effort: None,
            project: None,
            owner: None,
            repo: None,
        }
    }

    #[test]
    fn short_legacy_model_preserves_readable_username() {
        // Historical readable shape — `haiku-4.5` → `haiku45` — fits under
        // the bound and must be returned verbatim.
        let u = req("keystone-review", "claude", "haiku-4.5").username();
        assert_eq!(u, "bro-keystone-review-claude-haiku45");
        assert!(u.len() <= FORGEJO_MAX_USERNAME_LEN);
    }

    #[test]
    fn catalog_sonnet_model_id_still_fits_under_limit() {
        // `claude-sonnet-4-6` → 14-char slug; full stem is 39 chars
        // exactly — boundary case, must use the readable stem.
        let u = req("keystone-impl", "claude", "claude-sonnet-4-6").username();
        assert_eq!(u, "bro-keystone-impl-claude-claudesonnet46");
        assert!(u.len() <= FORGEJO_MAX_USERNAME_LEN);
    }

    #[test]
    fn long_catalog_haiku_model_id_truncates_and_hashes() {
        // `claude-haiku-4-5-20251001` slug is 21 chars; full readable stem
        // is 48 chars. Username must be bounded AND retain the
        // bro/provider prefix for operator legibility.
        let u = req("keystone-review", "claude", "claude-haiku-4-5-20251001").username();
        assert!(
            u.len() <= FORGEJO_MAX_USERNAME_LEN,
            "username '{u}' exceeds {FORGEJO_MAX_USERNAME_LEN} chars"
        );
        assert!(
            u.starts_with("bro-keystone-review-claude-"),
            "username '{u}' must keep the bro/provider prefix"
        );
        // Suffix is 6 lowercase hex chars from sha256(model).
        let tail = &u[u.len() - 6..];
        assert!(
            tail.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "tail '{tail}' must be 6 lowercase hex chars"
        );
        // The dash before the hex suffix separates the truncated slug.
        assert_eq!(
            u.as_bytes()[u.len() - 7],
            b'-',
            "the hex suffix must be dash-separated from the truncated slug"
        );
    }

    #[test]
    fn distinct_long_models_for_same_bro_do_not_collide() {
        // Two long model IDs that share the same readable prefix must
        // still produce distinct usernames once truncated.
        let a = req("keystone-review", "claude", "claude-haiku-4-5-20251001").username();
        let b = req("keystone-review", "claude", "claude-haiku-4-5-20260315").username();
        assert_ne!(
            a, b,
            "two different long model IDs must not collide on the username after truncation"
        );
        // And both must be deterministic.
        let a2 = req("keystone-review", "claude", "claude-haiku-4-5-20251001").username();
        assert_eq!(a, a2, "username must be deterministic for the same tuple");
    }

    #[test]
    fn username_is_deterministic_across_calls() {
        let r = req("keystone-impl", "claude", "claude-sonnet-4-6");
        assert_eq!(r.username(), r.username());
    }

    #[test]
    fn pathologically_long_bro_falls_back_to_hash_only_username() {
        // Bro name long enough that even `bro-<bro>-<provider>-` exceeds
        // the budget — fall back to a deterministic hash-only username
        // so the result still fits.
        let long_bro = "a".repeat(40);
        let u = req(&long_bro, "claude", "claude-haiku-4-5-20251001").username();
        assert!(
            u.len() <= FORGEJO_MAX_USERNAME_LEN,
            "fallback username '{u}' must still fit"
        );
        assert!(u.starts_with("bro-"));
    }

    #[test]
    fn system_events_readme_shows_catalog_model_and_bounded_username() {
        // examples/system-events/README.md drives operator copy-paste —
        // it must use the catalog model ID and the bounded username
        // shape produced by IdentityRequest::username() for the long ID.
        let readme = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/system-events/README.md"
        ))
        .unwrap();
        assert!(
            readme.contains("claude-haiku-4-5-20251001"),
            "README must use the catalog model ID for the Forgejo emit example"
        );
        // The computed bounded username for the catalog model ID.
        let expected = req("keystone-review", "claude", "claude-haiku-4-5-20251001").username();
        assert!(
            readme.contains(&expected),
            "README must show the bounded username '{expected}' produced by IdentityRequest::username() for the catalog model ID"
        );
        // No stale short slugs from the legacy example.
        assert!(
            !readme.contains("haiku45\""),
            "README must not reference the legacy 'haiku45' username slug"
        );
        assert!(
            !readme.contains("\"haiku-4.5\""),
            "README must not reference the legacy 'haiku-4.5' model ID in JSON-quoted contexts"
        );
    }

    #[test]
    fn identity_registry_reload_from_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let reg = IdentityRegistry::new(path.clone()).unwrap();
            reg.upsert(&test_identity("secret:persisted-token"))
                .unwrap();
        }
        let reg2 = IdentityRegistry::new(path).unwrap();
        let found = reg2
            .lookup(
                "forgejo",
                "local-forgejo15",
                "bro:keystone-review",
                "claude",
                "haiku-4.5",
            )
            .unwrap();
        assert_eq!(found.token_ref, "secret:persisted-token");
    }
}
