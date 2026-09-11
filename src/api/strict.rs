//! Unknown-key check for a request body, driven by its OpenAPI schema. The
//! stored shapes nested in a body (a monitor's `check`, a channel's `config`)
//! cannot carry `deny_unknown_fields` without breaking an agent on an older
//! build, so the boundary walks the value against the schema instead and
//! refuses any key no object declares.

use std::borrow::Cow;
use std::sync::LazyLock;

use serde_json::{Map, Value};

static COMPONENTS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::to_value(&super::docs::openapi().components)
        .ok()
        .and_then(|c| c.get("schemas").cloned())
        .unwrap_or(Value::Null)
});

/// Every key in `value` that no object in `schema` declares, as dotted paths.
pub fn unknown_keys(value: &Value, schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(value, schema, "", 0, &mut out);
    out
}

/// Sentence for the 422: the first offender, and what its object accepts.
pub fn describe(value: &Value, schema: &Value) -> Option<String> {
    let first = unknown_keys(value, schema).into_iter().next()?;
    let parent = match first.rfind(['.', '[']) {
        Some(i) if first[i..].starts_with('.') => &first[..i],
        Some(i) => &first[..i],
        None => "",
    };
    let parent = if first[parent.len()..].starts_with('[') {
        &first[..first.rfind('.').unwrap_or(0)]
    } else {
        parent
    };
    let mut msg = format!("unknown field `{first}`");
    let accepted = accepted_keys(value, schema, parent);
    if !accepted.is_empty() {
        let list: Vec<String> = accepted.iter().map(|k| format!("`{k}`")).collect();
        msg.push_str(&format!(", expected one of {}", list.join(", ")));
    }
    Some(msg)
}

