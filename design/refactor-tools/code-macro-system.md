---
title: "Code Macro System"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
  - macros
  - java
tags:
  - refactor-tools
  - macros
  - mcp
  - java
date: 2026-05-25
status: "design proposal"
brief: "Proposes a small user-definable code macro system backed by language adapters such as OpenRewrite and Palantir JavaPoet, exposed through a dedicated `macro_*` MCP cluster."
---

# Code Macro System

## Thesis

Blackbox should grow a dedicated `macro_*` tool cluster for typed,
user-definable code synthesis. A macro is not a refactor plan with more ceremony
and not a prompt template. It is a small, inspectable recipe that turns typed
inputs into a reviewable patch using language-aware backends.

For Java, the first useful shape is:

```text
MacroDefinition + typed inputs + project probes
  -> MacroPlan
  -> Java backend operations
  -> reviewed edit set + checks
```

The macro layer should be intentionally thin. It owns registration, schemas,
planning, provenance, and composition. It does not own Java parsing or source
printing. Java mutation and generation should back onto proven libraries:

- OpenRewrite for existing-source rewrites;
- Palantir JavaPoet for new Java declarations and files;
- existing Blackbox refactor tools only when a macro needs a true refactor that
  already exists.

## Broader Goal

Macros are the machinery for turning one-off project/library knowledge into
user-definable custom tooling.

Some behavior that currently appears as Rust-coded refactor plan kinds should
eventually become system-default or example macro libraries. Vaadin and Guice
are good examples: they encode library-specific conventions, package shapes,
annotation policies, binding rules, route registration behavior, and refusal
conditions. Those are real capabilities, but they should not all require new
Rust plan kinds forever.

The macro/refactor split should move toward this boundary:

- **Refactor core**: generic, reusable source transformations and transaction
  safety.
- **Language backends**: Java parsing, source generation, source rewriting,
  import management, and formatting.
- **Macro libraries**: project/library/framework behavior expressed as data and
  reusable recipes.

In other words, the system should let a user define the same class of behavior
that today lives in hard-coded Vaadin/Guice helpers, but scoped to their project
and reviewable in-repo. In v1, "define behavior" means compose existing backend
operations, probes, refusals, and validations as data. Defining a brand-new Java
rewrite still requires adding a backend operation or a vetted OpenRewrite recipe
pack; macros should not become arbitrary user code execution.

Hard rule: project/library-specific primitives do not belong in the Rust core
engine. A core operation like "insert this Java member from a template,"
"search for methods with this annotation and return type," or "add these
imports if absent" is acceptable. A core operation like "add a Guice binding" or
"register a Vaadin route" is not. Those belong in macro library data or in an
installable JVM recipe pack that the macro library depends on.

If a current Rust-coded primitive cannot be expressed under those rules, treat
that as an ontology gap. The answer is not "keep this framework primitive in
core"; the answer is to name the missing concept and decide whether it belongs
as:

- a generic macro operation;
- a generic Java backend operation;
- a bounded probe/predicate/refusal capability;
- a transaction/validation capability;
- an installable recipe-pack capability; or
- intentionally out of scope for data-defined macros.

This gap review should be part of migrating every existing library-specific
plan kind.

Recipe packs are executable code, not data. The boundary here is authority and
provenance, not sandbox security: a macro definition must not make executable
backend code look like ordinary project-scoped recipe data. Recipe packs should
be artifact-catalog-versioned backend extensions with an explicit operator
install/approval step, and project macros should reference them by id/version.
This keeps framework-specific code out of Rust core while making code-bearing
macro dependencies auditable.

## What Macros Are For

Refactor tools are about preserving existing behavior while changing structure:
extract class, move methods, rewrite call sites, organize imports, or rename a
symbol.

Macros are about instantiating a known project pattern:

