//! The configuration page's view of the loaded daemon config.
//!
//! [`redacted_config`] classifies every field of [`Config`] by destructuring
//! each config type without a rest pattern, so a field added to the schema
//! does not compile until it is classified here. A secret-bearing field, and
//! every token or credential file path, becomes [`ConfigValue::Redacted`],
//! which carries nothing: not the value, its length, or its file name. A URL
//! that carries user information is redacted whole. The surface table is
//! rendered by the surfaces section, not here.

use std::fmt::Display;
use std::path::{Path, PathBuf};

use bbox_config::config::{
    CodeCollectionConfig, CodeCollectionProducerConfig, Config, ConnectorProducerConfig,
    ConnectorProfile, ConnectorScopeGrant, DaemonConfig, ExecutorKind, IndexConfig, LspConfig,
    McpOAuthConfig, ProducerScopeClaimPolicy, ProvenanceConfig, ProviderConfig, ResolvedPathConfig,
    RetainedConversationSource, SourceConnectorsConfig, TranscriptConfig,
};

/// The fixed text every redacted value renders as.
pub(crate) const REDACTED: &str = "[redacted]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigValue {
    /// The field is not set (an absent option or an empty sentinel path).
    Unset,
    Text(String),
    List(Vec<String>),
    /// A secret or credential reference is configured; its value is withheld.
    Redacted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigRow {
    /// Dotted path of the field from the config root, for example
    /// `daemon.port` or `code_collection.producers[0].token_file`.
    pub(crate) key: String,
    pub(crate) value: ConfigValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigGroup {
    /// The top-level config table this group renders.
    pub(crate) name: &'static str,
    pub(crate) rows: Vec<ConfigRow>,
}

/// The effective config as grouped rows with every secret withheld.
pub(crate) fn redacted_config(config: &Config) -> Vec<ConfigGroup> {
    let Config {
        daemon,
        index,
        code_collection,
        source_connectors,
        provenance,
        providers,
        lsp,
        transcripts,
        // Rendered as the surface table with each surface's visible tools.
        surfaces: _,
        paths,
    } = config;
    vec![
        group("daemon", |rows| daemon_rows(rows, daemon)),
        group("index", |rows| index_rows(rows, index)),
        group("code_collection", |rows| {
            code_collection_rows(rows, code_collection)
        }),
        group("source_connectors", |rows| {
            source_connector_rows(rows, source_connectors)
        }),
        group("provenance", |rows| provenance_rows(rows, provenance)),
        group("providers", |rows| provider_rows(rows, providers)),
        group("lsp", |rows| lsp_rows(rows, lsp)),
        group("transcripts", |rows| transcript_rows(rows, transcripts)),
        group("paths", |rows| path_rows(rows, paths)),
    ]
}

fn group(name: &'static str, fill: impl FnOnce(&mut Rows)) -> ConfigGroup {
    let mut rows = Rows {
        prefix: name.to_string(),
        rows: Vec::new(),
    };
    fill(&mut rows);
    ConfigGroup {
        name,
        rows: rows.rows,
    }
}

/// Row builder under one key prefix.
struct Rows {
    prefix: String,
    rows: Vec<ConfigRow>,
}

impl Rows {
    fn push(&mut self, key: &str, value: ConfigValue) {
        self.rows.push(ConfigRow {
            key: format!("{}.{key}", self.prefix),
            value,
        });
    }

    fn nested(&mut self, key: &str, fill: impl FnOnce(&mut Rows)) {
        let mut inner = Rows {
            prefix: format!("{}.{key}", self.prefix),
            rows: Vec::new(),
        };
        fill(&mut inner);
        self.rows.extend(inner.rows);
    }

    fn text(&mut self, key: &str, value: impl Display) {
        self.push(key, ConfigValue::Text(value.to_string()));
    }

    fn optional(&mut self, key: &str, value: Option<impl Display>) {
        match value {
            Some(value) => self.text(key, value),
            None => self.push(key, ConfigValue::Unset),
        }
    }

    fn path(&mut self, key: &str, value: &Path) {
        self.text(key, value.display());
    }

    fn optional_path(&mut self, key: &str, value: Option<&PathBuf>) {
        self.optional(key, value.map(|path| path.display()));
    }

    fn list<T: Display>(&mut self, key: &str, values: impl IntoIterator<Item = T>) {
        self.push(
            key,
            ConfigValue::List(values.into_iter().map(|value| value.to_string()).collect()),
        );
    }

    /// A secret-bearing field: redacted when configured, unset otherwise.
    fn secret(&mut self, key: &str, configured: bool) {
        self.push(
            key,
            if configured {
                ConfigValue::Redacted
            } else {
                ConfigValue::Unset
            },
        );
    }

    /// A URL: redacted whole when it carries user information or does not
    /// parse, so a credential embedded in it never renders.
    fn url(&mut self, key: &str, value: Option<&str>) {
        let Some(raw) = value else {
            self.push(key, ConfigValue::Unset);
            return;
        };
        match reqwest::Url::parse(raw) {
            Ok(url) if url.username().is_empty() && url.password().is_none() => self.text(key, raw),
            _ => self.push(key, ConfigValue::Redacted),
        }
    }
}

fn daemon_rows(rows: &mut Rows, daemon: &DaemonConfig) {
    let DaemonConfig {
        port,
        bind,
        advertise_url,
        mcp_name,
        mcp_allowed_hosts,
        shutdown_grace_secs,
        task_ttl_ms,
        mcp_session_keepalive_secs,
        mcp_modern_lifecycle,
        ui_listen,
        poller_min_interval_secs,
        checkout_lifecycle_writer_wait_ms,
        executor,
        fleetd_endpoint,
        fleetd_token_file,
        fleetd_tls_fingerprint,
        fleetd_worker_home,
        fleetd_worker_bro_home,
        no_checkout_authority,
        admin_token_file,
        mcp_require_bearer,
        service_token_file,
        mcp_trusted_peer_networks,
        mcp_oauth,
    } = daemon;
    rows.text("port", port);
    rows.text("bind", bind);
    rows.url("advertise_url", advertise_url.as_deref());
    rows.text("mcp_name", mcp_name);
    rows.list("mcp_allowed_hosts", mcp_allowed_hosts);
    rows.text("shutdown_grace_secs", shutdown_grace_secs);
    rows.text("task_ttl_ms", task_ttl_ms);
    rows.text("mcp_session_keepalive_secs", mcp_session_keepalive_secs);
    rows.text("mcp_modern_lifecycle", mcp_modern_lifecycle);
    rows.optional("ui_listen", ui_listen.as_ref());
    rows.text("poller_min_interval_secs", poller_min_interval_secs);
    rows.text(
        "checkout_lifecycle_writer_wait_ms",
        checkout_lifecycle_writer_wait_ms,
    );
    rows.text(
        "executor",
        match executor {
            ExecutorKind::Fleetd => "fleetd",
            ExecutorKind::Local => "local",
        },
    );
    rows.url("fleetd_endpoint", fleetd_endpoint.as_deref());
    rows.secret("fleetd_token_file", fleetd_token_file.is_some());
    // A pinned certificate fingerprint is credential material for the
    // executor channel and is withheld like the token beside it.
    rows.secret("fleetd_tls_fingerprint", fleetd_tls_fingerprint.is_some());
    rows.optional_path("fleetd_worker_home", fleetd_worker_home.as_ref());
    rows.optional_path("fleetd_worker_bro_home", fleetd_worker_bro_home.as_ref());
    rows.text("no_checkout_authority", no_checkout_authority);
    rows.secret("admin_token_file", admin_token_file.is_some());
    rows.text("mcp_require_bearer", mcp_require_bearer);
    rows.secret("service_token_file", service_token_file.is_some());
    rows.list("mcp_trusted_peer_networks", mcp_trusted_peer_networks);
    match mcp_oauth {
        None => rows.push("mcp_oauth", ConfigValue::Unset),
        Some(oauth) => rows.nested("mcp_oauth", |rows| mcp_oauth_rows(rows, oauth)),
    }
}

/// The OAuth authorization server the MCP gate trusts. Nothing here is a
/// credential: the issuer, audience and key set location are public URLs
/// and the group is a name.
fn mcp_oauth_rows(rows: &mut Rows, oauth: &McpOAuthConfig) {
    let McpOAuthConfig {
        issuer,
        audience,
        required_group,
        jwks_uri,
    } = oauth;
    rows.url("issuer", Some(issuer));
    rows.url("audience", Some(audience));
    rows.text("required_group", required_group);
    rows.url("jwks_uri", jwks_uri.as_deref());
}

fn index_rows(rows: &mut Rows, index: &IndexConfig) {
    let IndexConfig {
        reindex_interval_secs,
        reindex_startup_delay_secs,
        background_full_reindex_ticks,
    } = index;
    rows.text("reindex_interval_secs", reindex_interval_secs);
    rows.optional(
        "reindex_startup_delay_secs",
        reindex_startup_delay_secs.as_ref(),
    );
    rows.optional(
        "background_full_reindex_ticks",
        background_full_reindex_ticks.as_ref(),
    );
}

fn code_collection_rows(rows: &mut Rows, config: &CodeCollectionConfig) {
    let CodeCollectionConfig {
        enabled,
        git_transport_enabled,
        knowledge_transport_enabled,
        max_manifest_files,
        max_manifest_logical_bytes,
        max_open_uploads_per_producer,
        retained_generations,
        unreferenced_blob_grace_hours,
        max_migration_survivor_rows,
        max_migration_survivor_bytes,
        max_git_history_commits,
        max_git_history_logical_bytes,
        cutback_retry_base_secs,
        cutback_retry_max_secs,
        cutback_max_attempts,
        producers,
    } = config;
    rows.text("enabled", enabled);
    rows.text("git_transport_enabled", git_transport_enabled);
    rows.text("knowledge_transport_enabled", knowledge_transport_enabled);
    rows.text("max_manifest_files", max_manifest_files);
    rows.text("max_manifest_logical_bytes", max_manifest_logical_bytes);
    rows.text(
        "max_open_uploads_per_producer",
        max_open_uploads_per_producer,
    );
    rows.text("retained_generations", retained_generations);
    rows.text(
        "unreferenced_blob_grace_hours",
        unreferenced_blob_grace_hours,
    );
    rows.text("max_migration_survivor_rows", max_migration_survivor_rows);
    rows.text("max_migration_survivor_bytes", max_migration_survivor_bytes);
    rows.text("max_git_history_commits", max_git_history_commits);
    rows.text(
        "max_git_history_logical_bytes",
        max_git_history_logical_bytes,
    );
    rows.text("cutback_retry_base_secs", cutback_retry_base_secs);
    rows.text("cutback_retry_max_secs", cutback_retry_max_secs);
    rows.text("cutback_max_attempts", cutback_max_attempts);
    for (index, producer) in producers.iter().enumerate() {
        rows.nested(&format!("producers[{index}]"), |rows| {
            code_producer_rows(rows, producer)
        });
    }
}

fn code_producer_rows(rows: &mut Rows, producer: &CodeCollectionProducerConfig) {
    let CodeCollectionProducerConfig {
        producer_id,
        token_file,
        token_files,
        scopes,
        claim_scopes,
        // Parsed for compatibility, ignored, and never serialized.
        retired_auto_publish: _,
    } = producer;
    rows.text("producer_id", producer_id);
    rows.secret("token_file", !token_file.as_os_str().is_empty());
    rows.secret("token_files", !token_files.is_empty());
    rows.list(
        "scopes",
        scopes
            .iter()
            .map(|scope| format!("{}:{}", scope.repo_id(), scope.bbox_root_relpath())),
    );
    rows.text(
        "claim_scopes",
        match claim_scopes {
            ProducerScopeClaimPolicy::None => "none",
            ProducerScopeClaimPolicy::Unclaimed => "unclaimed",
        },
    );
}

fn source_connector_rows(rows: &mut Rows, config: &SourceConnectorsConfig) {
    let SourceConnectorsConfig {
        enabled,
        producers,
        retained_conversations,
    } = config;
    rows.text("enabled", enabled);
    for (index, producer) in producers.iter().enumerate() {
        rows.nested(&format!("producers[{index}]"), |rows| {
            connector_producer_rows(rows, producer)
        });
    }
    for (index, retained) in retained_conversations.iter().enumerate() {
        rows.nested(&format!("retained_conversations[{index}]"), |rows| {
            let RetainedConversationSource {
                connector_source_id,
                connector_kind,
                workspace_id,
                remote_authority,
            } = retained;
            rows.text("connector_source_id", connector_source_id);
            rows.text("connector_kind", connector_kind);
            rows.text("workspace_id", workspace_id);
            rows.text("remote_authority", remote_authority);
        });
    }
}

fn connector_producer_rows(rows: &mut Rows, producer: &ConnectorProducerConfig) {
    let ConnectorProducerConfig {
        producer_id,
        token_file,
        token_files,
        scopes,
    } = producer;
    rows.text("producer_id", producer_id);
    rows.secret("token_file", !token_file.as_os_str().is_empty());
    rows.secret("token_files", !token_files.is_empty());
    for (index, grant) in scopes.iter().enumerate() {
        rows.nested(&format!("scopes[{index}]"), |rows| {
            let ConnectorScopeGrant {
                connector_source_id,
                connector_kind,
                remote_authority,
                profile,
            } = grant;
            rows.text("connector_source_id", connector_source_id);
            rows.text("connector_kind", connector_kind);
            rows.text("remote_authority", remote_authority);
            rows.text(
                "profile",
                match profile {
                    ConnectorProfile::File => "file",
                    ConnectorProfile::Conversation => "conversation",
                    ConnectorProfile::Transcript => "transcript",
                },
            );
        });
    }
}

fn provenance_rows(rows: &mut Rows, provenance: &ProvenanceConfig) {
    let ProvenanceConfig {
        git_notes_namespace,
    } = provenance;
    rows.text("git_notes_namespace", git_notes_namespace);
}

fn provider_rows(rows: &mut Rows, providers: &ProviderConfig) {
    let ProviderConfig {
        claude_bin,
        codex_bin,
        copilot_bin,
        vibe_bin,
        vibe_session_dir,
        extra_path,
    } = providers;
    rows.optional("claude_bin", claude_bin.as_ref());
    rows.optional("codex_bin", codex_bin.as_ref());
    rows.optional("copilot_bin", copilot_bin.as_ref());
    rows.optional("vibe_bin", vibe_bin.as_ref());
    rows.optional_path("vibe_session_dir", vibe_session_dir.as_ref());
    rows.list("extra_path", extra_path.iter().map(|path| path.display()));
}

fn lsp_rows(rows: &mut Rows, lsp: &LspConfig) {
    let LspConfig {
        idle_timeout_secs,
        request_timeout_secs,
        jdtls_init_timeout_secs,
        jdtls_ready_timeout_secs,
        rust_analyzer_init_timeout_secs,
        roslyn_init_timeout_secs,
        jdtls_bin,
        rust_analyzer_bin,
        roslyn_lsp_bin,
    } = lsp;
    rows.text("idle_timeout_secs", idle_timeout_secs);
    rows.text("request_timeout_secs", request_timeout_secs);
    rows.text("jdtls_init_timeout_secs", jdtls_init_timeout_secs);
    rows.text("jdtls_ready_timeout_secs", jdtls_ready_timeout_secs);
    rows.text(
        "rust_analyzer_init_timeout_secs",
        rust_analyzer_init_timeout_secs,
    );
    rows.text("roslyn_init_timeout_secs", roslyn_init_timeout_secs);
    rows.optional("jdtls_bin", jdtls_bin.as_ref());
    rows.optional("rust_analyzer_bin", rust_analyzer_bin.as_ref());
    rows.optional("roslyn_lsp_bin", roslyn_lsp_bin.as_ref());
}

fn transcript_rows(rows: &mut Rows, transcripts: &TranscriptConfig) {
    let TranscriptConfig { roots, codex_root } = transcripts;
    rows.optional("roots", roots.as_ref());
    rows.optional_path("codex_root", codex_root.as_ref());
}

fn path_rows(rows: &mut Rows, paths: &ResolvedPathConfig) {
    let ResolvedPathConfig {
        state_dir,
        knowledge_path,
        gaps_path,
        threads_path,
        notes_path,
        pins_path,
        checkout_mutations_path,
        producer_claims_path,
        projects_path,
        packets_dir,
        artifacts_dir,
        bro_home,
        index_path,
        vectors_path,
        backup_dir,
        global_common_md,
        global_claude_md,
        global_codex_md,
        defaults_memories_dir,
        user_memories_dir,
    } = paths;
    rows.path("state_dir", state_dir);
    rows.path("knowledge_path", knowledge_path);
    rows.path("gaps_path", gaps_path);
    rows.path("threads_path", threads_path);
    rows.path("notes_path", notes_path);
    rows.path("pins_path", pins_path);
    rows.path("checkout_mutations_path", checkout_mutations_path);
    rows.path("producer_claims_path", producer_claims_path);
    rows.path("projects_path", projects_path);
    rows.path("packets_dir", packets_dir);
    rows.path("artifacts_dir", artifacts_dir);
    rows.path("bro_home", bro_home);
    rows.path("index_path", index_path);
    rows.path("vectors_path", vectors_path);
    rows.path("backup_dir", backup_dir);
    rows.path("global_common_md", global_common_md);
    rows.path("global_claude_md", global_claude_md);
    rows.path("global_codex_md", global_codex_md);
    rows.path("defaults_memories_dir", defaults_memories_dir);
    rows.optional_path("user_memories_dir", user_memories_dir.as_ref());
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use bbox_config::config::CodeCollectionProducerConfig;
    use bbox_corpus_core::identity::PublishedScope;

    /// Secret values the populated test config carries. None may appear in
    /// any rendered value.
    pub(crate) const SECRET_SENTINELS: [&str; 8] = [
        "sentinel-fleetd-token-file",
        "sentinel-fleetd-tls-fingerprint",
        "sentinel-admin-token-file",
        "sentinel-service-token-file",
        "sentinel-code-token-file",
        "sentinel-code-token-files",
        "sentinel-connector-token-file",
        "sentinel-connector-token-files",
    ];

    /// Field names that mark a value as secret-bearing, independent of the
    /// classification above: a field whose name contains one of these must
    /// render redacted.
    const SECRET_FIELD_MARKERS: [&str; 6] = [
        "token",
        "secret",
        "password",
        "credential",
        "api_key",
        "fingerprint",
    ];

    /// Every field of the schema set, every secret set to a sentinel, and
    /// every list of tables given at least one entry, so the field walk
    /// below reaches every leaf. Producers set both token forms, a shape
    /// config validation refuses, so each secret field carries a value.
    pub(crate) fn populate_every_field(config: &mut Config) {
        let daemon = &mut config.daemon;
        daemon.advertise_url = Some("http://daemon.example.test:7264".into());
        daemon.ui_listen = Some("127.0.0.1:7300".into());
        daemon.mcp_allowed_hosts = vec!["daemon.example.test".into()];
        daemon.fleetd_endpoint = Some("https://fleetd.example.test".into());
        daemon.fleetd_token_file = Some("/run/sentinel-fleetd-token-file".into());
        daemon.fleetd_tls_fingerprint = Some("sentinel-fleetd-tls-fingerprint".into());
        daemon.fleetd_worker_home = Some("/srv/worker".into());
        daemon.fleetd_worker_bro_home = Some("/srv/worker/bro".into());
        daemon.admin_token_file = Some("/run/sentinel-admin-token-file".into());
        daemon.service_token_file = Some("/run/sentinel-service-token-file".into());
        daemon.mcp_trusted_peer_networks = vec!["10.0.0.0/8".into()];
        daemon.mcp_oauth = Some(McpOAuthConfig {
            issuer: "https://issuer.example.test/realms/test".into(),
            audience: "https://daemon.example.test/mcp".into(),
            required_group: "operators".into(),
            jwks_uri: Some("https://issuer.example.test/keys".into()),
        });

        config.index.reindex_startup_delay_secs = Some(5);
        config.index.background_full_reindex_ticks = Some(3);

        let scope = PublishedScope::try_new("repo_example", ".").unwrap();
        config.code_collection.producers = vec![
            CodeCollectionProducerConfig {
                producer_id: "code-single".into(),
                token_file: "/run/sentinel-code-token-file".into(),
                token_files: vec!["/run/sentinel-code-token-files".into()],
                scopes: vec![scope.clone()],
                claim_scopes: ProducerScopeClaimPolicy::Unclaimed,
                retired_auto_publish: Default::default(),
            },
            CodeCollectionProducerConfig {
                producer_id: "code-rotating".into(),
                token_file: "/run/sentinel-code-token-file".into(),
                token_files: vec!["/run/sentinel-code-token-files".into()],
                scopes: vec![scope],
                claim_scopes: ProducerScopeClaimPolicy::None,
                retired_auto_publish: Default::default(),
            },
        ];

        let source_id =
            bbox_corpus_core::project_catalog::ConnectorSourceId::parse("csrc_0000000000000001")
                .unwrap();
        let kind = bbox_corpus_core::project_catalog::ConnectorKind::parse("fixture").unwrap();
        config.source_connectors.enabled = true;
        config.source_connectors.producers = vec![ConnectorProducerConfig {
            producer_id: "connector".into(),
            token_file: "/run/sentinel-connector-token-file".into(),
            token_files: vec!["/run/sentinel-connector-token-files".into()],
            scopes: vec![ConnectorScopeGrant {
                connector_source_id: source_id.clone(),
                connector_kind: kind.clone(),
                remote_authority: "tenant.example.test".into(),
                profile: ConnectorProfile::Conversation,
            }],
        }];
        config.source_connectors.retained_conversations = vec![RetainedConversationSource {
            connector_source_id: bbox_corpus_core::project_catalog::ConnectorSourceId::parse(
                "csrc_0000000000000002",
            )
            .unwrap(),
            connector_kind: kind,
            workspace_id: "workspace".into(),
            remote_authority: "tenant.example.test".into(),
        }];

        let providers = &mut config.providers;
        providers.claude_bin = Some("/usr/local/bin/claude".into());
        providers.codex_bin = Some("/usr/local/bin/codex".into());
        providers.copilot_bin = Some("/usr/local/bin/copilot".into());
        providers.vibe_bin = Some("/usr/local/bin/vibe".into());
        providers.vibe_session_dir = Some("/srv/vibe".into());
        providers.extra_path = vec!["/opt/bin".into()];

        config.lsp.jdtls_bin = Some("/opt/jdtls".into());
        config.lsp.rust_analyzer_bin = Some("/opt/rust-analyzer".into());
        config.lsp.roslyn_lsp_bin = Some("/opt/roslyn".into());

        config.transcripts.roots = Some("/srv/transcripts".into());
        config.transcripts.codex_root = Some("/srv/codex".into());

        config.paths.user_memories_dir = Some("/srv/memories".into());
    }

    /// Every leaf of a serialized config section as a dotted path. An array
    /// of scalars is one leaf; an array of tables recurses per element.
    fn leaf_paths(prefix: &str, value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(fields) => {
                for (key, inner) in fields {
                    leaf_paths(&format!("{prefix}.{key}"), inner, out);
                }
            }
            serde_json::Value::Array(items)
                if items.iter().any(|item| item.is_object() || item.is_array()) =>
            {
                for (index, inner) in items.iter().enumerate() {
                    leaf_paths(&format!("{prefix}[{index}]"), inner, out);
                }
            }
            _ => out.push(prefix.to_string()),
        }
    }

    fn sections(config: &Config) -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("daemon", serde_json::to_value(&config.daemon).unwrap()),
            ("index", serde_json::to_value(&config.index).unwrap()),
            (
                "code_collection",
                serde_json::to_value(&config.code_collection).unwrap(),
            ),
            (
                "source_connectors",
                serde_json::to_value(&config.source_connectors).unwrap(),
            ),
            (
                "provenance",
                serde_json::to_value(&config.provenance).unwrap(),
            ),
            (
                "providers",
                serde_json::to_value(&config.providers).unwrap(),
            ),
            ("lsp", serde_json::to_value(&config.lsp).unwrap()),
            (
                "transcripts",
                serde_json::to_value(&config.transcripts).unwrap(),
            ),
            ("paths", serde_json::to_value(&config.paths).unwrap()),
        ]
    }

    /// The row whose key is the leaf path or an ancestor of it.
    fn covering_row<'a>(groups: &'a [ConfigGroup], leaf: &str) -> Option<&'a ConfigRow> {
        groups.iter().flat_map(|group| &group.rows).find(|row| {
            leaf == row.key
                || leaf
                    .strip_prefix(row.key.as_str())
                    .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
        })
    }

    fn rendered_values(groups: &[ConfigGroup]) -> Vec<String> {
        groups
            .iter()
            .flat_map(|group| &group.rows)
            .flat_map(|row| match &row.value {
                ConfigValue::Text(text) => vec![text.clone()],
                ConfigValue::List(items) => items.clone(),
                ConfigValue::Unset | ConfigValue::Redacted => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn every_config_field_renders_and_every_secret_field_is_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::server::SharedState::for_test(dir.path());
        let mut config = state.config.read().clone();
        populate_every_field(&mut config);
        let groups = redacted_config(&config);

        let mut walked = 0;
        let mut secret_fields = Vec::new();
        for (name, value) in sections(&config) {
            let mut leaves = Vec::new();
            leaf_paths(name, &value, &mut leaves);
            assert!(!leaves.is_empty(), "section {name} has no fields");
            for leaf in leaves {
                walked += 1;
                let row = covering_row(&groups, &leaf)
                    .unwrap_or_else(|| panic!("config field {leaf} is not rendered"));
                let field = leaf.rsplit(['.', ']']).next().unwrap_or(&leaf);
                if SECRET_FIELD_MARKERS
                    .iter()
                    .any(|marker| field.contains(marker))
                {
                    assert_eq!(
                        row.value,
                        ConfigValue::Redacted,
                        "secret-bearing field {leaf} must render the redaction marker"
                    );
                    secret_fields.push(leaf);
                }
            }
        }
        assert!(walked > 80, "the walk reached only {walked} fields");
        for expected in [
            "daemon.fleetd_token_file",
            "daemon.fleetd_tls_fingerprint",
            "daemon.admin_token_file",
            "daemon.service_token_file",
            "code_collection.producers[0].token_file",
            "code_collection.producers[1].token_files",
            "source_connectors.producers[0].token_file",
            "source_connectors.producers[0].token_files",
        ] {
            assert!(
                secret_fields.iter().any(|field| field == expected),
                "the walk must classify {expected} as secret-bearing: {secret_fields:?}"
            );
        }

        let values = rendered_values(&groups);
        for sentinel in SECRET_SENTINELS {
            assert!(
                values.iter().all(|value| !value.contains(sentinel)),
                "configured secret {sentinel} leaked into the rendered config"
            );
        }
    }

    #[test]
    fn unset_secrets_render_unset_and_credentialed_urls_are_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::server::SharedState::for_test(dir.path());
        let mut config = state.config.read().clone();
        config.daemon.service_token_file = None;
        config.daemon.fleetd_endpoint =
            Some("https://user:sentinel-pass@fleetd.example.test".into());
        config.daemon.advertise_url = Some("http://daemon.example.test:7264".into());
        let groups = redacted_config(&config);
        let row = |key: &str| {
            groups
                .iter()
                .flat_map(|group| &group.rows)
                .find(|row| row.key == key)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(row("daemon.service_token_file"), ConfigValue::Unset);
        assert_eq!(row("daemon.fleetd_endpoint"), ConfigValue::Redacted);
        assert_eq!(
            row("daemon.advertise_url"),
            ConfigValue::Text("http://daemon.example.test:7264".into())
        );
    }
}
