//! Explicit server admission policy. Catalogs are fixed for one harness session.

use super::*;
use anyhow::{Context, Result, ensure};

#[derive(Debug, Clone)]
pub struct McpServerPolicy {
    pub required: bool,
    pub startup_timeout_ms: u64,
    /// Remote calls only. In-process calls retain actual completion ownership.
    pub tool_timeout_ms: u64,
    pub exclude_tools: Vec<String>,
}

impl Default for McpServerPolicy {
    fn default() -> Self {
        Self {
            required: false,
            startup_timeout_ms: 30_000,
            tool_timeout_ms: 300_000,
            exclude_tools: Vec::new(),
        }
    }
}

impl McpConfig {
    pub fn from_json(cfg: &str) -> Result<Self> {
        let UniqueValue(value) = serde_json::from_str(cfg).context("invalid MCP config JSON")?;
        let root = value.as_object().context("MCP config must be an object")?;
        ensure!(
            root.keys()
                .all(|key| matches!(key.as_str(), "mcpServers" | "tool_placement")),
            "unsupported MCP config field"
        );
        let configured = root
            .get("mcpServers")
            .and_then(Value::as_object)
            .context("MCP config requires an mcpServers object")?;
        let mut servers = Vec::new();
        let mut server_policies = BTreeMap::new();
        for (name, value) in configured {
            validate_component(name)?;
            let server = value
                .as_object()
                .context("MCP server config must be an object")?;
            ensure!(
                server.keys().all(|key| matches!(
                    key.as_str(),
                    "type"
                        | "url"
                        | "headers"
                        | "command"
                        | "args"
                        | "env"
                        | "exclude_tools"
                        | "required"
                        | "startup_timeout_ms"
                        | "tool_timeout_ms"
                )),
                "unsupported MCP server config field"
            );
            let kind = match server.get("type") {
                Some(value) => value.as_str().context("MCP server type must be a string")?,
                None if server.contains_key("command") => "stdio",
                None if server.contains_key("url") => "http",
                None => anyhow::bail!("MCP server requires a transport type, command, or URL"),
            };
            let policy = McpServerPolicy {
                required: match server.get("required") {
                    None => false,
                    Some(value) => value.as_bool().context("MCP required must be a boolean")?,
                },
                startup_timeout_ms: match server.get("startup_timeout_ms") {
                    None => McpServerPolicy::default().startup_timeout_ms,
                    Some(value) => value
                        .as_u64()
                        .context("MCP startup_timeout_ms must be a positive integer")?,
                },
                exclude_tools: parse_string_array(server.get("exclude_tools"))?,
                tool_timeout_ms: match server.get("tool_timeout_ms") {
                    None => McpServerPolicy::default().tool_timeout_ms,
                    Some(value) => value
                        .as_u64()
                        .context("MCP tool_timeout_ms must be a positive integer")?,
                },
            };
            validate_policy(&policy)?;
            let config = match kind {
                "http" => {
                    ensure!(
                        !server.contains_key("command")
                            && !server.contains_key("args")
                            && !server.contains_key("env"),
                        "HTTP MCP config contains stdio fields"
                    );
                    let url = required_string(server, "url")?;
                    validate_url(&url)?;
                    McpServerConfig::Http {
                        name: name.clone(),
                        url,
                        headers: parse_string_map(server.get("headers"))?,
                        exclude_tools: Vec::new(),
                    }
                }
                "stdio" => {
                    ensure!(
                        !server.contains_key("url") && !server.contains_key("headers"),
                        "stdio MCP config contains HTTP fields"
                    );
                    McpServerConfig::Stdio {
                        name: name.clone(),
                        command: required_string(server, "command")?,
                        args: parse_string_array(server.get("args"))?,
                        env: parse_string_map(server.get("env"))?,
                    }
                }
                "sse" => anyhow::bail!(
                    "legacy SSE MCP transport is unsupported; type http requires a Streamable HTTP endpoint"
                ),
                _ => anyhow::bail!("unsupported MCP transport type"),
            };
            servers.push(config);
            server_policies.insert(name.clone(), policy);
        }
        let config = Self {
            servers,
            server_policies,
            tool_placement: parse_tool_placement_value(&value)?,
        };
        super::admission::validate_config(&config)?;
        Ok(config)
    }
}

