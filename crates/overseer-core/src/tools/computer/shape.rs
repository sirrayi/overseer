//! Output shaping for the cua-driver backend (D4): driver responses render
//! into compact, line-oriented text — one element/ref/app per line — so the
//! model reads structure without paying for a markdown tree or raw JSON.
//!
//! Everything here is a pure function over the driver response `Value`, so
//! the fake-driver tests and future field-name changes stay cheap.

use std::collections::HashMap;

use serde_json::Value;

/// The human line(s) a cua-driver result carries in `content[]` — joined
/// with newlines when there is more than one text part.
pub fn content_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k))
}

fn s<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    field(v, keys).and_then(Value::as_str)
}

/// `apps` render (D3): `pid  name  [front]`, running apps only — the driver
/// lists kernel threads and not-running installs too, which are noise here.
pub fn apps(result: &Value) -> String {
    let mut out = String::new();
    let Some(list) = result
        .pointer("/structuredContent/apps")
        .or_else(|| result.get("apps"))
        .and_then(Value::as_array)
    else {
        let t = content_text(result);
        return if t.is_empty() { "(no apps)".into() } else { t };
    };
    for app in list {
        if app.get("running").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let pid = app.get("pid").and_then(Value::as_i64).unwrap_or(0);
        let name = s(app, &["name"]).unwrap_or("?");
        let front = if app.get("active").and_then(Value::as_bool) == Some(true) {
            "  [front]"
        } else {
            ""
        };
        out.push_str(&format!("{pid}  {name}{front}\n"));
    }
    if out.is_empty() {
        out.push_str("(no running apps)");
    }
    out.trim_end_matches('\n').to_string()
}

/// `windows` render (D3): `window_id  pid  app  "title"  WxH`.
pub fn windows(result: &Value) -> String {
    let mut out = String::new();
    let Some(list) = result
        .pointer("/structuredContent/windows")
        .or_else(|| result.get("windows"))
        .and_then(Value::as_array)
    else {
        let t = content_text(result);
        return if t.is_empty() {
            "(no windows)".into()
        } else {
            t
        };
    };
    for w in list {
        let id = w.get("window_id").and_then(Value::as_i64).unwrap_or(0);
        let pid = w.get("pid").and_then(Value::as_i64).unwrap_or(0);
        let app = s(w, &["app_name", "app", "owner"]).unwrap_or("?");
        let title = s(w, &["title", "window_title"]).unwrap_or("");
        let wd = w
            .get("width")
            .and_then(Value::as_f64)
            .or_else(|| w.pointer("/bounds/width").and_then(Value::as_f64))
            .unwrap_or(0.0);
        let ht = w
            .get("height")
            .and_then(Value::as_f64)
            .or_else(|| w.pointer("/bounds/height").and_then(Value::as_f64))
            .unwrap_or(0.0);
        out.push_str(&format!(
            "{id}  {pid}  {app}  \"{title}\"  {wd:.0}x{ht:.0}\n"
        ));
    }
    if out.is_empty() {
        out.push_str("(no windows)");
    }
    out.trim_end_matches('\n').to_string()
}

/// One parsed AX element — fields are cua-driver's (verified 0.26.1);
/// `element_token` is the driver's preferred act-addressing token.
/// `frame` is the driver's reported `{x,y,w,h}` bounds (not rendered in
/// the observe text — it exists for coordinate acts and the live test).
#[derive(Debug, Default)]
pub struct Element {
    pub index: i64,
    pub token: Option<String>,
    pub role: String,
    pub label: String,
    pub value: Option<String>,
    pub actions: Vec<String>,
    pub depth: usize,
    pub parent: Option<i64>,
    /// (x, y, w, h) window-local bounds when AT-SPI reports usable ones.
    /// Only test code reads it today (the live coordinate proof).
    #[cfg(test)]
    pub frame: Option<(f64, f64, f64, f64)>,
}

