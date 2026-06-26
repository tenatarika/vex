//! Typed JSON-RPC parameter helpers.
//!
//! Pre-H8 the MCP server silently coerced wrong-typed fields to their
//! defaults (`as_bool().unwrap_or(false)` etc.), which hid integration
//! bugs in downstream agents. The helpers here return `ParamError` on
//! type mismatch; the dispatcher maps that error to JSON-RPC 2.0
//! `-32602 Invalid params`.
//!
//! Extracted from `main.rs` in the v1.21 split — see
//! `.claude/Task/v1.21-vex-mcp-split.md`.

use anyhow::Result;
use serde_json::Value;

/// Marker error returned by the typed-param helpers (H8). When
/// `handle_request` downcasts to this type it emits a JSON-RPC 2.0
/// `-32602 Invalid params` response instead of the generic `-32000`
/// server error. Pre-H8 the MCP server silently coerced wrong-typed
/// fields to their defaults (`as_bool().unwrap_or(false)` etc.), which
/// hid integration bugs in downstream agents.
#[derive(Debug, thiserror::Error)]
#[error("invalid params: {0}")]
pub(crate) struct ParamError(pub(crate) String);

impl ParamError {
    pub(crate) fn wrong_type(field: &str, expected: &str, actual: &Value) -> Self {
        let kind = match actual {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        Self(format!(
            "`{field}` must be {expected}; got {kind} ({actual})"
        ))
    }

    pub(crate) fn missing(field: &str) -> Self {
        Self(format!("missing required field `{field}`"))
    }

    /// For fields that also accept a deprecated alias. Tells the caller
    /// both names so an LLM agent that hallucinated the alias
    /// (`symbol` vs `symbols`, `names` vs `symbols`) sees the canonical
    /// shape without round-tripping back to the schema description.
    pub(crate) fn missing_with_alias(canonical: &str, legacy: &str) -> Self {
        Self(format!(
            "missing required field `{canonical}` \
             (legacy alias `{legacy}` also accepted)"
        ))
    }
}

/// Required string field. Fails with `-32602` if missing or wrong type.
pub(crate) fn req_str<'a>(args: &'a Value, field: &str) -> Result<&'a str> {
    let v = &args[field];
    if v.is_null() {
        return Err(ParamError::missing(field).into());
    }
    v.as_str()
        .ok_or_else(|| ParamError::wrong_type(field, "a string", v).into())
}

/// Optional string field. `None` when absent / null; fails with `-32602`
/// when present but not a string.
pub(crate) fn opt_str<'a>(args: &'a Value, field: &str) -> Result<Option<&'a str>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    Some(
        v.as_str()
            .ok_or_else(|| ParamError::wrong_type(field, "a string", v)),
    )
    .transpose()
    .map_err(Into::into)
}

/// Optional bool with default. Fails when present-but-not-bool — silent
/// coerce (`as_bool().unwrap_or(default)` on `"true"`-string) silently
/// dropped the value, which hid downstream type bugs.
pub(crate) fn opt_bool(args: &Value, field: &str, default: bool) -> Result<bool> {
    let v = &args[field];
    if v.is_null() {
        return Ok(default);
    }
    v.as_bool()
        .ok_or_else(|| ParamError::wrong_type(field, "a boolean", v).into())
}

/// Optional bool that distinguishes "absent / null" from an explicit value.
/// Returns `None` when the field is absent or null; fails on wrong type the
/// same way [`opt_bool`] does. Used by the `index`/`update` `gpu` arm so an
/// explicit `gpu: false` can forward `--no-gpu` (overriding `.vex.toml gpu =
/// true`), while an absent `gpu` forwards nothing (letting config / VEX_DEVICE
/// decide via the CLI's `Device::resolve`).
pub(crate) fn opt_bool_some(args: &Value, field: &str) -> Result<Option<bool>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    Some(
        v.as_bool()
            .ok_or_else(|| ParamError::wrong_type(field, "a boolean", v)),
    )
    .transpose()
    .map_err(Into::into)
}