- create a service interface, implementation, injection site, and binding;
- inject an event bus and add a subscriber/publisher shape;
- add a repository method plus DTO/projection skeleton;
- add a test fixture or architecture rule in the local house style;
- add a Vaadin presenter/view/service pattern across several files.

Those operations often include refactors, but the value is the pattern recipe,
not the existence of another route into `bbox_refactor_plan`.

## Namespace

Use a dedicated MCP prefix:

```text
macro_*
```

The Blackbox server identity already provides the product boundary. `macro_*`
is clearer than overloading `bbox_*`, and `work_*` is reserved for restricted
agents operating inside atoms/workflows.

Canonical tools:

- `macro_list`
- `macro_describe`
- `macro_validate`
- `macro_plan`
- `macro_apply`
- `macro_run`
- `macro_register`
- `macro_unregister`
- `macro_explain`

Generated per-macro tools may exist later as aliases, but the generic tools are
the stable API.

## Minimal Model

The core model should stay small enough that project-local macros are writable
by humans.

### `MacroDefinition`

A registered recipe:

```json
{
  "id": "java.add_service_boundary",
  "version": "1",
  "language": "java",
  "scope": "builtin|user|project",
  "title": "Add Java service boundary",
  "inputs_schema": {},
  "effects": ["creates_type", "changes_dependency_injection"],
  "authority_gates": [],
  "probes": [],
  "operations": [],
  "validations": [],
  "refusals": []
}
```

Definitions are data, not executable plugin code. Builtins ship with Blackbox.
Project macros live under `.bbox/macros/` so teams can review them like source.
User macros can live in operator config.

A definition must be able to express behavior, not just file templates. That
means it needs declarative probes, predicates, backend operations, and refusal
rules:

```json
{
  "probes": [
    { "id": "module", "kind": "java.search.type", "where": "inputs.guice_module" },
    {
      "id": "binding",
      "kind": "java.search.member",
      "in": "module.type",
      "annotation": "com.google.inject.Provides",
      "return_type": "inputs.binding_return_type"
    }
  ],
  "refusals": [
    {
      "when": "binding.exists",
      "code": "error.guice_binding_exists",
      "message": "Binding already exists for ${inputs.service_type}"
    }
  ],
  "operations": [
    {
      "kind": "java.template.insert_member",
      "target_type": "module.type",
      "imports": [
        "com.google.inject.Provides",
        "com.google.inject.Provider"
      ],
      "template": "@Provides\nProvider<${inputs.provided_type}> ${inputs.method_name}() {\n    return ${inputs.supplier_expression};\n}"
    }
  ]
}
```

The exact expression language should stay small, but this is the bar: a project
macro must be able to encode convention checks and refusal behavior, not merely
paste a snippet.

The v1 expression grammar should be deliberately bounded:

- dotted-path lookup into `inputs` and named probe results;
- string/boolean/null equality and existence checks;
- simple list membership;
- `${path.to.value}` interpolation in string fields;
- no arithmetic;
- no loops;
- no user-defined functions;
- no arbitrary boolean programs beyond `all` / `any` over bounded predicate
  lists.

### `MacroInvocation`

One call against a recipe:

- `macro_id` and optional `version`;
- `project_dir`;
- typed `inputs`;
- optional anchors: files, symbols, ranges, graph refs;
- explicit operator authority flags when needed.

Input validation happens before project mutation and before any expensive
backend work.

### `MacroPlan`

The review artifact:

```json
{
  "macro_id": "java.add_service_boundary",
  "summary": "Create BillingService boundary for BillingView.refresh",
  "semantic_status": "syntax_only|indexed_hints|lsp_verified|lsp_verified_partial|mixed|template_only",
  "operations": [
    {
      "id": "emit-interface",
      "kind": "emit",
      "semantic_status": "syntax_only"
    }
  ],
  "edits": [],
  "checks": [],
  "questions": [],
  "refusals": [],
  "backends_used": ["openrewrite", "javapoet"],
  "operator_opt_outs_used": [],
  "provenance": {}
}
```

