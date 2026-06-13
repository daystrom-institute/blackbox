use super::*;

/// A moved field that the source constructor initializes and must therefore be
/// threaded through the target's constructor (gap-9462575f). Its declaration
/// moves verbatim onto the target (via `moved_field_text`), so the target needs
/// a constructor parameter + assignment to initialize it, the source-side
/// `new Target(...)` must pass the original initializer expression, and the now
/// orphaned `this.<name> = <expr>;` in the source ctor must be deleted.
struct MovedThreadedField {
    name: String,
    type_name: String,
    /// The RHS of the source ctor's `this.<name> = <expr>;`, verbatim. Using the
    /// expression (not the field name) is what makes a field/parameter name
    /// mismatch — e.g. `this.fooAdmin = foosAdmin;` — thread correctly.
    init_expr: String,
    /// Byte range of the whole assignment statement in the source, for deletion.
    assign_range: (usize, usize),
}

/// Scan the source's first constructor for `this.<field> = <param>;` (or bare
/// `<field> = <param>;`) assignments to a moved field, where `<param>` is a bare
/// reference to one of the constructor's own parameters. Returns name ->
/// (param_expr, statement_range).
///
/// The constructor-parameter restriction is deliberate and load-bearing: it
/// pins the feature to the dependency-injection pattern (a moved `final` field
/// initialized from an injected ctor param). Those params SURVIVE the
/// extraction, so threading the param into `new Target(...)` is safe and
/// side-effect-free. An initializer that is a method call, a computed
/// expression, or another field (`this.grid = buildGrid()`) is NOT threaded —
/// it may reference members being moved away, have side effects, or be ordering-
/// sensitive; the existing behavior (declaration moves, operator handles wiring)
/// is correct there. A field assigned more than once (conditional/complex init)
/// is also omitted, since auto-threading one branch would be wrong.
fn moved_field_ctor_assignments(
    constructor: Node<'_>,
    source: &str,
    moved: &HashSet<&str>,
) -> std::collections::HashMap<String, (String, (usize, usize))> {
    let mut out: std::collections::HashMap<String, (String, (usize, usize))> =
        std::collections::HashMap::new();
    let param_names = constructor_parameter_names(constructor, source);
    let mut seen_twice: HashSet<String> = HashSet::new();
    let Some(body) = constructor.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    for stmt in body.named_children(&mut cursor) {
        if stmt.kind() != "expression_statement" {
            continue;
        }
        let Some(expr) = stmt.named_child(0) else {
            continue;
        };
        if expr.kind() != "assignment_expression" {
            continue;
        }
        let Some(lhs) = expr.child_by_field_name("left") else {
            continue;
        };
        let field_name = match lhs.kind() {
            "field_access" => match (
                lhs.child_by_field_name("object"),
                lhs.child_by_field_name("field"),
            ) {
                (Some(obj), Some(fld)) if obj.kind() == "this" => {
                    fld.utf8_text(source.as_bytes()).ok().map(str::to_string)
                }
                _ => None,
            },
            "identifier" => lhs.utf8_text(source.as_bytes()).ok().map(str::to_string),
            _ => None,
        };
        let Some(field_name) = field_name else {
            continue;
        };
        if !moved.contains(field_name.as_str()) {
            continue;
        }
        let Some(rhs) = expr.child_by_field_name("right") else {
            continue;
        };
        // Only a bare reference to a surviving constructor parameter qualifies.
        if rhs.kind() != "identifier" {
            continue;
        }
        let Ok(param) = rhs.utf8_text(source.as_bytes()) else {
            continue;
        };
        if !param_names.contains(param) {
            continue;
        }
        if out.contains_key(&field_name) {
            seen_twice.insert(field_name);
            continue;
        }
        out.insert(
            field_name,
            (param.to_string(), (stmt.start_byte(), stmt.end_byte())),
        );
    }
    for name in seen_twice {
        out.remove(&name);
    }
    out
}

