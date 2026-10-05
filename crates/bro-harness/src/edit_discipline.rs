//! Edit discipline: whether a session may mutate files through the raw edit
//! builtins.
//!
//! `free` leaves the transport's tool surface as it is. `structured` refuses
//! the raw edit builtins by exact name everywhere a model can reach a tool:
//! the wire array, the deferred catalog, the cell catalog and its executable
//! `tools.*` properties. A refused name is not merely absent. Calling it
//! returns a refusal that names the structured path, because an absent tool
//! reads as a missing capability and invites a shell workaround.
//!
//! The discipline belongs to the session. It is resolved once at session
//! build (explicit flag, then the value saved with the session, then `free`),
//! saved with every snapshot, and restored on resume without being passed
//! again. Tool filters compose with it: an allow list cannot bring a refused
//! name back, and the discipline grants nothing a filter denied.

use std::str::FromStr;

/// The raw edit builtins a structured session refuses, by exact name. The
/// list is complete on every transport, including those where one of them is
/// already unavailable.
pub const RAW_EDIT_TOOLS: [&str; 3] = ["file_edit", "file_write", "apply_patch"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditDiscipline {
    #[default]
    Free,
    Structured,
}

impl EditDiscipline {
    pub fn as_str(self) -> &'static str {
        match self {
            EditDiscipline::Free => "free",
            EditDiscipline::Structured => "structured",
        }
    }

    /// Resolve the session's discipline. An unknown value from either source
    /// is an error, never `free`.
    pub fn resolve(explicit: Option<&str>, saved: Option<&str>) -> Result<Self, String> {
        match explicit.or(saved) {
            Some(value) => value.parse(),
            None => Ok(EditDiscipline::Free),
        }
    }

    /// Why this session refuses `tool`, when it does.
    pub fn refusal(self, tool: &str) -> Option<String> {
        (self == EditDiscipline::Structured && RAW_EDIT_TOOLS.contains(&tool)).then(|| {
            format!(
                "{tool} is refused under structured edit discipline: make source edits with \
                 edits.begin / edits.* / edits.apply inside an exec cell"
            )
        })
    }

    /// Every refused name with its reason, for the surfaces that explain a
    /// refusal instead of reporting an unknown tool.
    pub fn refusals(self) -> Vec<(String, String)> {
        RAW_EDIT_TOOLS
            .iter()
            .filter_map(|tool| self.refusal(tool).map(|reason| (tool.to_string(), reason)))
            .collect()
    }
}

impl FromStr for EditDiscipline {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "free" => Ok(EditDiscipline::Free),
            "structured" => Ok(EditDiscipline::Structured),
            other => Err(format!(
                "unknown edit discipline '{other}': expected 'free' or 'structured'"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_prefers_explicit_then_saved_then_free_and_never_guesses() {
        assert_eq!(
            EditDiscipline::resolve(None, None),
            Ok(EditDiscipline::Free)
        );
        assert_eq!(
            EditDiscipline::resolve(None, Some("structured")),
            Ok(EditDiscipline::Structured)
        );
        assert_eq!(
            EditDiscipline::resolve(Some("free"), Some("structured")),
            Ok(EditDiscipline::Free)
        );
        assert!(EditDiscipline::resolve(Some("strict"), None).is_err());
        assert!(EditDiscipline::resolve(None, Some("")).is_err());
    }

    #[test]
    fn only_a_structured_session_refuses_and_only_the_raw_edit_tools() {
        assert!(EditDiscipline::Free.refusals().is_empty());
        let refusals = EditDiscipline::Structured.refusals();
        assert_eq!(refusals.len(), 3);
        for (tool, reason) in &refusals {
            assert!(RAW_EDIT_TOOLS.contains(&tool.as_str()));
            assert!(
                reason.starts_with(&format!("{tool} is refused")),
                "{reason}"
            );
            assert!(reason.contains("edits.apply"), "{reason}");
        }
        assert_eq!(EditDiscipline::Structured.refusal("file_read"), None);
        assert_eq!(EditDiscipline::Structured.refusal("shell_run"), None);
    }
}
