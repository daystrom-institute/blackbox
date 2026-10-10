//! The claude CLI argv subset the daemon sends a codex-lane worker.
//!
//! Flags the shim has no use for are accepted and ignored with a warning on
//! stderr, never a failure: the daemon composes one argv shape for every
//! vendor CLI lane.

use serde_json::Value;

/// What the dispatch asked for, parsed from argv.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ShimArgs {
    /// The thread to continue (`--resume`). A fresh session has none: the
    /// app-server mints the thread id.
    pub resume: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Dispatch context text, placed in the thread's developer instructions.
    pub append_system_prompt: Option<String>,
    /// A replacement for the model's base instructions. Empty means unset.
    pub system_prompt: Option<String>,
    /// JSON schema for the final message of every turn.
    pub json_schema: Option<Value>,
    /// `--mcp-config` payloads in the claude CLI's `{"mcpServers":{...}}`
    /// shape, in argv order.
    pub mcp_configs: Vec<String>,
    /// Only the servers named in `--mcp-config` are loaded.
    pub strict_mcp_config: bool,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// `--dangerously-skip-permissions`: the thread runs without a sandbox.
    pub skip_permissions: bool,
    /// `--include-partial-messages`: stream text deltas as `stream_event`s.
    pub include_partial_messages: bool,
    /// `--replay-user-messages`: echo each consumed user envelope.
    pub replay_user_messages: bool,
    /// Every flag or value that was accepted but has no effect, as a warning.
    pub warnings: Vec<String>,
}

const BOOLEAN_FLAGS: &[&str] = &[
    "-p",
    "--print",
    "--verbose",
    "--include-partial-messages",
    "--replay-user-messages",
    "--dangerously-skip-permissions",
    "--strict-mcp-config",
];

const VALUE_FLAGS: &[&str] = &[
    "--input-format",
    "--output-format",
    "--session-id",
    "--resume",
    "--model",
    "--effort",
    "--append-system-prompt",
    "--system-prompt",
    "--json-schema",
    "--mcp-config",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
];

