//! grep tool — ranked, count-capped content search. Respects .gitignore via
//! the `ignore` walker. SWE-agent's lesson holds: matches designed for the
//! model — file:line:content, capped, no noise.

use serde_json::{json, Value};

use super::{need_str, resolve, schema, ToolCtx, ToolOutput};

const MAX_MATCHES: usize = 200;
const MAX_LINE: usize = 500;

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "grep".into(),
        description: concat!(
            "Search file contents for a regex pattern. Returns file:line:match, ",
            "capped at 200 matches. Use `glob` when you only need file names. ",
            "Skips .gitignore'd files and binary files."
        )
        .into(),
        input_schema: schema(
            json!({
                "pattern": {"type": "string", "description": "Regex pattern to search for."},
                "path": {"type": "string", "description": "File or directory to search (default: working directory)."},
                "glob": {"type": "string", "description": "Optional glob filter on file names, e.g. '*.rs'."},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive match (default false)."}
            }),
            &["pattern"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let pattern = match need_str(input, "pattern") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let ignore_case = input
        .get("ignore_case")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let base = input
        .get("path")
        .and_then(Value::as_str)
        .map(|p| resolve(ctx, p))
        .unwrap_or_else(|| ctx.cwd.clone());

    let re = match regex_lite::RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            return ToolOutput::err(format!(
                "Invalid regex '{pattern}': {e}. Use a simpler pattern or escape metacharacters."
            ))
        }
    };

    let glob_filter = input
        .get("glob")
        .and_then(Value::as_str)
        .and_then(|g| globset::GlobBuilder::new(g).build().ok())
        .map(|g| g.compile_matcher());

    let mut matches: Vec<String> = Vec::new();
    let mut files_seen = 0usize;

    let mut search_file = |path: &std::path::Path, matches: &mut Vec<String>| {
        if let Some(gf) = &glob_filter {
            if let Some(name) = path.file_name() {
                if !gf.is_match(name) {
                    return;
                }
            }
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return; // binary/unreadable files skipped
        };
        files_seen += 1;
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                let l = if line.len() > MAX_LINE {
                    &line[..MAX_LINE]
                } else {
                    line
                };
                matches.push(format!("{}:{}:{}", path.display(), i + 1, l.trim_end()));
                if matches.len() >= MAX_MATCHES {
                    return;
                }
            }
        }
    };

    if base.is_file() {
        search_file(&base, &mut matches);
    } else if base.is_dir() {
        for entry in ignore::WalkBuilder::new(&base).hidden(true).build() {
            if matches.len() >= MAX_MATCHES {
                break;
            }
            let Ok(entry) = entry else { continue };
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                search_file(entry.path(), &mut matches);
            }
        }
    } else {
        return ToolOutput::err(format!(
            "{} does not exist or is not searchable.",
            base.display()
        ));
    }

    if matches.is_empty() {
        ToolOutput::ok(format!(
            "No matches for '{pattern}' (searched {files_seen} files)."
        ))
    } else {
        let mut text = matches.join("\n");
        if matches.len() >= MAX_MATCHES {
            text.push_str(&format!(
                "\n[match cap reached: {MAX_MATCHES} shown — narrow with `glob` or `path`]"
            ));
        }
        ToolOutput::ok(text)
    }
}