fn walk(value: &Value, schema: &Value, path: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 32 {
        return;
    }
    let schema = resolve(schema);
    if let Some(parts) = schema["allOf"].as_array() {
        return walk(value, &merge_all_of(parts), path, depth + 1, out);
    }
    if let Some(alternatives) = choices(&schema) {
        if let Some(alt) = pick(value, alternatives) {
            walk(value, alt, path, depth + 1, out);
        }
        return;
    }
    match value {
        Value::Object(map) => walk_object(map, &schema, path, depth, out),
        Value::Array(items) if schema["items"].is_object() => {
            for (i, item) in items.iter().enumerate() {
                walk(
                    item,
                    &schema["items"],
                    &format!("{path}[{i}]"),
                    depth + 1,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn walk_object(
    map: &Map<String, Value>,
    schema: &Value,
    path: &str,
    depth: usize,
    out: &mut Vec<String>,
) {
    let Some(properties) = schema["properties"].as_object() else {
        // A map, or a shape the schema leaves open: nothing to refuse, but a
        // typed map still has its values checked.
        if let Some(extra) = schema.get("additionalProperties").filter(|v| v.is_object()) {
            for (k, v) in map {
                walk(v, extra, &child(path, k), depth + 1, out);
            }
        }
        return;
    };
    for (k, v) in map {
        match properties.get(k) {
            Some(prop) => walk(v, prop, &child(path, k), depth + 1, out),
            None if is_open(schema) => {}
            None => out.push(child(path, k)),
        }
    }
}

/// The alternative the caller meant: the one whose every tag (`type`, `kind`,
/// `op`, `provider`: a one-value enum property) the value carries, most tags
/// first, else the one that fits the value's shape with the fewest
/// complaints.
fn pick<'a>(value: &Value, alternatives: &'a [Value]) -> Option<&'a Value> {
    if let Value::Object(map) = value {
        let best = alternatives
            .iter()
            .filter_map(|alt| tag_score(map, alt).map(|n| (n, alt)))
            .max_by_key(|(n, _)| *n);
        if let Some((n, alt)) = best
            && n > 0
        {
            return Some(alt);
        }
    }
    alternatives
        .iter()
        .filter(|alt| shape_fits(value, &flatten(alt)))
        .min_by_key(|alt| {
            let mut found = Vec::new();
            walk(value, alt, "", 0, &mut found);
            found.len()
        })
}

/// How many of the schema's tags the object carries, or `None` when it
/// contradicts one. Looks through `allOf` parts and nested choices without
/// merging anything.
fn tag_score(map: &Map<String, Value>, schema: &Value) -> Option<usize> {
    let schema = resolve(schema);
    if let Some(parts) = schema["allOf"].as_array() {
        return parts
            .iter()
            .try_fold(0, |n, part| Some(n + tag_score(map, part)?));
    }
    if let Some(alternatives) = choices(&schema) {
        return alternatives
            .iter()
            .filter_map(|alt| tag_score(map, alt))
            .max();
    }
    let mut n = 0;
    for (name, prop) in schema["properties"].as_object()? {
        if let Some([tag]) = prop["enum"].as_array().map(Vec::as_slice) {
            match map.get(name) {
                Some(v) if v == tag => n += 1,
                Some(_) => return None,
                None => {}
            }
        }
    }
    Some(n)
}

fn shape_fits(value: &Value, schema: &Value) -> bool {
    let declared: Vec<&str> = match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
        _ => return true,
    };
    let actual = match value {
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Null => "null",
    };
    declared
        .iter()
        .any(|d| *d == actual || (*d == "integer" && actual == "number"))
}

fn accepted_keys(value: &Value, schema: &Value, parent: &str) -> Vec<String> {
    let Some((value, schema)) = descend(value, schema, parent) else {
        return Vec::new();
    };
    let mut schema = flatten(&schema);
    while let Some(alternatives) = choices(&schema) {
        schema = match pick(&value, alternatives) {
            Some(alt) => flatten(alt),
            None => return Vec::new(),
        };
    }
    let mut keys: Vec<String> = schema["properties"]
        .as_object()
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

/// The value and schema at a dotted path such as `check.steps[2]` or `[3]`.
fn descend(value: &Value, schema: &Value, path: &str) -> Option<(Value, Value)> {
    let mut value = value.clone();
    let mut schema = flatten(schema);
    for token in tokens(path) {
        schema = settle(&value, &schema)?;
        match token {
            Token::Key(key) => {
                schema = flatten(schema["properties"].get(key)?);
                value = value.get(key)?.clone();
            }
            Token::Index(i) => {
                schema = flatten(&schema["items"]);
                value = value.get(i)?.clone();
            }
        }
    }
    Some((value, schema))
}

/// A choice schema resolved to the alternative the value fits.
fn settle(value: &Value, schema: &Value) -> Option<Value> {
    let mut schema = flatten(schema);
    while let Some(alternatives) = choices(&schema) {
        schema = flatten(pick(value, alternatives)?);
    }
    Some(schema)
}

enum Token<'a> {
    Key(&'a str),
    Index(usize),
}

fn tokens(path: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        let (key, rest) = segment.split_once('[').unwrap_or((segment, ""));
        if !key.is_empty() {
            out.push(Token::Key(key));
        }
        for index in rest.split('[').filter(|s| !s.is_empty()) {
            if let Ok(i) = index.trim_end_matches(']').parse() {
                out.push(Token::Index(i));
            }
        }
    }
    out
}

fn choices(schema: &Value) -> Option<&Vec<Value>> {
    schema["oneOf"]
        .as_array()
        .or_else(|| schema["anyOf"].as_array())
}

fn is_open(schema: &Value) -> bool {
    matches!(
        schema.get("additionalProperties"),
        Some(Value::Bool(true)) | Some(Value::Object(_))
    )
}

fn child(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn resolve(schema: &Value) -> Cow<'_, Value> {
    match schema["$ref"].as_str() {
        Some(reference) => {
            let name = reference.rsplit('/').next().unwrap_or_default();
            Cow::Borrowed(&COMPONENTS[name])
        }
        None => Cow::Borrowed(schema),
    }
}

/// A resolved schema with any `allOf` merged, so its `properties` are usable.
fn flatten(schema: &Value) -> Value {
    let schema = resolve(schema);
    match schema["allOf"].as_array() {
        Some(parts) => merge_all_of(parts),
        None => schema.into_owned(),
    }
}

/// One object schema from the parts, or a choice when a part is itself one:
/// an internally tagged variant wrapping a tagged enum (`sms` over its
/// providers) is the tag object joined to each provider in turn.
fn merge_all_of(parts: &[Value]) -> Value {
    let resolved: Vec<Value> = parts.iter().map(flatten).collect();
    for (i, part) in resolved.iter().enumerate() {
        if let Some(alternatives) = choices(part) {
            let rest: Vec<Value> = resolved
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, p)| p.clone())
                .collect();
            let expanded: Vec<Value> = alternatives
                .iter()
                .map(|alt| {
                    let mut with = rest.clone();
                    with.push(alt.clone());
                    merge_all_of(&with)
                })
                .collect();
            return serde_json::json!({ "oneOf": expanded });
        }
    }
    let mut properties = Map::new();
    let mut required = Vec::new();
    let mut open = false;
    for part in &resolved {
        if let Some(props) = part["properties"].as_object() {
            properties.extend(props.clone());
        }
        if let Some(req) = part["required"].as_array() {
            required.extend(req.iter().cloned());
        }
        open |= is_open(part);
    }
    let mut merged = Map::new();
    merged.insert("properties".into(), Value::Object(properties));
    merged.insert("required".into(), Value::Array(required));
    if open {
        merged.insert("additionalProperties".into(), Value::Bool(true));
    }
    Value::Object(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use utoipa::PartialSchema;

    fn schema_of<T: PartialSchema>() -> Value {
        serde_json::to_value(T::schema()).unwrap()
    }

    #[test]
    fn a_tagged_variant_is_checked_against_its_own_fields() {
        let schema = schema_of::<crate::domain::CheckSpec>();
        let ok = json!({ "type": "tcp", "host": "db", "port": 5432, "timeout": 1000 });
        assert!(unknown_keys(&ok, &schema).is_empty());
        let typo = json!({ "type": "tcp", "host": "db", "port": 5432, "timeuot": 1000 });
        assert_eq!(unknown_keys(&typo, &schema), ["timeuot"]);
        assert_eq!(
            describe(&typo, &schema).unwrap(),
            "unknown field `timeuot`, expected one of `host`, `port`, `timeout`, `type`"
        );
    }

    /// Every variant the schema declares accepts a body made of its own
    /// required keys, including each provider under `sms`, and a stray key
    /// in any of them is named with the right neighbours.
    #[test]
    fn every_channel_variant_accepts_its_own_keys() {
        let schema = schema_of::<crate::domain::ChannelConfig>();
        let mut seen = 0;
        for variant in leaves(&schema) {
            let mut body = Map::new();
            for key in variant["required"].as_array().into_iter().flatten() {
                let key = key.as_str().unwrap();
                let prop = &variant["properties"][key];
                body.insert(
                    key.to_string(),
                    prop["enum"]
                        .as_array()
                        .and_then(|e| e.first().cloned())
                        .unwrap_or(json!("x")),
                );
            }
            let body = Value::Object(body);
            assert!(unknown_keys(&body, &schema).is_empty(), "{body}");
            let mut typo = body.clone();
            typo["zzz"] = json!(1);
            let message = describe(&typo, &schema).unwrap();
            assert!(
                message.starts_with("unknown field `zzz`, expected one of "),
                "{message}"
            );
            for key in variant["properties"].as_object().unwrap().keys() {
                assert!(
                    message.contains(&format!("`{key}`")),
                    "{message} lacks {key}"
                );
            }
            seen += 1;
        }
        assert!(seen > 15, "{seen} variants");
    }

    /// Every fully expanded object alternative under a choice.
    fn leaves(schema: &Value) -> Vec<Value> {
        let schema = flatten(schema);
        match choices(&schema) {
            Some(alternatives) => alternatives.iter().flat_map(leaves).collect(),
            None => vec![schema],
        }
    }

    #[test]
    fn map_keys_and_array_items_are_walked_not_refused() {
        let schema = schema_of::<crate::domain::CheckSpec>();
        let check = json!({
            "type": "http", "url": "https://x", "method": "GET", "timeout": 1000,
            "follow_redirects": false, "max_redirects": 0, "verify_tls": true,
            "expected_status": { "kind": "one_of", "value": [200, 204] },
            "headers": { "X-Any": "1", "Other": "2" }
        });
        assert!(unknown_keys(&check, &schema).is_empty());
        let flow = json!({
            "type": "flow", "start_url": "https://x", "timeout": 1000, "step_timeout": 100,
            "verify_tls": true,
            "steps": [{ "op": "click", "selector": "a" }, { "op": "goto", "url": "https://y", "wait": 1 }]
        });
        assert_eq!(unknown_keys(&flow, &schema), ["steps[1].wait"]);
        assert_eq!(
            describe(&flow, &schema).unwrap(),
            "unknown field `steps[1].wait`, expected one of `op`, `url`"
        );
    }

    /// A hint survives a nullable wrapper and a body that is itself a list.
    #[test]
    fn the_hint_reaches_through_options_and_top_level_arrays() {
        let schema = schema_of::<crate::domain::TargetUpdate>();
        let patch = json!({ "alerts": [{ "channel_id": "c", "after_failures": 3 }] });
        assert_eq!(
            describe(&patch, &schema).unwrap(),
            "unknown field `alerts[0].after_failures`, expected one of `channel_id`"
        );
        let schema = schema_of::<Vec<crate::domain::NewTarget>>();
        let bulk = json!([{ "name": "a", "interval": 60, "check": { "type": "tcp", "host": "h", "port": 1, "timeuot": 1 } }]);
        assert_eq!(
            describe(&bulk, &schema).unwrap(),
            "unknown field `[0].check.timeuot`, expected one of `host`, `port`, `timeout`, `type`"
        );
    }
}