fn required_string(object: &serde_json::Map<String, Value>, key: &str) -> Result<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .with_context(|| format!("MCP {key} must be a nonempty string"))
}

pub(super) fn validate_component(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.contains("__")
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
        "MCP names must be nonempty ASCII letters/digits/dots/hyphens/underscores without the qualifier separator"
    );
    Ok(())
}

pub(super) fn validate_policy(policy: &McpServerPolicy) -> Result<()> {
    ensure!(
        (1..=300_000).contains(&policy.startup_timeout_ms),
        "MCP startup_timeout_ms must be between 1 and 300000"
    );
    ensure!(
        (1..=3_600_000).contains(&policy.tool_timeout_ms),
        "MCP tool_timeout_ms must be between 1 and 3600000"
    );
    Ok(())
}

pub(super) fn validate_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid MCP HTTP URL"))?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
        "MCP URL must use http or https with a host"
    );
    Ok(())
}

pub(super) fn parse_string_array(value: Option<&Value>) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .context("MCP list field must be an array of strings")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("MCP list item must be a string")
        })
        .collect()
}

pub(super) fn parse_string_map(value: Option<&Value>) -> Result<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    value
        .as_object()
        .context("MCP map field must be an object of strings")?
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                value
                    .as_str()
                    .context("MCP map value must be a string")?
                    .to_owned(),
            ))
        })
        .collect()
}

pub(super) fn parse_tool_placement_value(value: &Value) -> Result<ToolPlacementMap> {
    let Some(value) = value.get("tool_placement") else {
        return Ok(ToolPlacementMap::new());
    };
    value
        .as_object()
        .context("tool_placement must be an object")?
        .iter()
        .map(|(name, value)| {
            let placement = match value.as_str() {
                Some("in-box") => ToolPlacement::InBox,
                Some("out-box") => ToolPlacement::OutBox,
                Some("both") => ToolPlacement::Both,
                _ => anyhow::bail!("tool_placement values must be in-box, out-box, or both"),
            };
            Ok((name.clone(), placement))
        })
        .collect()
}

// serde_json::Value normally keeps only the last duplicate object key. At this
// boundary that could silently replace a server definition, policy, or credential
// reference before admission can detect a collision.
struct UniqueValue(Value);

impl<'de> serde::Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueValue;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                value: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Bool(value)))
            }
            fn visit_i64<E: serde::de::Error>(
                self,
                value: i64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_u64<E: serde::de::Error>(
                self,
                value: u64,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_f64<E: serde::de::Error>(
                self,
                value: f64,
            ) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("invalid MCP number"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value.into())))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                value: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::String(value)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueValue(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate MCP config key"));
                    }
                    let UniqueValue(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_server_and_policy_keys_are_rejected() {
        for input in [
            r#"{"mcpServers":{"fixture":{"command":"one"},"fixture":{"command":"two"}}}"#,
            r#"{"mcpServers":{"fixture":{"command":"one","required":true,"required":false}}}"#,
        ] {
            assert!(McpConfig::from_json(input).is_err());
        }
    }
    #[test]
    fn remote_tool_deadline_is_explicit_bounded_and_defaults_to_five_minutes() {
        let default = McpConfig::from_json(
            r#"{"mcpServers":{"fixture":{"url":"https://fixture.invalid/mcp"}}}"#,
        )
        .unwrap();
        assert_eq!(default.server_policies["fixture"].tool_timeout_ms, 300_000);
        let explicit = McpConfig::from_json(r#"{"mcpServers":{"fixture":{"url":"https://fixture.invalid/mcp","tool_timeout_ms":25}}}"#).unwrap();
        assert_eq!(explicit.server_policies["fixture"].tool_timeout_ms, 25);
        for value in [
            serde_json::json!(0),
            serde_json::json!(3_600_001),
            serde_json::json!("30"),
        ] {
            let config = serde_json::json!({"mcpServers":{"fixture":{"url":"https://fixture.invalid/mcp","tool_timeout_ms":value}}});
            assert!(McpConfig::from_json(&config.to_string()).is_err());
        }
    }
}
