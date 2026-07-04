// Vendored from openai/codex codex-rs/code-mode (Apache-2.0); see crate NOTICE.
use serde_json::Value as JsonValue;

use crate::response::DEFAULT_IMAGE_DETAIL;
use crate::response::FunctionCallOutputContentItem;
use crate::response::ImageDetail;

const IMAGE_HELPER_EXPECTS_MESSAGE: &str = "image expects a non-empty image URL string, an object with image_url and optional detail, or a raw MCP image block";
const CODEX_IMAGE_DETAIL_META_KEY: &str = "codex/imageDetail";

macro_rules! caught_or_unknown {
    ($tc:expr) => {{
        if $tc.has_caught() {
            $tc.exception()
                .map(|exception| value_to_error_text(&mut $tc, exception))
                .unwrap_or_else(|| "unknown code mode exception".to_string())
        } else {
            "unknown code mode exception".to_string()
        }
    }};
}

pub(super) fn serialize_output_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<String, String> {
    if value.is_undefined()
        || value.is_null()
        || value.is_boolean()
        || value.is_number()
        || value.is_big_int()
        || value.is_string()
    {
        return Ok(value.to_rust_string_lossy(scope));
    }

    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    if let Some(stringified) = v8::json::stringify(&tc, value) {
        return Ok(stringified.to_rust_string_lossy(&tc));
    }
    if tc.has_caught() {
        return Err(tc
            .exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "unknown code mode exception".to_string()));
    }
    Ok(value.to_rust_string_lossy(&tc))
}

pub(super) fn normalize_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    detail_override: Option<String>,
) -> Result<FunctionCallOutputContentItem, ()> {
    let result = (|| -> Result<FunctionCallOutputContentItem, String> {
        let (image_url, detail) = if value.is_string() {
            (value.to_rust_string_lossy(scope), None)
        } else if value.is_object() && !value.is_array() {
            let object = v8::Local::<v8::Object>::try_from(value)
                .map_err(|_| IMAGE_HELPER_EXPECTS_MESSAGE.to_string())?;
            if let Some(image) = parse_non_mcp_output_image(scope, object)? {
                image
            } else {
                parse_mcp_output_image(scope, value)?
            }
        } else {
            return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
        };

        if image_url.is_empty() {
            return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
        }
        let lower = image_url.to_ascii_lowercase();
        if !(lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("data:"))
        {
            return Err("image expects an http(s) or data URL".to_string());
        }

        let detail = detail_override.or(detail);
        let detail = match detail {
            Some(detail) => {
                let normalized = detail.to_ascii_lowercase();
                Some(match normalized.as_str() {
                    "auto" => ImageDetail::Auto,
                    "low" => ImageDetail::Low,
                    "high" => ImageDetail::High,
                    "original" => ImageDetail::Original,
                    _ => {
                        return Err(
                            "image detail must be one of: auto, low, high, original".to_string()
                        );
                    }
                })
            }
            None => Some(DEFAULT_IMAGE_DETAIL),
        };

        Ok(FunctionCallOutputContentItem::InputImage { image_url, detail })
    })();

    match result {
        Ok(item) => Ok(item),
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            Err(())
        }
    }
}

fn parse_non_mcp_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    object: v8::Local<'_, v8::Object>,
) -> Result<Option<(String, Option<String>)>, String> {
    let image_url_key = v8::String::new(scope, "image_url")
        .ok_or_else(|| "failed to allocate image helper keys".to_string())?;
    let Some(image_url) = object.get(scope, image_url_key.into()) else {
        return Ok(None);
    };
    if image_url.is_undefined() {
        return Ok(None);
    }
    if !image_url.is_string() {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    }
    let detail_key = v8::String::new(scope, "detail")
        .ok_or_else(|| "failed to allocate image helper keys".to_string())?;
    let detail = parse_image_detail_value(scope, object.get(scope, detail_key.into()))?;
    Ok(Some((image_url.to_rust_string_lossy(scope), detail)))
}

fn parse_mcp_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<(String, Option<String>), String> {
    let Some(result) = v8_value_to_json(scope, value)? else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    };
    let JsonValue::Object(result) = result else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    };
    let Some(item_type) = result.get("type").and_then(JsonValue::as_str) else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    };
    if item_type != "image" {
        return Err(format!(
            "image only accepts MCP image blocks, got \"{item_type}\""
        ));
    }
    let data = result
        .get("data")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "image expected MCP image data".to_string())?;
    if data.is_empty() {
        return Err("image expected MCP image data".to_string());
    }

    let image_url = if data.to_ascii_lowercase().starts_with("data:") {
        data.to_string()
    } else {
        let mime_type = result
            .get("mimeType")
            .or_else(|| result.get("mime_type"))
            .and_then(JsonValue::as_str)
            .filter(|mime_type| !mime_type.is_empty())
            .unwrap_or("application/octet-stream");
        format!("data:{mime_type};base64,{data}")
    };
    let detail = result
        .get("_meta")
        .and_then(JsonValue::as_object)
        .and_then(|meta| meta.get(CODEX_IMAGE_DETAIL_META_KEY))
        .and_then(JsonValue::as_str)
        .filter(|detail| matches!(*detail, "auto" | "low" | "high" | "original"))
        .map(str::to_string);
    Ok((image_url, detail))
}

