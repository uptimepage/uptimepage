//! Unknown-key check for a request body, driven by its OpenAPI schema. The
//! stored shapes nested in a body (a monitor's `check`, a channel's `config`)
//! cannot carry `deny_unknown_fields` without breaking an agent on an older
//! build, so the boundary walks the value against the schema instead and
//! refuses any key no object declares.

use serde_json::{Map, Value};
use utoipa::ToSchema;

/// A body type's schema and the named schemas its `$ref`s point at.
pub struct BodySchema {
    root: Value,
    components: Map<String, Value>,
    unresolved: Option<String>,
}

impl BodySchema {
    pub fn of<T: ToSchema>() -> Self {
        let mut named = Vec::new();
        T::schemas(&mut named);
        let root = serde_json::to_value(T::schema()).unwrap_or_default();
        let components = named
            .into_iter()
            .map(|(name, schema)| (name, serde_json::to_value(schema).unwrap_or_default()))
            .collect();
        let mut schema = Self {
            root,
            components,
            unresolved: None,
        };
        schema.unresolved = schema
            .refs()
            .into_iter()
            .find(|name| !schema.components.contains_key(*name))
            .map(str::to_string);
        schema
    }

    pub fn root(&self) -> &Value {
        &self.root
    }

    pub fn components(&self) -> &Map<String, Value> {
        &self.components
    }

    /// A `$ref` the type does not collect, under which the walk would wave
    /// every key through.
    pub fn unresolved(&self) -> Option<&str> {
        self.unresolved.as_deref()
    }

    /// Every schema name the root reaches through `$ref`s, collected or not.
    pub fn refs(&self) -> Vec<&str> {
        let mut todo = vec![&self.root];
        let mut seen = Vec::new();
        while let Some(node) = todo.pop() {
            match node {
                Value::Object(map) => {
                    if let Some(name) = map.get("$ref").and_then(Value::as_str).map(ref_name)
                        && !seen.contains(&name)
                    {
                        seen.push(name);
                        todo.extend(self.components.get(name));
                    }
                    todo.extend(map.values());
                }
                Value::Array(items) => todo.extend(items),
                _ => {}
            }
        }
        seen
    }

    /// Every key in `value` that no object in the schema declares, as dotted
    /// paths.
    pub fn unknown_keys(&self, value: &Value) -> Vec<String> {
        let mut out = Vec::new();
        self.walk(value, &self.root, "", 0, &mut out);
        out
    }

    /// Sentence for the 422: the first offender, and what its object accepts.
    pub fn describe(&self, value: &Value) -> Option<String> {
        let first = self.unknown_keys(value).into_iter().next()?;
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
        let accepted = self.accepted_keys(value, parent);
        if !accepted.is_empty() {
            let list: Vec<String> = accepted.iter().map(|k| format!("`{k}`")).collect();
            msg.push_str(&format!(", expected one of {}", list.join(", ")));
        }
        Some(msg)
    }

