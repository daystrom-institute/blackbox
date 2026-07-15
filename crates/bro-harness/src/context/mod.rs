use bro_tools::ToolCx;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::path::PathBuf;

pub mod dispatch;
pub mod sections;
pub mod shadow_selector;
pub mod window;
pub mod world_state;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentRole {
    User,
    Developer,
}

impl FragmentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            FragmentRole::User => "user",
            FragmentRole::Developer => "developer",
        }
    }
}

pub trait ContextualUserFragment {
    fn role(&self) -> FragmentRole;
    fn markers(&self) -> (&'static str, &'static str);
    fn body(&self) -> String;

    fn render(&self) -> String {
        let (open, close) = self.markers();
        let body = self.body();
        if open.is_empty() && close.is_empty() {
            body
        } else {
            format!("{open}{body}{close}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMessage {
    pub role: FragmentRole,
    pub text_blocks: Vec<String>,
}

pub fn build_contextual_user_message(sections: Vec<String>) -> Option<TextMessage> {
    build_text_message(FragmentRole::User, sections)
}

pub fn build_developer_update_item(sections: Vec<String>) -> Option<TextMessage> {
    build_text_message(FragmentRole::Developer, sections)
}

fn build_text_message(role: FragmentRole, sections: Vec<String>) -> Option<TextMessage> {
    if sections.is_empty() {
        return None;
    }
    Some(TextMessage {
        role,
        text_blocks: sections,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContextItem {
    pub cwd: String,
    pub shell: Option<String>,
    pub current_date: Option<String>,
    pub timezone: Option<String>,
}

impl TurnContextItem {
    pub fn to_side(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn from_side(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentContext {
    pub cwd: String,
    pub shell: Option<String>,
    pub current_date: Option<String>,
    pub timezone: Option<String>,
}

impl EnvironmentContext {
    pub fn from_tool_cx(cx: &ToolCx) -> Self {
        let (current_date, timezone) = local_time_context();
        Self {
            cwd: cx.root.to_string_lossy().into_owned(),
            shell: shell_from_env(),
            current_date: Some(current_date),
            timezone: Some(timezone),
        }
    }

    pub fn to_turn_context_item(&self) -> TurnContextItem {
        TurnContextItem {
            cwd: self.cwd.clone(),
            shell: self.shell.clone(),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentContextDelta {
    pub cwd: Option<String>,
    pub current_date: Option<String>,
    pub timezone: Option<String>,
}

impl EnvironmentContextDelta {
    pub fn from_turn_context_item(
        before: &TurnContextItem,
        after: &EnvironmentContext,
    ) -> Option<Self> {
        let delta = Self {
            cwd: (before.cwd != after.cwd).then(|| after.cwd.clone()),
            current_date: (before.current_date != after.current_date)
                .then(|| after.current_date.clone())
                .flatten(),
            timezone: (before.timezone != after.timezone)
                .then(|| after.timezone.clone())
                .flatten(),
        };
        delta.has_changes().then_some(delta)
    }

    fn has_changes(&self) -> bool {
        self.cwd.is_some() || self.current_date.is_some() || self.timezone.is_some()
    }
}

impl ContextualUserFragment for EnvironmentContextDelta {
    fn role(&self) -> FragmentRole {
        FragmentRole::User
    }

    fn markers(&self) -> (&'static str, &'static str) {
        ("<environment_context>", "</environment_context>")
    }

    fn body(&self) -> String {
        let mut lines = Vec::new();
        if let Some(cwd) = &self.cwd {
            lines.push(format!("  <cwd>{}</cwd>", escape_xml(cwd)));
        }
        if let Some(current_date) = &self.current_date {
            lines.push(format!(
                "  <current_date>{}</current_date>",
                escape_xml(current_date)
            ));
        }
        if let Some(timezone) = &self.timezone {
            lines.push(format!("  <timezone>{}</timezone>", escape_xml(timezone)));
        }
        format!("\n{}\n", lines.join("\n"))
    }
}

impl ContextualUserFragment for EnvironmentContext {
    fn role(&self) -> FragmentRole {
        FragmentRole::User
    }

    fn markers(&self) -> (&'static str, &'static str) {
        ("<environment_context>", "</environment_context>")
    }

    fn body(&self) -> String {
        let mut lines = vec![format!("  <cwd>{}</cwd>", escape_xml(&self.cwd))];
        if let Some(shell) = &self.shell {
            lines.push(format!("  <shell>{}</shell>", escape_xml(shell)));
        }
        if let Some(current_date) = &self.current_date {
            lines.push(format!(
                "  <current_date>{}</current_date>",
                escape_xml(current_date)
            ));
        }
        if let Some(timezone) = &self.timezone {
            lines.push(format!("  <timezone>{}</timezone>", escape_xml(timezone)));
        }
        // Codex also has network permissions, filesystem permission profile,
        // and subagents fields. Stage 1 omits them until bro-harness has a
        // faithful source for each value.
        format!("\n{}\n", lines.join("\n"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInstructions {
    pub directory: String,
    pub text: String,
    pub loaded_paths: Vec<PathBuf>,
}

impl UserInstructions {
    pub fn from_project(cwd: &Path) -> Option<Self> {
        crate::project_doc::discover(cwd).map(|overlay| Self {
            directory: cwd.to_string_lossy().into_owned(),
            text: overlay.text,
            loaded_paths: overlay.loaded_paths,
        })
    }
}

impl ContextualUserFragment for UserInstructions {
    fn role(&self) -> FragmentRole {
        FragmentRole::User
    }

    fn markers(&self) -> (&'static str, &'static str) {
        ("# AGENTS.md instructions for ", "</INSTRUCTIONS>")
    }

    fn body(&self) -> String {
        format!("{}\n\n<INSTRUCTIONS>\n{}\n", self.directory, self.text)
    }
}

impl From<TextMessage> for Value {
    fn from(message: TextMessage) -> Self {
        json!({
            "role": message.role.as_str(),
            "content": message.text_blocks,
        })
    }
}

fn shell_from_env() -> Option<String> {
    std::env::var("SHELL").ok().filter(|s| !s.is_empty())
}

fn local_time_context() -> (String, String) {
    match iana_time_zone::get_timezone() {
        Ok(timezone) => (chrono::Local::now().date_naive().to_string(), timezone),
        Err(_) => (
            chrono::Utc::now().date_naive().to_string(),
            "Etc/UTC".to_string(),
        ),
    }
}

fn escape_xml(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_render_wraps_body_in_markers() {
        let ctx = EnvironmentContext {
            cwd: "/repo".into(),
            shell: Some("/bin/zsh".into()),
            current_date: Some("2026-06-06".into()),
            timezone: Some("America/Edmonton".into()),
        };
        let rendered = ctx.render();
        assert!(rendered.starts_with("<environment_context>\n"));
        assert!(rendered.ends_with("\n</environment_context>"));
        assert!(rendered.contains("<cwd>/repo</cwd>"));
    }

    #[test]
    fn environment_context_body_contains_emitted_fields() {
        let ctx = EnvironmentContext {
            cwd: "/repo".into(),
            shell: Some("zsh".into()),
            current_date: Some("2026-06-06".into()),
            timezone: Some("America/Edmonton".into()),
        };
        let body = ctx.body();
        assert!(body.contains("<cwd>/repo</cwd>"));
        assert!(body.contains("<shell>zsh</shell>"));
        assert!(body.contains("<current_date>2026-06-06</current_date>"));
        assert!(body.contains("<timezone>America/Edmonton</timezone>"));
        assert!(!body.contains("safety_policy"));
    }

    #[test]
    fn environment_context_delta_ignores_shell_and_renders_changed_fields_only() {
        let before = TurnContextItem {
            cwd: "/repo".into(),
            shell: Some("/bin/bash".into()),
            current_date: Some("2026-06-06".into()),
            timezone: Some("America/Edmonton".into()),
        };
        let shell_only = EnvironmentContext {
            cwd: "/repo".into(),
            shell: Some("/bin/zsh".into()),
            current_date: Some("2026-06-06".into()),
            timezone: Some("America/Edmonton".into()),
        };
        assert_eq!(
            EnvironmentContextDelta::from_turn_context_item(&before, &shell_only),
            None
        );

        let changed = EnvironmentContext {
            cwd: "/other".into(),
            shell: Some("/bin/zsh".into()),
            current_date: Some("2026-06-07".into()),
            timezone: Some("America/Edmonton".into()),
        };
        let delta =
            EnvironmentContextDelta::from_turn_context_item(&before, &changed).expect("delta");
        let rendered = delta.render();
        assert!(rendered.contains("<cwd>/other</cwd>"));
        assert!(rendered.contains("<current_date>2026-06-07</current_date>"));
        assert!(!rendered.contains("<shell>"));
        assert!(!rendered.contains("<timezone>"));
    }

    #[test]
    fn turn_context_item_round_trips_through_side() {
        let item = TurnContextItem {
            cwd: "/repo".into(),
            shell: Some("/bin/zsh".into()),
            current_date: Some("2026-06-06".into()),
            timezone: Some("America/Edmonton".into()),
        };

        assert_eq!(TurnContextItem::from_side(&item.to_side()), Some(item));
    }

    #[test]
    fn turn_context_item_from_side_is_tolerant() {
        assert_eq!(TurnContextItem::from_side(&Value::Null), None);
        assert_eq!(
            TurnContextItem::from_side(&json!({"cwd": 7, "timezone": []})),
            None
        );
    }

    #[test]
    fn contextual_user_message_is_one_user_message_with_text_blocks() {
        let message =
            build_contextual_user_message(vec!["one".into(), "two".into()]).expect("message");
        assert_eq!(message.role, FragmentRole::User);
        assert_eq!(message.text_blocks, vec!["one", "two"]);
    }

    #[test]
    fn developer_update_item_uses_developer_role() {
        let message = build_developer_update_item(vec!["policy".into()]).expect("message");
        assert_eq!(message.role, FragmentRole::Developer);
        assert_eq!(message.text_blocks, vec!["policy"]);
    }

    #[test]
    fn user_instructions_render_wraps_agents_text() {
        let instructions = UserInstructions {
            directory: "/repo".into(),
            text: "rule".into(),
            loaded_paths: Vec::new(),
        };
        let rendered = instructions.render();
        assert!(rendered.starts_with("# AGENTS.md instructions for /repo"));
        assert!(rendered.contains("<INSTRUCTIONS>\nrule\n"));
        assert!(rendered.ends_with("</INSTRUCTIONS>"));
    }
}
