+++
title = "Scoped pins — hot context for one active execution lane"
tags = ["pin", "pins", "scoped", "ambient", "session", "bro", "thread", "work_item", "active-arc", "runbook"]
order = 4
template = false
+++
# Scoped pins — hot context for one active execution lane

`bbox_pin` is for context that should stay hot across turns without becoming standing repo policy.

## What a pin is

A pin is:

- persisted across daemon restarts
- never rendered into `CLAUDE.md` / `AGENTS.md` / `GEMINI.md`
- injected only when the current dispatch matches its scope

Current scopes:

- `session`
- `bro`
- `thread`
- `work_item`

Think of pins as scoped ambient context, not durable law.

## Use pins for

- active migration or initiative guidance
- bounded executor or reviewer charters for the current arc
- phase-specific sequencing notes
- current-lane metadata that must stay hot across resumes

Examples:

- "for the current scoping migration, validate every cut against the canonical doc before proposing code"
- "this executor is acting as architecture reviewer for the current arc"
- "for this work item, treat graph authority as canonical and tool residue as non-authoritative"

## Do not use pins for

- standing repo conventions
- user preferences that future unrelated sessions should always inherit
- architecture decisions that need rationale and supersession
- searchable cold facts that do not need prompt residency

Those belong in:

- `bbox_learn` for standing rules
- `bbox_decide` for commitments with rationale
- `bbox_remember` for cold searchable recall

## Quick test

Ask:

- Should every future agent in this repo load this by default?
  If yes, it is not a pin.
- Should only the current session/bro/thread/work-item see it?
  If yes, it probably is a pin.

## Why this tool exists

Without a hot scoped lane, agents over-promote active-work guidance into rendered memory just to keep it visible across turns.

That causes memory-boundary corruption:

- temporary initiative guidance becomes fake standing policy
- unrelated future sessions inherit stale arc-specific instructions
- repo agent files accumulate historical residue that looks official

Pins exist to prevent that failure mode.
