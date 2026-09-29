// gantry — a JSON Schema (draft-07 subset) validator for gantry's published
// `--json` schemas (`docs/schemas/*.json`).
//
// The diagnostics commands publish their output contracts as JSON Schema
// files; the acceptance for bf-1n4 is that `why --json` validates against
// its published schema. This module is what makes that sentence executable:
// tests load the published file and run real command output through
// [`validate`] — the schema is load-bearing, not decorative documentation.
//
// Why in-repo rather than a validator crate: gantry ships as a single static
// binary with three runtime dependencies (serde, toml, dirs), and the
// published schemas only ever use the small keyword set below. A focused
// validator keeps the dep tree flat, and — because it is exercised by the
// same tests that validate the output — it cannot drift from the subset the
// schemas actually use without failing a test.
//
// Supported keywords (everything `docs/schemas/*` uses):
//   - `type` — string or array of strings ("object", "array", "string",
//     "boolean", "null", "number", "integer"); unknown type names are
//     ignored so a future schema upgrade cannot be silently mistaken for
//     validation
//   - `enum`, `const`
//   - `properties`, `required`, `additionalProperties` (boolean form)
//   - `items` (array schemas)
//
// Deliberately unsupported: `$ref`, `oneOf`/`anyOf`/`allOf`, pattern
// keywords. The published schemas are written against this subset (inline,
// nullability via `"type": ["x", "null"]`); a schema using an unsupported
// keyword would validate loosely rather than correctly — which is why the
// files carry no such keywords.

use serde_json::Value;

/// Validate one document against one schema.
///
/// Returns `Ok(())` when the document conforms, or every violation found,
/// each carrying the JSON path it occurred at (`$.run.verdict.exit_code`).
/// All violations are collected, not just the first — a schema-drift report
/// you have to fix one error per run is a bad report.
pub fn validate(schema: &Value, doc: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    validate_node(schema, doc, "$", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Validate one node, appending violations to `errors`. `path` is the
/// document location being validated (the `$` root, then dotted/bracketed
/// steps).
fn validate_node(schema: &Value, doc: &Value, path: &str, errors: &mut Vec<String>) {
    // Boolean schemas: `true` accepts anything, `false` nothing (draft-07 §4.3).
    match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errors.push(format!("{path}: schema forbids any value"));
            return;
        }
        Value::Object(_) => {}
        _ => {
            errors.push(format!("{path}: unsupported schema shape (not an object)"));
            return;
        }
    }
    let schema = schema.as_object().unwrap();

    // `type`: further keyword checks are meaningless against a value of the
    // wrong type, so a type mismatch stops this branch.
    match schema.get("type") {
        Some(Value::String(t)) => {
            if !type_matches(t, doc) {
                errors.push(format!(
                    "{path}: expected type {t}, got {}",
                    json_type_name(doc)
                ));
                return;
            }
        }
        Some(Value::Array(ts)) => {
            let matched = ts
                .iter()
                .any(|t| t.as_str().map(|t| type_matches(t, doc)).unwrap_or(false));
            if !matched {
                let names: Vec<String> = ts
                    .iter()
                    .map(|t| t.as_str().unwrap_or("?").to_string())
                    .collect();
                errors.push(format!(
                    "{path}: expected one of type [{}], got {}",
                    names.join(", "),
                    json_type_name(doc)
                ));
                return;
            }
        }
        _ => {}
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(doc) {
            errors.push(format!("{path}: value {doc} is not one of the enum values"));
        }
    }

    if let Some(expected) = schema.get("const") {
        if expected != doc {
            errors.push(format!(
                "{path}: value {doc} does not equal const {expected}"
            ));
        }
    }

    if doc.is_object() {
        let map = doc.as_object().unwrap();

        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (key, subschema) in properties {
                if let Some(value) = map.get(key) {
                    validate_node(subschema, value, &format!("{path}.{key}"), errors);
                }
            }
        }

        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    errors.push(format!("{path}: missing required property '{key}'"));
                }
            }
        }

        // additionalProperties: only the boolean `false` form (the list form
        // is a schema — the published files don't use it).
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            let known: std::collections::HashSet<&str> = schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|properties| properties.keys().map(String::as_str).collect())
                .unwrap_or_default();
            for key in map.keys() {
                if !known.contains(key.as_str()) {
                    errors.push(format!(
                        "{path}: unexpected property '{key}' (additionalProperties is false)"
                    ));
                }
            }
        }
    }

    if let Some(items) = doc.as_array() {
        if let Some(item_schema) = schema.get("items") {
            for (index, value) in items.iter().enumerate() {
                validate_node(item_schema, value, &format!("{path}[{index}]"), errors);
            }
        }
    }
}

/// Whether `doc` is of the JSON Schema type `name`. An unrecognized type
/// name matches everything: the alternative (always failing) would make a
/// future draft keyword masquerade as validation.
fn type_matches(name: &str, doc: &Value) -> bool {
    match name {
        "object" => doc.is_object(),
        "array" => doc.is_array(),
        "string" => doc.is_string(),
        "boolean" => doc.is_boolean(),
        "null" => doc.is_null(),
        "number" => doc.is_number(),
        // JSON Schema "integer" means a number with no fractional part; the
        // documents we validate emit serde integers, so i64/u64 is the check.
        "integer" => doc.is_i64() || doc.is_u64(),
        _ => true,
    }
}

