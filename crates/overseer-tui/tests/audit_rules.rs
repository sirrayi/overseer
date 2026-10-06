//! Audit (test/audit-surfaces): persisted "always" rules. Copied from
//! `audit_surfaces.rs` (`tui-rules-newline-injection` only).

use std::path::PathBuf;
use std::sync::Arc;

use overseer_core::perm::{AskDecision, AskHandler, Gate, Policy, Preset, Verdict};
use serde_json::json;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "audit-tui-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// AllowAlways on a model-supplied multi-line command writes the raw
/// command into the line-oriented rules file, so each embedded line
/// becomes its own permanent allow rule.
#[test]
fn allow_always_cannot_plant_extra_rules_via_newlines() {
    let dir = tmp("rules");
    let rules = dir.join("rules");
    let mut p = Policy::preset(Preset::WorkspaceWrite, dir.clone());
    p.load_rules(rules.clone());
    p.ask_handler = Some(AskHandler(Arc::new(|_| AskDecision::AllowAlways)));
    let cmd = json!({"command": "git push origin main\nbash:npm publish"});
    assert_eq!(p.gate("bash", &cmd), Gate::Allow);

    // A fresh session: the user never approved a bare `npm publish`.
    let mut p2 = Policy::preset(Preset::WorkspaceWrite, dir.clone());
    p2.load_rules(rules.clone());
    let v = p2.check("bash", &json!({"command": "npm publish"}));
    let text = std::fs::read_to_string(&rules).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        matches!(v, Verdict::Ask { .. }),
        "`npm publish` became pre-approved: {v:?}; rules file = {text:?}"
    );
}
