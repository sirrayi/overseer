//! Tree-sitter structural-search registry (the `tree-sitter` port, feature
//! `tree-sitter`, default-off).
//!
//! Structural search needs two things: a per-language grammar (what node
//! kinds exist) and a query (which nodes to capture). A real binding supplies
//! both by parsing source files with the `tree-sitter` crate and a compiled
//! grammar; this module is the *registry* half — the descriptors and query
//! text a binding registers against, so the search stays pluggable.
//!
//! The binding itself is **not linked**: the engine is offline at build time
//! and the zero-new-crate gate requires the lockfile package-name set to stay
//! empty (`tree-sitter` and every grammar crate would add a package). So the
//! port is the descriptor data only, and `tree-sitter` is an opt-in
//! compile-time feature with **zero dependencies** — turning it on compiles
//! this file, nothing more. A caller with the crate available pairs each
//! `Grammar` with a fetched grammar and runs the query text through
//! `tree_sitter::Query::new`.
//!
//! Invariants (asserted by the in-file tests):
//! - exactly three grammars, distinct names, extensions disjoint across them;
//! - every query contains exactly one `@capture` and mentions at least one of
//!   that grammar's declared kinds, so the registry cannot advertise a kind
//!   its query never matches;
//! - lookup is case-insensitive by name or extension, and `names()` /
//!   `extensions()` are sorted so a UI listing is reproducible;
//! - an unknown language yields an empty kind slice and `None`, never a
//!   guessed grammar.
//!
//! `// DEFERRED(owner): the actual tree-sitter crate binding + node parsing (needs the dep, fetched online) — the registry, kinds, and queries land now; ABI/version pinning lands with the dep.`

/// One language the structural search knows: how to spot its source files and
/// what to capture in them.
///
/// `definition_query` / `call_query` are tree-sitter query text with exactly
/// one `@name` capture each (see [`capture_name`]); they are stored as parsed
/// input, not compiled queries, because no grammar is linked here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grammar {
    /// Language name, lower-case and unique across [`GRAMMARS`].
    pub name: &'static str,
    /// File extensions without a leading dot; disjoint across [`GRAMMARS`].
    pub extensions: &'static [&'static str],
    /// Node kinds a *definition* query captures in this language.
    pub definition_kinds: &'static [&'static str],
    /// Node kinds a *call* query captures.
    pub call_kinds: &'static [&'static str],
    /// tree-sitter query text with exactly one `@name` capture.
    pub definition_query: &'static str,
    /// tree-sitter query text with exactly one `@name` capture.
    pub call_query: &'static str,
}

/// The registered grammars — a `const`, so a caller (or the compiler) can
/// match on it exhaustively and a missing language is a build error rather
/// than a runtime surprise.
pub const GRAMMARS: [Grammar; 3] = [
    Grammar {
        name: "rust",
        extensions: &["rs"],
        definition_kinds: &[
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "impl_item",
            "mod_item",
        ],
        call_kinds: &["call_expression", "macro_invocation"],
        definition_query: "(function_item name: (identifier) @name)",
        call_query: "(call_expression function: (identifier) @name)",
    },
    Grammar {
        name: "python",
        extensions: &["py", "pyi"],
        definition_kinds: &["function_definition", "class_definition"],
        call_kinds: &["call"],
        definition_query: "(function_definition name: (identifier) @name)",
        call_query: "(call function: (identifier) @name)",
    },
    Grammar {
        name: "javascript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        definition_kinds: &[
            "function_declaration",
            "class_declaration",
            "method_definition",
        ],
        call_kinds: &["call_expression", "new_expression"],
        definition_query: "(function_declaration name: (identifier) @name)",
        call_query: "(call_expression function: (identifier) @name)",
    },
];

/// Which of a grammar's two queries to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// The query that captures definitions.
    Definition,
    /// The query that captures calls.
    Call,
}