/// The JSON type of a document value, for error messages.
fn json_type_name(doc: &Value) -> &'static str {
    match doc {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn violations(schema: Value, doc: Value) -> Vec<String> {
        validate(&schema, &doc).expect_err("expected violations")
    }

    #[test]
    fn a_conforming_document_validates() {
        let schema = json!({
            "type": "object",
            "required": ["schema_version", "found"],
            "properties": {
                "schema_version": {"const": 1},
                "found": {"type": "boolean"}
            }
        });
        let doc = json!({"schema_version": 1, "found": false});
        assert!(validate(&schema, &doc).is_ok());
    }

    #[test]
    fn a_missing_required_property_is_reported_with_its_path() {
        let schema = json!({
            "type": "object",
            "required": ["found"],
            "properties": {"found": {"type": "boolean"}}
        });
        let errors = violations(schema, json!({"other": true}));
        assert_eq!(
            errors,
            vec!["$: missing required property 'found'".to_string()]
        );
    }

    #[test]
    fn a_type_mismatch_names_both_types() {
        let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}});
        let errors = violations(schema, json!({"n": "5"}));
        assert_eq!(
            errors,
            vec!["$.n: expected type integer, got string".to_string()]
        );
    }

    #[test]
    fn every_violation_is_collected_not_just_the_first() {
        let schema = json!({
            "type": "object",
            "required": ["a", "b"],
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "string"}
            }
        });
        let errors = violations(schema, json!({"a": 1}));
        assert_eq!(errors.len(), 2, "{errors:?}");
    }

    #[test]
    fn enum_rejects_values_outside_the_set() {
        let schema = json!({"enum": ["remote", "local"]});
        assert!(validate(&schema, &json!("remote")).is_ok());
        let errors = violations(schema, json!("sideways"));
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("not one of the enum values"),
            "{errors:?}"
        );
    }

    #[test]
    fn additional_properties_false_rejects_unknown_keys() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {"known": {"type": "boolean"}}
        });
        assert!(validate(&schema, &json!({"known": true})).is_ok());
        let errors = violations(schema, json!({"known": true, "extra": 1}));
        assert_eq!(
            errors,
            vec!["$: unexpected property 'extra' (additionalProperties is false)".to_string()]
        );
    }

    #[test]
    fn array_items_are_validated_with_indexed_paths() {
        let schema = json!({
            "type": "array",
            "items": {"type": "string"}
        });
        assert!(validate(&schema, &json!(["a", "b"])).is_ok());
        let errors = violations(schema, json!(["a", 3]));
        assert_eq!(
            errors,
            vec!["$[1]: expected type string, got number".to_string()]
        );
    }

    #[test]
    fn nullable_type_accepts_null_and_the_real_type() {
        let schema = json!({"type": ["object", "null"], "properties": {"x": {"type": "integer"}}});
        assert!(validate(&schema, &json!(null)).is_ok());
        assert!(validate(&schema, &json!({"x": 2})).is_ok());
        assert!(violations(schema, json!({"x": "no"})).len() == 1);
    }

    #[test]
    fn object_keywords_do_not_fire_on_null_when_null_is_allowed() {
        // The nullable verdict shape: required object properties must not be
        // reported missing when the value is legitimately null.
        let schema = json!({
            "type": ["object", "null"],
            "required": ["verdict"],
            "properties": {"verdict": {"type": "string"}}
        });
        assert!(validate(&schema, &json!(null)).is_ok());
        assert!(violations(schema, json!({})).len() == 1);
    }

    #[test]
    fn nested_documents_report_the_full_path() {
        let schema = json!({
            "type": "object",
            "properties": {
                "run": {
                    "type": "object",
                    "properties": {
                        "verdict": {"type": "object", "properties": {"exit_code": {"type": "integer"}}}
                    }
                }
            }
        });
        let errors = violations(schema, json!({"run": {"verdict": {"exit_code": "zero"}}}));
        assert_eq!(
            errors,
            vec!["$.run.verdict.exit_code: expected type integer, got string".to_string()]
        );
    }

    #[test]
    fn the_false_boolean_schema_rejects_everything() {
        let errors = violations(json!(false), json!("anything"));
        assert_eq!(errors, vec!["$: schema forbids any value".to_string()]);
        assert!(validate(&json!(true), &json!("anything")).is_ok());
    }

    #[test]
    fn a_non_object_schema_shape_is_reported_not_panic() {
        let errors = violations(json!("not a schema"), json!({"x": 1}));
        assert!(errors[0].contains("unsupported schema shape"), "{errors:?}");
    }

    #[test]
    fn unknown_type_names_validate_loosely_rather_than_failing() {
        // A draft keyword this subset does not know must not silently turn
        // into a rejection (or an acceptance that looks like validation).
        let schema = json!({"type": "fancy"});
        assert!(validate(&schema, &json!("anything")).is_ok());
    }

    #[test]
    fn an_integer_fraction_is_rejected() {
        let schema = json!({"type": "integer"});
        assert!(violations(schema.clone(), json!(1.5)).len() == 1);
        assert!(validate(&schema, &json!(2)).is_ok());
    }
}
