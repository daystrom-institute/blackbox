# Vendored Codex Material

The prompt prose under `prompts/base_instructions/` and the code-mode freeform
grammar are vendored from `openai/codex`, licensed under Apache-2.0.

Sources:

- `default.md`: `/Users/invidious/repos/codex/codex-rs/protocol/src/prompts/base_instructions/default.md`
- `fallback.md`: `/Users/invidious/repos/codex/codex-rs/models-manager/prompt.md`
- `gpt-5.5.md`: `.models[] | select(.slug=="gpt-5.5") | .model_messages.instructions_template`, with `{{ personality }}` resolved to the default empty personality, from `/Users/invidious/repos/codex/codex-rs/models-manager/models.json`
- `gpt-5-codex.md`: `.models[] | select(.slug=="gpt-5.3-codex") | .model_messages.instructions_template`, with `{{ personality }}` resolved to the default empty personality, from `/Users/invidious/repos/codex/codex-rs/models-manager/models.json`
- `CODE_MODE_FREEFORM_GRAMMAR`: `/Users/invidious/repos/codex/codex-rs/core/src/tools/code_mode/execute_spec.rs`

Stage 0 bakes Codex's default empty personality into these model-family prompt
files. Dynamic personality substitution maps to the later PersonalitySpec
fragment work in the codexification charter.

Upstream NOTICE:

OpenAI Codex
Copyright 2025 OpenAI

This project includes code derived from [Ratatui](https://github.com/ratatui/ratatui), licensed under the MIT license.
Copyright (c) 2016-2022 Florian Dehau
Copyright (c) 2023-2025 The Ratatui Developers
