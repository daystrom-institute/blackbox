use serde::{Deserialize, Serialize};

/// Languages auto-detected on a project root. Used by polyglot-aware
/// services (LSP session manager, refactor dispatch) to pick which
/// per-language backend to lazily spawn for a given canonical project
/// directory. Extensible — add a variant + detector to grow the set.
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Java,
    Rust,
    #[serde(alias = "cs", alias = "c-sharp")]
    Csharp,
}