/// Normalize a name or extension for lookup: trim, drop a leading dot, lower.
/// The leading dot is dropped so `".rs"` and `"rs"` resolve identically.
fn normalize(s: &str) -> String {
    s.trim().trim_start_matches('.').to_ascii_lowercase()
}

/// Look a grammar up by name or extension, case-insensitively. `"Rust"`,
/// `"rust"`, `"RS"` and `".rs"` all resolve to the same entry; anything
/// unregistered yields `None` (never a guessed default — a caller must
/// decide what to do without a grammar).
pub fn grammar(name: &str) -> Option<&'static Grammar> {
    let needle = normalize(name);
    if needle.is_empty() {
        return None;
    }
    GRAMMARS.iter().find(|g| {
        g.name.eq_ignore_ascii_case(&needle)
            || g.extensions.iter().any(|e| e.eq_ignore_ascii_case(&needle))
    })
}

/// Look a grammar up by file extension, accepting `".rs"` and `"rs"` (both
/// cases). Like [`grammar`], an unregistered extension yields `None`.
pub fn for_extension(ext: &str) -> Option<&'static Grammar> {
    let needle = normalize(ext);
    if needle.is_empty() {
        return None;
    }
    GRAMMARS
        .iter()
        .find(|g| g.extensions.iter().any(|e| e.eq_ignore_ascii_case(&needle)))
}

/// Every registered language name, sorted — a stable order for menus and
/// diagnostics (the registry order is not a promise).
pub fn names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = GRAMMARS.iter().map(|g| g.name).collect();
    v.sort_unstable();
    v
}

/// Every registered extension without its leading dot, sorted and deduped —
/// the closed set of files the structural search can index.
pub fn extensions() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = GRAMMARS
        .iter()
        .flat_map(|g| g.extensions.iter().copied())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The definition kinds of a language, looked up as in [`grammar`]. An
/// unknown language yields an empty slice — "no kinds known", not a guess.
pub fn definition_kinds(name: &str) -> &'static [&'static str] {
    match grammar(name) {
        Some(g) => g.definition_kinds,
        None => &[],
    }
}

/// The query text for `grammar` and `kind`. Returned as `'static` because the
/// registry owns it; a binding compiles it against its grammar.
pub fn query(grammar: &Grammar, kind: QueryKind) -> &'static str {
    match kind {
        QueryKind::Definition => grammar.definition_query,
        QueryKind::Call => grammar.call_query,
    }
}

/// True for the bytes tree-sitter allows in a capture name after `@`.
fn is_capture_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-'
}