/// Optional u64 with default. Fails on negative / float / string input —
/// `serde_json::Value::as_u64()` returns `None` for all three, which the
/// old `unwrap_or(default)` silently masked.
pub(crate) fn opt_u64(args: &Value, field: &str, default: u64) -> Result<u64> {
    let v = &args[field];
    if v.is_null() {
        return Ok(default);
    }
    v.as_u64()
        .ok_or_else(|| ParamError::wrong_type(field, "a non-negative integer", v).into())
}

/// Optional u64 that distinguishes "absent / null" from "explicit value".
/// Returns `None` when the field is absent or null; fails on wrong type
/// the same way [`opt_u64`] does. Used by the `bundle` arm where a `0`
/// fallback would leak `--depth 0` (etc.) to the CLI — there the
/// presence of the field is itself the signal to forward it.
pub(crate) fn opt_u64_some(args: &Value, field: &str) -> Result<Option<u64>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    Some(
        v.as_u64()
            .ok_or_else(|| ParamError::wrong_type(field, "a non-negative integer", v)),
    )
    .transpose()
    .map_err(Into::into)
}

/// Optional f64. `None` when absent / null.
pub(crate) fn opt_f64(args: &Value, field: &str) -> Result<Option<f64>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    Some(
        v.as_f64()
            .ok_or_else(|| ParamError::wrong_type(field, "a number", v)),
    )
    .transpose()
    .map_err(Into::into)
}

/// Optional string-array. `None` when absent / null; fails when present
/// but not an array or when an element is not a string.
pub(crate) fn opt_str_array<'a>(args: &'a Value, field: &str) -> Result<Option<Vec<&'a str>>> {
    let v = &args[field];
    if v.is_null() {
        return Ok(None);
    }
    let arr = v
        .as_array()
        .ok_or_else(|| ParamError::wrong_type(field, "a string array", v))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, elem) in arr.iter().enumerate() {
        let s = elem
            .as_str()
            .ok_or_else(|| ParamError::wrong_type(&format!("{field}[{i}]"), "a string", elem))?;
        out.push(s);
    }
    Ok(Some(out))
}

/// Read a string-valued argument under its canonical name, falling back
/// to a legacy alias. When the legacy alias is used, the alias name is
/// pushed into `deprecated` so the JSON-RPC response can surface a
/// deprecation notice via `_meta.deprecated_args`. See
/// `docs/MCP-SCHEMA.md` for the canonical vocabulary and the back-compat
/// policy.
pub(crate) fn read_canonical_str<'a>(
    args: &'a Value,
    canonical: &str,
    legacy: &str,
    deprecated: &mut Vec<String>,
) -> Result<Option<&'a str>> {
    if let Some(s) = opt_str(args, canonical)? {
        return Ok(Some(s));
    }
    if let Some(s) = opt_str(args, legacy)? {
        deprecated.push(legacy.to_string());
        return Ok(Some(s));
    }
    Ok(None)
}

/// Array variant of `read_canonical_str` — used by tools whose primary
/// argument is `string[]` (e.g. `check`, `show`).
pub(crate) fn read_canonical_array<'a>(
    args: &'a Value,
    canonical: &str,
    legacy: &str,
    deprecated: &mut Vec<String>,
) -> Result<Option<&'a Vec<Value>>> {
    let cv = &args[canonical];
    if !cv.is_null() {
        let arr = cv
            .as_array()
            .ok_or_else(|| ParamError::wrong_type(canonical, "an array", cv))?;
        return Ok(Some(arr));
    }
    let lv = &args[legacy];
    if !lv.is_null() {
        let arr = lv
            .as_array()
            .ok_or_else(|| ParamError::wrong_type(legacy, "an array", lv))?;
        deprecated.push(legacy.to_string());
        return Ok(Some(arr));
    }
    Ok(None)
}
