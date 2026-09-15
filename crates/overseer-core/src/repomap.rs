//! Repo-map + symbol reads (playbook Ch.4 §5, P3.7).
//!
//! A ~1K-token structural index of the workspace: symbol definitions
//! extracted per-language, ranked by cross-file reference count
//! (PageRank-lite — files "vote" for the symbols they mention), rendered
//! under a byte budget so the model gets the map without reading files.
//!
//! v1 extractor is regex-based for rs/py/js/ts/tsx/go — deterministic,
//! dependency-free, and honest about its limits (no tree-sitter dep yet;
//! the index is rebuilt per call so invalidation can't go stale).
//!
//! Tools backed by this index:
//! - `repo_map`  → ranked `name — file:line (refs:N)` lines ≤4KB
//! - `symbol`    → go_to_definition: every `path:line` a name is defined
//!   at, plus the files that reference it

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Output budget ≈1K tokens.
pub const MAP_BUDGET: usize = 4_000;
const MAX_FILES: usize = 5_000;
const MAX_FILE_BYTES: u64 = 512 * 1024;

/// Extensions we can extract symbols from.
const EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "jsx", "ts", "tsx", "go", "c", "h", "cpp", "hpp", "java", "rb",
];

/// Dirs never indexed — dependency trees, build output, VCS.
const SKIP_DIRS: &[&str] = &[
    "target", "node_modules", ".git", ".hg", ".svn", "dist", "build", "__pycache__",
    ".overseer", "vendor", ".venv", "venv",
];

#[derive(Debug, Default, Clone)]
pub struct Symbol {
    /// Definition sites.
    pub defs: Vec<(PathBuf, usize)>,
    /// Files mentioning the name (the rank "votes").
    pub refs: std::collections::HashSet<PathBuf>,
}

pub struct Index {
    /// name → symbol info
    pub symbols: HashMap<String, Symbol>,
    pub files_indexed: usize,
}

/// Extract definition sites from one file's text by extension.
/// Returns (line_number, symbol_name) pairs. Cheap and approximate —
/// the ranking, not the parser, is the load-bearing part.
pub fn extract(path: &Path, text: &str) -> Vec<(usize, String)> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let l = i + 1;
        let t = line.trim_start();
        match ext {
            "rs" => {
                for kw in ["fn ", "struct ", "enum ", "trait ", "const ", "type ", "impl "] {
                    if let Some(rest) = t.strip_prefix(kw).or_else(|| {
                        t.strip_prefix("pub ").and_then(|r| r.strip_prefix(kw))
                    }) {
                        if let Some(name) = ident_head(rest) {
                            out.push((l, name));
                        }
                        break;
                    }
                }
            }
            "py" => {
                for kw in ["def ", "class "] {
                    if let Some(rest) = t.strip_prefix(kw) {
                        if let Some(name) = ident_head(rest) {
                            out.push((l, name));
                        }
                        break;
                    }
                }
            }
            "js" | "jsx" | "ts" | "tsx" => {
                for kw in ["function ", "class ", "interface ", "type "] {
                    if let Some(rest) = t
                        .strip_prefix(kw)
                        .or_else(|| t.strip_prefix("export ").and_then(|r| r.strip_prefix(kw)))
                        .or_else(|| {
                            t.strip_prefix("export default ")
                                .and_then(|r| r.strip_prefix(kw))
                        })
                    {
                        if let Some(name) = ident_head(rest) {
                            out.push((l, name));
                        }
                        break;
                    }
                }
                // `const foo =` arrow/function bindings
                for kw in ["const ", "let "] {
                    if let Some(rest) = t.strip_prefix(kw).or_else(|| {
                        t.strip_prefix("export ").and_then(|r| r.strip_prefix(kw))
                    }) {
                        if let Some((name, tail)) = rest.split_once('=') {
                            if let Some(name) = ident_head(name) {
                                if tail.contains("=>") || tail.contains("function") {
                                    out.push((l, name));
                                }
                            }
                        }
                        break;
                    }
                }
            }
            "go" => {
                if let Some(rest) = t.strip_prefix("func ") {
                    // `func (r *T) name(` — method names count too.
                    let rest = if rest.starts_with('(') {
                        rest.split_once(')').map(|(_, r)| r).unwrap_or(rest)
                    } else {
                        rest
                    };
                    if let Some(name) = ident_head(rest) {
                        out.push((l, name));
                    }
                }
                if let Some(rest) = t.strip_prefix("type ") {
                    if let Some(name) = ident_head(rest) {
                        out.push((l, name));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Leading identifier chars: `[A-Za-z_][A-Za-z0-9_]*`.
fn ident_head(s: &str) -> Option<String> {
    let name: String = s
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() || name.chars().next().unwrap().is_numeric() {
        None
    } else {
        Some(name)
    }
}

/// Walk the workspace and build the index. Bounded: ≤MAX_FILES files,
/// ≤512KB each, source extensions only, noisy dirs skipped.
pub fn build(root: &Path) -> Index {
    let mut idx = Index {
        symbols: HashMap::new(),
        files_indexed: 0,
    };
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        if files.len() >= MAX_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(p);
                }
            } else if EXTENSIONS
                .iter()
                .any(|x| p.extension().and_then(|e| e.to_str()) == Some(x))
                && p.metadata().map(|m| m.len()) .unwrap_or(0) <= MAX_FILE_BYTES
            {
                files.push(p);
            }
        }
    }
    files.sort(); // deterministic
    // One read pass: extract defs AND cache per-file identifier sets for
    // the reference vote (identifier-set membership is O(1) per symbol —
    // no rescanning the tree per symbol).
    let mut file_idents: Vec<(PathBuf, std::collections::HashSet<String>)> = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        idx.files_indexed += 1;
        for (line, name) in extract(&f, &text) {
            idx.symbols
                .entry(name)
                .or_default()
                .defs
                .push((f.clone(), line));
        }
        file_idents.push((f, tokenize_idents(&text)));
    }
    // Reference pass: every file whose identifier set contains the name
    // votes for it — ranking signal, not a read.
    for (f, idents) in &file_idents {
        for (name, sym) in idx.symbols.iter_mut() {
            if idents.contains(name) {
                sym.refs.insert(f.clone());
            }
        }
    }
    idx
}

