//! glob tool — file-name discovery, count-capped. Returns names only
//! (SWE-agent's finding: file-name lists are what the model needs from
//! discovery; more context proved counterproductive).
//!
//! The B2 port adds `fd`'s two filters: `file_type` (file/dir/symlink —
//! discovery is not only about files) and `extension` (a comma list, the
//! `fd -e rs -e py` shape). Both are applied during the walk, so a filtered
//! search never burns the 500-path cap on results the caller will discard.

use serde_json::{json, Value};

use super::{need_str, resolve, schema, ToolCtx, ToolOutput};

const MAX_PATHS: usize = 500;

/// What kind of entry to return (fd's `--type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    File,
    Dir,
    Symlink,
}

impl EntryType {
    pub const ALL: [EntryType; 3] = [EntryType::File, EntryType::Dir, EntryType::Symlink];

    pub const fn as_str(self) -> &'static str {
        match self {
            EntryType::File => "file",
            EntryType::Dir => "dir",
            EntryType::Symlink => "symlink",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "file" | "f" => Ok(EntryType::File),
            "dir" | "directory" | "d" => Ok(EntryType::Dir),
            "symlink" | "link" | "l" => Ok(EntryType::Symlink),
            other => Err(format!(
                "Invalid file_type '{other}' — want one of: file, dir, symlink."
            )),
        }
    }
}

