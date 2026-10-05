//! `repo_map` + `symbol` tools (P3.7): the structural index is on-demand
//! (a tool call, not a resident prompt section — a rebuilt map every turn
//! would sit above the cache boundary and kill prefix stability).

use serde_json::{json, Value};

use super::{need_str, schema, ToolCtx, ToolOutput};
use crate::provider::ToolSpec;

pub fn spec_map() -> ToolSpec {
    ToolSpec {
        name: "repo_map".into(),
        description: concat!(
            "Ranked map of the repo's top symbols (~1K tokens): name, ",
            "definition site, and cross-file reference count. Use before ",
            "reading files to orient in an unfamiliar codebase."
        )
        .into(),
        input_schema: schema(json!({}), &[]),
    }
}

pub fn spec_symbol() -> ToolSpec {
    ToolSpec {
        name: "symbol".into(),
        description: concat!(
            "go_to_definition + find_references for a symbol name: every ",
            "definition site (path:line) and the files that reference it."
        )
        .into(),
        input_schema: schema(
            json!({
                "name": {
                    "type": "string",
                    "description": "Symbol name to look up (exact match)."
                }
            }),
            &["name"],
        ),
    }
}

pub fn run_map(_input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    ToolOutput::ok(crate::repomap::render_map(&ctx.cwd))
}

pub fn run_symbol(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let name = match need_str(input, "name") {
        Ok(n) => n,
        Err(e) => return e,
    };
    // B1-6 (llama-index parent-child): narrow-retrieve — the definition
    // plus a 5-line window per site, with a `read` offset/limit hint to
    // expand. Whole-file dumps are the token-wasteful failure this kills.
    let idx = crate::repomap::build(&ctx.cwd);
    let base = crate::repomap::lookup(&idx, &ctx.cwd, name);
    let Some(sym) = idx.symbols.get(name) else {
        return ToolOutput::ok(base);
    };
    let mut out = base;
    out.push_str("\nWindows:\n");
    for (path, line) in sym.defs.iter().take(5) {
        match crate::repomap::window(&ctx.cwd, path, *line, 2) {
            Some(w) => {
                let rel = path.strip_prefix(&ctx.cwd).unwrap_or(path);
                out.push_str(&format!("--- {}:{line}\n{w}\n", rel.display()));
            }
            None => continue,
        }
    }
    ToolOutput::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-sym-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx(dir: &std::path::Path) -> ToolCtx<'static> {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagent_seq: 0,
            checkpoint: None,
            sandbox: false,
            broker: None,
        }
    }

    #[test]
    fn symbol_returns_window_with_valid_read_hint() {
        // D4: the hint must be executable read syntax (path + offset/limit),
        // and the expansion path must work.
        let dir = tmpdir();
        std::fs::write(
            dir.join("a.rs"),
            "fn alpha() {\n    let x = 1;\n    let y = 2;\n    x + y\n}\n",
        )
        .unwrap();
        let mut c = ctx(&dir);
        let out = run_symbol(&serde_json::json!({"name": "alpha"}), &mut c);
        assert!(!out.is_error);
        assert!(out.text.contains("a.rs"), "got: {}", out.text);
        assert!(
            out.text.contains("with offset=") && out.text.contains("limit="),
            "valid read hint, got: {}",
            out.text
        );
        assert!(
            !out.text.contains("`read a.rs:"),
            "no colon-range syntax, got: {}",
            out.text
        );
        // Expansion path: parse offset/limit from the hint and read.
        let hint = out
            .text
            .lines()
            .find(|l| l.contains("with offset="))
            .expect("hint line")
            .to_string();
        let off: usize = hint
            .split("offset=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .expect("offset parses");
        let lim: usize = hint
            .split("limit=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.split(']').next())
            .and_then(|s| s.parse().ok())
            .expect("limit parses");
        let mut c2 = ctx(&dir);
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        let expanded = crate::tools::read::run(
            &serde_json::json!({"path": "a.rs", "offset": off, "limit": lim}),
            &mut c2,
            &mut reg,
        );
        assert!(
            !expanded.is_error,
            "expansion read works: {}",
            expanded.text
        );
        assert!(expanded.text.contains("alpha"));
    }
}
