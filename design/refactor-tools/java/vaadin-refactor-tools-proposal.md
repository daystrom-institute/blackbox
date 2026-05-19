---
title: "Vaadin Refactor Tools Proposal"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - refactor-tools
  - java
  - vaadin
tags:
  - refactor-tools
  - java
  - vaadin
  - implemented-atoms
status: "archived as implemented in the Vaadin Java refactor toolsuite"
brief: "Implemented Vaadin-aware Java refactor plan kinds and atoms for decomposing large server-side Flow views, synthesizing new views/components, and hardening UI/session lifecycle usage."
---

# Vaadin Refactor Tools Proposal

This proposal added a Vaadin-specific refactor layer on top of the existing Java
plan kinds. The target shape is a server-side Flow application where large
views often accumulate constructor injection, grids/tabs/widgets,
route/navigation logic, dialogs, event handlers, and background/UI access
concerns in one class.

Generic Java refactor primitives can move methods and fields, but Vaadin work
needs more domain rules:

- Components have lifecycle boundaries (`attach`, `detach`, UI/session scope).
- View classes participate in routes, page titles, app layout, navigation, and
  role authorization.
- Server-side UI code can accidentally capture stale `UI` / `VaadinSession`
  objects or touch components off the UI lock.
- Extracting a visually coherent widget is usually more useful than extracting
  an arbitrary method cluster.
- Synthesizing a new view must update route, privilege, and navigation surfaces
  together.

## Implementation Status

Archived on 2026-05-19. The implemented surface includes the Vaadin
read-only analysis/audit/inventory plan kinds, the component/grid/dialog
extraction plan kinds, view synthesis, route/access registration, navigation
helper extraction, thin wrapper atoms, workflow atoms, discovery/dispatch/
behavior-smoke eval rows, and Java plan-kind atom guard coverage.

This document remains as the design and acceptance record. Current behavior is
owned by the Rust modules under `src/refactor/java/`, atom manifests under
`system-defaults/atoms/refactor/java-vaadin-*.json`, and eval catalog entries
under `eval/atoms/refactor/`.

## Current Useful Building Blocks

Existing Java atoms should remain the base layer:

- `java-decompose-god-class`
- `java-extract-class-cohesive-clusters`
- `java-cluster-inject-params`
- `java-split-god-method`
- `java-eliminate-dead-code`
- `java-replace-vaadin-static-lookup`
- `java-vaadin-provider-binding-generation`
- `java-concurrency-antipattern-audit`

The Vaadin-specific layer should call these where they fit, but it should not
pretend that generic Java cohesion equals Vaadin component cohesion.

## Global Implementation Contract

Each shipped plan kind has:

- a formal input shape in `RefactorPlanParams` or a typed Java-specific
  parameter object translated from `RefactorPlanParams`
- one or more atom manifests under `system-defaults/atoms/refactor/`
- discovery, dispatch, and behavior-smoke eval catalog coverage
- coverage in the Java plan-kind atom guard when added to the dispatch table

V1 Vaadin plan kinds should default to `SemanticStatus::SyntaxOnly`. A future
JDTLS-backed variant may claim LSP verification only when it fails closed on
JDTLS unavailability; it must not silently downgrade semantic claims.

Every mutating Vaadin plan kind must inspect source/target lifecycle and scope
annotations before editing. Relevant annotations include `@PreserveOnRefresh`,
`@RouteScoped`, UI-scoped annotations, Vaadin-session-scoped annotations, and
framework-specific access annotations such as `@PermitAll`, `@RolesAllowed`,
and `@AnonymousAllowed`. When the target class cannot preserve those semantics,
the plan must refuse or require an explicit operator-supplied scope/access
policy.

## Implemented Plan Kinds

### `java_vaadin_view_structure_analysis`

Read-only analysis for one Vaadin view/component class.

Inputs:

- `project_dir`
- `source`
- optional `module_name`

Detect and report:

- route metadata: `@Route`, `@PageTitle`, layout class, route constants
- route access annotations such as `@PermitAll`, `@RolesAllowed`, and
  `@AnonymousAllowed`
- preservation/scope annotations such as `@PreserveOnRefresh`, route scope,
  UI scope, and session scope
- superclass and implemented lifecycle interfaces
- injected constructor parameters grouped by responsibility
- fields by category: Vaadin component, grid/table, data provider, dialog
  provider, backend/admin service, event bus, state
- UI composition methods and component ownership
- event listener registrations and whether detach cleanup is visible
- `UI.getCurrent()` / `VaadinSession.getCurrent()` use sites
- navigation use sites and target routes
- async/UI access patterns: `CompletableFuture`, executor, `ui.access`,
  `push`, captured `UI`, captured session