`MacroPlan` is not a `RefactorPlan`. It may contain delegated refactor-plan
summaries, but its first-class shape is "this recipe will create/modify these
artifacts using these backends."

Macro semantic status should reuse the existing refactor vocabulary where it
can:

- `syntax_only`
- `indexed_hints`
- `lsp_verified`
- `lsp_verified_partial`

Macro-only statuses are additive:

- `template_only`: generated text has not been parsed or semantically checked;
- `mixed`: nested operations have different statuses, listed on each
  `MacroOperation`.

No backend-selected `ast_checked` downgrade should silently become
`template_only`; if an operator or macro requires an AST backend and the backend
is unavailable, planning fails.

### `MacroOperation`

Use a small operation vocabulary:

- `probe`: inspect project style, packages, symbols, DI conventions, or build
  commands;
- `emit`: generate a new artifact from a typed backend operation;
- `rewrite`: transform existing source using a language backend;
- `delegate_refactor`: call an existing Blackbox refactor kind when that is the
  right primitive;
- `validate`: parse, organize imports, compile, or test;
- `record`: leave structured residue or follow-up notes.

That is enough. Do not define a large pseudo-language of low-level edit steps.
The Java backend should handle Java detail.

## Macro Libraries

Macro definitions should be grouped into libraries:

- `builtin.java`: tiny cross-project Java operations and examples;
- `builtin.java.guice`: Guice binding/injection recipes;
- `builtin.java.vaadin`: Vaadin route/view/provider recipes;
- `project`: repo-owned macros under `.bbox/macros/`;
- `user`: operator-owned macros shared across repos.

System-default libraries are not privileged because they are hard-coded. They
are privileged because Blackbox ships them, tests them, and documents them.
Where possible, they should use the same data format as project macros.

This gives an upgrade path for existing library-specific Rust helpers:

1. Keep the Rust helper only when it passes the genericity test: a
   non-Guice/non-Vaadin macro could plausibly use the same transformation or
   backend primitive. "Complex" is not enough.
2. Extract the library-specific policy into a macro definition.
3. Attempt to express the old behavior using the current macro ontology and
   backend operations.
4. For every part that cannot be expressed, record an ontology gap before adding
   code. The gap must name the missing capability, classify it, and explain why
   it is generic rather than project/library-specific.
5. Fill only generic gaps in the engine/backend, or ship framework-specific
   logic as an installable JVM recipe pack owned by the macro library. Do not
   add a Guice/Vaadin-specific primitive to the Rust core.
6. Keep the old refactor plan kind as a compatibility wrapper if callers depend
   on it.

Vaadin and Guice should become proving grounds for this migration. They are
specific enough to need recipes, common enough to justify shipped examples, and
currently visible as library taint in the Rust refactor surface.

### Ontology Gap Records

An ontology gap record should be small and reviewable:

```json
{
  "source_plan_kind": "java_vaadin_provider_binding_generation",
  "missing_capability": "find methods by annotation and erased/generic return type",
  "classification": "java_backend_probe",
  "genericity_argument": "needed for Guice @Provides, Spring @Bean, JAX-RS routes, and project-local annotation conventions",
  "not_library_policy": "does not mention Guice, Vaadin, UI, or VaadinSession",
  "candidate_surface": "java.search.member",
  "fallback": "recipe_pack"
}
```

The key test is whether a non-Guice/non-Vaadin macro could plausibly use the
same capability. If not, keep it out of the engine and express it as macro data
or recipe-pack code.

### Worked Migration Sketch

Current Rust helper: `java_vaadin_provider_binding_generation`.

Library macro target:

```text
builtin.java.vaadin.ensure_provider_bindings
```

Macro data owns:

- the required bindings: `Provider<UI>` and `Provider<VaadinSession>`;
- imports: `com.google.inject.Provides`, `com.google.inject.Provider`,
  `com.vaadin.flow.component.UI`, `com.vaadin.flow.server.VaadinSession`;