pub(crate) fn plan_extract_java_class(p: &RefactorPlanParams) -> Result<String> {
    let source_path = resolve_path(p.project_dir.as_deref(), &p.source)?;
    let target_path = p
        .target
        .as_deref()
        .ok_or_else(|| anyhow!("target is required for extract_java_class"))
        .and_then(|target| resolve_path(p.project_dir.as_deref(), target))?;
    if source_path == target_path {
        bail!("source and target must be different files");
    }

    let parsed = parse_source_file(&source_path)?;
    if parsed.language != "java" {
        bail!("extract_java_class only supports java files");
    }
    let class_node = find_first_class_declaration(parsed.tree.root_node())
        .ok_or_else(|| anyhow!("no class declaration found in {}", source_path.display()))?;
    let target_class_name = java_target_type_name(p, &target_path)?;
    let delegate_field = p
        .delegate_field
        .as_deref()
        .ok_or_else(|| anyhow!("delegate_field is required for extract_java_class"))?;
    validate_java_member_name(delegate_field, "delegate_field")?;

    let method_names = p.item_names.as_deref().unwrap_or_default();
    let mut selected_methods = select_java_methods_by_name(&parsed, method_names)?;
    let moved_field_names = p.move_fields.as_deref().unwrap_or_default();
    let selected_fields = select_java_fields_by_name(&parsed, moved_field_names)?;
    let moved_field_set = selected_fields
        .iter()
        .map(|field| field.name.as_str())
        .collect::<HashSet<_>>();
    let captured_variables = captured_fields_for_methods(&parsed, &selected_methods);

    // Wiring strategy resolution. Framework-neutral: the engine emits the
    // delegate-field / target-constructor shape the caller selected and adds
    // the caller-supplied injection annotations as data. It does NOT inspect
    // the source for DI markers or second-guess the choice — that policy
    // (e.g. refusing `own_construction` on a DI-managed class) lives in the
    // macro layer (`builtin.java.guice`), not in the engine.
    let wiring = p.wiring_mode.clone().unwrap_or_default();
    let wiring_strategy = match wiring.strategy.as_deref() {
        Some(m) => {
            if !matches!(m, "own_construction" | "external_injection" | "none") {
                bail!(
                    "error.bad_input(code=invalid_wiring_strategy): wiring_mode.strategy must be \
                     one of `own_construction`, `external_injection`, `none` (got `{m}`)"
                );
            }
            m.to_string()
        }
        None => "own_construction".to_string(),
    };
    let external_injection = wiring_strategy == "external_injection";
    let delegate_field_annotations = wiring.delegate_field_annotations.unwrap_or_default();
    let delegate_field_modifiers = wiring.delegate_field_modifiers.unwrap_or_default();
    let delegate_field_annotation_imports =
        wiring.delegate_field_annotation_imports.unwrap_or_default();
    let target_constructor_annotations = wiring.target_constructor_annotations.unwrap_or_default();
    let target_constructor_annotation_imports = wiring
        .target_constructor_annotation_imports
        .unwrap_or_default();

    // Mutable-capture-with-write refusal. A capture that is (a) non-final on
    // the source, (b) not listed in `move_fields`, and (c) WRITTEN inside any
    // extracted method body cannot be promoted to a `final` constructor
    // parameter — the moved code would fail `cannot assign to final variable`.
    // Refuse the plan with an operator-actionable error pointing at the
    // exact fields to add to `move_fields` (which then routes them through
    // the rewrite_remaining_accessors / generated-setter path).
    let written_mutable_captures: Vec<String> = captured_variables
        .iter()
        .filter(|c| c.source_mutable && !c.source_static_final)
        .filter(|c| !moved_field_set.contains(c.name.as_str()))
        .filter_map(|c| {
            if extracted_methods_write_to(&parsed, &selected_methods, &c.name) {
                Some(c.name.clone())
            } else {
                None
            }
        })
        .collect();
    if !written_mutable_captures.is_empty() {
        bail!(
            "error.bad_input(code=mutable_capture_with_write): extracted method bodies write to \
             mutable source field(s) {fields:?}. Promoting them to a final constructor parameter \
             on the target would fail with `cannot assign to final variable`. Add them to \
             `move_fields` to route them through the delegate-with-generated-setter path instead.",
            fields = written_mutable_captures
        );
    }

    let class_dependency_report = if p.deep_analysis.unwrap_or(false) {
        analyze_extracted_dependencies(
            &parsed,
            &selected_methods,
            p.project_dir.as_deref().map(Path::new),
        )
    } else {
        Default::default()
    };

    // Gap 20: split captures into static-final constants vs instance
    // captures. Constants are moved declaration-and-initializer onto the
    // target (mirroring move_java_constant semantics), instance captures
    // become constructor parameters.
    let static_final_capture_names: HashSet<&str> = captured_variables
        .iter()
        .filter(|capture| capture.source_static_final)
        .filter(|capture| !moved_field_set.contains(capture.name.as_str()))
        .map(|capture| capture.name.as_str())
        .collect();
    let dependency_params = captured_variables
        .iter()
        .filter(|capture| !moved_field_set.contains(capture.name.as_str()))
        .filter(|capture| !static_final_capture_names.contains(capture.name.as_str()))
        .map(|capture| JavaParameterSpec {
            type_name: capture.source_type.clone(),
            name: capture.name.clone(),
        })
        .collect::<Vec<_>>();

    // `callback_externals`: source-class methods the operator wants threaded
    // through the target as a functional-interface callback instead of
    // surfacing as a `// FIXME: external call` marker. Classify each into
    // (Runnable / Supplier<R> / Consumer<T> / Function<T,R>) by inspecting
    // the source method's signature. 2+ arg methods are refused with a
    // dedicated bad_input code.
    let callback_specs: Vec<CallbackSpec> = {
        let names = p.callback_externals.as_deref().unwrap_or_default();
        if names.is_empty() {
            Vec::new()
        } else {
            let mut specs = Vec::new();
            for name in names {
                // The method must exist on the source class.
                let method_node = find_node(parsed.tree.root_node(), |node| {
                    node.kind() == "method_declaration"
                        && node
                            .child_by_field_name("name")
                            .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
                            == Some(name.as_str())
                })
                .ok_or_else(|| {
                    anyhow!(
                        "error.bad_input(code=callback_method_not_found): \
                         `callback_externals` entry `{name}` is not a method declaration on the \
                         source class"
                    )
                })?;
                specs.push(classify_callback_method(&parsed, method_node)?);
            }
            specs
        }
    };
    // Append callback ctor params after instance captures so the wiring's
    // positional arg list is (captured1, captured2, ..., callback1, ...).
    let mut all_target_ctor_params = dependency_params.clone();
    for spec in &callback_specs {
        all_target_ctor_params.push(JavaParameterSpec {
            type_name: spec.interface_type.clone(),
            name: spec.field_name.clone(),
        });
    }

    // gap-9462575f: moved fields the source constructor initializes (typically
    // `final` DI-injected dependencies) must be threaded through the target's
    // constructor. Their declarations move verbatim with no initializer, so
    // without a ctor param the target has an uninitialized final field, and the
    // source-side `this.field = <expr>;` becomes an orphan assignment to a field
    // that no longer exists there. Capture the initializer expression verbatim
    // (so a field/param name mismatch threads correctly) + the statement range
    // to delete. The target ctor params + assignments are added for every wiring
    // strategy; threading the exprs into `new Target(...)` is own_construction-
    // only and happens at the wiring site below.
    let moved_ctor_assignments = first_constructor_node(class_node, &parsed.source)
        .map(|ctor| moved_field_ctor_assignments(ctor, &parsed.source, &moved_field_set))
        .unwrap_or_default();
    // Stable order matching the moved field declaration order.
    let moved_threaded: Vec<MovedThreadedField> = selected_fields
        .iter()
        .filter_map(|f| {
            moved_ctor_assignments
                .get(&f.name)
                .map(|(expr, range)| MovedThreadedField {
                    name: f.name.clone(),
                    type_name: f.type_name.clone(),
                    init_expr: expr.clone(),
                    assign_range: *range,
                })
        })
        .collect();
    // Target ctor takes captured deps, then callbacks, then the threaded moved
    // fields. The field DECLARATIONS for the moved ones come from
    // `moved_field_text`, so they are NOT added to `dependency_field_text` —
    // only to the constructor param list below.
    let mut ctor_params = all_target_ctor_params.clone();
    ctor_params.extend(moved_threaded.iter().map(|m| JavaParameterSpec {
        type_name: m.type_name.clone(),
        name: m.name.clone(),
    }));

    // Locate the source-side `field_declaration` node for each static-final
    // capture so we can (a) render the original declaration verbatim onto
    // the target and (b) emit a removal edit on the source.
    let mut moved_constant_fields: Vec<JavaField> = Vec::new();
    for name in static_final_capture_names.iter() {
        let field = java_fields(&parsed)
            .into_iter()
            .find(|field| field.name == *name);
        if let Some(field) = field {
            moved_constant_fields.push(field);
        }
    }
    moved_constant_fields.sort_by_key(|field| field.item.byte_start);

    // Gap 24: decide the visibility floor for delegate-rewritten methods.
    // Default floor is `package` (`update_java_callers` rewrites local calls
    // through the delegate, which only requires package-visible access);
    // if the target ends up in a different package than the source, the
    // floor escalates to `public`.
    let source_package = extract_java_package(&parsed.source);
    // Source class name is needed early for G19 auto-qualify of
    // public-static external calls (which fires before the existing
    // inner-type-qualify pass).
    let source_class_name = java_class_name(class_node, &parsed.source);
    // Gap 2: derive the target package via the unified resolver. Precedence is
    // target_prelude > existing target file's package > source-root-derived from
    // target_path > source's package (same-directory targets only) > hard error.
    // This avoids silent path↔package contradictions on cross-package extracts.
    let target_package =
        resolve_java_target_package(p, &parsed.source, &source_path, &target_path)?;
    let cross_package = match (source_package.as_deref(), target_package.as_deref()) {
        (Some(src), Some(tgt)) => src != tgt,
        (None, Some(_)) | (Some(_), None) => true,
        (None, None) => false,
    };
    let mut visibility_floor = if cross_package { "public" } else { "package" };
    if let Some(requested) = p.visibility.as_deref() {
        validate_java_visibility(requested)?;
        if java_visibility_rank(requested) > java_visibility_rank(visibility_floor) {
            visibility_floor = match requested {
                "public" => "public",
                "protected" => "protected",
                "package" => "package",
                "private" => "private",
                _ => visibility_floor,
            };
        }
    }

    selected_methods.sort_by_key(|method| method.item.byte_start);
    let method_text = selected_methods
        .iter()
        .map(|method| extract_method_text_with_visibility_floor(&parsed, method, visibility_floor))
        .collect::<Vec<_>>()
        .join("\n\n");
    let moved_field_text = selected_fields
        .iter()
        .map(|field| {
            parsed.source[field.item.leading_trivia_start..field.item.byte_end]
                .trim_matches('\n')
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let moved_constants_text = moved_constant_fields
        .iter()
        .map(|field| extract_field_text_with_visibility_floor(&parsed, field, visibility_floor))
        .collect::<Vec<_>>()
        .join("\n");
    let dependency_field_text = all_target_ctor_params
        .iter()
        .map(|param| format!("    private final {} {};", param.type_name, param.name))
        .collect::<Vec<_>>()
        .join("\n");
    let constructor_text = if ctor_params.is_empty() {
        String::new()
    } else {
        let raw = java_constructor_decl(&target_class_name, "public", &ctor_params, true, None)?;
        // External-injection wiring may decorate the generated target
        // constructor with caller-supplied annotations (e.g. `@Inject`) so an
        // external owner constructs it. The annotation lines go immediately
        // preceding `public <TargetClass>(...)`; matching imports are added to
        // the target file below. Framework-neutral — the annotation text is
        // macro data, not a hard-coded DI-library name.
        if external_injection && !target_constructor_annotations.is_empty() {
            let needle = format!("public {target_class_name}(");
            if let Some(pos) = raw.find(&needle) {
                let line_start = raw[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
                // Match the constructor's leading indent.
                let indent: String = raw[line_start..pos].chars().collect();
                let mut out =
                    String::with_capacity(raw.len() + 16 * target_constructor_annotations.len());
                out.push_str(&raw[..line_start]);
                for ann in &target_constructor_annotations {
                    out.push_str(&indent);
                    out.push_str(ann);
                    out.push('\n');
                }
                out.push_str(&raw[line_start..]);
                out
            } else {
                raw
            }
        } else {
            raw
        }
    };

    // Gap 18 + Gap 6: generate getter/setter methods on the target for each
    // moved field, plus rewrite remaining source-side reads/writes through
    // the delegate.
    //
    // Gap 6 decouples this from `deep_analysis`. Previously the accessor
    // rewrite required `deep_analysis=true` AND `rewrite_remaining_accessors`
    // unset/true; with `deep_analysis=false` a `move_fields` extract would
    // silently miscompile (the field declarations move but every read/write
    // in the source class stays as a bare reference). The accessor rewriter
    // doesn't need the full call-graph walk — it only needs to know "this
    // field was moved." So we now default `rewrite_remaining=true` whenever
    // `move_fields` is non-empty, regardless of `deep_analysis`.
    //
    // Operator opt-out: `rewrite_remaining_accessors=false` (preserves the
    // legacy report-only behavior under `deep_analysis=true`, and disables
    // the new always-on rewrite under `deep_analysis=false`).
    //
    // Generated accessors honour the same visibility floor used for the
    // moved methods (`package` same-package, `public` cross-package).
    let rewrite_remaining =
        !selected_fields.is_empty() && p.rewrite_remaining_accessors.unwrap_or(true);
    let accessor_specs: Vec<DelegateAccessorSpec> = if rewrite_remaining {
        selected_fields
            .iter()
            .map(DelegateAccessorSpec::from_field)
            .collect()
    } else {
        Vec::new()
    };
    let accessor_visibility = if cross_package { "public" } else { "package" };
    let accessor_text = if accessor_specs.is_empty() {
        String::new()
    } else {
        accessor_specs
            .iter()
            .map(|spec| render_delegate_accessors(spec, accessor_visibility))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let target_body = [
        moved_constants_text,
        dependency_field_text,
        moved_field_text,
        constructor_text,
        method_text,
        accessor_text,
    ]
    .into_iter()
    .filter(|part| !part.trim().is_empty())
    .collect::<Vec<_>>()
    .join("\n\n");
    let prelude = java_default_target_prelude(p, &parsed.source, target_package.as_deref());
    let original_target_bytes = if target_path.exists() {
        fs::read(&target_path)?
    } else {
        Vec::new()
    };
    if !original_target_bytes.is_empty() {
        bail!("extract_java_class currently requires a missing or empty target file");
    }
    let mut raw_target_content = java_class_wrapper(&target_class_name, &prelude, &target_body);

    // `callback_externals`: rewrite each callback invocation inside the
    // extracted method bodies BEFORE organize_imports + FIXME insertion run.
    // After rewrite, calls like `refreshGrid()` become `refreshGrid.run()`,
    // so the import-walker sees them as `Runnable`-typed field accesses and
    // the external-call FIXME finder no longer matches them by method name.
    if !callback_specs.is_empty() {
        let callback_edits = compute_callback_call_rewrites(&raw_target_content, &callback_specs)?;
        raw_target_content = apply_text_edits(&raw_target_content, &callback_edits);
    }

    // Gap 25: prune unused imports on the generated target. The prelude
    // copies every import from the source; the heuristic walks the in-memory
    // target AST and drops imports whose simple name is never referenced as
    // a `type_identifier`. Best-effort — on any failure we keep the over-
    // imported content rather than fail the whole extraction.
    let project_dir_for_imports = p
        .project_dir
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| target_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let mut target_content =
        match heuristic_java_organize_imports_text(&project_dir_for_imports, &raw_target_content) {
            Ok(pruned) => pruned,
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "extract_java_class: heuristic_java_organize_imports_text failed; \
                     keeping unpruned target imports"
                );
                raw_target_content
            }
        };
    // Inject any functional-interface imports needed by the callbacks
    // (Runnable lives in java.lang; Consumer/Supplier/Function live under
    // java.util.function and need an explicit import even though their
    // simple name is referenced by the ctor parameter type).
    for spec in &callback_specs {
        if let Some(fqcn) = spec.extra_import.as_deref() {
            target_content = java_inject_import(&target_content, fqcn);
        }
    }

    // The target-constructor annotations (added above, only when a
    // parameterized ctor is generated) need their imports on the target file.
    // Plain (non-conservative) add — matches the legacy target-side behavior,
    // which did not apply the source-side wildcard-skip on the target.
    if external_injection
        && !target_constructor_annotations.is_empty()
        && !all_target_ctor_params.is_empty()
    {
        for fqcn in &target_constructor_annotation_imports {
            target_content = java_inject_import(&target_content, fqcn);
        }
    }

    // G13: propagate class-level annotations from source to target when
    // the moved body references identifiers that the annotation processor
    // generated. Without this, every @Slf4j source class produces an
    // uncompilable target because the moved log.info(...) call references
    // a generated `log` field that doesn't exist on the target.
    //
    // mode = auto (default): scan moved bodies for known annotation-
    //   processor-generated identifiers; propagate the responsible
    //   annotation and its import when the source carries it.
    // mode = all: copy every class-level annotation.
    // mode = none: current behavior, no propagation.
    // mode = list:@A,@B: operator-supplied allowlist.
    let propagate_mode = p
        .propagate_class_annotations
        .as_deref()
        .unwrap_or("auto")
        .to_string();
    if propagate_mode != "none" {
        let source_annotations = crate::java::class_dependency::collect_class_level_annotations(
            class_node,
            &parsed.source,
        );
        if !source_annotations.is_empty() {
            // Map of well-known annotation -> (generated identifier(s), import FQCN).
            // v1 covers @Slf4j; other Lombok generators (@Getter, @Setter, @Data)
            // generate per-field accessors whose names depend on the source's
            // own fields and aren't worth pattern-matching here. Operators
            // can request them explicitly via mode=all or list:@Data.
            let auto_map: &[(&str, &[&str], &str)] =
                &[("@Slf4j", &["log"], "lombok.extern.slf4j.Slf4j")];
            let list_allowlist: Option<Vec<String>> = propagate_mode
                .strip_prefix("list:")
                .map(|s| s.split(',').map(|a| a.trim().to_string()).collect());
            let mut propagations: Vec<(String, Option<String>)> = Vec::new();
            for raw in &source_annotations {
                let simple = raw.trim();
                // Strip args from `@Foo(...)`: keep `@Foo`.
                let head = simple
                    .find('(')
                    .map(|i| &simple[..i])
                    .unwrap_or(simple)
                    .trim()
                    .to_string();
                let should_propagate = match propagate_mode.as_str() {
                    "all" => true,
                    "auto" => {
                        if let Some((_name, generated_ids, _fqcn)) =
                            auto_map.iter().find(|(n, _, _)| *n == head)
                        {
                            // Check whether any moved method body references
                            // a generated identifier.
                            generated_ids.iter().any(|gid| {
                                target_content_references_identifier(&target_content, gid)
                            })
                        } else {
                            false
                        }
                    }
                    _ => list_allowlist
                        .as_ref()
                        .map(|l| l.iter().any(|a| a == &head))
                        .unwrap_or(false),
                };
                if !should_propagate {
                    continue;
                }
                let fqcn = auto_map
                    .iter()
                    .find(|(n, _, _)| *n == head)
                    .map(|(_, _, fqcn)| fqcn.to_string());
                propagations.push((raw.trim().to_string(), fqcn));
            }
            if !propagations.is_empty() {
                // Inject `@A\n@B\n` right before the `public class <Name>` line.
                let needle = format!("public class {target_class_name}");
                if let Some(decl_pos) = target_content.find(&needle) {
                    let line_start = target_content[..decl_pos]
                        .rfind('\n')
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    let mut annotations_block = String::new();
                    for (annot, _) in &propagations {
                        annotations_block.push_str(annot);
                        annotations_block.push('\n');
                    }
                    let mut patched =
                        String::with_capacity(target_content.len() + annotations_block.len());
                    patched.push_str(&target_content[..line_start]);
                    patched.push_str(&annotations_block);
                    patched.push_str(&target_content[line_start..]);
                    target_content = patched;
                }
                // Inject imports for known annotations.
                for (_, fqcn) in &propagations {
                    if let Some(fqcn) = fqcn.as_deref() {
                        target_content = java_inject_import(&target_content, fqcn);
                    }
                }
            }
        }
    }

    // Gap 22 / Gap 23: scaffold unresolved deps in the generated target text.
    // Only meaningful when deep_analysis was on (the report is empty otherwise).
    if p.deep_analysis.unwrap_or(false) {
        // Collect extracted method names (the methods that just moved to the
        // target). Used to test interface satisfaction for Gap 23.
        let extracted_method_names: HashSet<String> = selected_methods
            .iter()
            .filter_map(|m| m.item.name.clone())
            .collect();

        // Gap 23: pick interfaces to inject into the target's class declaration.
        //
        // Triggers (union):
        //   1. Any interface that appears in `inherited_dependencies` (an
        //      extracted method called a method declared on the interface).
        //   2. Any interface from the source class's `implements` clause
        //      whose declared methods are all present in the extracted set
        //      — the source's interface contract migrated wholesale to the
        //      target. This case can NOT surface in `inherited_dependencies`
        //      because the analyzer drops calls to extracted methods, so the
        //      `implements`-list scan is the only way to spot it.
        let project_dir_path = p.project_dir.as_deref().map(Path::new);
        let mut implements_to_add: Vec<String> = Vec::new();
        let mut interface_imports: Vec<String> = Vec::new();
        let mut unsatisfied_interfaces: Vec<(String, Vec<String>)> = Vec::new();
        let mut interface_sources: BTreeMap<String, ()> = BTreeMap::new();
        for dep in &class_dependency_report.inherited_dependencies {
            if dep.source_kind == "interface" {
                interface_sources.insert(dep.source.clone(), ());
            }
        }
        // Trigger (2): scan source class's `implements` chain for interfaces
        // whose method set is entirely extracted.
        if let Some(project_dir) = project_dir_path {
            for super_name in collect_java_super_type_names(class_node, &parsed.source) {
                // Skip types we already have via inherited_dependencies.
                if interface_sources.contains_key(&super_name) {
                    continue;
                }
                let type_paths = project_java_type_paths(project_dir);
                let Some(Some(path)) = type_paths.get(&super_name) else {
                    continue;
                };
                let Ok(parsed_super) = parse_source_file(path) else {
                    continue;
                };
                let Some(super_node) =
                    find_java_type_declaration_by_name(&parsed_super, &super_name)
                else {
                    continue;
                };
                if java_type_kind_label(super_node) != "interface" {
                    continue;
                }
                let declared = collect_java_type_method_names(&parsed_super, super_node);
                if declared.is_empty() {
                    continue;
                }
                // All declared methods must be in the extracted set.
                let all_satisfied = declared.iter().all(|m| extracted_method_names.contains(m));
                if all_satisfied {
                    interface_sources.insert(super_name, ());
                }
            }
        }
        for interface_name in interface_sources.keys() {
            // Need a project_dir to resolve the interface for both the type
            // index and the method-list lookup.
            let Some(project_dir) = project_dir_path else {
                continue;
            };
            // Gather the interface's ABSTRACT methods only — methods the
            // implementer must explicitly provide. `default`, `static`,
            // and `private` interface methods already have bodies on the
            // interface itself and don't need to be in the extracted set.
            // If the interface is not uniquely resolvable in the project,
            // skip it — we can't safely add `implements` for a type we
            // can't import.
            let Some(declared_methods) =
                collect_interface_abstract_method_names(project_dir, interface_name)
            else {
                continue;
            };
            // Satisfaction: every abstract method on the interface must be
            // in the extracted method set. Else the target is abstract.
            let unsatisfied: Vec<String> = declared_methods
                .iter()
                .filter(|m| !extracted_method_names.contains(m.as_str()))
                .cloned()
                .collect();
            // Look up FQCN to add a matching import when the interface lives
            // in a different package.
            let type_index = build_java_type_index(project_dir).ok();
            let fqcn_opt = type_index
                .as_ref()
                .and_then(|idx| idx.top_level.get(interface_name))
                .and_then(|slot| slot.clone());
            if let Some(fqcn) = fqcn_opt.as_ref() {
                let target_pkg = extract_java_package(&target_content);
                let same_pkg = target_pkg.as_deref().is_some_and(|pkg| {
                    fqcn.strip_suffix(&format!(".{interface_name}")) == Some(pkg)
                });
                if !same_pkg {
                    interface_imports.push(fqcn.clone());
                }
            }
            implements_to_add.push(interface_name.clone());
            if !unsatisfied.is_empty() {
                unsatisfied_interfaces.push((interface_name.clone(), unsatisfied));
            }
        }
        for fqcn in &interface_imports {
            target_content = java_inject_import(&target_content, fqcn);
        }
        if !implements_to_add.is_empty() {
            target_content =
                java_inject_implements(&target_content, &target_class_name, &implements_to_add);
        }
        // Insert FIXME marker above the class declaration for any
        // implements that the target cannot satisfy.
        if !unsatisfied_interfaces.is_empty() {
            let needle = format!("public class {target_class_name}");
            if let Some(decl_at) = target_content.find(&needle) {
                let line_start = target_content[..decl_at]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let indent: String = target_content[line_start..decl_at]
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                let mut comment = String::new();
                for (interface, missing) in &unsatisfied_interfaces {
                    let names = missing.join(", ");
                    comment.push_str(&format!(
                        "{indent}// FIXME: target now implements {interface} but does not satisfy method(s) <{names}>;\n"
                    ));
                    comment.push_str(&format!(
                        "{indent}// either also extract the listed method(s) or remove the implements clause.\n"
                    ));
                }
                let mut patched = String::with_capacity(target_content.len() + comment.len());
                patched.push_str(&target_content[..line_start]);
                patched.push_str(&comment);
                patched.push_str(&target_content[line_start..]);
                target_content = patched;
            }
        }

        // Gap 22: insert FIXME comment lines above each external_call site
        // in the generated target body. `callback_externals` methods are
        // already threaded through the target as functional-interface
        // callbacks (call sites rewritten to `field.run()` / `.accept(...)`
        // etc.); they are NOT external calls that need a FIXME.
        //
        // G19 + G14: public-static externals get auto-qualified to
        // `<SourceClass>.<method>(...)` and skip the FIXME entirely.
        // The import to the source class is already added by the
        // existing post-extract pass; the call site just needs the
        // class qualifier prepended to compile.
        let callback_names: HashSet<&str> = callback_specs
            .iter()
            .map(|c| c.method_name.as_str())
            .collect();
        let mut qualified_static_externals = false;
        for call in &class_dependency_report.external_calls {
            if callback_names.contains(call.method.as_str()) {
                continue;
            }
            let is_public_static =
                call.source_is_static && call.source_visibility.as_deref() == Some("public");
            if is_public_static {
                target_content = java_qualify_unqualified_calls(
                    &target_content,
                    &call.method,
                    &source_class_name,
                );
                qualified_static_externals = true;
                continue;
            }
            let fixme = fixme_external_call(&call.method);
            let (next, _count) =
                java_insert_fixme_above_calls(&target_content, &call.method, &fixme);
            target_content = next;
        }
        // G19: cross-package qualifications of public-static externals
        // need the source-class import on the target. Same import the
        // inner-type-qualifier pass adds when it qualifies an inner type
        // reference — share the path.
        if qualified_static_externals && cross_package {
            if let Some(src_pkg) = source_package.as_deref() {
                let source_fqcn = format!("{src_pkg}.{source_class_name}");
                target_content = java_inject_import(&target_content, &source_fqcn);
            }
        }

        // Gap 23 (class branch): for inherited deps with source_kind=="class",
        // do NOT auto-add `extends`. Insert FIXMEs above each call site.
        for dep in &class_dependency_report.inherited_dependencies {
            if dep.source_kind != "class" {
                continue;
            }
            let fixme = fixme_inherited_class_call(&dep.method, &dep.source);
            let (next, _count) =
                java_insert_fixme_above_calls(&target_content, &dep.method, &fixme);
            target_content = next;
        }

        // Gap 29: warn the operator about mutable captures that were
        // promoted to `final` constructor params on the target. The source
        // field is non-final (its value can change at runtime); the target
        // only sees a snapshot taken at construction time. This is a silent
        // semantic bug — surface it as a FIXME directly above the
        // generated `private final <Type> <name>;` line so it travels with
        // the target file rather than getting buried in JSON.
        //
        // Under external_injection the delegate/target lifecycle is owned by
        // an external party (a DI container, etc.): the target's captured ctor
        // params are freshly resolved at construction time, not stale snapshots
        // of source fields. Suppress the snapshot FIXMEs in that case — they're
        // misleading noise. (Contract: external_injection means container-owned
        // construction; own_construction is the only strategy that truly
        // snapshots source state.)
        let suppress_mutable_capture_fixmes = external_injection;
        if !suppress_mutable_capture_fixmes {
            for capture in &captured_variables {
                if !capture.source_mutable {
                    continue;
                }
                if capture.source_static_final {
                    // Static-finals route through the Gap 20 constants path —
                    // they aren't constructor params and can't be "snapshotted".
                    continue;
                }
                if moved_field_set.contains(capture.name.as_str()) {
                    // Field is being moved, not captured as a param.
                    continue;
                }
                target_content = java_insert_fixme_above_mutable_capture(&target_content, capture);
            }
        }
    }

    // Source-class inner type references (Gap 5). Methods being moved may
    // reference an inner type (enum / class / record / interface) declared
    // on the source class — bare names that resolve from the source's
    // package but won't resolve from the target's. Rewrite each such
    // reference in the target body to `<SourceClass>.<InnerType>`, add
    // a source-class import on the target (cross-package only), and widen
    // the inner type's visibility on the source to at least the same
    // floor the planner applies to moved methods.
    let inner_type_decls: BTreeMap<String, Node<'_>> = {
        let mut map = BTreeMap::new();
        if let Some(class_body) = class_node.child_by_field_name("body") {
            let mut cursor = class_body.walk();
            for child in class_body.named_children(&mut cursor) {
                let kind = child.kind();
                if matches!(
                    kind,
                    "class_declaration"
                        | "interface_declaration"
                        | "enum_declaration"
                        | "record_declaration"
                ) {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                            map.insert(name.to_string(), child);
                        }
                    }
                }
            }
        }
        map
    };
    let mut referenced_inner_types: BTreeSet<String> = BTreeSet::new();
    if !inner_type_decls.is_empty() {
        // Walk the assembled target text for type_identifier references
        // matching an inner-type name. Skip references already qualified
        // (scoped_type_identifier — the operator wrote `Outer.Inner`
        // themselves) and the inner-type declarations the target may have
        // itself (defensive — they shouldn't appear, but if a moved cluster
        // somehow brought one along, don't qualify the declaration site).
        if let Ok(target_tree) = parse_source("java", &target_content) {
            let mut edits: Vec<(usize, usize, String)> = Vec::new();
            let mut stack = vec![target_tree.root_node()];
            while let Some(node) = stack.pop() {
                let mut c = node.walk();
                for ch in node.named_children(&mut c) {
                    stack.push(ch);
                }
                if node.kind() != "type_identifier" {
                    continue;
                }
                let Ok(text) = node.utf8_text(target_content.as_bytes()) else {
                    continue;
                };
                if !inner_type_decls.contains_key(text) {
                    continue;
                }
                // Skip references that are already part of a
                // `Outer.Inner` qualified type. tree-sitter-java exposes
                // that as `scoped_type_identifier` containing two
                // `type_identifier` children; if our match is one of
                // them, the parent's kind tells us.
                if node
                    .parent()
                    .map(|p| p.kind() == "scoped_type_identifier")
                    .unwrap_or(false)
                {
                    continue;
                }
                edits.push((
                    node.start_byte(),
                    node.end_byte(),
                    format!("{source_class_name}.{text}"),
                ));
                referenced_inner_types.insert(text.to_string());
            }
            // Also pick up uppercase-initial identifier receivers of
            // method_invocation / field_access that match an inner enum
            // name (`InnerEnum.VALUE`).
            let mut stack2 = vec![target_tree.root_node()];
            while let Some(node) = stack2.pop() {
                let mut c = node.walk();
                for ch in node.named_children(&mut c) {
                    stack2.push(ch);
                }
                if !matches!(node.kind(), "method_invocation" | "field_access") {
                    continue;
                }
                let Some(receiver) = node.child_by_field_name("object") else {
                    continue;
                };
                if receiver.kind() != "identifier" {
                    continue;
                }
                let Ok(text) = receiver.utf8_text(target_content.as_bytes()) else {
                    continue;
                };
                if !inner_type_decls.contains_key(text) {
                    continue;
                }
                edits.push((
                    receiver.start_byte(),
                    receiver.end_byte(),
                    format!("{source_class_name}.{text}"),
                ));
                referenced_inner_types.insert(text.to_string());
            }
            // G18: method_reference qualifier position. `Inner::new`,
            // `Inner::method`, `Inner::staticMethod` move to the target
            // unqualified and fail to resolve there because `Inner` is
            // a member of the source class, not the target's package.
            // tree-sitter-java parses `method_reference` with the
            // qualifier as the first named child (identifier or
            // type_identifier). Rewrite the qualifier to
            // `<SourceClass>.<Inner>`.
            let mut stack3 = vec![target_tree.root_node()];
            while let Some(node) = stack3.pop() {
                let mut c = node.walk();
                for ch in node.named_children(&mut c) {
                    stack3.push(ch);
                }
                if node.kind() != "method_reference" {
                    continue;
                }
                let mut qc = node.walk();
                let Some(qualifier) = node.named_children(&mut qc).next() else {
                    continue;
                };
                if !matches!(qualifier.kind(), "identifier" | "type_identifier") {
                    continue;
                }
                let Ok(text) = qualifier.utf8_text(target_content.as_bytes()) else {
                    continue;
                };
                if !inner_type_decls.contains_key(text) {
                    continue;
                }
                edits.push((
                    qualifier.start_byte(),
                    qualifier.end_byte(),
                    format!("{source_class_name}.{text}"),
                ));
                referenced_inner_types.insert(text.to_string());
            }
            edits.sort_by_key(|e| e.0);
            // Dedupe overlapping edits (e.g., a type_identifier inside a
            // method_invocation receiver matched twice).
            edits.dedup_by_key(|e| e.0);
            target_content = apply_text_edits(&target_content, &edits);
        }
        // Cross-package: target needs to import the source class so the
        // qualified `<SourceClass>.<InnerType>` references resolve.
        if !referenced_inner_types.is_empty() && cross_package {
            if let Some(src_pkg) = source_package.as_deref() {
                let source_fqcn = format!("{src_pkg}.{source_class_name}");
                target_content = java_inject_import(&target_content, &source_fqcn);
            }
        }
    }

    // Cross-package bare-field-to-getter rewrites (Gap 6). When the moved
    // code accesses a `private` field of a source-class inner type via a
    // method parameter (e.g. `truckTicket.direction`), the bare access
    // fails to compile from a different package. The inner type often
    // already declares a public getter (`getDirection()`); rewrite the
    // bare access to the getter call so cross-package extracts compile
    // without manual fixup.
    //
    // G17: Java records use `componentName()` accessor naming (no `get`
    // prefix), and the auto-generated backing fields are private. Cross-
    // CLASS bare access fails regardless of package — same-package
    // extracts hit the same compile error a cross-package extract would
    // for a POJO. So record components get rewritten unconditionally
    // when the inner type is a record_declaration; POJO rewriting stays
    // gated on cross_package.
    let any_inner_records = inner_type_decls
        .values()
        .any(|node| node.kind() == "record_declaration");
    if (cross_package || any_inner_records) && !inner_type_decls.is_empty() {
        let mut field_to_getter_by_type: BTreeMap<String, BTreeMap<String, String>> =
            BTreeMap::new();
        for (type_name, type_node) in &inner_type_decls {
            // G17: record components. tree-sitter-java exposes the
            // record header parameters via the `parameters` field on
            // record_declaration. Each formal_parameter has a `name`
            // child; the accessor is the bare component name (no prefix).
            if type_node.kind() == "record_declaration" {
                if let Some(params) = type_node.child_by_field_name("parameters") {
                    let mut pc = params.walk();
                    let mut getter_map: BTreeMap<String, String> = BTreeMap::new();
                    for p in params.named_children(&mut pc) {
                        if p.kind() != "formal_parameter" {
                            continue;
                        }
                        if let Some(name_node) = p.child_by_field_name("name") {
                            if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                                getter_map.insert(name.to_string(), name.to_string());
                            }
                        }
                    }
                    if !getter_map.is_empty() {
                        field_to_getter_by_type.insert(type_name.clone(), getter_map);
                    }
                }
                continue;
            }
            // POJO path is cross-package-only. Same-package extracts
            // retain bare access for non-record inner types.
            if !cross_package {
                continue;
            }
            let Some(type_body) = type_node.child_by_field_name("body") else {
                continue;
            };
            let mut fields: Vec<(String, String)> = Vec::new();
            let mut getters: BTreeSet<String> = BTreeSet::new();
            let mut tcur = type_body.walk();
            for child in type_body.named_children(&mut tcur) {
                match child.kind() {
                    "field_declaration" => {
                        if let (Some(n), Some(ty)) = (
                            java_field_declaration_name(child, &parsed.source),
                            java_field_type_text(child, &parsed.source),
                        ) {
                            fields.push((n, ty));
                        }
                    }
                    "method_declaration" => {
                        let mods = collect_java_modifiers(child);
                        let is_public = mods.iter().any(|(m, _, _)| m == "public");
                        if !is_public {
                            continue;
                        }
                        let no_params = child
                            .child_by_field_name("parameters")
                            .map(|p| {
                                let mut pcur = p.walk();
                                !p.named_children(&mut pcur)
                                    .any(|n| n.kind() == "formal_parameter")
                            })
                            .unwrap_or(false);
                        if !no_params {
                            continue;
                        }
                        if let Some(name_node) = child.child_by_field_name("name") {
                            if let Ok(name) = name_node.utf8_text(parsed.source.as_bytes()) {
                                getters.insert(name.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            let mut getter_map: BTreeMap<String, String> = BTreeMap::new();
            for (field_name, field_type) in fields {
                let cap = {
                    let mut chars = field_name.chars();
                    match chars.next() {
                        Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
                        None => String::new(),
                    }
                };
                let mut candidates = vec![format!("get{cap}")];
                if field_type.trim() == "boolean" {
                    candidates.insert(0, format!("is{cap}"));
                }
                for candidate in candidates {
                    if getters.contains(&candidate) {
                        getter_map.insert(field_name.clone(), candidate);
                        break;
                    }
                }
            }
            if !getter_map.is_empty() {
                field_to_getter_by_type.insert(type_name.clone(), getter_map);
            }
        }

        if !field_to_getter_by_type.is_empty() {
            // Re-parse target_content (post Gap-5 inner-type qualification)
            // and walk for `field_access` whose receiver is a parameter
            // typed as one of these inner types.
            if let Ok(target_tree) = parse_source("java", &target_content) {
                let mut edits: Vec<(usize, usize, String)> = Vec::new();
                let mut tstack = vec![target_tree.root_node()];
                while let Some(tnode) = tstack.pop() {
                    let mut c = tnode.walk();
                    for ch in tnode.named_children(&mut c) {
                        tstack.push(ch);
                    }
                    if tnode.kind() != "method_declaration"
                        && tnode.kind() != "constructor_declaration"
                    {
                        continue;
                    }
                    // Map parameter name → inner-type simple name.
                    let mut typed_receivers: BTreeMap<String, String> = BTreeMap::new();
                    if let Some(params) = tnode.child_by_field_name("parameters") {
                        let mut pc = params.walk();
                        for p in params.named_children(&mut pc) {
                            if p.kind() != "formal_parameter" {
                                continue;
                            }
                            let ty = p
                                .child_by_field_name("type")
                                .and_then(|t| t.utf8_text(target_content.as_bytes()).ok())
                                .map(|s| s.trim().to_string());
                            let name = p
                                .child_by_field_name("name")
                                .and_then(|n| n.utf8_text(target_content.as_bytes()).ok())
                                .map(|s| s.to_string());
                            if let (Some(ty), Some(name)) = (ty, name) {
                                // Param type may be `Outer.Inner` after the
                                // Gap-5 qualification pass; strip the
                                // qualifier to look up by simple name.
                                let simple = ty
                                    .rsplit_once('.')
                                    .map(|(_, s)| s.trim().to_string())
                                    .unwrap_or_else(|| ty.trim().to_string());
                                if field_to_getter_by_type.contains_key(&simple) {
                                    typed_receivers.insert(name, simple);
                                }
                            }
                        }
                    }
                    let Some(body) = tnode.child_by_field_name("body") else {
                        continue;
                    };
                    // Also pick up local variables declared inside the
                    // method body whose type matches an inner record/POJO
                    // — the user's call sites often go through
                    // `var r = fetchRecord(...); r.field`. Without this,
                    // only method-parameter receivers get rewritten and
                    // local-variable receivers leak through as raw
                    // bare-field reads.
                    let mut decl_stack = vec![body];
                    while let Some(node) = decl_stack.pop() {
                        let mut dc = node.walk();
                        for ch in node.named_children(&mut dc) {
                            decl_stack.push(ch);
                        }
                        match node.kind() {
                            "local_variable_declaration" => {
                                let Some(ty_node) = node.child_by_field_name("type") else {
                                    continue;
                                };
                                let Ok(ty_text) = ty_node.utf8_text(target_content.as_bytes())
                                else {
                                    continue;
                                };
                                let simple = ty_text
                                    .rsplit_once('.')
                                    .map(|(_, s)| s.trim().to_string())
                                    .unwrap_or_else(|| ty_text.trim().to_string());
                                if !field_to_getter_by_type.contains_key(&simple) {
                                    continue;
                                }
                                let mut vc = node.walk();
                                for child in node.named_children(&mut vc) {
                                    if child.kind() != "variable_declarator" {
                                        continue;
                                    }
                                    if let Some(name_node) = child.child_by_field_name("name") {
                                        if let Ok(name) =
                                            name_node.utf8_text(target_content.as_bytes())
                                        {
                                            typed_receivers
                                                .insert(name.to_string(), simple.clone());
                                        }
                                    }
                                }
                            }
                            "enhanced_for_statement" => {
                                let Some(ty_node) = node.child_by_field_name("type") else {
                                    continue;
                                };
                                let Ok(ty_text) = ty_node.utf8_text(target_content.as_bytes())
                                else {
                                    continue;
                                };
                                let simple = ty_text
                                    .rsplit_once('.')
                                    .map(|(_, s)| s.trim().to_string())
                                    .unwrap_or_else(|| ty_text.trim().to_string());
                                if !field_to_getter_by_type.contains_key(&simple) {
                                    continue;
                                }
                                if let Some(name_node) = node.child_by_field_name("name") {
                                    if let Ok(name) = name_node.utf8_text(target_content.as_bytes())
                                    {
                                        typed_receivers.insert(name.to_string(), simple);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    if typed_receivers.is_empty() {
                        continue;
                    }
                    // Walk this method's body for field_access on those
                    // receivers.
                    let mut bstack = vec![body];
                    while let Some(node) = bstack.pop() {
                        let mut bc = node.walk();
                        for ch in node.named_children(&mut bc) {
                            bstack.push(ch);
                        }
                        if node.kind() != "field_access" {
                            continue;
                        }
                        let Some(obj) = node.child_by_field_name("object") else {
                            continue;
                        };
                        if obj.kind() != "identifier" {
                            continue;
                        }
                        let Ok(obj_name) = obj.utf8_text(target_content.as_bytes()) else {
                            continue;
                        };
                        let Some(ty) = typed_receivers.get(obj_name) else {
                            continue;
                        };
                        let Some(field_node) = node.child_by_field_name("field") else {
                            continue;
                        };
                        let Ok(field_name) = field_node.utf8_text(target_content.as_bytes()) else {
                            continue;
                        };
                        let Some(getter_map) = field_to_getter_by_type.get(ty) else {
                            continue;
                        };
                        let Some(getter) = getter_map.get(field_name) else {
                            continue;
                        };
                        edits.push((
                            node.start_byte(),
                            node.end_byte(),
                            format!("{obj_name}.{getter}()"),
                        ));
                    }
                }
                edits.sort_by_key(|e| e.0);
                edits.dedup_by_key(|e| e.0);
                target_content = apply_text_edits(&target_content, &edits);
            }
        }
    }

    let mut source_edits = Vec::new();
    let removed_ranges = selected_methods
        .iter()
        .map(|method| (method.item.leading_trivia_start, method.item.byte_end))
        .chain(
            selected_fields
                .iter()
                .map(|field| (field.item.leading_trivia_start, field.item.byte_end)),
        )
        .chain(
            moved_constant_fields
                .iter()
                .map(|field| (field.item.leading_trivia_start, field.item.byte_end)),
        )
        .collect::<Vec<_>>();
    for (start, end) in &removed_ranges {
        source_edits.push(TextEdit {
            byte_start: *start,
            byte_end: *end,
            replacement: String::new(),
        });
    }

    // gap-9462575f: delete the now-orphaned `this.<moved> = <expr>;` source ctor
    // assignments — the field has moved to the target, which the source now
    // constructs (own_construction) or which an external owner populates. The
    // initializer expression has already been threaded into the wiring args
    // above. These statements live in the ctor body, disjoint from the moved
    // field declarations removed above.
    let src_bytes = parsed.source.as_bytes();
    for m in &moved_threaded {
        let (stmt_start, stmt_end) = m.assign_range;
        let line_start = parsed.source[..stmt_start]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(stmt_start);
        // Only swallow the whole physical line when the statement is ALONE on
        // it (everything before it is indentation), so no blank line is left
        // behind. A single-line constructor body —
        // `OrderService(Repo repo) { this.repo = repo; }` — shares the line
        // with the signature and opening brace; deleting to line start there
        // would eat the ctor declaration AND collide with the wiring insert at
        // the body start. In that case delete just the statement (plus one
        // trailing space) and leave the rest of the line intact.
        let alone_on_line = parsed.source[line_start..stmt_start].trim().is_empty();
        let (del_start, mut del_end) = if alone_on_line {
            let mut end = stmt_end;
            while end < src_bytes.len()
                && src_bytes[end] != b'\n'
                && src_bytes[end].is_ascii_whitespace()
            {
                end += 1;
            }
            if end < src_bytes.len() && src_bytes[end] == b'\n' {
                end += 1;
            }
            (line_start, end)
        } else {
            (stmt_start, stmt_end)
        };
        if !alone_on_line && del_end < src_bytes.len() && src_bytes[del_end] == b' ' {
            del_end += 1;
        }
        source_edits.push(TextEdit {
            byte_start: del_start,
            byte_end: del_end,
            replacement: String::new(),
        });
    }

    // Source-side visibility widening for referenced inner types (Gap 5).
    // Widen each below-floor inner-type declaration to the floor so the
    // qualified `<SourceClass>.<InnerType>` references on the new target
    // can reach them. Inner types already at/above the floor stay
    // unchanged.
    for name in &referenced_inner_types {
        if let Some(inner_node) = inner_type_decls.get(name) {
            let mods = collect_java_modifiers(*inner_node);
            let current = java_visibility_from_mods(&mods);
            if java_visibility_rank(current) >= java_visibility_rank(visibility_floor) {
                continue;
            }
            let new_visibility = if visibility_floor == "package" {
                None
            } else {
                Some(visibility_floor)
            };
            let vis_edit =
                build_visibility_rewrite_edit(*inner_node, &mods, new_visibility, &parsed.source);
            source_edits.push(vis_edit);
        }
    }

    let field_insert_at = java_class_body_insert_position(class_node, &parsed.source);
    let delegate_edit_idx = source_edits.len();
    // Source-side delegate field declaration varies by wiring strategy:
    // - own_construction: `private final <Target> <delegate>;` (paired with
    //   the `this.delegate = new Target(...)` ctor wiring below).
    // - external_injection: caller-supplied annotations + modifiers, then
    //   `<Target> <delegate>;` (no synthesized `final`, no ctor wiring — an
    //   external owner populates it after construction).
    // - none: skip the delegate field decl entirely. Operator wires by hand.
    let delegate_decl = match wiring_strategy.as_str() {
        "external_injection" => {
            let mut decl = String::from("\n    ");
            for ann in &delegate_field_annotations {
                decl.push_str(ann);
                decl.push(' ');
            }
            for modifier in &delegate_field_modifiers {
                decl.push_str(modifier);
                decl.push(' ');
            }
            decl.push_str(&format!("{target_class_name} {delegate_field};"));
            decl
        }
        "none" => String::new(),
        _ => format!("\n    private final {target_class_name} {delegate_field};"),
    };
    source_edits.push(TextEdit {
        byte_start: field_insert_at,
        byte_end: field_insert_at,
        replacement: delegate_decl,
    });
    // External-injection delegate-field annotations need their imports on the
    // source file. Added conservatively (see `java_conservative_import_edit`):
    // any non-static wildcard import suppresses the explicit single-type add.
    if external_injection {
        for fqcn in &delegate_field_annotation_imports {
            if let Some(edit) = java_conservative_import_edit(&parsed.source, fqcn) {
                source_edits.push(edit);
            }
        }
    }
    // Gap 5: when the target type lands in a different package than the source,
    // the source needs `import <target_pkg>.<TargetClass>;` so the new delegate
    // field declaration resolves. Same-package targets resolve implicitly.
    if cross_package {
        if let Some(target_pkg) = target_package.as_deref() {
            let fqcn = if target_pkg.is_empty() {
                target_class_name.clone()
            } else {
                format!("{target_pkg}.{target_class_name}")
            };
            if let Some(import_edit) = java_source_import_edit(&parsed.source, &fqcn) {
                source_edits.push(import_edit);
            }
        }
    }
    // Source-side wiring args: capture names verbatim, then `this::<method>`
    // for each callback (the target's ctor takes the captures first, then the
    // functional-interface callbacks, in declaration order).
    let assignment = {
        let mut args: Vec<String> = dependency_params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        for spec in &callback_specs {
            args.push(format!("this::{}", spec.method_name));
        }
        // gap-9462575f: thread each moved-field initializer expression, in the
        // same order they were appended to `ctor_params`.
        for m in &moved_threaded {
            args.push(m.init_expr.clone());
        }
        format!(
            "this.{delegate_field} = new {target_class_name}({});",
            args.join(", ")
        )
    };
    // Track the wiring edit so we can post-process its position after the
    // accessor-rewrite pass (Gap 8: topo-sort wiring against accessor edits
    // that introduce `<delegate_field>.<getter>()` reads earlier in the
    // same ctor body — the wiring must come before any such read).
    let mut wiring_state: Option<WiringInsertState> = None;
    // external_injection and none skip ctor wiring entirely — the delegate is
    // populated by an external owner (or the operator), not by a source-side
    // `this.delegate = new Target(...)` assignment.
    let skip_ctor_wiring = external_injection || wiring_strategy == "none";
    if skip_ctor_wiring {
        // No ctor wiring to insert. wiring_state stays None so the
        // post-process accessor topo-sort treats this case as "no
        // wiring edit to relocate."
    } else if let Some(constructor) = first_constructor_node(class_node, &parsed.source) {
        // Gap 7: when any captured-param name refers to a field rather than
        // a constructor parameter, that name is `null` until its own
        // `this.field = ...` assignment runs. Inserting the wiring as the
        // FIRST body statement evaluates the field reads too early — for
        // `final` fields the compiler rejects it (`might not have been
        // initialized`); for non-final fields it silently captures null.
        //
        // Defer the wiring statement until after the last source-ctor
        // assignment to any of those field-only captures. When every
        // captured name is also a ctor parameter (in scope from the
        // parameter list), insertion at the body start is safe and matches
        // the legacy placement.
        let ctor_params = constructor_parameter_names(constructor, &parsed.source);
        let field_only_captures: HashSet<&str> = dependency_params
            .iter()
            .map(|p| p.name.as_str())
            .filter(|name| !ctor_params.contains(*name))
            .collect();
        let lower_bound = if field_only_captures.is_empty() {
            constructor_body_insert_position(constructor, &parsed.source)
        } else {
            match last_field_assign_end_in_constructor(
                constructor,
                &parsed.source,
                &field_only_captures,
            ) {
                Some(end) => end,
                None => constructor_body_insert_position(constructor, &parsed.source),
            }
        };
        let body_range = constructor
            .child_by_field_name("body")
            .map(|b| (b.start_byte(), b.end_byte()));
        let edit_idx = source_edits.len();
        source_edits.push(TextEdit {
            byte_start: lower_bound,
            byte_end: lower_bound,
            replacement: format!("\n        {assignment}"),
        });
        wiring_state = Some(WiringInsertState {
            edit_idx,
            body_range,
        });
    } else {
        let class_name = java_class_name(class_node, &parsed.source);
        let constructor =
            java_constructor_decl(&class_name, "public", &[], false, Some(&assignment))?;
        source_edits[delegate_edit_idx].replacement = format!(
            "\n    private final {target_class_name} {delegate_field};\n\n{}",
            constructor.trim_end()
        );
    }
    // Compute caller rewrites separately so they can be threaded into the
    // accessor-rewrite pass below (Gap 1: caller-rewrite zero-width inserts
    // inside an LHS-write RHS must be absorbed into the LHS-write rendering,
    // not added to the global edit list).
    let caller_edits =
        java_caller_rewrite_edits(&parsed, method_names, delegate_field, &removed_ranges)?;

    // Gap 18 + Gap 6: rewrite each remaining source-side read/write of a
    // moved field through the delegate's generated getter/setter. Now fires
    // whenever `move_fields` is non-empty (regardless of `deep_analysis`)
    // unless the operator explicitly passes `rewrite_remaining_accessors=false`.
    if rewrite_remaining && !accessor_specs.is_empty() {
        // Skip ranges include both the moved field declarations AND the
        // moved method bodies — accesses inside moved methods are about to
        // be deleted as part of the extraction, so rewriting them would
        // overlap with the deletion edit.
        let mut skip_ranges: Vec<(usize, usize)> = selected_fields
            .iter()
            .map(|field| (field.item.byte_start, field.item.byte_end))
            .collect();
        skip_ranges.extend(
            selected_methods
                .iter()
                .map(|method| (method.item.byte_start, method.item.byte_end)),
        );
        let (accessor_rewrite_edits, residual_caller_edits) =
            compute_remaining_accessor_rewrite_edits(
                &parsed,
                &accessor_specs,
                &skip_ranges,
                delegate_field,
                caller_edits,
            )?;
        source_edits.extend(residual_caller_edits);
        source_edits.extend(accessor_rewrite_edits);
    } else {
        source_edits.extend(caller_edits);
    }

    // Gap 8: surface a stacked-extract ordering conflict when an accessor
    // rewrite earlier in the same ctor body would read `<delegate_field>`
    // before the planned wiring assigns it (compile-time
    // `variable <name> might not have been initialized`).
    //
    // We DON'T auto-rewrite: the only safe move would be pulling wiring
    // back past its field-only-capture lower bound (Gap 7's invariant),
    // which trades the compile error for a silent null-capture of those
    // captures. A proper fix needs to also relocate the field-only-capture
    // assignments themselves, which is invasive. For v1 the planner just
    // emits a tracing warning with diagnostics — the operator then either
    // (a) swaps statements manually, or (b) re-orders the extract sequence
    // so the delegate field exists before its consumers in the source ctor.
    if let Some(state) = wiring_state.as_ref() {
        if let Some((body_start, body_end)) = state.body_range {
            let wiring_pos = source_edits[state.edit_idx].byte_start;
            let earliest_accessor_in_ctor = source_edits
                .iter()
                .enumerate()
                .filter(|(idx, _)| *idx != state.edit_idx)
                .map(|(_, e)| e)
                .filter(|e| e.byte_start >= body_start && e.byte_end <= body_end)
                .map(|e| e.byte_start)
                .filter(|pos| *pos < wiring_pos)
                .min();
            if let Some(min_pos) = earliest_accessor_in_ctor {
                let (acc_line, _) = line_col(&parsed.source, min_pos);
                let (wir_line, _) = line_col(&parsed.source, wiring_pos);
                tracing::warn!(
                    target = "refactor",
                    kind = "extract_java_class",
                    code = "ctor_wiring_ordering_conflict",
                    delegate_field = %delegate_field,
                    wiring_line = wir_line,
                    accessor_line = acc_line,
                    "stacked-extract: delegate `{}` is read by an accessor \
                     rewrite at line {} but its wiring assignment lands at line {} — \
                     the post-apply ctor will fail to compile with \
                     `variable {} might not have been initialized`. Swap the \
                     two statements manually after apply.",
                    delegate_field,
                    acc_line,
                    wir_line,
                    delegate_field,
                );
            }
        }
    }
    let _ = wiring_state; // suppress unused-binding warning when no rewrites land

    // Constants whose declaration is moving lose their bare-name binding in
    // the source. Rewrite every remaining source-side reference (outside the
    // extracted methods and outside the moved declarations) to a qualified
    // `<TargetClass>.<CONST>` form. Constants are also rendered on the target
    // with a widened visibility (handled above by
    // `extract_field_text_with_visibility_floor`), so the qualified
    // references resolve from a different package when cross_package.
    let constant_names: Vec<String> = moved_constant_fields
        .iter()
        .map(|field| field.name.clone())
        .collect();
    if !constant_names.is_empty() {
        let mut const_skip_ranges: Vec<(usize, usize)> = selected_methods
            .iter()
            .map(|method| (method.item.byte_start, method.item.byte_end))
            .collect();
        const_skip_ranges.extend(
            selected_fields
                .iter()
                .map(|field| (field.item.byte_start, field.item.byte_end)),
        );
        const_skip_ranges.extend(
            moved_constant_fields
                .iter()
                .map(|field| (field.item.byte_start, field.item.byte_end)),
        );
        source_edits.extend(compute_remaining_constant_qualify_edits(
            &parsed,
            &constant_names,
            &const_skip_ranges,
            &target_class_name,
        ));
    }

    // G5: source-side delegate wrappers. When enabled, generate a thin
    // wrapper method on the source for each moved PUBLIC non-static
    // method. The wrapper signature matches the original; the body
    // delegates to the target via the new delegate field. Cross-file
    // callers holding references to the source class continue to
    // compile against the wrapper.
    if p.source_delegate_wrappers.unwrap_or(false) {
        let mut wrappers: Vec<String> = Vec::new();
        for method in &selected_methods {
            // Find the method declaration node by matching byte range.
            let mut method_node: Option<Node<'_>> = None;
            let mut stack = vec![class_node];
            while let Some(node) = stack.pop() {
                let mut c = node.walk();
                for ch in node.named_children(&mut c) {
                    stack.push(ch);
                }
                if node.kind() == "method_declaration"
                    && node.start_byte() == method.item.byte_start
                {
                    method_node = Some(node);
                    break;
                }
            }
            let Some(method_node) = method_node else {
                continue;
            };
            // Only wrap PUBLIC non-static methods. Static methods are
            // handled by G19 (class-qualified call auto-rewrite at the
            // call site, not via a source-side wrapper).
            if !java_method_has_modifier(method_node, &parsed.source, "public") {
                continue;
            }
            if java_method_has_modifier(method_node, &parsed.source, "static") {
                continue;
            }
            let Some(name_node) = method_node.child_by_field_name("name") else {
                continue;
            };
            let Ok(method_name) = name_node.utf8_text(parsed.source.as_bytes()) else {
                continue;
            };
            // Extract return type text — may be void.
            let return_type = method_node
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
                .unwrap_or("void");
            // Parameter list — verbatim text from source so generics +
            // annotations + final keywords carry through.
            let params_text = method_node
                .child_by_field_name("parameters")
                .and_then(|n| n.utf8_text(parsed.source.as_bytes()).ok())
                .unwrap_or("()");
            // Argument names for the delegate call.
            let mut arg_names: Vec<String> = Vec::new();
            if let Some(params) = method_node.child_by_field_name("parameters") {
                let mut pc = params.walk();
                for p_node in params.named_children(&mut pc) {
                    if p_node.kind() != "formal_parameter" && p_node.kind() != "spread_parameter" {
                        continue;
                    }
                    if let Some(name_n) = p_node.child_by_field_name("name") {
                        if let Ok(text) = name_n.utf8_text(parsed.source.as_bytes()) {
                            arg_names.push(text.to_string());
                        }
                    }
                }
            }
            let args_joined = arg_names.join(", ");
            // G5-FU: preserve any `throws T1, T2` clause from the original
            // method. tree-sitter-java exposes it as a `throws`-kind child
            // sibling of `parameters`. Without this, delegating to a moved
            // method that declares checked exceptions fails compilation.
            let throws_clause: String = {
                let mut cursor = method_node.walk();
                method_node
                    .children(&mut cursor)
                    .find(|c| c.kind() == "throws")
                    .and_then(|c| c.utf8_text(parsed.source.as_bytes()).ok())
                    .map(|s| format!(" {}", s.trim()))
                    .unwrap_or_default()
            };
            let body = if return_type.trim() == "void" {
                format!("        {delegate_field}.{method_name}({args_joined});")
            } else {
                format!("        return {delegate_field}.{method_name}({args_joined});")
            };
            let wrapper = format!(
                "\n    public {return_type} {method_name}{params_text}{throws_clause} {{\n{body}\n    }}\n"
            );
            wrappers.push(wrapper);
        }
        if !wrappers.is_empty() {
            // Insert wrappers at the position immediately before the
            // class body's closing brace.
            if let Some(body) = class_node.child_by_field_name("body") {
                let insert_at = body.end_byte().saturating_sub(1);
                let mut joined = String::new();
                for w in wrappers {
                    joined.push_str(&w);
                }
                source_edits.push(TextEdit {
                    byte_start: insert_at,
                    byte_end: insert_at,
                    replacement: joined,
                });
            }
        }
    }

    source_edits.sort_by_key(|edit| edit.byte_start);
    ensure_non_overlapping(&source_edits)?;

    // Gap 26: when fields are moved AND deep_analysis is on, surface every
    // remaining read/write of the moved fields that still lives in the
    // source class. Mirrors `plan_move_java_field`'s contract — operators
    // get one shape across both plan kinds.
    let remaining_source_accessors =
        if !selected_fields.is_empty() && p.deep_analysis.unwrap_or(false) {
            // Skip ranges include both the moved field declarations (about
            // to be deleted from source) AND the bodies of every method in
            // `item_names` (those methods move to the target, so accesses
            // inside them are about to leave the source class entirely —
            // listing them as "remaining" is a false positive).
            let mut skip_ranges = selected_fields
                .iter()
                .map(|field| (field.item.byte_start, field.item.byte_end))
                .collect::<Vec<_>>();
            skip_ranges.extend(
                selected_methods
                    .iter()
                    .map(|method| (method.item.byte_start, method.item.byte_end)),
            );
            let moved_field_names_owned = selected_fields
                .iter()
                .map(|field| field.name.clone())
                .collect::<Vec<_>>();
            compute_remaining_source_accessors(&parsed, &moved_field_names_owned, &skip_ranges)
        } else {
            Vec::new()
        };

    // Preview report for moved constants — populated under deep_analysis
    // analogous to `remaining_source_accessors`. The qualifier rewrites
    // themselves run unconditionally (see above) so cross-cluster refs
    // never silently miscompile.
    let remaining_source_constant_refs =
        if !constant_names.is_empty() && p.deep_analysis.unwrap_or(false) {
            let mut skip = selected_methods
                .iter()
                .map(|m| (m.item.byte_start, m.item.byte_end))
                .collect::<Vec<_>>();
            skip.extend(
                selected_fields
                    .iter()
                    .map(|f| (f.item.byte_start, f.item.byte_end)),
            );
            skip.extend(
                moved_constant_fields
                    .iter()
                    .map(|f| (f.item.byte_start, f.item.byte_end)),
            );
            compute_remaining_source_constant_refs(&parsed, &constant_names, &skip)
        } else {
            Vec::new()
        };

    // Gap 4: cross-file caller rewrites for moved static members. Every
    // `OldClass.<static>` reference in other project files gets its
    // qualifier rewritten to `NewClass`. Only static methods + moved
    // constants qualify here — instance methods on the source class still
    // resolve through the source-side private delegate field, which is
    // unreachable from other files (a future iteration could surface a
    // forwarding-method advisory for cross-file instance callers).
    let mut moved_static_items: Vec<MovedStaticItem> = Vec::new();
    for method in &selected_methods {
        let Some(method_name) = method.item.name.as_deref() else {
            continue;
        };
        let method_node = find_node(parsed.tree.root_node(), |node| {
            (node.kind() == "method_declaration" || node.kind() == "constructor_declaration")
                && node.start_byte() == method.item.byte_start
                && node.end_byte() == method.item.byte_end
        });
        if let Some(node) = method_node {
            if method_is_static(node) {
                moved_static_items.push(MovedStaticItem {
                    name: method_name.to_string(),
                    kind: "method",
                });
            }
        }
    }
    // moved_constant_fields covers static-final captures of extracted
    // methods (the Gap-20 path). selected_fields covers explicit
    // `move_fields` entries — when an operator moves a constant via
    // move_fields without it being captured, it lands here. Both
    // populate the cross-file rewrite list.
    for field in &moved_constant_fields {
        moved_static_items.push(MovedStaticItem {
            name: field.name.clone(),
            kind: "field",
        });
    }
    for field in &selected_fields {
        // Already covered by moved_constant_fields?
        if moved_constant_fields.iter().any(|f| f.name == field.name) {
            continue;
        }
        let field_node = find_node(parsed.tree.root_node(), |node| {
            node.kind() == "field_declaration"
                && node.start_byte() == field.item.byte_start
                && node.end_byte() == field.item.byte_end
        });
        if let Some(node) = field_node {
            if has_java_modifier(node, "static") && has_java_modifier(node, "final") {
                moved_static_items.push(MovedStaticItem {
                    name: field.name.clone(),
                    kind: "field",
                });
            }
        }
    }
    let cross_file_edits = if let Some(project_dir) = p.project_dir.as_deref() {
        compute_cross_file_static_caller_edits(
            Path::new(project_dir),
            &source_path,
            &target_path,
            &source_class_name,
            &target_class_name,
            target_package.as_deref(),
            &moved_static_items,
        )
    } else {
        Vec::new()
    };

    let mut all_edits = vec![
        FileEdit {
            path: path_string(&source_path),
            original_sha256: sha256_hex(parsed.source.as_bytes()),
            edits: source_edits,
            new_text: None,
        },
        FileEdit {
            path: path_string(&target_path),
            original_sha256: sha256_hex(&original_target_bytes),
            edits: vec![TextEdit {
                byte_start: 0,
                byte_end: original_target_bytes.len(),
                replacement: target_content,
            }],
            new_text: None,
        },
    ];
    let mut all_validations: Vec<ValidationStep> = parse_validation_step_for_path(&source_path)
        .into_iter()
        .chain(parse_validation_step_for_path(&target_path))
        .collect();
    for fe in &cross_file_edits {
        let caller_path = PathBuf::from(&fe.path);
        all_validations.extend(parse_validation_step_for_path(&caller_path));
    }
    all_edits.extend(cross_file_edits);

    let plan = RefactorPlan {
        title: format!(
            "Extract Java class {} from {}",
            target_class_name,
            source_path.display()
        ),
        kind: "extract_java_class".to_string(),
        semantic_status: SemanticStatus::SyntaxOnly,
        dry_run: true,
        file_moves: Vec::new(),
        file_creates: Vec::new(),
        edits: all_edits,
        validations: all_validations,
        items: selected_methods
            .into_iter()
            .map(|method| method.item)
            .chain(selected_fields.into_iter().map(|field| field.item))
            .collect(),
        leftovers: Vec::new(),
        captured_variables,
        remaining_source_accessors,
        remaining_source_constant_refs,
        external_calls: class_dependency_report.external_calls,
        inherited_dependencies: class_dependency_report.inherited_dependencies,
        deep_analysis: None,
        plan_status: PlanStatus::Planned,
        fixme_count: None,
        operator_opt_outs_used: Vec::new(),
    };
    Ok(serde_json::to_string_pretty(&plan)?)
}

/// G13: simple word-boundary check for whether the target body
/// references a bare identifier. Used by the propagate_class_annotations
/// auto-mode detection to decide whether moved bodies reference an
/// annotation-processor-generated identifier (e.g. `log` for @Slf4j).
fn target_content_references_identifier(text: &str, ident: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = ident.as_bytes();
    let mut cursor = 0usize;
    while cursor + needle.len() <= bytes.len() {
        let Some(rel) = text[cursor..].find(ident) else {
            return false;
        };
        let pos = cursor + rel;
        let after = pos + needle.len();
        let prev_ok = pos == 0 || {
            let prev = bytes[pos - 1] as char;
            !(prev == '_' || prev == '$' || prev.is_ascii_alphanumeric())
        };
        let next_ok = after == bytes.len() || {
            let next = bytes[after] as char;
            !(next == '_' || next == '$' || next.is_ascii_alphanumeric())
        };
        if prev_ok && next_ok {
            return true;
        }
        cursor = pos + needle.len();
    }
    false
}

/// Add `fqcn` as a single-type import on `source`, conservatively: if the
/// source has ANY non-static wildcard import, skip the add and return `None`.
///
/// Framework-neutral import hygiene. A wildcard import could already supply the
/// fqcn's simple name; adding an explicit single-type import would silently
/// change which symbol resolves bare. We cannot probe the classpath, so we
/// decline rather than risk it. Used for injected annotation imports whose
/// simple name (e.g. `Inject`) is commonly wildcard-exported by DI libraries.
fn java_conservative_import_edit(source: &str, fqcn: &str) -> Option<TextEdit> {
    let has_wildcard = source.lines().any(|line| {
        let t = line.trim();
        t.starts_with("import ")
            && !t.starts_with("import static ")
            && t.trim_end_matches(';').trim_end().ends_with(".*")
    });
    if has_wildcard {
        return None;
    }
    java_source_import_edit(source, fqcn)
}
