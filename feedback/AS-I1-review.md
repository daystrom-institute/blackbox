Issues:
1. `validate_agent_install` accepts a flat/top-level manifest when `manifest` is absent. AS-I1 should validate the canonical wrapper `{kind,name,version,supersedes?,manifest}` only; this compatibility path lets non-canonical agents install.
2. Top-level `kind` is not required or checked. `AgentArtifact.kind` should be present and equal to `"agent"`.
3. `version` validation accepts negative numbers and floats because it only checks `is_number()` plus `as_u64() == Some(0)`. Require a positive integer.
4. The install hook builds a fresh empty `AgentAdapterRegistry` per install, so any `dispatch_adapter` is rejected until a daemon-wide registry is wired into shared state.
5. `lint_manifest` slices strings with `&item[..50]`; byte slicing can panic on non-ASCII. Use `chars().take(50)`.
6. `filter_overlay` accepts arbitrary strings like `"not a tool pattern"`. Validate against the existing MCP filter grammar or explicitly mark this as deferred.

Nits:
7. `AgentArtifact.version` is `serde_json::Value`; prefer the real positive integer type once canonical shape is enforced.
8. `supersedes` shape is not validated.
9. `brofile_inline` presence is checked, but inline brofile schema/provider/lens validation is deferred.
10. `inputs.prompt_template` parseability is not checked.
11. `use anyhow::{bail, Result};` leaves `bail` unused and makes a domain validation result look like `anyhow`.