- provider method names: `provideUiProvider`,
  `provideVaadinSessionProvider`;
- provider bodies: `return UI::getCurrent;` and
  `return VaadinSession::getCurrent;`;
- refusal policy: Spring project detected, non-Guice module, Vaadin not
  detected, duplicate provider already present.

Generic backend/probe capabilities required:

- `java.search.type`: find the target module type in a source file;
- `java.search.project_text`: detect package/import/text markers across a
  project with bounded directory exclusions;
- `java.search.member`: find methods by annotation and erased/generic return
  type;
- `java.template.insert_member`: insert a Java member template into a target
  type with imports and formatting;
- `java.validate.parse` and import organization.

Ontology gaps found while translating:

```json
[
  {
    "missing_capability": "find methods by annotation and erased/generic return type",
    "classification": "java_backend_probe",
    "genericity_argument": "usable for Guice @Provides, Spring @Bean, JAX-RS endpoints, and project-local annotation conventions",
    "candidate_surface": "java.search.member"
  },
  {
    "missing_capability": "scan project Java sources for bounded text/import/package markers with standard build-directory exclusions",
    "classification": "java_backend_probe",
    "genericity_argument": "usable for framework detection, source-set detection, migration guards, and project-local convention probes",
    "candidate_surface": "java.search.project_text"
  },
  {
    "missing_capability": "insert Java member from parameterized template with imports",
    "classification": "java_backend_operation",
    "genericity_argument": "usable for provider methods, lifecycle hooks, test methods, event subscribers, and generated adapters",
    "candidate_surface": "java.template.insert_member"
  }
]
```

No gap should mention `UI`, `VaadinSession`, `@Provides`, or Spring refusal as
engine concepts. Those remain macro-library data.

## Java Backend

The Java macro backend should be a thin adapter over OpenRewrite and Palantir
JavaPoet. The JVM sidecar should own Java declaration modeling because it is
closest to both libraries' native APIs. Rust should not maintain a third Java
AST-shaped model.

### Sidecar-Owned Declaration Model

The sidecar can keep a deliberately small declaration model:

```text
JavaTypeDecl
  package
  kind: class|interface|record|enum
  name
  modifiers
  annotations
  extends
  implements
  members

JavaMemberDecl
  field | constructor | method | nested_type

JavaMethodDecl
  modifiers
  return_type
  name
  parameters
  throws
  body: explicit | delegate | throw_stub | empty

JavaDependencyDecl
  type
  field_name
  injection: constructor | field | provider
  qualifier
```

This model should not be a Rust public API and should not appear in MCP schemas
as a generic "Java AST." It is sidecar glue so the same typed macro operation
can generate a new file with JavaPoet or insert an equivalent member with
OpenRewrite `JavaTemplate`. It should not contain framework-shaped structs such
as `GuiceBindingDecl` or `VaadinRouteDecl`; those concepts live in macro data or
recipe packs.

Rust-side macro definitions should talk in macro/backend operation schemas, for
example:

```json
{
  "kind": "java.emit.type",
  "backend": "javapoet",
  "type": "interface",
  "name": "inputs.service_name",
  "package": "inputs.service_package",
  "methods": "inputs.method_contracts"
}
```

The sidecar translates that operation into JavaPoet/OpenRewrite-native objects.

### Backend Responsibilities

OpenRewrite should own existing-source transformation:

- insert a field/method/constructor into an existing type;
- add constructor injection to a class;
- replace one method body;
- search for declarations, annotations, types, imports, and invocation shapes;
- insert a member from a parameterized `JavaTemplate`;
- apply a vetted OpenRewrite recipe from an installed macro library recipe pack;
- rewrite selected invocations or receivers;
- manage imports and formatting where possible.

Palantir JavaPoet should own isolated source generation:

- new interface files;
- new implementation class files;
- generated method signatures and constructors;
- simple test skeletons or architecture-rule classes.

Existing refactor tools should be callable as a macro operation when they are
the right tool:

- `extract_java_class` for real method/field extraction;
- `extract_java_code_block_to_method` or `convert_method_to_class` for local
  behavior movement;
- `migrate_java_method_receiver` for receiver rewrites and DI-aware caller
  updates;
- `java_split_provider` for provider-chain cleanup;
- `extract_java_interface`, `add_java_implements`, and
  `java_lsp_organize_imports` where already adequate.

This keeps boundaries clean: macros compose patterns, OpenRewrite/JavaPoet edit
Java, and refactor tools remain available for semantic movements that are
already implemented.

Backends must be pure `EditSet` producers. OpenRewrite, JavaPoet, and any
future sidecar must return candidate edits and metadata; they must not write
project files directly. All mutation flows through the shared transaction layer.

Backend unavailability fails closed. If the macro operation asks for OpenRewrite
or JavaPoet and the JVM, sidecar jar, classpath, or selected recipe is missing
or crashes, planning returns a typed backend error such as
`error.backend_unavailable`. It must not silently downgrade to a template-only
plan.

### Rust Integration Shape

In Rust, this likely wants three narrow traits/modules:

```rust
trait MacroPlanner {
    fn plan(&self, invocation: MacroInvocation) -> Result<MacroPlan>;
}

trait JavaMacroBackend {
    fn execute(&self, op: JavaMacroBackendOp) -> Result<EditSet>;
    fn organize_imports(&self, files: &[PathBuf]) -> Result<EditSet>;
}

trait EditTransaction {
    fn preview(&self, edits: EditSet) -> Result<PatchPreview>;
    fn apply(&self, edits: EditSet, confirm: bool) -> Result<ApplyReport>;
}
```

`EditTransaction` can be factored from the same safety machinery used by
refactor apply/run: path checks, preimage hashes, dirty-file refusal, parse
validation, and rollback. The important point is that macros should reuse the
transaction layer, not force every macro operation to masquerade as a refactor
plan kind.

This factoring is real implementation work, not a wrapper rename. Existing
refactor apply is byte-range `TextEdit` + `original_sha256` centric, while an
OpenRewrite helper may naturally return whole-file transformed source. The
transaction layer therefore needs either:

- a reliable diff-back-to-byte-ranges step before apply; or
- a first-class whole-file replacement edit that is hash-guarded, parse
  validated, and rollback-capable.

The second option is likely cleaner for sidecar output.

### Command Policy

Macro backend execution is internal to planning and should not be modeled as a
shell command step. The daemon invokes the Java sidecar through a controlled
backend adapter and receives an `EditSet`.

Validation commands are different. Java macros need `mvn`, `gradle`, `javac`,
or project-specific test commands, while atom-dispatched `bbox_refactor_run`
commands are deliberately cargo-only. Macro-run therefore needs its own
language-aware command policy:

- operator-origin macro runs may execute explicit operator-supplied validation
  commands under the normal project/path/touches rules;
- agent-origin macro runs may execute only commands selected from a
  macro/language allowlist; project-declared validation profiles are honored for
  agent-origin runs only when the selected command also appears on that fixed
  allowlist or the profile has explicit operator approval;
- mutating validation commands must declare touched files or refuse;
- backend sidecar invocation is not user-controlled shell and is not expanded
  from macro data.

This keeps Java validation possible without weakening the refactor atom
allowlist.

## Existing Code To Reuse

The current repo already has useful Java/refactor components:

- `src/refactor/mod.rs` defines the refactor plan/apply/run safety model,
  command steps, Java parameter/field specs, and plan dispatch.
- `src/refactor/java/di_plumbing.rs` handles `@Inject` namespace detection,
  direct versus `Provider<T>` fields, existing injected-field reuse, and field
  creation.
