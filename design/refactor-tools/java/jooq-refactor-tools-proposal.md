---
title: "jOOQ Refactor Tools Proposal"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - refactor-tools
  - java
  - jooq
tags:
  - refactor-tools
  - java
  - jooq
  - proposed-atoms
status: "proposed; awaiting implementation"
brief: "Proposed jOOQ-aware Java refactor plan kinds and atoms for query extraction, repository synthesis, transaction hardening, codegen inventory, and typed projection support."
---

# jOOQ Refactor Tools Proposal

## Motivation

Large Java applications that use jOOQ often accumulate query logic across UI
classes, admin/service classes, scheduled jobs, report generators, and ad hoc
utility layers. The generated schema model gives strong type information, but
the surrounding application can still drift into repeated query fragments,
ambiguous transaction ownership, generated record leakage across boundaries,
stringly typed aliases, and large classes whose real cohesion is a set of
query shapes rather than a UI or service responsibility.

The useful refactor surface is broader than "move this method". A jOOQ-aware
tool should understand `DSLContext` source and qualifier, generated table and
record references, projection-to-constructor mappings, `multiset` nesting,
transaction callback scopes, fetch shape, pagination, raw SQL fragments, and
code generation configuration. It should also support synthesis: creating a
new repository, data service, query method, DTO projection, or condition
builder from an operator-supplied shape while preserving the existing
dependency injection and transaction style.

This document proposes generic plan kinds and atoms. It intentionally does not
encode details from any one application.

## Observed Generic Patterns

- Generated jOOQ sources are produced from one or more schemas and wired into
  the Java build as source or compiled artifacts.
- Codegen configuration may use forced types, converters, bindings, and custom
  generator strategies that add interfaces or naming conventions to generated
  records.
- Application code often injects `Provider<DSLContext>` or qualified
  `DSLContext` providers, including read-only or schema-specific contexts.
- Query logic appears directly in views, reports, scheduled jobs, admin
  classes, repositories, and utility classes.
- UI and API layers may bind directly to generated jOOQ records, making later
  domain or DTO boundaries harder to enforce.
- Projection-heavy reads use `Records.mapping(...)`, Java records, local DTOs,
  generated records, nested `DSL.multiset(...)`, and hand-built constructor
  arguments.
- Transaction bodies mix `dsl.transaction(...)`,
  `dsl.transactionResult(...)`, `config.dsl()`, helper methods that accept a
  `DSLContext`, and occasional nested transaction callbacks.
- Complex query fragments use CTEs, window functions, alias tables,
  `DSL.field(String, Class)`, `DSL.table(String)`, `DSL.name(...)`,
  `DSL.inline(...)`, and `DSL.val(...)`.
- Repeated condition construction, joins, projections, ordering, and pagination
  logic often form natural extraction targets.

## Plan Kinds

### `java_jooq_codegen_config_inventory`

Read-only inventory of jOOQ generation configuration.

The plan reports generation tasks, generated source roots, schema-to-package
mappings, forced types, converters, bindings, custom generator strategy hooks,
and generated source ownership.

### `java_jooq_query_structure_analysis`

Read-only analysis of jOOQ query blocks in selected Java files.

For each query block, the plan reports:

- `DSLContext` origin, qualifier annotation if visible, and provider field.
- Tables, aliases, joins, CTEs, nested `multiset` queries, and raw fragments.
- Projection fields, constructor mapping target, fetch shape, and nullability
  hazards visible from the chain.
- Captured parameters and locally computed conditions.
- Transaction scope and whether the query uses the callback-owned context.
- Boundary crossing, such as returning generated records from UI/API-facing
  methods.

### `java_jooq_dsl_context_audit`

Read-only audit of `DSLContext` ownership and transaction usage.

The plan identifies provider fields, qualified contexts, direct `Provider.get()`
usage, methods that accept `DSLContext`, transaction callback scopes, nested
transactions, context mixing inside callbacks, and reads performed through
contexts whose visible qualifier annotations or operator-supplied policy mark
them as schema-specific or write-oriented. It must not infer read/write safety
from method names, statement shape, table names, or schema names; it reports
evidence and asks the operator to confirm intended policy.

### `java_jooq_projection_mapping_analysis`

Read-only analysis of selected projections and mapping targets.

The plan compares selected jOOQ fields, nested records, `Records.mapping(...)`
constructors, Java record constructors, generated record constructors, and
manual lambdas. It reports arity risks, alias/type drift, nullable-to-primitive
risks, generated record boundary leakage, and opportunities to synthesize a
named projection DTO.

### `java_jooq_raw_sql_fragment_audit`

Read-only audit of stringly typed jOOQ fragments.

The plan finds `DSL.field(String, ...)`, `DSL.table(String)`,
`DSL.condition(String)`, string aliases, and CTE field lookups. It classifies
each as one of:

- Generated-table replaceable.
- Alias/name expression that should use `DSL.name(...)`.
- Dynamic SQL that must remain explicit.
- Unsafe or ambiguous fragment needing operator review.

