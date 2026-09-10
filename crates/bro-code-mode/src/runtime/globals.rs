// Vendored from openai/codex codex-rs/code-mode (Apache-2.0); see crate NOTICE.
use super::RuntimeState;
use super::callbacks::clear_timeout_callback;
use super::callbacks::exit_callback;
use super::callbacks::image_callback;
use super::callbacks::load_callback;
use super::callbacks::notify_callback;
use super::callbacks::set_timeout_callback;
use super::callbacks::store_callback;
use super::callbacks::text_callback;
use super::callbacks::tool_callback;
use super::callbacks::yield_control_callback;

pub(super) fn install_globals(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    let global = scope.get_current_context().global(scope);
    delete_global(scope, global, "console")?;
    delete_global(scope, global, "Atomics")?;
    delete_global(scope, global, "SharedArrayBuffer")?;
    delete_global(scope, global, "WebAssembly")?;

    let tools = build_tools_object(scope)?;
    let all_tools = build_all_tools_value(scope)?;
    let clear_timeout = helper_function(scope, "clearTimeout", clear_timeout_callback)?;
    let set_timeout = helper_function(scope, "setTimeout", set_timeout_callback)?;
    let text = helper_function(scope, "text", text_callback)?;
    let image = helper_function(scope, "image", image_callback)?;
    let store = helper_function(scope, "store", store_callback)?;
    let load = helper_function(scope, "load", load_callback)?;
    let notify = helper_function(scope, "notify", notify_callback)?;
    let yield_control = helper_function(scope, "yield_control", yield_control_callback)?;
    let exit = helper_function(scope, "exit", exit_callback)?;

    set_global(scope, global, "tools", tools.into())?;
    install_namespace_globals(scope, global)?;
    set_global(scope, global, "ALL_TOOLS", all_tools)?;
    set_global(scope, global, "clearTimeout", clear_timeout.into())?;
    set_global(scope, global, "setTimeout", set_timeout.into())?;
    set_global(scope, global, "text", text.into())?;
    set_global(scope, global, "image", image.into())?;
    set_global(scope, global, "store", store.into())?;
    set_global(scope, global, "load", load.into())?;
    set_global(scope, global, "notify", notify.into())?;
    set_global(scope, global, "yield_control", yield_control.into())?;
    set_global(scope, global, "exit", exit.into())?;
    Ok(())
}

fn build_tools_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Object>, String> {
    let tools = v8::Object::new(scope);
    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.enabled_tools.clone())
        .unwrap_or_default();

    for (tool_index, tool) in enabled_tools.iter().enumerate() {
        // Namespace-bound bindings project as `<namespace>.<method>` globals
        // (install_namespace_globals), not as flat `tools.*` properties.
        if tool.namespace_binding.is_some() {
            continue;
        }
        let name = v8::String::new(scope, &tool.global_name)
            .ok_or_else(|| "failed to allocate tool name".to_string())?;
        let function = tool_function(scope, tool_index)?;
        // Local addition (not vendored): installation failure is not admission.
        if tools.set(scope, name.into(), function.into()) != Some(true) {
            return Err(format!("failed to install tool {}", tool.canonical_name));
        }
    }
    Ok(tools)
}

/// Globals the runtime owns; a namespace global may not shadow them.
const RESERVED_GLOBALS: [&str; 15] = [
    "tools",
    "ALL_TOOLS",
    "text",
    "image",
    "store",
    "load",
    "notify",
    "yield_control",
    "exit",
    "setTimeout",
    "clearTimeout",
    "console",
    "Atomics",
    "SharedArrayBuffer",
    "WebAssembly",
];