- `src/refactor/java/extract_class.rs` contains hard-won extraction behavior:
  captured dependencies, `wiring_mode`, Guice field-injection refusal, callback
  externals, moved fields, and residue reporting.
- `src/refactor/java/migrate_receiver.rs`, `split_provider.rs`, and
  `replace_static_ref.rs` already cover DI-aware call-site rewrites.
- `src/refactor/java/leaf_plans.rs` includes `extract_java_interface`,
  `add_java_implements`, and `java_lsp_organize_imports`.
- `src/refactor/java/vaadin_provider_bindings.rs` and
  `vaadin_view_synthesis.rs` show useful generation/refusal patterns.

Macros should reuse these as helpers or delegated operations, but the long-term
goal is sharper than reuse: identify which pieces are generic Java backend
operations and which pieces are library policy that belongs in default macro
libraries.

Examples:

- Java member insertion from a parameterized template is a reusable backend
  operation; "this member is a Guice binding" is macro-library data.
- "Refuse Spring/Vaadin projects for this Guice provider binding recipe" is
  macro-library policy.
- Generic annotation-value search is a reusable probe; Vaadin route collision
  detection is a macro-library use of that probe.
- "Generate this specific Vaadin view skeleton with these access-policy rules"
  is macro-library behavior.
- Field/constructor insertion can be backend operations; DI field naming and
  `Provider<T>` preference are macro/project policy unless expressed as a
  generic naming template.
- "This project prefers provider injection for session-scoped objects" is
  project macro configuration.

The migration output for a current Rust helper should include both a proposed
macro definition and any ontology gap records discovered while translating it.
If the gap list is large, the design should be refined before implementation;
otherwise the macro system is only wrapping bespoke tools.

## Useful V1

A useful v1 should not be a catalog demo. It should ship one real composite
macro:

```text
java.add_service_boundary
```

The macro takes an existing caller and creates a service boundary around one
named operation.

Required inputs:

- `caller_file`
- `caller_type`
- `caller_method`
- `service_name`
- `service_package`
- `implementation_name`
- `implementation_package`
- `guice_module`
- `method_contract`
- either `implementation_body` + `caller_replacement`, or a supported
  `behavior_source` that delegates to existing refactor tooling

Plan behavior:

1. Probe caller type, method ambiguity, package roots, DI style, existing
   constructors, and Guice module shape.
2. Use JavaPoet to emit the service interface.
3. Use JavaPoet to emit the implementation class.
4. Use OpenRewrite to inject the service dependency into the caller.
5. Use OpenRewrite to replace the caller method body with a delegation.
6. Use OpenRewrite to add the Guice binding.
7. Use OpenRewrite/JDTLS/refactor import tooling to organize touched files.
8. Add a validation command when supplied, or when detected and permitted by the
   active command policy.
9. Report residue as structured follow-up.

This is still useful when v1 accepts explicit `implementation_body` and
`caller_replacement`: the macro owns all boring structural work, while the
caller supplies the business-specific code body that the engine should not
invent.

`behavior_source` is explicitly a later/harder mode unless backed by an
available operation. The design should not pretend that arbitrary inline
statements can already become a service method. Supported v1 options are:

- explicit `implementation_body` plus explicit `caller_replacement`;
- delegation to an existing refactor kind when the source shape matches it;
- refusal with `error.behavior_move_unsupported` when no backend/refactor
  operation can perform the move safely.

V1 refusal cases:

- caller method is missing or ambiguous;
- service or implementation type already exists;
- Guice module cannot be found or is ambiguous;
- binding already exists;
- injection style cannot be classified;
- requested public API change lacks operator authority;
- backend cannot produce parse-valid Java.

## V1 Implementation Tasks

1. Add `macro_*` MCP tools for registry, validation, planning, apply, and run.
2. Add a project/user/builtin macro registry with list-before-register
   semantics.
