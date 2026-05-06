# Agentic Corpus Data Export Policy

Blackbox routes embedding text by bucket. The default configuration sends every
bucket to Voyage:

- `knowledge`
- `code`
- `docs`
- `transcripts`
- `git_message`
- `notes`

This default favors high-quality retrieval out of the box. It also means bucket
content leaves the machine and is subject to Voyage's service terms, security
controls, and data-retention policy.

## Opt Out Per Bucket

Embedding routes are configured at `~/.config/blackbox/embed.toml`.

```toml
[embed.providers.voyage]
api_key_env = "VOYAGE_API_KEY"
model = "voyage-code-3"
rate_limit_per_min = 100

[embed.providers.ollama]
endpoint = "http://localhost:11434"
model = "nomic-embed-text"

[embed.routes]
knowledge = "voyage"
code = "ollama"
docs = "voyage"
transcripts = "ollama"
git_message = "voyage"
notes = "voyage"
```

Per-project overrides can route sensitive code locally without changing the
global default:

```toml
[embed.routes.per_project."<project_id>"]
code = "ollama"
docs = "ollama"
transcripts = "ollama"
```

## Privacy-Conscious Local Config

Use Ollama for every bucket when no indexed content should leave the host:

```toml
[embed.providers.ollama]
endpoint = "http://localhost:11434"
model = "nomic-embed-text"

[embed.routes]
knowledge = "ollama"
code = "ollama"
docs = "ollama"
transcripts = "ollama"
git_message = "ollama"
notes = "ollama"
```

## Recommendation

For sensitive projects, override at least `code` and `transcripts` to Ollama.
Those buckets carry source text, shell output context, and conversation content.
Non-sensitive projects can stay on Voyage when recall quality matters more than
local-only processing.