    fn walk(&self, value: &Value, schema: &Value, path: &str, depth: usize, out: &mut Vec<String>) {
        if depth > 32 {
            return;
        }
        let schema = self.resolve(schema);
        if let Some(parts) = schema["allOf"].as_array() {
            return self.walk(value, &self.merge_all_of(parts), path, depth + 1, out);
        }
        if let Some(alternatives) = choices(schema) {
            if let Some(alt) = self.pick(value, alternatives) {
                self.walk(value, alt, path, depth + 1, out);
            }
            return;
        }
        match value {
            Value::Object(map) => self.walk_object(map, schema, path, depth, out),
            Value::Array(items) if schema["items"].is_object() => {
                for (i, item) in items.iter().enumerate() {
                    self.walk(
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
        &self,
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
                    self.walk(v, extra, &child(path, k), depth + 1, out);
                }
            }
            return;
        };
        for (k, v) in map {
            match properties.get(k) {
                Some(prop) => self.walk(v, prop, &child(path, k), depth + 1, out),
                None if is_open(schema) => {}
                None => out.push(child(path, k)),
            }
        }
    }

    /// The alternative the caller meant: the one whose every tag (`type`, `kind`,
    /// `op`, `provider`: a one-value enum property) the value carries, most tags
    /// first, else the one that fits the value's shape with the fewest
    /// complaints.
    fn pick<'a>(&self, value: &Value, alternatives: &'a [Value]) -> Option<&'a Value> {
        if let Value::Object(map) = value {
            let best = alternatives
                .iter()
                .filter_map(|alt| self.tag_score(map, alt).map(|n| (n, alt)))
                .max_by_key(|(n, _)| *n);
            if let Some((n, alt)) = best
                && n > 0
            {
                return Some(alt);
            }
        }
        alternatives
            .iter()
            .filter(|alt| shape_fits(value, &self.flatten(alt)))
            .min_by_key(|alt| {
                let mut found = Vec::new();
                self.walk(value, alt, "", 0, &mut found);
                found.len()
            })
    }

    /// How many of the schema's tags the object carries, or `None` when it
    /// contradicts one. Looks through `allOf` parts and nested choices without
    /// merging anything.
    fn tag_score(&self, map: &Map<String, Value>, schema: &Value) -> Option<usize> {
        let schema = self.resolve(schema);
        if let Some(parts) = schema["allOf"].as_array() {
            return parts
                .iter()
                .try_fold(0, |n, part| Some(n + self.tag_score(map, part)?));
        }
        if let Some(alternatives) = choices(schema) {
            return alternatives
                .iter()
                .filter_map(|alt| self.tag_score(map, alt))
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

    fn accepted_keys(&self, value: &Value, parent: &str) -> Vec<String> {
        let Some((value, schema)) = self.descend(value, parent) else {
            return Vec::new();
        };
        let mut schema = self.flatten(&schema);
        while let Some(alternatives) = choices(&schema) {
            schema = match self.pick(&value, alternatives) {
                Some(alt) => self.flatten(alt),
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
    fn descend(&self, value: &Value, path: &str) -> Option<(Value, Value)> {
        let mut value = value.clone();
        let mut schema = self.flatten(&self.root);
        for token in tokens(path) {
            schema = self.settle(&value, &schema)?;
            match token {
                Token::Key(key) => {
                    schema = self.flatten(schema["properties"].get(key)?);
                    value = value.get(key)?.clone();
                }
                Token::Index(i) => {
                    schema = self.flatten(&schema["items"]);
                    value = value.get(i)?.clone();
                }
            }
        }
        Some((value, schema))
    }

    /// A choice schema resolved to the alternative the value fits.
    fn settle(&self, value: &Value, schema: &Value) -> Option<Value> {
        let mut schema = self.flatten(schema);
        while let Some(alternatives) = choices(&schema) {
            schema = self.flatten(self.pick(value, alternatives)?);
        }
        Some(schema)
    }

    fn resolve<'s>(&'s self, schema: &'s Value) -> &'s Value {
        static NULL: Value = Value::Null;
        match schema["$ref"].as_str() {
            Some(reference) => self.components.get(ref_name(reference)).unwrap_or(&NULL),
            None => schema,
        }
    }

    /// A resolved schema with any `allOf` merged, so its `properties` are usable.
    fn flatten(&self, schema: &Value) -> Value {
        let schema = self.resolve(schema);
        match schema["allOf"].as_array() {
            Some(parts) => self.merge_all_of(parts),
            None => schema.clone(),
        }
    }

    /// One object schema from the parts, or a choice when a part is itself one:
    /// an internally tagged variant wrapping a tagged enum (`sms` over its
    /// providers) is the tag object joined to each provider in turn.
    fn merge_all_of(&self, parts: &[Value]) -> Value {
        let resolved: Vec<Value> = parts.iter().map(|p| self.flatten(p)).collect();
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
                        self.merge_all_of(&with)
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
}

/// The schema name a `$ref` such as `#/components/schemas/CheckSpec` points at.
pub fn ref_name(reference: &str) -> &str {
    reference.rsplit('/').next().unwrap_or(reference)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tagged_variant_is_checked_against_its_own_fields() {
        let schema = BodySchema::of::<crate::domain::CheckSpec>();
        let ok = json!({ "type": "tcp", "host": "db", "port": 5432, "timeout": 1000 });
        assert!(schema.unknown_keys(&ok).is_empty());
        let typo = json!({ "type": "tcp", "host": "db", "port": 5432, "timeuot": 1000 });
        assert_eq!(schema.unknown_keys(&typo), ["timeuot"]);
        assert_eq!(
            schema.describe(&typo).unwrap(),
            "unknown field `timeuot`, expected one of `host`, `port`, `timeout`, `type`"
        );
    }

    /// Every variant the schema declares accepts a body made of its own
    /// required keys, including each provider under `sms`, and a stray key
    /// in any of them is named with the right neighbours.
    #[test]
    fn every_channel_variant_accepts_its_own_keys() {
        let schema = BodySchema::of::<crate::domain::ChannelConfig>();
        let mut seen = 0;
        for variant in leaves(&schema, schema.root()) {
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
            assert!(schema.unknown_keys(&body).is_empty(), "{body}");
            let mut typo = body.clone();
            typo["zzz"] = json!(1);
            let message = schema.describe(&typo).unwrap();
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
    fn leaves(schema: &BodySchema, node: &Value) -> Vec<Value> {
        let node = schema.flatten(node);
        match choices(&node) {
            Some(alternatives) => alternatives
                .iter()
                .flat_map(|alt| leaves(schema, alt))
                .collect(),
            None => vec![node],
        }
    }

    #[test]
    fn map_keys_and_array_items_are_walked_not_refused() {
        let schema = BodySchema::of::<crate::domain::CheckSpec>();
        let check = json!({
            "type": "http", "url": "https://x", "method": "GET", "timeout": 1000,
            "follow_redirects": false, "max_redirects": 0, "verify_tls": true,
            "expected_status": { "kind": "one_of", "value": [200, 204] },
            "headers": { "X-Any": "1", "Other": "2" }
        });
        assert!(schema.unknown_keys(&check).is_empty());
        let flow = json!({
            "type": "flow", "start_url": "https://x", "timeout": 1000, "step_timeout": 100,
            "verify_tls": true,
            "steps": [{ "op": "click", "selector": "a" }, { "op": "goto", "url": "https://y", "wait": 1 }]
        });
        assert_eq!(schema.unknown_keys(&flow), ["steps[1].wait"]);
        assert_eq!(
            schema.describe(&flow).unwrap(),
            "unknown field `steps[1].wait`, expected one of `op`, `url`"
        );
    }

    /// A hint survives a nullable wrapper and a body that is itself a list.
    #[test]
    fn the_hint_reaches_through_options_and_top_level_arrays() {
        let schema = BodySchema::of::<crate::domain::TargetUpdate>();
        let patch = json!({ "alerts": [{ "channel_id": "c", "after_failures": 3 }] });
        assert_eq!(
            schema.describe(&patch).unwrap(),
            "unknown field `alerts[0].after_failures`, expected one of `channel_id`"
        );
        let schema = BodySchema::of::<Vec<crate::domain::NewTarget>>();
        let bulk = json!([{ "name": "a", "interval": 60, "check": { "type": "tcp", "host": "h", "port": 1, "timeuot": 1 } }]);
        assert_eq!(
            schema.describe(&bulk).unwrap(),
            "unknown field `[0].check.timeuot`, expected one of `host`, `port`, `timeout`, `type`"
        );
    }
}