/// Parse `structuredContent.elements[]` defensively: missing fields become
/// defaults and unknown keys are ignored — the driver may add fields.
pub fn parse_elements(result: &Value) -> Vec<Element> {
    result
        .pointer("/structuredContent/elements")
        .or_else(|| result.get("elements"))
        .and_then(Value::as_array)
        .map(|els| {
            els.iter()
                .map(|e| Element {
                    index: e
                        .get("element_index")
                        .and_then(Value::as_i64)
                        .or_else(|| e.get("index").and_then(Value::as_i64))
                        .unwrap_or(-1),
                    token: s(e, &["element_token", "token"]).map(str::to_string),
                    role: s(e, &["role"]).unwrap_or("").to_string(),
                    label: s(e, &["label", "name"]).unwrap_or("").to_string(),
                    value: e.get("value").and_then(|v| match v {
                        Value::Null => None,
                        Value::String(t) if t.is_empty() => None,
                        Value::String(t) => Some(t.clone()),
                        other => Some(other.to_string()),
                    }),
                    actions: e
                        .get("actions")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                    depth: e.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize,
                    parent: e
                        .get("parent_index")
                        .or_else(|| e.get("parent"))
                        .and_then(Value::as_i64),
                    #[cfg(test)]
                    frame: e.get("frame").and_then(|f| {
                        Some((
                            f.get("x").and_then(Value::as_f64)?,
                            f.get("y").and_then(Value::as_f64)?,
                            f.get("w")
                                .or_else(|| f.get("width"))
                                .and_then(Value::as_f64)?,
                            f.get("h")
                                .or_else(|| f.get("height"))
                                .and_then(Value::as_f64)?,
                        ))
                    }),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `observe` render (D4): `[idx] role "label" = value  {actions}`, two
/// spaces per depth level, capped at `limit` rendered lines. Unlabeled,
/// action-less containers are dropped unless a kept element sits below
/// them — they only matter as ancestry. Ends with the truncation line
/// when the tree is bigger than what is shown.
pub fn elements(result: &Value, limit: usize) -> String {
    let els = parse_elements(result);
    if els.is_empty() {
        return "(no elements)".to_string();
    }
    // keep = carries content, or an element below it does (parents of kept
    // elements stay so the indentation still reads as a tree).
    let mut kept: Vec<bool> = els
        .iter()
        .map(|e| !e.label.is_empty() || !e.actions.is_empty())
        .collect();
    let pos: HashMap<i64, usize> = els.iter().enumerate().map(|(i, e)| (e.index, i)).collect();
    for i in (0..els.len()).rev() {
        if !kept[i] {
            continue;
        }
        let mut p = els[i].parent;
        // Walk the parent chain; the hop bound guards a malformed cycle.
        for _ in 0..els.len() {
            let Some(pi) = p else { break };
            let Some(&j) = pos.get(&pi) else { break };
            if kept[j] {
                break;
            }
            kept[j] = true;
            p = els[j].parent;
        }
    }
    let mut out = String::new();
    let mut shown = 0usize;
    let mut hidden_kept = 0usize;
    let kept_total = kept.iter().filter(|&&k| k).count();
    for (e, &keep) in els.iter().zip(kept.iter()) {
        if !keep {
            continue;
        }
        if shown >= limit {
            hidden_kept += 1;
            continue;
        }
        let indent = "  ".repeat(e.depth.min(20));
        let value = e
            .value
            .as_deref()
            .map(|v| format!(" = \"{v}\""))
            .unwrap_or_default();
        let actions = if e.actions.is_empty() {
            String::new()
        } else {
            format!("  {{{}}}", e.actions.join(", "))
        };
        out.push_str(&format!(
            "{indent}[{}] {} \"{}\"{value}{actions}\n",
            e.index, e.role, e.label
        ));
        shown += 1;
    }
    // How much the model is not seeing: our own line cap, plus whatever the
    // driver truncated server-side (`total_element_count` covers both).
    let mut more = hidden_kept;
    if let Some(total) = result
        .pointer("/structuredContent/total_element_count")
        .or_else(|| result.pointer("/structuredContent/element_count"))
        .and_then(Value::as_u64)
    {
        more = more.max((total as usize).saturating_sub(shown));
    }
    let _ = kept_total;
    if more > 0 {
        out.push_str(&format!("… {more} more — narrow with query\n"));
    }
    out.trim_end_matches('\n').to_string()
}

/// `verify` render (D3): the driver's `status` is `satisfied` /
/// `unsatisfied` / `unknown` — and unknown is NEVER success.
pub fn verify(result: &Value) -> (bool, String) {
    let status = result
        .pointer("/structuredContent/status")
        .or_else(|| result.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let satisfied = status == "satisfied";
    let mut out = format!("verify: {status}");
    if let Some(preds) = result
        .pointer("/structuredContent/predicates")
        .and_then(Value::as_array)
    {
        for p in preds {
            let idx = p.get("index").and_then(Value::as_u64).unwrap_or(0);
            let st = s(p, &["status"]).unwrap_or("unknown");
            let reason = s(p, &["unknown_reason"])
                .map(|r| format!(" ({r})"))
                .unwrap_or_default();
            out.push_str(&format!("\n  [{idx}] {st}{reason}"));
        }
    }
    if status == "unknown" {
        out.push_str(" — not satisfied: unknown is never success");
    }
    if out.len() == format!("verify: {status}").len() {
        let t = content_text(result);
        if !t.is_empty() {
            out.push_str(&format!(" — {t}"));
        }
    }
    (satisfied, out)
}

/// `browser` snapshot render (D4): `ref  role "name"  (value)` lines,
/// capped at `limit`. The semantic_v2 ref list's field names come from the
/// owner's driver dump — they are searched defensively and the pre-rendered
/// content text is the fallback.
pub fn browser_refs(result: &Value, limit: usize) -> String {
    let Some(sc) = result.get("structuredContent").or(Some(result)) else {
        return content_text(result);
    };
    let refs = ["refs", "elements", "nodes", "items", "matches"]
        .iter()
        .find_map(|k| sc.get(*k).and_then(Value::as_array));
    let Some(refs) = refs else {
        let t = content_text(result);
        return if t.is_empty() {
            "(empty snapshot)".into()
        } else {
            t
        };
    };
    let mut out = String::new();
    let mut shown = 0usize;
    for r in refs {
        if shown >= limit {
            break;
        }
        let Some(rf) = s(r, &["ref", "element_ref", "id"]) else {
            continue;
        };
        let role = s(r, &["role"]).unwrap_or("");
        let name = s(r, &["name", "label", "text"]).unwrap_or("");
        let value = r
            .get("value")
            .and_then(|v| match v {
                Value::Null => None,
                Value::String(t) if t.is_empty() => None,
                Value::String(t) => Some(t.clone()),
                other => Some(other.to_string()),
            })
            .map(|v| format!("  ({v})"))
            .unwrap_or_default();
        out.push_str(&format!("{rf}  {role} \"{name}\"{value}\n"));
        shown += 1;
    }
    if refs.len() > shown {
        out.push_str(&format!(
            "… {} more — narrow with query\n",
            refs.len() - shown
        ));
    }
    if out.is_empty() {
        out.push_str("(empty snapshot)");
    }
    out.trim_end_matches('\n').to_string()
}

/// Dig a `target_id`/`tab_id` pair out of a `get_browser_state` bind result:
/// the driver's exact nesting for it is only attested by the owner's dump,
/// so every object level is searched.
pub fn target_and_tab(v: &Value) -> Option<(String, String)> {
    fn find<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
        match v {
            Value::Object(m) => {
                if let Some(s) = m.get(key).and_then(Value::as_str) {
                    return Some(s);
                }
                m.values().find_map(|x| find(x, key))
            }
            Value::Array(a) => a.iter().find_map(|x| find(x, key)),
            _ => None,
        }
    }
    let sc = v.get("structuredContent").unwrap_or(v);
    let target = find(sc, "target_id")?;
    let tab = find(sc, "tab_id")?;
    Some((target.to_string(), tab.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apps_renders_running_only() {
        let r = json!({"structuredContent": {"apps": [
            {"pid": 11, "name": "TextEdit", "running": true, "active": true},
            {"pid": 12, "name": "kworker", "running": true, "active": false},
            {"pid": 13, "name": "NotRunning", "running": false}
        ]}});
        let out = apps(&r);
        assert_eq!(out, "11  TextEdit  [front]\n12  kworker");
        assert!(!out.contains("NotRunning"));
    }

    #[test]
    fn windows_renders_id_pid_app_title_size() {
        let r = json!({"structuredContent": {"windows": [
            {"window_id": 101, "pid": 11, "app_name": "TextEdit", "title": "Doc",
             "width": 800, "height": 600}
        ]}});
        assert_eq!(windows(&r), "101  11  TextEdit  \"Doc\"  800x600");
    }

    #[test]
    fn elements_render_drops_empty_containers_and_truncates() {
        let r = json!({"structuredContent": {
            "snapshot_id": "snap-1",
            "elements": [
                {"element_index": 0, "role": "AXWindow", "label": "", "actions": [], "depth": 0, "parent_index": null},
                {"element_index": 1, "role": "AXGroup", "label": "", "actions": [], "depth": 1, "parent_index": 0},
                {"element_index": 2, "role": "AXTextField", "label": "Name", "value": "hi", "actions": ["AXPress"], "depth": 2, "parent_index": 1},
                {"element_index": 3, "role": "AXStaticText", "label": "", "actions": [], "depth": 2, "parent_index": 1},
                {"element_index": 4, "role": "AXButton", "label": "Go", "actions": ["AXPress"], "depth": 2, "parent_index": 1}
            ],
            "element_count": 5,
            "total_element_count": 9
        }});
        let out = elements(&r, 150);
        assert_eq!(
            out,
            "[0] AXWindow \"\"\n  [1] AXGroup \"\"\n    [2] AXTextField \"Name\" = \"hi\"  {AXPress}\n    [4] AXButton \"Go\"  {AXPress}\n… 5 more — narrow with query"
        );
        // The cap applies to rendered lines.
        let out = elements(&r, 1);
        assert!(out.contains("[0] AXWindow"), "{out}");
        assert!(out.contains("more — narrow with query"), "{out}");
    }

    #[test]
    fn verify_unknown_is_not_satisfied() {
        let r = json!({"structuredContent": {"status": "unknown", "predicates": [
            {"index": 0, "status": "unknown", "observed_json": null, "unknown_reason": "untrusted_source"}
        ]}});
        let (sat, text) = verify(&r);
        assert!(!sat);
        assert!(text.contains("unknown"), "{text}");
        let (sat, _) = verify(&json!({"structuredContent": {"status": "satisfied"}}));
        assert!(sat);
    }

    #[test]
    fn browser_refs_render_and_cap() {
        let r = json!({"structuredContent": {"refs": [
            {"ref": "p1:0", "role": "link", "name": "Home"},
            {"ref": "p1:1", "role": "button", "name": "Go", "value": "go"}
        ]}});
        assert_eq!(
            browser_refs(&r, 150),
            "p1:0  link \"Home\"\np1:1  button \"Go\"  (go)"
        );
    }

    #[test]
    fn target_and_tab_finds_nested_ids() {
        let r = json!({"structuredContent": {"bound": {"target_id": "t-1", "tab_id": "tab-1"}}});
        assert_eq!(
            target_and_tab(&r),
            Some(("t-1".to_string(), "tab-1".to_string()))
        );
    }
}