/// Local addition (not vendored): install nested namespace objects for tools
/// carrying a [`NamespaceBinding`](crate::description::NamespaceBinding) —
/// e.g. a tool named `code.items` bound to namespace `code` / method `items`
/// becomes `await code.items(...)` in the cell. Dispatch is identical to
/// `tools.*`: the same per-index trampoline, the same host seam and filter.
fn install_namespace_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
) -> Result<(), String> {
    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.enabled_tools.clone())
        .unwrap_or_default();

    let mut grouped: std::collections::BTreeMap<String, Vec<(String, usize)>> =
        std::collections::BTreeMap::new();
    for (tool_index, tool) in enabled_tools.iter().enumerate() {
        if let Some(binding) = &tool.namespace_binding {
            grouped
                .entry(crate::description::normalize_code_mode_identifier(
                    &binding.namespace,
                ))
                .or_default()
                .push((
                    crate::description::normalize_code_mode_identifier(&binding.method),
                    tool_index,
                ));
        }
    }

    for (namespace, methods) in grouped {
        if RESERVED_GLOBALS.contains(&namespace.as_str()) {
            return Err(format!(
                "namespace global `{namespace}` would shadow a runtime global"
            ));
        }
        // Local addition (not vendored): do not shadow V8 builtins such as
        // Promise, JSON, or globalThis with a host-projected namespace.
        let namespace_key = v8::String::new(scope, &namespace)
            .ok_or_else(|| "failed to allocate namespace name".to_string())?;
        if global.has_own_property(scope, namespace_key.into()) == Some(true) {
            return Err(format!(
                "namespace global `{namespace}` would shadow an existing global"
            ));
        }
        let object = v8::Object::new(scope);
        for (method, tool_index) in methods {
            let key = v8::String::new(scope, &method)
                .ok_or_else(|| "failed to allocate namespace method name".to_string())?;
            let function = tool_function(scope, tool_index)?;
            if object.set(scope, key.into(), function.into()) != Some(true) {
                return Err(format!(
                    "failed to install namespace method {namespace}.{method}"
                ));
            }
        }
        set_global(scope, global, &namespace, object.into())?;
    }
    Ok(())
}

/// Local addition (not vendored): schema-bearing discovery covers the exact
/// installed coordinates, including namespace methods absent from `tools`.
fn build_all_tools_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<v8::Local<'s, v8::Value>, String> {
    let enabled_tools = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.enabled_tools.clone())
        .unwrap_or_default();
    let entries = enabled_tools
        .iter()
        .map(|tool| {
            let namespace = tool.namespace_binding.as_ref().map(|binding| {
                crate::description::normalize_code_mode_identifier(&binding.namespace)
            });
            let method = tool
                .namespace_binding
                .as_ref()
                .map(|binding| crate::description::normalize_code_mode_identifier(&binding.method))
                .unwrap_or_else(|| tool.global_name.clone());
            let callable = format!("{}.{}", namespace.as_deref().unwrap_or("tools"), method);
            serde_json::json!({
                "name": tool.global_name, "canonical_name": tool.canonical_name,
                "description": tool.description, "namespace": namespace,
                "method": method, "callable": callable, "kind": tool.kind,
                "input_schema": tool.input_schema, "output_schema": tool.output_schema,
                "declaration": tool.declaration,
            })
        })
        .collect::<Vec<_>>();
    super::value::json_to_v8(scope, &serde_json::Value::Array(entries))
        .ok_or_else(|| "failed to allocate ALL_TOOLS discovery metadata".to_string())
}

fn helper_function<'s, F>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    callback: F,
) -> Result<v8::Local<'s, v8::Function>, String>
where
    F: v8::MapFnTo<v8::FunctionCallback>,
{
    let name =
        v8::String::new(scope, name).ok_or_else(|| "failed to allocate helper name".to_string())?;
    let template = v8::FunctionTemplate::builder(callback)
        .data(name.into())
        .build(scope);
    template
        .get_function(scope)
        .ok_or_else(|| "failed to create helper function".to_string())
}

fn tool_function<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tool_index: usize,
) -> Result<v8::Local<'s, v8::Function>, String> {
    let data = v8::String::new(scope, &tool_index.to_string())
        .ok_or_else(|| "failed to allocate tool callback data".to_string())?;
    let template = v8::FunctionTemplate::builder(tool_callback)
        .data(data.into())
        .build(scope);
    template
        .get_function(scope)
        .ok_or_else(|| "failed to create tool function".to_string())
}

fn set_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
    name: &str,
    value: v8::Local<'s, v8::Value>,
) -> Result<(), String> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| format!("failed to allocate global `{name}`"))?;
    if global.set(scope, key.into(), value) == Some(true) {
        Ok(())
    } else {
        Err(format!("failed to set global `{name}`"))
    }
}

fn delete_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<'s, v8::Object>,
    name: &str,
) -> Result<(), String> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| format!("failed to allocate global `{name}`"))?;
    if global.delete(scope, key.into()) == Some(true) {
        Ok(())
    } else {
        Err(format!("failed to remove global `{name}`"))
    }
}