3. Add `MacroDefinition`, `MacroInvocation`, `MacroPlan`, `MacroOperation`, and
   `EditSet` data structures.
4. Add the minimal declarative machinery needed for probes, predicates,
   refusals, and backend operations in macro definitions.
5. Factor or expose a shared edit transaction layer from refactor apply/run so
   macros get the same safety properties.
6. Add a Java macro backend sidecar/helper:
   - OpenRewrite for existing-source rewrites;
   - Palantir JavaPoet for new source generation;
   - a sidecar-owned declaration model;
   - fail-closed backend availability reporting.
7. Add whole-file replacement support or diff-back-to-byte-range conversion for
   sidecar-produced edits.
8. Add the macro validation command policy for Java/operator/agent origins.
9. Implement `java.add_service_boundary`.
10. Ship `builtin.java.guice` and `builtin.java.vaadin` as example/default macro
   libraries where at least one existing Rust-coded library-specific behavior is
   represented in macro data.
11. Add focused fixtures that prove the macro can create a boundary in a Guice
   Java project and refuse the common ambiguous cases.

## Relationship To Refactor Tools

Macros and refactor tools should be complementary:

- Refactor tools expose semantic transformations as direct primitives.
- Macros expose named project/code patterns that may call several primitives or
  backend operations.
- Both should share edit transaction safety.
- A macro may delegate to `bbox_refactor_plan` when that is the shortest correct
  path.
- A refactor tool should not need to know about macro definitions.

Do not route all Java macro work through new refactor plan kinds just to reuse
the existing MCP surface. That makes macros less useful and turns the design
into "refactor helpers with extra steps."

Likewise, `macro_apply` should not grow an independent apply engine. It should
be the macro-facing confirmation/provenance wrapper over the shared
`EditTransaction`, consuming a `MacroPlan` and applying its `EditSet`. If the
shared transaction becomes separately exposed later, `macro_apply` can remain as
ergonomic sugar that preserves macro audit fields.

## Safety Invariants

- Macro planning is read-only.
- Macro application is hash-guarded and rollback-capable.
- Macro backends are pure `EditSet` producers and do not write files directly.
- Project macros are data, not automatically trusted code execution.
- Public API, destructive, transaction-boundary, and runtime-behavior effects
  require explicit authority gates.
- Agents may pass through operator-supplied authority flags but must not infer
  them.
- Consumed authority flags are recorded on `MacroPlan.operator_opt_outs_used`.
- LLM-generated method bodies must be labeled as `template_only` or `mixed`
  unless parser/LSP/build checks validate them.
- Dynamic generated tools are aliases over generic `macro_*` tools.

## Storage

Recommended project layout:

```text
.bbox/
  macros/
    java.add-service-boundary.json
    java.inject-event-bus.json
    project.add-layer-rule.json
```

Definitions should be JSON or TOML with JSONSchema-friendly input schemas.
Long code templates, if any, should live in sibling template files rather than
inside provider memory.

## Later Macro Families

After `java.add_service_boundary` proves the model:

- `java.inject_event_bus`: detect scan-only versus explicit registration,
  inject publisher/subscriber dependencies, create event type when requested,
  and refuse mixed event contracts.
- `java.add_arch_rule`: generate or update ArchUnit tests from typed layer
  policy.
- `java.add_repository_method`: generate repository method, projection/DTO
  shape, mapper skeleton, and validation hook.
- `project.*`: project-local recipes that encode package layout, DI modules,
  naming policy, and test style.

## Open Questions

- Should macro definitions enter the artifact catalog immediately, or should v1
  use project/user registries only?
- Should the Java backend sidecar be OpenRewrite-first with JavaPoet embedded,
  or two helpers behind one Rust `JavaMacroBackend` adapter?
- How much of refactor apply/run should be factored into a shared transaction
  module before implementing `macro_apply`?
- Should generated per-macro MCP tools be opt-in to avoid catalog bloat?