- candidate extraction groups with reasons and risk flags

Output should be analysis JSON, not edits. It should include
`candidate_components`, `candidate_services`, `candidate_dialogs`,
`candidate_grid_factories`, `route_surfaces`, `lifecycle_findings`, and
`ui_access_findings`.

Acceptance:

- Never mutate source.
- Refuse non-Java files and non-Vaadin classes unless `allow_plain_component`
  is true.
- Prefer tree-sitter syntax plus textual Vaadin conventions in v1; do not
  claim semantic certainty without JDTLS.
- Include line/byte ranges for every candidate so follow-up plan kinds can
  round-trip the exact members.

### `java_vaadin_extract_component`

Move a coherent component/widget section out of a view into a dedicated Vaadin
component class.

Inputs:

- `project_dir`
- `source`
- `target`
- `module_name` for the new component class
- `item_names` / `move_fields` or a `candidate_id` from
  `java_vaadin_view_structure_analysis`
- `component_base`: `Composite<Div>`, `Composite<VerticalLayout>`,
  `VerticalLayout`, `HorizontalLayout`, or explicit FQCN
- optional `parameters_type_name`
- optional `public_methods` such as `load`, `refresh`, `setContext`, or
  `applyFilter`
- optional `target_scope` and `target_access_policy`

Behavior:

- Generate the component class with constructor-injected dependencies.
- Move component-owned fields and UI builder/load methods.
- Replace source view members with one injected/constructed component field.
- Preserve Vaadin imports and organize both source and target.
- Expose only explicit public methods required by the source view.

Acceptance:

- Refuse if moved methods mutate unrelated source-view state unless the state is
  passed through an explicit parameter object or callback.
- Refuse if listener cleanup cannot be preserved.
- Refuse when source scope/preservation annotations cannot be propagated or
  replaced by an explicit `target_scope`.
- Refuse if the extraction would require moving `@Route` or route lifecycle
  methods; use a view synthesis/migration workflow instead.
- Refuse if extracted constructor/provider dependencies have route/UI/session
  scope and the target class would be unscoped.
- Treat generated component class as the owner of its Vaadin child components.

### `java_vaadin_extract_grid_component`

Specialized extraction for `Grid<T>` / `TreeGrid<T>` setup.

Inputs:

- `project_dir`
- `source`
- `target`
- `module_name`
- `grid_field` or `factory_method`
- optional `row_type`
- optional `data_provider_fields`

Behavior:

- Move grid field/factory, column setup, renderers, item-click listeners,
  selection listeners, and footer/header helpers into a component or factory.
- Generate a small public API: `setItems`, `setDataProvider`, `refresh`,
  selected-item accessors, and explicit event callback hooks.
- Preserve generic row type and renderer imports.

Acceptance:

- Refuse if listeners call source-view methods unless callbacks are supplied.
- Refuse if the grid field is shared across multiple visual sections.
- Refuse if the grid or its `DataProvider` captures route/UI/session-scoped
  services that would not survive the target component/factory scope.
- Report the grid's data refresh ownership and any service/provider binding that
  remains in the source view.
- Report any data provider/state that remains in the source view.

### `java_vaadin_extract_dialog_class`

Extract inline dialog creation/opening into a dedicated `Dialog` class and
replace source use with `Provider<DialogType>` or explicit construction.

Inputs:

- `project_dir`
- `source`
- `target`
- `module_name`
- dialog creation method or inline range
- optional `provider_field`

Behavior:

- Generate a dialog class extending `Dialog` or the local dialog base class.
- Move dialog-owned fields, binder setup, event listeners, and save/cancel
  handlers.
- Add provider injection to the caller when requested.
- Replace inline creation with `provider.get().open(...)` or a typed factory
  call.

Acceptance:

- Refuse if dialog save/cancel logic mutates caller state without explicit
  callbacks.
- Preserve event bus publication and listener registration.
- Refuse when dialog-owned dependencies have route/UI/session scope and the
  generated dialog class would not carry an equivalent scope policy.
- Avoid singleton dialog generation for UI/session-scoped state.

### `java_vaadin_static_ui_context_audit`

Read-only audit for static UI/session access and unsafe background interaction.

Inputs:

- `project_dir`
- `source` or `sources`

Detect:

- `UI.getCurrent()` / `VaadinSession.getCurrent()` in services/admins/helpers
- `UI.getCurrent()` inside async tasks
- captured `UI` used after detach without guard
- component mutation outside `ui.access`
- `VaadinSession.getCurrent()` in executor work
- missing `addDetachListener` cleanup for long-lived registrations/tasks

Acceptance:

- Analysis-only.
- Report confidence and specific remediation: inject `Provider<UI>`, pass
  current UI explicitly, use `ui.access`, add detach cleanup, or keep as view
  local.
- Do not auto-rewrite concurrency or session semantics.

### `java_vaadin_synthesize_view`

Create a new server-side Vaadin view from project conventions.

Inputs:

- `project_dir`
- `target`
- `module_name`
- `route_path`
- `page_title`
- `layout_class`
- `base_class` such as `AbstractView` or another project-convention base class
- optional constructor dependencies
- optional explicit `route_access_policy`
- optional content skeleton: empty layout, grid CRUD, tabbed view, dashboard
  widget host, or form view

Behavior:

- Generate a compiling view class with `@Route`, `@PageTitle`, route/page
  constants when local convention uses them, constructor injection, and a
  minimal layout.
- Add imports and package declaration from target path.
- Optionally add a smoke IT skeleton if the project has Vaadin TestBench or
  route smoke tests.

Acceptance:

- Refuse if route path already exists.
- Refuse if target class already exists unless `merge_existing` is explicit.
- Refuse if the project uses annotation-based route security and
  `route_access_policy` is absent.
- Do not emit `@PermitAll`, `@RolesAllowed`, `@AnonymousAllowed`, or equivalent
  access annotations unless the operator supplied `route_access_policy`.
- Do not register authorization/navigation silently; pair with
  `java_vaadin_register_route_access`.

### `java_vaadin_register_route_access`

Update project-specific route access/navigation surfaces for a new or moved
view.

Inputs:

- `project_dir`
- `view_source`
- `view_class`
- `route_path`
- optional `role_policy`
- optional `nav_group`

Behavior:

- Locate authorization registries such as route-access maps, role lists, or
  annotation-based security conventions.
- Add the new view to the appropriate role/list entries.
- Locate navigation menu surfaces when project conventions are detectable.
- Produce a plan that modifies only the route/access files.

Acceptance:

- Refuse if more than one authorization system is detected.
- Refuse if role mapping is ambiguous and no `role_policy` was supplied.
- Refuse if annotation-based security and registry-based security both appear
  active unless the operator chooses the authority of record.
- Keep route synthesis and route access registration as separate plans so
  operators can review authorization explicitly.

### `java_vaadin_navigation_helper_extract`

Extract repeated `UI.getCurrent().navigate(...)` and query-parameter building
into a typed navigation helper.

Inputs:

- `project_dir`
- `source`
- target helper path/class
- selected navigation methods or use-site ranges

Behavior:

- Generate typed helper methods such as `toTargetView(...)`,
  `toFeatureIndex(...)`, or names supplied by the operator.
- Replace source call sites with injected helper calls.
- Optionally use `Provider<UI>` or pass `UI` explicitly.

Acceptance:

- Refuse if the current view's route parameter state is mutated as part of
  navigation unless those effects are modeled in the helper API.
- Refuse if the source view implements navigation lifecycle interfaces such as
  `BeforeEnterObserver`, `BeforeLeaveObserver`, `AfterNavigationObserver`, or
  reroute/forward callbacks and the helper API does not model those effects.
- Report all remaining direct `UI.getCurrent().navigate` calls in the source.

## Implemented Atoms

Every new plan kind below should ship with a thin wrapper atom of the same
surface name using kebab case, for example
`java_vaadin_view_structure_analysis` -> `java-vaadin-view-structure-analysis`.
The workflow atoms in this section compose those wrappers and existing Java
atoms.

### `java-vaadin-decompose-view`

Supervised workflow for large Vaadin views.

Sequence:

1. `java_vaadin_view_structure_analysis`
2. `java-cluster-inject-params`
3. operator selects one candidate
4. `java_vaadin_extract_component`, `java_vaadin_extract_grid_component`, or
   `java_vaadin_extract_dialog_class`
5. organize imports and surface an operator-run focused compile command
6. `java-eliminate-dead-code`

For large dashboard-style views, this should produce a phase plan rather than
one batch apply. One widget/section per round is the default.

### `java-vaadin-dashboard-widget-extract`

Specialized atom for dashboard-style views.

Use when a view owns many repeated widgets/cards/grids. It should:

- classify widget fields and load methods
- generate a widget component or widget host class
- move widget-specific dependencies to constructor injection
- expose a small parameter object such as `WidgetContext`
- preserve refresh/reload hooks

Acceptance:

- Extract only one widget candidate per dispatch unless the operator supplies an
  explicit ordered candidate list.
- Refuse if a candidate owns route lifecycle methods, route parameters, or
  shared view-level state that cannot be passed through `WidgetContext`.