fn parse_image_detail_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: Option<v8::Local<'s, v8::Value>>,
) -> Result<Option<String>, String> {
    match value {
        Some(value) if value.is_string() => Ok(Some(value.to_rust_string_lossy(scope))),
        Some(value) if value.is_null() || value.is_undefined() => Ok(None),
        Some(_) => Err("image detail must be a string when provided".to_string()),
        None => Ok(None),
    }
}

pub(super) fn v8_value_to_json(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<Option<JsonValue>, String> {
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let Some(stringified) = v8::json::stringify(&tc, value) else {
        if tc.has_caught() {
            return Err(tc
                .exception()
                .map(|exception| value_to_error_text(&mut tc, exception))
                .unwrap_or_else(|| "unknown code mode exception".to_string()));
        }
        return Ok(None);
    };
    serde_json::from_str(&stringified.to_rust_string_lossy(&tc))
        .map(Some)
        .map_err(|err| format!("failed to serialize JavaScript value: {err}"))
}

/// Local addition (not vendored): serialize a stored value while preserving
/// function source at any depth reachable by JSON.stringify's replacer.
pub(super) fn v8_value_to_json_preserving_functions(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    function_source_key: &str,
) -> Result<Option<JsonValue>, String> {
    let key = serde_json::to_string(function_source_key)
        .map_err(|err| format!("failed to encode function source key: {err}"))?;
    let source = format!(
        r#"((value) => JSON.stringify(value, function (_key, current) {{
            if (typeof current === "function") {{
                return {{ [{key}]: Function.prototype.toString.call(current) }};
            }}
            return current;
        }}))"#
    );

    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let Some(source) = v8::String::new(&tc, &source) else {
        return Err("failed to allocate stored-value serializer".to_string());
    };
    let Some(serializer_value) = v8::Script::compile(&tc, source, None).and_then(|s| s.run(&tc))
    else {
        return Err(caught_or_unknown!(tc));
    };
    let serializer = v8::Local::<v8::Function>::try_from(serializer_value)
        .map_err(|_| "stored-value serializer did not compile to a function".to_string())?;
    let receiver = v8::undefined(&tc).into();
    let Some(stringified) = serializer.call(&tc, receiver, &[value]) else {
        return Err(caught_or_unknown!(tc));
    };
    if stringified.is_undefined() {
        return Ok(None);
    }
    if !stringified.is_string() {
        return Err("stored-value serializer did not return a JSON string".to_string());
    }
    serde_json::from_str(&stringified.to_rust_string_lossy(&tc))
        .map(Some)
        .map_err(|err| format!("failed to serialize JavaScript value: {err}"))
}

pub(super) fn json_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &JsonValue,
) -> Option<v8::Local<'s, v8::Value>> {
    let json = serde_json::to_string(value).ok()?;
    let json = v8::String::new(scope, &json)?;
    v8::json::parse(scope, json)
}

/// Local addition (not vendored): revive stored function-source envelopes at
/// any depth while parsing a stored JSON value back into V8.
pub(super) fn json_to_v8_reviving_functions<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &JsonValue,
    function_source_key: &str,
) -> Result<Option<v8::Local<'s, v8::Value>>, String> {
    let json =
        serde_json::to_string(value).map_err(|err| format!("failed to encode JSON: {err}"))?;
    let key = serde_json::to_string(function_source_key)
        .map_err(|err| format!("failed to encode function source key: {err}"))?;
    let source = format!(
        r#"((json) => JSON.parse(json, function (_key, current) {{
            if (current && typeof current === "object" && typeof current[{key}] === "string") {{
                const revived = (0, eval)("(" + current[{key}] + ")");
                if (typeof revived !== "function") {{
                    throw new TypeError("stored function source did not compile to a function");
                }}
                return revived;
            }}
            return current;
        }}))"#
    );

    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let Some(source) = v8::String::new(&tc, &source) else {
        return Err("failed to allocate stored-value reviver".to_string());
    };
    let Some(reviver_value) = v8::Script::compile(&tc, source, None).and_then(|s| s.run(&tc))
    else {
        return Err(caught_or_unknown!(tc));
    };
    let reviver = v8::Local::<v8::Function>::try_from(reviver_value)
        .map_err(|_| "stored-value reviver did not compile to a function".to_string())?;
    let Some(json) = v8::String::new(&tc, &json) else {
        return Err("failed to allocate stored JSON".to_string());
    };
    let receiver = v8::undefined(&tc).into();
    match reviver.call(&tc, receiver, &[json.into()]) {
        Some(revived) => Ok(Some(revived)),
        None => Err(caught_or_unknown!(tc)),
    }
}

pub(super) fn value_to_error_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> String {
    if value.is_object()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(value)
        && let Some(key) = v8::String::new(scope, "stack")
        && let Some(stack) = object.get(scope, key.into())
        && stack.is_string()
    {
        return stack.to_rust_string_lossy(scope);
    }
    value.to_rust_string_lossy(scope)
}

pub(super) fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    if let Some(message) = v8::String::new(scope, message) {
        scope.throw_exception(message.into());
    }
}
