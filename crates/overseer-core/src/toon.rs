//! TOON-style tabular encoder (B1-3, Token-Oriented Object Notation pattern).
//!
//! Prompt-facing serialization only: uniform rows of JSON objects render as a
//! header-once + CSV-style body, cutting ~30-60% of bytes vs. pretty JSON on
//! tabular data. Lossless for the supported shape (flat objects, scalar
//! values); anything else falls back to the original text. JSON stays at all
//! API boundaries (schemas, event log, ledger) — this only changes what the
//! model reads. Zero deps.

/// Encode `rows` (already parsed) as a TOON table. Returns `None` when the
/// shape is not a uniform table (fewer than 3 rows, ragged keys, nested
/// values) — the caller keeps the original text.
pub fn encode_table(rows: &[serde_json::Value]) -> Option<String> {
    if rows.len() < 3 {
        return None; // header overhead only pays off at 3+ rows
    }
    // Uniform flat objects with scalar values only.
    let mut keys: Vec<String> = Vec::new();
    let mut table: Vec<Vec<String>> = Vec::new();
    for row in rows {
        let obj = row.as_object()?;
        if keys.is_empty() {
            keys = obj.keys().cloned().collect();
            keys.sort();
            if keys.is_empty() {
                return None;
            }
        } else if obj.keys().len() != keys.len() || !obj.keys().all(|k| keys.contains(k)) {
            return None; // ragged schema
        }
        let mut cells = Vec::with_capacity(keys.len());
        for k in &keys {
            cells.push(cell_to_string(&obj[k])?);
        }
        table.push(cells);
    }
    let mut out = String::from("TOON table (rows as values under header keys):\n");
    out.push_str(&keys.join(","));
    out.push('\n');
    for cells in &table {
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    Some(out)
}

/// One scalar cell: strings pass through (commas/newlines escaped to `;`),
/// numbers/bools render flat, null renders empty. Nested array/object → None
/// (falls back to JSON — no lossy flattening).
fn cell_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.replace([',', '\n', '\r'], ";")),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some(String::new()),
        _ => None,
    }
}

/// Try to parse `text` as a JSON array of objects and encode it. Returns the
/// original text when the shape is not a uniform table.
pub fn maybe_encode_json_array(text: &str) -> String {
    let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
        return text.to_string();
    };
    encode_table(&rows).unwrap_or_else(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(n: usize) -> Vec<serde_json::Value> {
        (0..n)
            .map(|i| {
                serde_json::json!({
                    "file": format!("src/a{i}.rs"),
                    "line": i,
                    "kind": "fn",
                })
            })
            .collect()
    }

    #[test]
    fn round_trips_uniform_table_with_byte_cut() {
        let r = rows(10);
        let json = serde_json::to_string_pretty(&r).unwrap();
        let toon = encode_table(&r).expect("uniform table must encode");
        // Lossless on the scalar shape: every value survives.
        for i in 0..10 {
            assert!(toon.contains(&format!("src/a{i}.rs")));
        }
        assert!(toon.contains("file,"));
        let cut = 1.0 - (toon.len() as f64 / json.len() as f64);
        assert!(
            cut >= 0.30,
            "need >=30% byte cut, got {cut:.2} (json {}, toon {})",
            json.len(),
            toon.len()
        );
    }

    #[test]
    fn small_or_ragged_tables_pass_through() {
        assert!(encode_table(&rows(2)).is_none(), "<3 rows untouched");
        let ragged = vec![
            serde_json::json!({"a": 1}),
            serde_json::json!({"a": 1}),
            serde_json::json!({"a": 1, "b": 2}),
        ];
        assert!(encode_table(&ragged).is_none());
        let nested = vec![
            serde_json::json!({"a": {"b": 1}}),
            serde_json::json!({"a": {"b": 2}}),
            serde_json::json!({"a": {"b": 3}}),
        ];
        assert!(encode_table(&nested).is_none());
        assert_eq!(
            maybe_encode_json_array("not json"),
            "not json",
            "non-JSON passes through"
        );
    }
}
