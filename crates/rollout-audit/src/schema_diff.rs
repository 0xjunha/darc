use serde_json::{Map, Value};

const SCHEMA_DIFF_LIMIT: usize = 8;
const ORDER_INSENSITIVE_SCHEMA_ARRAY_KEYS: &[&str] =
    &["allOf", "anyOf", "enum", "oneOf", "required", "type"];

/// Stores one human-readable difference inside a schema drift summary.
struct SchemaDifference {
    path: String,
    message: String,
}

/// Normalizes one JSON-like schema value to ignore irrelevant ordering noise.
pub(super) fn normalize_json(value: Value) -> Value {
    normalize_json_at_path(value, "$")
}

/// Summarizes the first few relevant normalized schema differences.
pub(super) fn summarize_schema_differences(left: &Value, right: &Value) -> Vec<String> {
    let mut differences = Vec::new();
    collect_schema_differences(left, right, "$", &mut differences, SCHEMA_DIFF_LIMIT + 1);
    let truncated = differences.len() > SCHEMA_DIFF_LIMIT;
    differences.truncate(SCHEMA_DIFF_LIMIT);
    let mut summary = differences
        .into_iter()
        .map(|difference| format!("{}: {}", difference.path, difference.message))
        .collect::<Vec<_>>();
    if truncated {
        summary.push("additional differences omitted".to_owned());
    }
    summary
}

/// Truncates one long single-line string for compact error summaries.
pub(super) fn truncate_text(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_owned();
    }
    let truncated = text
        .chars()
        .take(max_len.saturating_sub(3))
        .collect::<String>();
    format!("{truncated}...")
}

/// Normalizes one JSON value with path-aware array handling.
fn normalize_json_at_path(value: Value, path: &str) -> Value {
    match value {
        Value::Array(items) => {
            let items = items
                .into_iter()
                .enumerate()
                .map(|(index, item)| normalize_json_at_path(item, &format!("{path}[{index}]")))
                .collect::<Vec<_>>();
            if !schema_array_order_is_irrelevant(path) {
                return Value::Array(items);
            }
            let mut items = items
                .into_iter()
                .map(|item| {
                    let stable = serde_json::to_string(&item).unwrap_or_default();
                    (item, stable)
                })
                .collect::<Vec<_>>();
            items.sort_by(|(_, left_stable), (_, right_stable)| left_stable.cmp(right_stable));
            Value::Array(items.into_iter().map(|(item, _)| item).collect())
        }
        Value::Object(map) => {
            let mut entries = map.into_iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut sorted = Map::with_capacity(entries.len());
            for (key, child) in entries {
                let child_path = format!("{path}/{key}");
                sorted.insert(key, normalize_json_at_path(child, &child_path));
            }
            Value::Object(sorted)
        }
        other => other,
    }
}

/// Returns whether one schema array path is semantically order-insensitive.
fn schema_array_order_is_irrelevant(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|key| ORDER_INSENSITIVE_SCHEMA_ARRAY_KEYS.contains(&key))
}

/// Collects a bounded list of structural JSON differences for user-facing drift output.
fn collect_schema_differences(
    left: &Value,
    right: &Value,
    path: &str,
    differences: &mut Vec<SchemaDifference>,
    limit: usize,
) {
    if differences.len() >= limit {
        return;
    }

    match (left, right) {
        (Value::Object(left_map), Value::Object(right_map)) => {
            let mut keys = left_map
                .keys()
                .chain(right_map.keys())
                .cloned()
                .collect::<Vec<_>>();
            keys.sort();
            keys.dedup();

            for key in keys {
                if differences.len() >= limit {
                    return;
                }
                let child_path = format!("{path}/{key}");
                match (left_map.get(&key), right_map.get(&key)) {
                    (Some(left_child), Some(right_child)) => {
                        collect_schema_differences(
                            left_child,
                            right_child,
                            &child_path,
                            differences,
                            limit,
                        );
                    }
                    (Some(_), None) => differences.push(SchemaDifference {
                        path: child_path,
                        message: "removed key".to_owned(),
                    }),
                    (None, Some(_)) => differences.push(SchemaDifference {
                        path: child_path,
                        message: "added key".to_owned(),
                    }),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(left_items), Value::Array(right_items)) => {
            if left_items.len() != right_items.len() {
                differences.push(SchemaDifference {
                    path: path.to_owned(),
                    message: format!(
                        "array length changed from {} to {}",
                        left_items.len(),
                        right_items.len()
                    ),
                });
                if differences.len() >= limit {
                    return;
                }
            }

            for (index, (left_item, right_item)) in
                left_items.iter().zip(right_items.iter()).enumerate()
            {
                if differences.len() >= limit {
                    return;
                }
                let child_path = format!("{path}[{index}]");
                collect_schema_differences(left_item, right_item, &child_path, differences, limit);
            }
        }
        _ if left == right => {}
        _ => differences.push(SchemaDifference {
            path: path.to_owned(),
            message: format!(
                "changed from {} to {}",
                describe_json_value(left),
                describe_json_value(right)
            ),
        }),
    }
}

/// Describes one JSON value compactly for drift summaries.
fn describe_json_value(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            truncate_text(&serde_json::to_string(value).unwrap_or_default(), 80)
        }
        Value::Array(items) => format!("array(len={})", items.len()),
        Value::Object(_) => "object".to_owned(),
    }
}
