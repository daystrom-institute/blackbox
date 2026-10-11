# Provider execution lanes

Blackbox admits and supervises native provider sessions on the execution host.
GLM, DeepSeek, MiniMax and Kimi run through the Claude CLI with their own
configuration directories. Codex runs through its app-server protocol in
`bro-worker`. Fleetd and the local executor share the same typed worker contract.

Provider discovery, allocation and dispatch admit only these supported providers.
Historical provider identities remain readable in stored tasks and transcripts;
they have no executable lane, model catalog or resume capability. The executor
refuses them before resolving a binary or starting a child.

Model inference, compaction, tool execution, source editing and language-server
integration belong to the native provider. Blackbox supplies corpus MCP tools,
workspace identity, scoped dispatch context, event persistence and supervision.
