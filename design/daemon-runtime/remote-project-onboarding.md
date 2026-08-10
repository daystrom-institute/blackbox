---
title: "Remote project onboarding through the collector backchannel"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - project-catalog
tags: [locality, onboarding, collector, catalog, transport]
brief: "New-project registration with zero daemon checkout access: the checkout-owner collector probes locally and presents an authenticated, scope-bound onboarding request; the daemon validates it against the producer grant and performs the catalog mutation. Onboarding is operator-config-driven on both ends; no agent self-service, no MCP-triggered checkout writes."
---

# Remote project onboarding through the collector backchannel

## 0. Problem

The locality program cut every existing-project read/write path over to
checkout-owner transports, but never re-homed the onboarding lifecycle.
`bbox_project_init` and `bbox_project_register` still stat, probe, and write
the checkout daemon-side. With the daemon in the cage and checkouts on b1,
registering a new project fails closed with "path does not exist". The first
live attempt (pg-flare, 2026-08-10) surfaced the hole; gap-f2618892 records it.

## 1. Decision

Onboarding rides the collector backchannel. The checkout-owner collector is
the only component that already has both checkout filesystem access and an
authenticated, scope-bound daemon channel. A new project is onboarded by
operator config on both ends, never by an agent tool call:

1. The operator adds the scope to the daemon's producer config
   (`[code_collection.producers]` scopes in the cage stack config).
2. The operator adds the project to the collector config on the checkout host
   (root, scope, published ref).
3. The collector, on its next cycle, probes the checkout locally and presents
   an authenticated onboarding request. The daemon validates the request
   against the producer grant and runs the catalog find-or-create/attach
   composite. Registration becomes a derived consequence of the operator's
   two-sided config intent.

An agent or operator may still run `bbox-code-collector --config <cfg> init
<path>` on the checkout host to write `.bbox` scaffolding (config.toml with
the first-commit repo_id, mcp.json, local/.gitignore). That is a local file
operation; no daemon round-trip is needed for scaffolding.

## 2. Trust model

The producer token is scope-bound. An onboarding request authenticated by
that token is authoritative only for scopes present in the producer grant,
which the operator controls daemon-side. The collector cannot register
anything the operator did not already write into the daemon config. The
daemon revalidates every probed field it can check independently: scope
membership in the grant, repo_id equality with the scope, relpath shape, and
scope uniqueness across the catalog.

Compare the rejected alternative: executor-mediated registration (daemon asks
fleetd to probe/init on the worker host in response to an MCP tool call).
That grants any MCP caller the ability to trigger writes into checkouts and
creates a new daemon-trusts-worker surface. The collector backchannel has
neither property.

## 3. Protocol

One authenticated internal endpoint:

```text
POST /internal/code-source/v1/catalog/onboard
```

Request: producer bearer token; the published scope; the probed checkout
facts (canonical checkout dir, checkout project dir, project root relpath,
checkout kind, validated scope as read from the committed config, branch ref,
capabilities, declared aliases, checkout-id). The daemon:

1. authenticates the token and checks scope membership in the grant;
2. checks repo_id/scope consistency and catalog uniqueness;
3. runs the existing register/attach composite (the same transaction the
   local arm uses) with the presented probe facts;
4. returns the receipt (project_id, attachment_id, created/already_attached,
   epoch) plus nomination outcome.

The daemon treats the presented facts as worker claims: every field that can
be validated daemon-side is validated, and the residual trust (the canonical
path string itself) is exactly the trust the existing publication lanes
already place in the producer.

The collector drives idempotently: before each publication cycle it probes
onboarding for every configured project and onboards any scope the daemon
reports as unknown. Retries are safe; the composite is find-or-create.

## 4. MCP behavior

`bbox_project_register` and `bbox_project_init` on a path the daemon cannot
stat fail closed with a typed error naming the collector-driven flow
(`error.project_onboarding_remote`), instead of a bare "path does not exist".
The tools keep their existing local behavior when the path IS daemon-local
(the dev-daemon and same-host cases remain supported).

## 5. Marker interaction

A newly onboarded project is not covered by any locality marker (markers pin
explicit project sets). It renders through the named compatibility lanes
until an operator runs the relevant cutover ceremony for it. Knowledge
transport coverage classifies it uncovered; nothing fails open.

## 5a. Startup ordering decisions (from the 2026-08-10 live fire)

Two fail-closed startup gates had to learn about onboarding before this flow
could run, both proven against the live estate:

1. **Producer grant admission.** The producer-auth constructor resolved every
   configured scope against the catalog at startup and refused to boot on an
   unregistered scope, which made "add the scope to the daemon config first"
   impossible. A configured catalog-mode scope with no project is now admitted
   as pending-onboarding: it is excluded from every publication lane and only
   the onboard endpoint may accept it. Bridge-mode resolution still fails
   closed. Proven: the 2026-08-10 deploy with a pre-registered scope refused
   boot with `code-collection scope is not registered`.
2. **Marker verify vs. crash-wedged staging.** The code-source locality
   startup verify required every covered project's active generation to be in
   state `Active`, but a crash between the activation journal write and the
   final state flip leaves a completed staging wedged at `StagingIndex`, and
   the reconciler that would repair it runs after the gate. The gate now
   checks the workspace manifest and activation journal first and accepts
   `StagingIndex` when both agree (staging completed; only the flip was lost);
   any other non-Active state still refuses. Proven live: the 2026-08-10 OOM
   during post-reindex activation wedged one project exactly this way and
   bricked startup until the state was repaired by hand; the regression test
   wedges a fixture the same way.

## 6. Non-goals

- No agent self-service registration.
- No MCP-triggered writes into checkouts.
- No relaxation of producer scope grants; onboarding cannot widen them.
- No automatic marker coverage for new projects.

## 7. Verification

- protocol: round-trip, unknown-variant refusal, tampered-scope refusal;
- daemon: grant-membership enforcement, repo_id mismatch refusal, duplicate
  scope refusal, idempotent re-onboard, receipt shape;
- collector: probe correctness on a fixture checkout, init subcommand file
  output, disabled-without-config behavior;
- end to end on the estate: onboard a synthetic throwaway repo, prove
  catalog attachment, first collection, and search visibility.
