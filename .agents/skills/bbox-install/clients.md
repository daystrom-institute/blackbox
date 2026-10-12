# Connect interactive clients and dispatch accounts

Read each requested CLI's active configuration and account selection before
editing. List existing MCP registrations and preserve unrelated entries. One
default-profile edit does not configure every account or launcher-managed home.
Use one canonical `blackbox` entry for ordinary interactive work:
`https://<daemon-origin>/mcp?surface=interactive`. Use the `ops` surface for
operator calls; do not duplicate it into the normal catalog without a reason.

For Codex, select the active `$CODEX_HOME/config.toml` (default
`~/.codex/config.toml`) for each profile:

```toml
[mcp_servers.blackbox]
url = "https://<daemon-origin>/mcp?surface=interactive"
```

For a Claude Code account, locate the account's actual active configuration and
merge the equivalent entry into its `mcpServers` table:

```json
{"mcpServers":{"blackbox":{"type":"http","url":"https://<daemon-origin>/mcp?surface=interactive"}}}
```

These examples specify the endpoint, not credentials. Configure the client's
supported bearer or OAuth mechanism to match the daemon's auth policy. For
other interactive providers, inspect their installed CLI help and current config
schema instead of copying another provider's JSON shape. Reload or start a new
client session when needed, then verify discovery and a read through that client.

`bro` has its own daemon/client configuration. A configured
`[client].token_file` or `BLACKBOX_CLIENT_TOKEN_FILE` supplies its service bearer;
an unreadable configured token is an error, not permission to omit auth. Never
echo token values while validating connectivity.

## Dispatch is separate from interactive MCP

Use `bro_providers` to inspect the available dispatch providers. Native CLI MCP
compatibility does not mean that provider is dispatchable by Blackbox.

Inspect existing account and brofile routing with
`bro_brofile(action="list_accounts")`, `get_account`, and `action="list"`.
Follow pagination and read exact redacted records when summaries are insufficient.
Configure only the requested accounts on the host where workers execute. Codex
account routing uses `CODEX_HOME`; a profile path on the daemon host does not
stand in for the worker host's credential home. Never copy an entire unrelated
profile just to borrow credentials.

Dispatch MCP configuration uses the operator surface:

```sh
bro mcp call bro_mcp '{"action":"list","scope":"global"}' --surface ops
```

List before adding with the same surface and intended scope. Its supported
registration transports are HTTP and SSE; `sync` is unavailable, and the daemon
does not rewrite persistent provider CLI configurations. Worker connections are
injected per dispatch. Catalog-owned project configuration requires an owner
transport and currently refuses without it; do not silently redirect it into a
different store. Verify interactive clients and dispatched workers independently.