impl ShimArgs {
    pub fn parse<I, S>(argv: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        let mut args = ShimArgs::default();
        let mut index = 0;
        while index < argv.len() {
            let raw = &argv[index];
            index += 1;
            let (flag, inline_value) = match raw.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
                _ => (raw.as_str(), None),
            };
            if BOOLEAN_FLAGS.contains(&flag) {
                args.apply_boolean(flag);
                continue;
            }
            if VALUE_FLAGS.contains(&flag) {
                let value = match inline_value {
                    Some(value) => value,
                    None => {
                        let Some(value) = argv.get(index) else {
                            anyhow::bail!("{flag} needs a value");
                        };
                        index += 1;
                        value.clone()
                    }
                };
                args.apply_value(flag, value)?;
                continue;
            }
            if flag.starts_with('-') {
                // An unknown flag followed by a bare token most likely takes
                // it as its value; both are ignored together.
                if inline_value.is_none()
                    && let Some(next) = argv.get(index)
                    && !next.starts_with('-')
                {
                    index += 1;
                    args.warnings
                        .push(format!("ignoring unknown flag {flag} {next:?}"));
                } else {
                    args.warnings.push(format!("ignoring unknown flag {raw}"));
                }
                continue;
            }
            args.warnings.push(format!(
                "ignoring positional argument {raw:?}: turns arrive on stdin"
            ));
        }
        Ok(args)
    }

    fn apply_boolean(&mut self, flag: &str) {
        match flag {
            "--include-partial-messages" => self.include_partial_messages = true,
            "--replay-user-messages" => self.replay_user_messages = true,
            "--dangerously-skip-permissions" => self.skip_permissions = true,
            "--strict-mcp-config" => self.strict_mcp_config = true,
            // `-p` and `--verbose` describe the headless stream-json mode the
            // shim always runs in.
            _ => {}
        }
    }

    fn apply_value(&mut self, flag: &str, value: String) -> anyhow::Result<()> {
        match flag {
            "--input-format" | "--output-format" => {
                if value != "stream-json" {
                    self.warnings.push(format!(
                        "{flag} {value} is not supported; speaking stream-json"
                    ));
                }
            }
            // The app-server mints thread ids; a requested id cannot be
            // honored, and the first event reports the real one.
            "--session-id" => {}
            "--resume" => self.resume = non_empty(value),
            "--model" => self.model = non_empty(value),
            "--effort" => self.effort = non_empty(value),
            "--append-system-prompt" => {
                if let Some(text) = non_empty(value) {
                    self.append_system_prompt = Some(match self.append_system_prompt.take() {
                        Some(existing) => format!("{existing}\n\n{text}"),
                        None => text,
                    });
                }
            }
            "--system-prompt" => {
                if value.is_empty() {
                    self.warnings.push(
                        "--system-prompt \"\" has no app-server equivalent; keeping the \
                         model's base instructions"
                            .to_string(),
                    );
                } else {
                    self.system_prompt = Some(value);
                }
            }
            "--json-schema" => {
                let schema: Value = serde_json::from_str(&value)
                    .map_err(|error| anyhow::anyhow!("--json-schema is not JSON: {error}"))?;
                self.json_schema = Some(schema);
            }
            "--mcp-config" => self.mcp_configs.push(value),
            "--allowedTools" | "--allowed-tools" => self.allowed_tools.extend(split_tools(&value)),
            "--disallowedTools" | "--disallowed-tools" => {
                self.disallowed_tools.extend(split_tools(&value))
            }
            _ => unreachable!("{flag} is listed in VALUE_FLAGS"),
        }
        Ok(())
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

/// Tool lists are comma or whitespace separated, as the claude CLI takes them.
fn split_tools(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|tool| !tool.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_daemon_argv() {
        let args = ShimArgs::parse([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
            "--dangerously-skip-permissions",
            "--resume",
            "thread-1",
            "--model",
            "gpt-6-sol",
            "--effort",
            "high",
            "--append-system-prompt",
            "persona",
            "--json-schema",
            r#"{"type":"object"}"#,
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--strict-mcp-config",
            "--allowedTools",
            "mcp__a__x,mcp__a__y",
            "--disallowedTools",
            "mcp__b__z",
        ])
        .unwrap();
        assert_eq!(args.resume.as_deref(), Some("thread-1"));
        assert_eq!(args.model.as_deref(), Some("gpt-6-sol"));
        assert_eq!(args.effort.as_deref(), Some("high"));
        assert_eq!(args.append_system_prompt.as_deref(), Some("persona"));
        assert_eq!(
            args.json_schema,
            Some(serde_json::json!({"type": "object"}))
        );
        assert_eq!(args.mcp_configs, [r#"{"mcpServers":{}}"#]);
        assert!(args.strict_mcp_config);
        assert!(args.skip_permissions);
        assert!(args.include_partial_messages);
        assert!(args.replay_user_messages);
        assert_eq!(args.allowed_tools, ["mcp__a__x", "mcp__a__y"]);
        assert_eq!(args.disallowed_tools, ["mcp__b__z"]);
        assert!(args.warnings.is_empty(), "{:?}", args.warnings);
    }

    #[test]
    fn unknown_flags_and_unusable_values_warn_instead_of_failing() {
        let args = ShimArgs::parse([
            "--session-id",
            "requested",
            "--frobnicate",
            "value",
            "--flag-only",
            "--system-prompt",
            "",
            "--output-format=text",
            "stray",
        ])
        .unwrap();
        assert_eq!(args.resume, None);
        assert_eq!(args.system_prompt, None);
        assert_eq!(args.warnings.len(), 5, "{:?}", args.warnings);
        assert!(args.warnings[0].contains("--frobnicate"));
        assert!(args.warnings[1].contains("--flag-only"));
    }

    #[test]
    fn a_value_flag_without_its_value_or_a_bad_schema_fails() {
        assert!(ShimArgs::parse(["--model"]).is_err());
        assert!(ShimArgs::parse(["--json-schema", "{nope"]).is_err());
    }
}
