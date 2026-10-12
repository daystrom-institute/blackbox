# Brofile context and routing

List before creating or replacing a brofile. `bro_brofile` defaults to the
global store; project scope requires `scope="project"` and `project_dir`.
Global scope rejects `project_dir`. Catalog-owned project brofiles currently
refuse because there is no owner transport; do not bypass that refusal by
writing a daemon-local path. A global brofile or an existing supported route
may satisfy the task without changing project configuration.

Put provider/model/account choices in brofile fields and prompt-assembly policy
in `context`, rather than encoding configuration in the `lens` prose.

| `context.provider_defaults` | Behavior |
| --- | --- |
| `default` | Keep normal provider instructions. |
| `suppress_when_supported` | Best effort suppression; unsupported providers continue. |
| `strict_suppress` | Refuse when the provider cannot enforce suppression. |
| `explicit_only` | Request operator-supplied material only; refuse when unsupported. |

The current Claude CLI dispatch lane supports suppression. The Codex
app-server lane does not: `strict_suppress` and `explicit_only` fail with
`error.provider_defaults_unsupported`. Best effort on Codex does not establish
that its defaults were suppressed. Inspect `bro_providers` for available
provider identities rather than copying a historical provider/model list.

Keep MCP/tool and account configuration intact. A provider's bare mode is not
an interchangeable suppression control if it also removes these settings.

Account reads use `bro_brofile(action="list_accounts")` and `get_account`.
They return redacted configuration, not credentials. Do not infer credential
validity from a stored account name. Follow `next_offset` for inventories and
`body.next_cursor` as `cursor` for exact reads.

Create an authorized brofile with `bro_brofile(action="create", name=...,
provider=..., model=..., lens=..., context={...})`, then read it back. A changed
brofile does not rewrite an existing provider session's launch context. Test
prompt-sensitive changes with a fresh `bro_exec` and a small observable task,
not by resuming an old session.