/// All identifiers in a file's text — the per-file "vocabulary" the
/// reference vote checks against.
fn tokenize_idents(text: &str) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            if cur.len() > 1 {
                set.insert(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if cur.len() > 1 {
        set.insert(cur);
    }
    set
}

/// The ~1K-token map: symbols ranked by refs×3+defs, one line each,
/// byte-capped with a truncation marker.
pub fn render_map(root: &Path) -> String {
    let idx = build(root);
    if idx.symbols.is_empty() {
        return "(no symbols found — is this a source tree?)".into();
    }
    let mut ranked: Vec<(&String, &Symbol)> = idx.symbols.iter().collect();
    ranked.sort_by(|(an, a), (bn, b)| {
        (b.refs.len() * 3 + b.defs.len())
            .cmp(&(a.refs.len() * 3 + a.defs.len()))
            .then(an.cmp(bn)) // deterministic tiebreak
    });
    let mut out = format!("## Repo map — {} files indexed\n", idx.files_indexed);
    let mut used = out.len();
    for (name, sym) in ranked {
        let (p, l) = &sym.defs[0];
        let rel = p.strip_prefix(root).unwrap_or(p);
        let line = format!("{} — {}:{l} (refs:{})", name, rel.display(), sym.refs.len());
        if used + line.len() + 1 > MAP_BUDGET {
            out.push_str("[...map budget exhausted — use `symbol` for detail...]\n");
            break;
        }
        out.push_str(&line);
        out.push('\n');
        used += line.len() + 1;
    }
    out
}

/// go_to_definition + reference listing for one symbol.
pub fn lookup(root: &Path, name: &str) -> String {
    let idx = build(root);
    match idx.symbols.get(name) {
        None => format!("no symbol '{name}' in {} indexed files", idx.files_indexed),
        Some(s) => {
            let mut out = format!("## {name}\nDefinitions:\n");
            for (p, l) in &s.defs {
                let rel = p.strip_prefix(root).unwrap_or(p);
                out.push_str(&format!("  {}:{l}\n", rel.display()));
            }
            let mut refs: Vec<_> = s.refs.iter().collect();
            refs.sort();
            out.push_str(&format!("Referenced by {} file(s):\n", refs.len()));
            for p in refs.iter().take(30) {
                let rel = p.strip_prefix(root).unwrap_or(p);
                out.push_str(&format!("  {}\n", rel.display()));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-map-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn extracts_rust_symbols() {
        let dir = tmpdir();
        std::fs::write(
            dir.join("lib.rs"),
            "pub fn helper() {}\nstruct Foo {}\nfn main() {}\n",
        )
        .unwrap();
        let idx = build(&dir);
        assert!(idx.symbols.contains_key("helper"));
        assert!(idx.symbols.contains_key("Foo"));
        assert_eq!(idx.symbols["main"].defs[0].1, 3);
    }

    #[test]
    fn extracts_py_and_go() {
        let dir = tmpdir();
        std::fs::write(dir.join("a.py"), "def run():\n    pass\nclass C:\n    pass\n").unwrap();
        std::fs::write(dir.join("b.go"), "func Run() {}\ntype T struct{}\n").unwrap();
        let idx = build(&dir);
        assert!(idx.symbols.contains_key("run"));
        assert!(idx.symbols.contains_key("C"));
        assert!(idx.symbols.contains_key("Run"));
        assert!(idx.symbols.contains_key("T"));
    }

    #[test]
    fn ranking_prefers_referenced_symbols() {
        let dir = tmpdir();
        std::fs::write(dir.join("core.rs"), "fn hot() {}\nfn cold() {}\n").unwrap();
        std::fs::write(dir.join("u1.rs"), "// calls hot()\n").unwrap();
        std::fs::write(dir.join("u2.rs"), "// also hot()\n").unwrap();
        let map = render_map(&dir);
        let hot_pos = map.find("hot —").unwrap();
        let cold_pos = map.find("cold —").unwrap();
        assert!(hot_pos < cold_pos, "referenced symbol ranks first");
    }

    #[test]
    fn lookup_gives_defs_and_refs() {
        let dir = tmpdir();
        std::fs::write(dir.join("a.rs"), "fn thing() {}\n").unwrap();
        std::fs::write(dir.join("b.rs"), "// uses thing\n").unwrap();
        let out = lookup(&dir, "thing");
        assert!(out.contains("a.rs:1"));
        assert!(out.contains("b.rs"));
        assert!(lookup(&dir, "nope").contains("no symbol"));
    }

    #[test]
    fn skips_target_and_hidden_dirs() {
        let dir = tmpdir();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/x.rs"), "fn junk() {}\n").unwrap();
        std::fs::write(dir.join("real.rs"), "fn real() {}\n").unwrap();
        let idx = build(&dir);
        assert!(!idx.symbols.contains_key("junk"));
        assert!(idx.symbols.contains_key("real"));
    }
}
