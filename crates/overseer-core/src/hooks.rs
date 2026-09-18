//! Deterministic tool hooks (ECC hook pattern, arsenal B2).
//!
//! ECC runs hooks as *scripts* (a shell command per event, exit code 2 =
//! block). That shape buys arbitrary behavior at the cost of a second,
//! unaudited execution surface. This port keeps the contract — a hook may
//! block a tool call at the dispatch boundary and may annotate a result —
//! and drops the surface: hooks here are **data**, evaluated as pure
//! predicates over `(event, tool, payload)`.
//!
//! Rules live in `<root>/.overseer/hooks.json`:
//!
//! ```json
//! [{"event":"pre_tool_use","tool":"bash","contains":"git push","reason":"…"}]
//! ```
//!
//! - `event`  — `pre_tool_use` (may block) or `post_tool_use` (annotates).
//! - `tool`   — optional; absent matches any tool.
//! - `contains` — case-insensitive substring of the serialized payload
//!   (the tool input for pre, the result text for post).
//! - `reason` — shown to the model on a block; prefixed onto the result on
//!   an annotation.
//!
//! Evaluation order is file order; the first pre-rule that hits blocks.
//! A hook never *allows* anything: the L4 gate downstream still has to
//! agree, so a permissive hooks file can't widen the trust boundary.
//! `// DEFERRED(owner): hook scripts (ECC's exit-code form) — data rules
//! cover the block/annotate contract; revisit only if a rule needs a
//! subprocess, which would re-open the RCE surface this port removes.`

use std::path::Path;

use serde_json::Value;

/// Which boundary a rule observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    /// Before dispatch: a hit blocks the call (it never runs).
    PreToolUse,
    /// After dispatch: a hit annotates the result text.
    PostToolUse,
}

impl HookKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            HookKind::PreToolUse => "pre_tool_use",
            HookKind::PostToolUse => "post_tool_use",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pre_tool_use" => Ok(HookKind::PreToolUse),
            "post_tool_use" => Ok(HookKind::PostToolUse),
            other => Err(format!(
                "hooks: bad event `{other}` — want pre_tool_use|post_tool_use"
            )),
        }
    }
}

/// One rule from the hooks file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRule {
    pub kind: HookKind,
    /// `None` matches every tool.
    pub tool: Option<String>,
    pub contains: String,
    pub reason: String,
}

/// File the registry loads rules from, relative to the policy root.
pub const HOOKS_FILE: &str = ".overseer/hooks.json";

/// Load rules from `<root>/.overseer/hooks.json`. Missing or unreadable
/// file → no rules (fail-open: hooks are an extra guardrail, not the gate).
/// A malformed file also yields no rules — a broken hooks file must not
/// brick the toolset; the parse error is available via `parse_rules` for
/// callers that want to report it.
pub fn load(root: &Path) -> Vec<HookRule> {
    let path = root.join(HOOKS_FILE);
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_rules(&text).unwrap_or_default()
}

/// Parse the hooks-file grammar. Errors name the offending rule index.
pub fn parse_rules(text: &str) -> Result<Vec<HookRule>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("hooks: invalid JSON — {e}"))?;
    let arr = v
        .as_array()
        .ok_or_else(|| "hooks: top level must be an array of rules".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("hooks: rule {i} must be an object"))?;
        let event = obj
            .get("event")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("hooks: rule {i} is missing `event`"))?;
        let kind = HookKind::parse(event).map_err(|e| format!("{e} (rule {i})"))?;
        let contains = obj
            .get("contains")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("hooks: rule {i} needs a non-empty `contains`"))?
            .to_string();
        let tool = obj
            .get("tool")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        let reason = obj
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("blocked by hooks rule")
            .to_string();
        out.push(HookRule {
            kind,
            tool,
            contains,
            reason,
        });
    }
    Ok(out)
}

/// First rule of `kind` that matches. Matching is case-insensitive
/// substring over `payload` plus an exact tool-name filter.
fn hit<'a>(
    rules: &'a [HookRule],
    kind: HookKind,
    tool: &str,
    payload: &str,
) -> Option<&'a HookRule> {
    let payload = payload.to_lowercase();
    rules.iter().find(|r| {
        r.kind == kind
            && r.tool.as_deref().is_none_or(|t| t == tool)
            && payload.contains(&r.contains.to_lowercase())
    })
}

/// Pre-dispatch verdict: `Some(reason)` blocks the call. Callers pass the
/// serialized tool input as the payload.
pub fn maybe_block(rules: &[HookRule], tool: &str, input: &Value) -> Option<String> {
    hit(rules, HookKind::PreToolUse, tool, &input.to_string()).map(|r| r.reason.clone())
}

/// Post-dispatch annotation: `Some(reason)` is appended to the result text.
pub fn post_notice(rules: &[HookRule], tool: &str, result: &str) -> Option<String> {
    hit(rules, HookKind::PostToolUse, tool, result).map(|r| r.reason.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_names_the_faulty_rule() {
        // Valid rule parses; each malformed shape names its index/field.
        let ok = r#"[{"event":"pre_tool_use","contains":"drop table","reason":"nope"}]"#;
        let rules = parse_rules(ok).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].kind, HookKind::PreToolUse);
        assert_eq!(rules[0].tool, None);

        assert!(parse_rules("{}").unwrap_err().contains("array"));
        assert!(parse_rules(r#"[{"contains":"x"}]"#)
            .unwrap_err()
            .contains("rule 0 is missing `event`"));
        assert!(parse_rules(r#"[{"event":"nope","contains":"x"}]"#)
            .unwrap_err()
            .contains("bad event"));
        assert!(parse_rules(r#"[{"event":"pre_tool_use","contains":""}]"#)
            .unwrap_err()
            .contains("non-empty `contains`"));
    }

    #[test]
    fn pre_rule_blocks_only_its_tool_and_payload() {
        let rules = parse_rules(
            r#"[{"event":"pre_tool_use","tool":"bash","contains":"git push --force","reason":"force push"}]"#,
        )
        .unwrap();
        assert_eq!(
            maybe_block(
                &rules,
                "bash",
                &json!({"command": "git push --force origin"})
            )
            .as_deref(),
            Some("force push")
        );
        // Case-insensitive payload match, but the tool filter still applies.
        assert!(
            maybe_block(&rules, "bash", &json!({"command": "GIT PUSH --FORCE"})).is_some(),
            "payload match is case-insensitive"
        );
        assert!(maybe_block(&rules, "write", &json!({"content": "git push --force"})).is_none());
        assert!(maybe_block(&rules, "bash", &json!({"command": "git push origin"})).is_none());
    }

    #[test]
    fn post_rule_annotates_without_blocking() {
        let rules = parse_rules(
            r#"[{"event":"post_tool_use","contains":"panic","reason":"output mentions a panic"}]"#,
        )
        .unwrap();
        assert!(maybe_block(&rules, "bash", &json!({"command": "x"})).is_none());
        assert_eq!(
            post_notice(&rules, "bash", "thread 'main' panicked").as_deref(),
            Some("output mentions a panic")
        );
        assert!(post_notice(&rules, "bash", "all good").is_none());
    }

    #[test]
    fn missing_or_broken_file_yields_no_rules() {
        let dir = std::env::temp_dir().join(format!("overseer-hooks-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir).is_empty(), "no file → no rules");
        std::fs::create_dir_all(dir.join(".overseer")).unwrap();
        std::fs::write(dir.join(HOOKS_FILE), "{not json").unwrap();
        assert!(load(&dir).is_empty(), "broken file → no rules (fail-open)");
    }
}