- Refuse if async load logic captures stale `UI` / `VaadinSession` or mutates
  components outside `ui.access`.
- A widget component owns visible component composition; a widget host owns
  orchestration among multiple child widgets. The atom must choose one target
  shape and report why.

### `java-vaadin-create-view-with-access`

Two-phase synthesis workflow:

1. `java_vaadin_synthesize_view`
2. operator review
3. `java_vaadin_register_route_access`
4. optional route smoke test generation

This atom must not silently authorize a route. Authorization is a separate
operator-reviewed step.

### `java-vaadin-dialog-extract-workflow`

Workflow over `java_vaadin_extract_dialog_class` plus provider injection and
call-site replacement.

Useful for large views that construct multiple dialogs inline or keep dialog
fields around only for open/close coordination.

### `java-vaadin-grid-component-extract-workflow`

Workflow over `java_vaadin_extract_grid_component`, followed by import cleanup
and caller API minimization.

Useful for views like sales/reporting pages where grid configuration dominates
class size.

### `java-vaadin-ui-lifecycle-audit`

Read-only atom combining:

- `java_vaadin_static_ui_context_audit`
- existing `java-concurrency-antipattern-audit`
- optional `find_java_usages` for event bus register/unregister pairs

Output should be a risk-ranked list, not an apply plan.

### `java-vaadin-route-inventory`

Read-only project atom that inventories:

- all `@Route` classes
- route paths and layout classes
- page titles
- authorization coverage
- navigation menu coverage
- orphaned views not referenced by role/nav surfaces
- duplicate or conflicting route constants

This should be the first atom before creating or moving multiple views.

### Atom Composition Contracts

Each shipped atom should declare `composition.may_invoke_atoms` explicitly:

- `java-vaadin-decompose-view`: may invoke
  `atom:java-vaadin-view-structure-analysis@v1`,
  `atom:java-cluster-inject-params@v1`,
  `atom:java-vaadin-extract-component@v1`,
  `atom:java-vaadin-extract-grid-component@v1`,
  `atom:java-vaadin-extract-dialog-class@v1`, and
  `atom:java-eliminate-dead-code@v1`.
- `java-vaadin-dashboard-widget-extract`: may invoke the Vaadin view-structure
  analysis atom and `atom:java-vaadin-extract-component@v1`.
- `java-vaadin-create-view-with-access`: may invoke only the synthesize-view
  and register-route-access atoms:
  `atom:java-vaadin-synthesize-view@v1` and
  `atom:java-vaadin-register-route-access@v1`.
- `java-vaadin-dialog-extract-workflow`: may invoke
  `atom:java-vaadin-extract-dialog-class@v1` and import/dead-code cleanup
  atoms.
- `java-vaadin-grid-component-extract-workflow`: may invoke
  `atom:java-vaadin-extract-grid-component@v1` and import/dead-code cleanup
  atoms.
- `java-vaadin-ui-lifecycle-audit`: may invoke
  `atom:java-vaadin-static-ui-context-audit@v1`,
  `atom:java-concurrency-antipattern-audit@v1`, and read-only usage-search
  atoms. It must invoke no mutating atoms.
- `java-vaadin-route-inventory` should be read-only and invoke no mutating
  atoms.

## Large View Decomposition Strategy

For a large Vaadin route view, the pragmatic sequence should be:

1. Run `java-vaadin-route-inventory` for project conventions.
2. Run `java_vaadin_view_structure_analysis` on the target view.
3. Run constructor clustering to identify service bundles.
4. Extract low-risk leaf components first:
   - individual grids
   - pure widget components
   - dialog launchers
   - navigation helper methods
5. Extract larger dashboard sections only after leaf dependencies shrink.
6. Keep the original view as route shell / coordinator until the end.
7. Run dead-code/import cleanup after each accepted extraction.

Avoid extracting `@Route`, URL parameter handling, or navigation lifecycle first.
Those are the highest-risk seams because they anchor Vaadin routing and page
state.

## Implementation Notes

- Keep Vaadin plan kinds conservative and syntax-first in v1.
- Prefer analysis plan kinds that produce stable `candidate_id` values; atoms
  can re-run analysis before applying a stale candidate.
- Keep route creation, access registration, and navigation-menu changes as
  separate plans.
- Prefer generated parameter/context records over widening method signatures
  with many arguments.
- When a tool cannot preserve listener cleanup or UI access semantics, it should
  refuse rather than emit TODO-laden source.

## Non-Goals

- Do not attempt pixel-perfect UI redesign.
- Do not synthesize business logic.
- Do not auto-authorize routes.
- Do not move Vaadin state into singleton services.
- Do not replace server-side Flow with React/Hilla as part of these atoms.