It must not rewrite arbitrary SQL strings without an explicit operator mapping.

### `java_jooq_raw_sql_fragment_rewrite`

Mutating rewrite of audited raw SQL fragments to safer jOOQ expressions.

The input is an approved audit result plus operator mappings for each fragment
to rewrite. The plan may replace raw field or table strings with generated
jOOQ references, `DSL.name(...)` expressions, or typed alias fields. It refuses
dynamic SQL, ambiguous CTE fields, or any rewrite whose target cannot be
expressed from the approved mapping.

### `java_jooq_extract_query_method`

Mutating extraction of a selected jOOQ fluent chain into a named method in the
same class.

The plan preserves the existing `DSLContext` source, fetch shape, mapping
target, imports, comments attached to the query, and transaction context. It
parameterizes captured local values and refuses extraction when the selected
range crosses unrelated side effects or transaction boundaries.

### `java_jooq_extract_query_object`

Mutating extraction of a selected query block into a query object or query
factory class.

The new class owns the typed query method and accepts a `DSLContext` or
provider according to operator policy. The source class delegates to it. The
plan is useful for complex CTE/window/multiset queries that are too large for a
private helper but do not yet justify a repository.

### `java_jooq_extract_repository`

Mutating extraction of a cohesive group of query and persistence methods into
a repository or data service.

Inputs include target package, class name, dependency injection style,
`DSLContext` qualifier policy, transaction boundary policy, selected methods,
and desired public API. The plan preserves method signatures unless the
operator explicitly requests DTO boundary changes. It refuses to merge methods
using incompatible contexts or transaction ownership without explicit mapping.

### `java_jooq_synthesize_repository`

Mutating synthesis of a new repository or data service from an operator-defined
schema model slice.

Inputs should include:

- Target package, class name, and injection style.
- `DSLContext` provider or qualifier policy.
- Read-only versus read/write policy.
- Tables and generated record types.
- Methods to generate, such as `findById`, `list`, `exists`, `insert`,
  `update`, count, paged search, explicit hard delete, and explicit
  soft-delete.
- Return shape: generated record, DTO, Java record, or mapper method.
- Transaction policy for writes and multi-step operations.

The tool should generate conservative, compile-oriented code and require the
operator to provide any business rules, security filters, soft-delete columns,
tenant filters, auditing fields, sequence usage, or authorization policy.
Soft-delete synthesis requires the operator to provide the exact column,
predicate, and update values. Hard-delete synthesis requires explicit operator
scope because it is destructive.

### `java_jooq_synthesize_query_method`

Mutating synthesis of one typed query method from an operator-supplied query
shape.

The input model includes tables, aliases, joins, selected fields, conditions,
parameters, grouping, ordering, pagination, fetch shape, and mapping target.
The output is a method in an existing class or a new repository/query object.
The plan must prefer generated jOOQ references over raw SQL strings and must
not invent database semantics that are absent from the input.

### `java_jooq_synthesize_dto_projection`

Mutating synthesis of a DTO or Java record plus matching jOOQ projection.

The tool creates a named projection type and rewrites a selected
`Records.mapping(...)` or lambda mapping to use it. It must preserve field
order, aliases, nested `multiset` result types, nullability-visible types, and
imports. It should support local nested DTOs only when that matches the
surrounding code style; otherwise it creates a package-level type.

### `java_jooq_condition_builder_extract`

Mutating extraction of repeated `Condition` construction into a named helper,
specification, or repository-private method.

The plan captures optional filters, enum/string conversion, date ranges,
search terms, soft-delete predicates, and authorization predicates. It refuses
to merge similar conditions when differences affect result visibility or
write-safety.

### `java_jooq_transaction_boundary_normalize`

Mutating rewrite that normalizes a method to use the correct callback-owned
`DSLContext`.

Examples include replacing provider lookups inside a transaction callback with
`config.dsl()`, passing the transaction context into helper methods, and
removing accidental mixed-context calls. The plan must preserve nesting
semantics unless the operator explicitly asks to flatten nested transactions.

### `java_jooq_generated_record_boundary_analysis`

Read-only analysis of generated jOOQ records crossing UI, API, or domain
boundaries.

The plan reports boundary crossings and suggests DTO or mapper targets. It
does not change return types.

### `java_jooq_generated_record_boundary_rewrite`

Mutating rewrite that hardens selected generated-record boundaries.

The plan can synthesize DTOs and mappers, then rewrite selected methods to
return DTOs instead of generated records. It requires explicit operator scope
because it can change public surface.

### `java_jooq_codegen_extension_apply`

Mutating apply plan for jOOQ codegen extension configuration.

The plan can add or update forced types, converters, bindings, or generator
strategy rules when the operator supplies the exact database type pattern,
Java type, converter/binding class, target schemas, and validation command. It
must not infer broad forced-type rules from a single usage site.

## Atomic Agents

Each plan kind should have a narrow atom that either produces a plan or applies
an already approved plan:

- `java-jooq-codegen-inventory`
- `java-jooq-query-structure-analysis`
- `java-jooq-dsl-context-audit`
- `java-jooq-projection-mapping-analysis`
- `java-jooq-raw-sql-fragment-audit`
- `java-jooq-raw-sql-fragment-rewrite`
- `java-jooq-extract-query-method`
- `java-jooq-extract-query-object`
- `java-jooq-extract-repository`
- `java-jooq-synthesize-repository`
- `java-jooq-synthesize-query-method`
- `java-jooq-synthesize-dto-projection`
- `java-jooq-condition-builder-extract`
- `java-jooq-transaction-boundary-normalize`
- `java-jooq-generated-record-boundary-analysis`
- `java-jooq-generated-record-boundary-rewrite`
- `java-jooq-codegen-extension-apply`

Atoms must stay atomic: they may carry operator-provided policy through to the
plan, but they must not invent schema qualifiers, read/write safety,
authorization filters, soft-delete semantics, auditing behavior, or public API
changes.

## Higher-Level Workflows

### Query Extraction From Large Class

1. Run `java-jooq-query-structure-analysis` on the class.
2. Run `java-jooq-dsl-context-audit` when multiple contexts or transactions
   are visible.
3. Extract simple chains with `java-jooq-extract-query-method`.
4. Extract cohesive query families with `java-jooq-extract-query-object` or
   `java-jooq-extract-repository`.
5. Compile the touched module.

### Repository Synthesis

1. Run `java-jooq-codegen-inventory` to learn generated packages and codegen
   ownership.
2. Operator supplies tables, target package, injection style, context policy,
   method list, and transaction policy.
3. Run `java-jooq-synthesize-repository`.
4. Run `java-jooq-projection-mapping-analysis` on generated projection methods.
5. Compile the touched module.

### DTO Boundary Hardening

1. Run `java-jooq-generated-record-boundary-analysis`.
2. Operator selects methods whose return surface may change.
3. Run `java-jooq-synthesize-dto-projection`.
4. Run `java-jooq-generated-record-boundary-rewrite` on selected call sites.
5. Compile and run focused tests where available.

### Transaction Safety Cleanup

1. Run `java-jooq-dsl-context-audit`.
2. Operator confirms expected transaction policy.
3. Run `java-jooq-transaction-boundary-normalize` on selected methods.
4. Compile and run focused tests for write paths where available.

### Raw Fragment Hardening

1. Run `java-jooq-raw-sql-fragment-audit`.
2. Operator confirms mappings for ambiguous aliases or dynamic SQL.
3. Run `java-jooq-raw-sql-fragment-rewrite` to replace approved fragments
   with generated references or `DSL.name(...)` based expressions.
4. Compile the touched module.

## Validation Contract

The v1 implementation may be syntax-guided using Java parsing plus jOOQ naming
conventions. It must be explicit about semantic confidence:

- `syntax_only`: parsed Java and local symbol evidence only.
- `compile_verified`: touched module compiled after the plan was applied.

No plan should claim SQL semantic correctness without an actual compile and,
where relevant, a database-backed integration check. Mutating plans should
preserve imports, formatting, comments attached to selected query blocks,
`DSLContext` qualifier annotations, and transaction callback ownership.

Generated jOOQ sources are read-only by default. Codegen configuration,
generator strategies, converters, and bindings are writable only for plan kinds
that explicitly target codegen.

## Refusal Rules

A jOOQ plan kind must fail closed when:

- The selected query crosses a transaction boundary that the operator did not
  include in scope.
- The source and target use incompatible `DSLContext` qualifiers.
- The plan would infer authorization, tenant, soft-delete, audit, or read-only
  semantics not present in the input.
- The selected projection cannot be matched to the mapping target arity.
- A generated source file would be edited outside a codegen-specific plan.
- A public return type would change without explicit operator approval.
- Raw SQL string rewriting requires understanding dynamic SQL that is not
  represented in the Java AST.
- A hard-delete or soft-delete method is requested without explicit operator
  scope and exact deletion policy.

## Non-Goals

- Database schema migration generation.
- Index or query performance tuning beyond surfacing obvious query shape.
- Replacing jOOQ with an ORM or hand-written SQL layer.
- Inferring business rules from table or column names.
- Automatically changing application security or authorization behavior.
- Proving runtime SQL equivalence without integration tests.

## Implementation Notes

The first useful version should favor high-quality analysis and conservative
single-query transformations. Repository synthesis should initially target
straightforward CRUD, list, exists, and typed projection methods before trying
to synthesize complex CTE/window/multiset queries. Complex queries are better
handled by extract-query-object plus human review until enough fixtures exist
to validate larger synthesis.

The durable `RefactorPlan` should carry an `operator_policy_inputs_used` field
listing operator-supplied policies consumed by the plan, such as context
qualifier, read/write intent, public API change approval, deletion policy, and
transaction normalization approval. This field belongs on the saved plan, not
only in a summary. These policies are not agent discretion; they are inputs
from the operator.