/// Normalize an extension filter (`rs`, `.rs`, `RS` → `rs`), dropping empty
/// entries so `-e rs,` is not an error.
pub fn parse_extensions(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Whether an entry's name satisfies the extension filter (empty = any).
pub fn extension_matches(path: &std::path::Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let ext = ext.to_ascii_lowercase();
            extensions.contains(&ext)
        }
        None => false,
    }
}

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "glob".into(),
        description: concat!(
            "Find files by glob pattern (e.g. '**/*.rs'). Returns paths only, ",
            "capped at 500; skips .gitignore'd files."
        )
        .into(),
        input_schema: schema(
            json!({
                "pattern": {"type": "string", "description": "Matched against paths relative to `path`."},
                "path": {"type": "string", "description": "Directory to search (default: working directory)."},
                "file_type": {
                    "type": "string",
                    "enum": ["file", "dir", "symlink"],
                    "description": "Default: file."
                },
                "extension": {
                    "type": "string",
                    "description": "Comma-separated, e.g. 'rs,py' or '.rs'."
                }
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
    let file_type = match input.get("file_type").and_then(Value::as_str) {
        Some(t) => match EntryType::parse(t) {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(e),
        },
        None => EntryType::File,
    };
    let extensions = input
        .get("extension")
        .and_then(Value::as_str)
        .map(parse_extensions)
        .unwrap_or_default();
    let base = input
        .get("path")
        .and_then(Value::as_str)
        .map(|p| resolve(ctx, p))
        .unwrap_or_else(|| ctx.cwd.clone());

    let glob = match globset::GlobBuilder::new(pattern).build() {
        Ok(g) => g.compile_matcher(),
        Err(e) => return ToolOutput::err(format!("Invalid glob '{pattern}': {e}")),
    };

    if !base.is_dir() {
        return ToolOutput::err(format!("{} is not a directory.", base.display()));
    }

    let mut found: Vec<String> = Vec::new();
    for entry in ignore::WalkBuilder::new(&base).hidden(true).build() {
        let Ok(entry) = entry else { continue };
        let Ok(kind) = entry.file_type().ok_or(()) else {
            continue;
        };
        let matches_type = match file_type {
            EntryType::File => kind.is_file(),
            EntryType::Dir => kind.is_dir(),
            EntryType::Symlink => kind.is_symlink(),
        };
        if !matches_type {
            continue;
        }
        if !extension_matches(entry.path(), &extensions) {
            continue;
        }
        let rel = entry.path().strip_prefix(&base).unwrap_or(entry.path());
        // The search root itself is not a result.
        if rel.as_os_str().is_empty() {
            continue;
        }
        if glob.is_match(rel) {
            found.push(entry.path().display().to_string());
            if found.len() >= MAX_PATHS {
                break;
            }
        }
    }
    found.sort();

    if found.is_empty() {
        let filter = if extensions.is_empty() {
            String::new()
        } else {
            format!(" with extension {}", extensions.join("|"))
        };
        ToolOutput::ok(format!(
            "No {} entries match '{pattern}'{filter} under {}.",
            file_type.as_str(),
            base.display()
        ))
    } else {
        let mut text = found.join("\n");
        if found.len() >= MAX_PATHS {
            text.push_str(&format!(
                "\n[cap reached: {MAX_PATHS} shown — narrow the pattern]"
            ));
        }
        ToolOutput::ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-glob-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx(dir: &Path) -> ToolCtx<'static> {
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

    fn tree(dir: &Path) {
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/lib.py"), "x = 1\n").unwrap();
        std::fs::write(dir.join("README.md"), "# hi\n").unwrap();
        std::fs::write(dir.join("src/nested/deep.RS"), "//\n").unwrap();
    }

    #[test]
    fn extension_filter_is_case_insensitive_and_accepts_dots() {
        let dir = tmpdir();
        tree(&dir);
        let mut c = ctx(&dir);
        let rs = run(&json!({"pattern": "**/*", "extension": "rs"}), &mut c);
        assert!(!rs.is_error, "{}", rs.text);
        assert!(rs.text.contains("main.rs"), "{}", rs.text);
        assert!(rs.text.contains("deep.RS"), "case-insensitive: {}", rs.text);
        assert!(!rs.text.contains("lib.py"), "{}", rs.text);
        // A leading dot and a multi-extension list both work.
        let both = run(&json!({"pattern": "**/*", "extension": ".py, .md"}), &mut c);
        assert!(both.text.contains("lib.py"));
        assert!(both.text.contains("README.md"));
        assert!(!both.text.contains("main.rs"));
        // No match → the message names the filter, not just the pattern.
        let none = run(&json!({"pattern": "**/*.rs", "extension": "cobol"}), &mut c);
        assert!(none.text.contains("No file entries match"), "{}", none.text);
        assert!(none.text.contains("cobol"), "{}", none.text);
    }

    #[test]
    fn file_type_selects_dirs_and_rejects_bad_values() {
        let dir = tmpdir();
        tree(&dir);
        let mut c = ctx(&dir);
        let dirs = run(&json!({"pattern": "**", "file_type": "dir"}), &mut c);
        assert!(!dirs.is_error, "{}", dirs.text);
        assert!(dirs.text.contains("src"), "{}", dirs.text);
        assert!(dirs.text.contains("nested"), "{}", dirs.text);
        assert!(
            !dirs.text.contains("main.rs"),
            "a dir search returns no files: {}",
            dirs.text
        );
        // Short forms and a case-insensitive spelling parse.
        assert_eq!(EntryType::parse("D").unwrap(), EntryType::Dir);
        assert_eq!(EntryType::parse(" f ").unwrap(), EntryType::File);
        assert_eq!(EntryType::parse("link").unwrap(), EntryType::Symlink);
        // An unknown type names the alternatives.
        let bad = run(&json!({"pattern": "**", "file_type": "socket"}), &mut c);
        assert!(bad.is_error);
        assert!(bad.text.contains("socket"), "{}", bad.text);
        assert!(bad.text.contains("symlink"), "{}", bad.text);
    }

    #[test]
    fn extension_helpers_are_pure() {
        assert_eq!(parse_extensions(" .RS , py ,, "), vec!["rs", "py"]);
        assert!(parse_extensions("").is_empty());
        assert!(
            extension_matches(Path::new("a/b.rs"), &[]),
            "no filter = any"
        );
        assert!(extension_matches(
            Path::new("a/b.RS"),
            &parse_extensions("rs")
        ));
        assert!(!extension_matches(
            Path::new("a/b"),
            &parse_extensions("rs")
        ));
        assert!(!extension_matches(
            Path::new("a/b.py"),
            &parse_extensions("rs")
        ));
    }
}
