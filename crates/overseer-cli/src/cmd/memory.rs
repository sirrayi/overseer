//! `overseer memory where|search` — inspect the memory stores.

use crate::args::{self, Arg};
use crate::flags::{exec_from, EXEC_FLAGS};
use crate::session::memory_store_list;

/// `overseer memory where [FLAGS]` prints the resolved store paths;
/// `overseer memory search <query…> [FLAGS]` prints the ranked hits the
/// `memory` tool's `op=search` would return. Store resolution follows the
/// exec flags (`--cwd`, `--memory`, `--no-memory`, `--bare`).
pub(crate) fn cmd_memory(argv: &[String]) -> i32 {
    let parsed = match args::parse(argv, EXEC_FLAGS) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("overseer memory: {e}");
            return 2;
        }
    };
    let words: Vec<String> = parsed.positionals().iter().map(|w| w.to_string()).collect();
    let flag_args = parsed
        .args
        .into_iter()
        .filter(|a| matches!(a, Arg::Flag { .. }))
        .collect();
    let flags = match exec_from(flag_args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer memory: {e}");
            return 2;
        }
    };
    let cwd = flags
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| flags.cwd.clone());
    let stores = memory_store_list(&flags, &cwd);
    match words.first().map(String::as_str) {
        Some("where") if words.len() == 1 => {
            if stores.is_empty() {
                println!("memory: off");
            }
            for (scope, dir) in &stores {
                println!("{}: {}", scope.name(), dir.display());
            }
            0
        }
        Some("search") if words.len() > 1 => {
            let query = words[1..].join(" ");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let idx = overseer_core::memory::index::Index::build(&stores, now);
            match overseer_core::tools::memory_tool::search_text(&idx, &query, now) {
                Some(out) => {
                    print!("{out}");
                    0
                }
                None => {
                    println!("no notes match `{query}`");
                    1
                }
            }
        }
        _ => {
            eprintln!("overseer memory: expected `where` or `search <query>`");
            2
        }
    }
}
