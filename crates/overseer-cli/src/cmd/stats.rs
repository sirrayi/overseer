//! `overseer stats` — the ledger dashboard.

use std::path::PathBuf;

use crate::args;

/// `overseer stats <session-dir>` — the cache-hit-rate dashboard
/// (playbook P1.1: ≥90% in-session is the SEV target).
pub(crate) fn cmd_stats(argv: &[String]) -> i32 {
    let parsed = match args::parse(argv, &[]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("overseer stats: {e}");
            return 2;
        }
    };
    let Some(dir) = parsed.positionals().first().map(|d| d.to_string()) else {
        eprintln!("overseer stats: session dir required");
        return 2;
    };
    let records = overseer_core::ledger::Ledger::read_all(PathBuf::from(&dir).join("ledger.jsonl"));
    if records.is_empty() {
        eprintln!("overseer stats: no ledger records in {dir}");
        return 1;
    }
    let s = overseer_core::ledger::Ledger::summarize(&records);
    println!("session: {dir}");
    println!("calls:        {}", s.calls);
    println!(
        "input tokens: {} (cache-read {})",
        s.input_tokens, s.cache_read_tokens
    );
    println!("output tokens:{}", s.output_tokens);
    println!(
        "cache hit:    {:.1}%{}",
        s.cache_hit_rate * 100.0,
        if s.cache_hit_rate >= 0.9 {
            "  (≥90% SEV target met)"
        } else {
            ""
        }
    );
    println!("cost:         ${:.4}", s.total_cost_usd);
    println!("latency:      {}ms total", s.latency_ms);
    0
}