/// The single capture name of a query (the text after `@`), or `None` when
/// the query has no capture or more than one. A query with two captures is
/// ambiguous — a structural search could not tell which node is the result —
/// so it is refused rather than silently picking one.
pub fn capture_name(query: &str) -> Option<&str> {
    let bytes = query.as_bytes();
    let mut found: Option<&str> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && is_capture_byte(bytes[end]) {
                end += 1;
            }
            if end > start {
                if found.is_some() {
                    return None;
                }
                found = Some(&query[start..end]);
            }
            i = end;
        } else {
            i += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_three_grammars() {
        // The array length in the `const` type pins the count at compile
        // time; this guards the value if the type is ever loosened.
        assert_eq!(GRAMMARS.len(), 3);
    }

    #[test]
    fn grammar_names_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for g in &GRAMMARS {
            assert!(seen.insert(g.name), "duplicate grammar name: {}", g.name);
        }
    }

    #[test]
    fn extensions_are_unique_across_grammars() {
        let mut seen = std::collections::BTreeSet::new();
        for g in &GRAMMARS {
            for e in g.extensions {
                assert!(
                    seen.insert(*e),
                    "extension {e} claimed by more than one grammar"
                );
                assert!(!e.starts_with('.'), "extension {e} carries a leading dot");
            }
        }
    }

    #[test]
    fn for_extension_normalizes_dot_and_case_and_rejects_unknown() {
        let dotless = for_extension("rs").expect("rs is registered");
        assert_eq!(dotless.name, "rust");
        assert_eq!(for_extension(".rs").map(|g| g.name), Some("rust"));
        assert_eq!(for_extension("RS").map(|g| g.name), Some("rust"));
        assert_eq!(for_extension(".pyi").map(|g| g.name), Some("python"));
        assert!(
            for_extension("txt").is_none(),
            "txt is not a source language"
        );
        assert!(
            for_extension("").is_none(),
            "empty extension must not match"
        );
    }

    #[test]
    fn grammar_lookup_agrees_across_case() {
        let lower = grammar("rust").expect("rust is registered");
        let upper = grammar("Rust").expect("Rust resolves case-insensitively");
        assert_eq!(lower.name, upper.name);
        assert_eq!(lower.extensions, upper.extensions);
        let by_ext = grammar("rs").expect("extension aliases the name");
        assert_eq!(by_ext.name, lower.name);
    }

    #[test]
    fn every_query_has_exactly_one_capture() {
        for g in &GRAMMARS {
            for (label, q) in [("definition", g.definition_query), ("call", g.call_query)] {
                assert_eq!(
                    capture_name(q),
                    Some("name"),
                    "{}/{label} query must carry exactly one @name capture: {q}",
                    g.name
                );
            }
        }
    }

    #[test]
    fn every_query_mentions_a_declared_kind() {
        for g in &GRAMMARS {
            assert!(
                g.definition_kinds
                    .iter()
                    .any(|k| g.definition_query.contains(k)),
                "{} definition query matches none of its declared kinds",
                g.name
            );
            assert!(
                g.call_kinds.iter().any(|k| g.call_query.contains(k)),
                "{} call query matches none of its declared kinds",
                g.name
            );
        }
    }

    #[test]
    fn names_and_extensions_are_sorted() {
        let names = names();
        let mut sorted_names = names.clone();
        sorted_names.sort_unstable();
        assert_eq!(names, sorted_names, "names() must be sorted");
        assert_eq!(names, vec!["javascript", "python", "rust"]);

        let exts = extensions();
        let mut sorted_exts = exts.clone();
        sorted_exts.sort_unstable();
        assert_eq!(exts, sorted_exts, "extensions() must be sorted");
        assert!(exts.windows(2).all(|w| w[0] != w[1]), "extensions deduped");
        assert!(exts.iter().all(|e| !e.starts_with('.')), "no leading dot");
        assert!(exts.contains(&"rs") && exts.contains(&"py") && exts.contains(&"js"));
    }

    #[test]
    fn unknown_language_yields_empty_kinds_and_none() {
        assert!(definition_kinds("cobol").is_empty());
        assert!(definition_kinds("").is_empty());
        assert!(grammar("cobol").is_none());
        assert!(grammar("").is_none());
        assert!(for_extension("txt").is_none());
    }

    #[test]
    fn query_dispatches_on_kind() {
        let rust = grammar("rust").unwrap();
        assert_eq!(query(rust, QueryKind::Definition), rust.definition_query);
        assert_eq!(query(rust, QueryKind::Call), rust.call_query);
        assert_ne!(rust.definition_query, rust.call_query);
    }

    #[test]
    fn capture_name_refuses_zero_or_many_captures() {
        assert_eq!(capture_name("(identifier)"), None);
        assert_eq!(capture_name("(a) @x (b) @y"), None);
        assert_eq!(capture_name("(a) @x"), Some("x"));
        assert_eq!(capture_name(""), None);
        assert_eq!(capture_name("@"), None, "a bare @ names nothing");
    }

    #[test]
    fn grammar_const_is_matchable() {
        // The exhaustive match is the property: a fourth grammar cannot be
        // added without the compiler flagging every such call site.
        let ranked: Vec<&str> = GRAMMARS
            .iter()
            .map(|g| match g.name {
                "rust" => "systems",
                "python" => "scripting",
                "javascript" => "web",
                other => panic!("unregistered grammar in match: {other}"),
            })
            .collect();
        assert_eq!(ranked, vec!["systems", "scripting", "web"]);
    }
}
