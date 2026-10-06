//! `overseer-life` probe: `cargo run -p overseer-life --example probe --
//! <source|all> [--json] [--live]`
//!
//! Without --live: credential discovery only (found/where/kind — never
//! secret values). With --live: one bounded request per source to its
//! usage endpoint, wrapped in a per-source request budget of 1. Keychain
//! secret reads are OFF in the probe (R3); file-based credentials only.

use overseer_life::connector::Ctx;
use overseer_life::http::Http;
use overseer_life::registry;
use overseer_life::snapshot::Snapshot;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let live = args.iter().any(|a| a == "--live");
    let source = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
        .unwrap_or("all");

    let connectors = registry();
    let mut results = Vec::new();
    for connector in &connectors {
        let id = connector.id().0.clone();
        if source != "all" && id != source {
            continue;
        }
        let mut http = Http::new();
        // R5: at most one request per source per run. The budget is
        // enforced at the transport layer, so a connector that tried a
        // second call (e.g. Cursor's best-effort credits) simply gets
        // "budget exhausted" instead of a second request.
        http.max_requests = Some(1);
        let ctx = Ctx::system(http, /* allow_keychain_secrets */ false);
        if live {
            let snap = connector.fetch(&ctx);
            results.push(serde_json::to_value(&snap).expect("snapshot serializes"));
            if !json {
                print_snapshot(&snap);
            }
        } else {
            let found = connector.discover(&ctx);
            let val = serde_json::json!({
                "source": id,
                "mode": "discovery",
                "credentials": found,
            });
            results.push(val);
            if !json {
                println!("{id}:");
                for d in &found {
                    println!(
                        "  {} {} — {}",
                        if d.present { "found" } else { "absent" },
                        d.location,
                        d.contains
                    );
                }
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&results).unwrap());
    }
    // --json-out writes redacted JSON next to stdout for artifact capture.
    if let Some(path) = args
        .iter()
        .position(|a| a == "--json-out")
        .and_then(|i| args.get(i + 1))
    {
        let text = serde_json::to_string_pretty(&results).unwrap();
        if let Err(e) = std::fs::write(path, format!("{text}\n")) {
            eprintln!("probe: could not write {path}: {e}");
        }
    }
}

fn status_label(s: &Snapshot) -> String {
    match &s.status {
        overseer_life::Status::Ok => "ok".into(),
        overseer_life::Status::NeedsAuth { hint } => format!("needs_auth ({hint})"),
        overseer_life::Status::Expired => "expired".into(),
        overseer_life::Status::Unavailable { reason } => format!("unavailable ({reason})"),
        overseer_life::Status::BudgetExceeded { .. } => "budget_exceeded".into(),
        overseer_life::Status::Error { kind, message } => format!("error {kind:?}: {message}"),
    }
}

fn print_snapshot(s: &Snapshot) {
    let account = s
        .account
        .label
        .clone()
        .or_else(|| s.account.fingerprint.clone().map(|f| format!("fp:{f}")))
        .unwrap_or_else(|| "-".into());
    println!("{} [{}] {}", s.source.0, account, status_label(s));
    for l in &s.limits {
        let mut line = format!("  limit {}", l.name);
        if let Some(p) = l.used_percent {
            line.push_str(&format!(" {p:.1}%"));
        }
        if let (Some(u), Some(lim)) = (l.used, l.limit) {
            line.push_str(&format!(" {u}/{lim} {}", l.unit.as_deref().unwrap_or("")));
        }
        if let Some(r) = l.resets_at_ms {
            line.push_str(&format!(" resets_at_ms {r}"));
        }
        println!("{line}");
    }
    for m in &s.metrics {
        println!(
            "  metric {} = {} {}",
            m.name,
            m.value,
            m.unit.as_deref().unwrap_or("")
        );
    }
    for l in &s.usage_lines {
        println!("  {}: {}", l.label, l.value);
    }
}
