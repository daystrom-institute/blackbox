# Provider execution lanes

Blackbox admits and supervises native provider sessions on the execution host.
GLM, DeepSeek, MiniMax and Kimi run through the Claude CLI with their own
configuration directories. Codex runs through its app-server protocol in
`bro-worker`. Antigravity (`agy`) runs through its Agent Client Protocol server
in `bro-worker`. Fleetd and the local executor share the same typed worker
contract.

Provider discovery, allocation and dispatch admit only these supported providers.
Historical provider identities remain readable in stored tasks and transcripts;
they have no executable lane, model catalog or resume capability. The executor
refuses them before resolving a binary or starting a child.

Model inference, compaction, tool execution, source editing and language-server
integration belong to the native provider. Blackbox supplies corpus MCP tools,
workspace identity, scoped dispatch context, event persistence and supervision.

## Agy lane

Each dispatch starts one `agy acp` server on the execution host. The adapter
speaks the Agent Client Protocol over its stdio and emits the shared worker
event envelope. The server assigns the session id.

Session settings are typed and cross the executor boundary:

- The model and the thinking level are session options. The model is set
  first, because each model offers its own levels; a model that offers none
  gets none. The server carries neither into a new process, so a resume names
  them exactly as a fresh dispatch does.
- Persona and dispatch scope are appended to the session's system prompt. The
  server keeps the append with the session.
- Dispatch MCP servers are attached to the session when it opens or resumes.
  They are never written to the user's agy configuration. References to worker
  variables in their headers and environment are resolved by the adapter.

The session runs in the executing user's own agy home, with that user's
sign-in, MCP servers, skills and hooks. A server the user's configuration
defines keeps that definition when a dispatch server shares its name: on a host
whose agy configuration registers the daemon's MCP name, the session uses that
registration and not the dispatch's endpoint.

The server runs with permission prompts off and has no per-tool gate. The lane
takes no default recursion guard and refuses a dispatch that carries tool allow
or deny filters. It also refuses structured output, a service tier and
provider-default suppression.

A prompt sent during a running turn joins that turn. The server answers the
earlier prompt request at once and carries the turn on under the new one, so a
turn ends when every prompt sent for it has been answered. An interrupt cancels
the session's running turn; input queued behind it starts the next turn.
