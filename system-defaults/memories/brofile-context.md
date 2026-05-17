+++
title = "Brofile context - provider default suppression and minimal probes"
tags = ["bro", "brofile", "context", "provider-defaults", "suppression", "teams"]
order = 6
template = false
+++
# Brofile context - provider default suppression and minimal probes

Brofiles may carry a `context` policy that affects how provider prompt
material is assembled for dispatch. This is the correct place for
provider-default suppression and other context assembly knobs; do not encode
these controls as ad hoc prose in `lens`.

## Provider default policy

Use `context.provider_defaults` to control provider-loaded harness markdown
and default system-prompt material:

- `default`: normal provider behavior.
- `suppress_when_supported`: request suppression where the provider has a
  known control, and continue without failing on unsupported providers.
- `strict_suppress`: fail closed when the selected provider cannot enforce
  suppression.
- `explicit_only`: operator-supplied prompt material only; fail closed where
  the provider cannot enforce suppression.

Known suppression-capable providers are Claude-compatible transports
(`claude`, `glm`, `deepseek`), `codex`, and `inception`. Mixed-provider
ensembles that include unsupported providers should usually use
`suppress_when_supported`; use `strict_suppress` only when unsupported
providers must be excluded rather than allowed through.

Suppression must preserve MCP/tool configuration. Do not use provider bare or
ignore-user-config modes as a generic default-suppression mechanism when they
also disable MCP or account configuration.

## Minimal probe profiles

Probe brofiles should be small, project-local when they are repo-specific, and
resolvable by the same names as any global defaults they intentionally
override. A typical minimal probe lens is:

`You are a minimal probe drone. Follow the prompt exactly. Return only the requested output.`

Keep the model/account/provider selection in the brofile fields. Keep prompt
assembly policy in `context`. This makes teamplates reusable and lets
`bro_brofile(action="get"|"list")` show the full operational contract.

## Team/session implications

Changing a brofile or project-local override is not retroactive for a live
provider session. If the goal is to test a new context policy, dissolve and
recreate the team so members start fresh sessions from the new brofiles.

`bro_broadcast` resumes existing team member sessions on later rounds. For
fresh-context validation, instantiate a fresh team or dissolve/recreate the
existing team before broadcasting.

## Creation surface

Use `bro_brofile(action="create", context={...})` for new brofiles rather than
editing JSON by hand. For project-local brofiles, pass both
`scope="project"` and `project_dir`.

Before creating or overwriting, list first:

`bro_brofile(action="list", scope="project", project_dir="/path/to/repo")`

Then create with context:

`bro_brofile(action="create", scope="project", project_dir="/path/to/repo", name="drone-probe-codex-spark", provider="codex", model="gpt-5.3-codex-spark", lens="You are a minimal probe drone. Follow the prompt exactly. Return only the requested output.", context={"provider_defaults":"suppress_when_supported"})`

Validate by reading the brofile back and, for prompt-sensitive changes,
dispatching a fresh session or recreated team with a constrained prompt such
as `PING respond only PONG`.
