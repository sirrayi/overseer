//! Consult mode: one request to the heavy tier — no tools, no loop. The
//! task dir still gets an event log and a ledger so tracing and
//! settlement work exactly as for looping subagents.

use std::path::Path;

use super::route::Route;
use super::Attempt;
use crate::agent::RunOutcome;
use crate::event::{EventKind, EventLog};
use crate::ir::{Block, Message};
use crate::ledger::{Ledger, UsageRecord};
use crate::provider::{Provider, Request};

const MAX_TOKENS: u32 = 2_000;

pub(super) fn call(
    provider: &dyn Provider,
    route: &Route,
    prompt: &str,
    dir: &Path,
    cwd: &Path,
    cap: f64,
) -> Attempt {
    run(provider, route, prompt, dir, cwd, cap)
        .unwrap_or_else(|e| Attempt::failed(format!("consult: {e}")))
}

fn run(
    provider: &dyn Provider,
    route: &Route,
    prompt: &str,
    dir: &Path,
    cwd: &Path,
    cap: f64,
) -> std::io::Result<Attempt> {
    let mut log = EventLog::create(dir.join("events.jsonl"))?;
    let mut ledger = Ledger::create(dir.join("ledger.jsonl"))?;
    log.append(EventKind::SessionStart {
        session_id: dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        cwd: cwd.display().to_string(),
        model: route.model.clone(),
        harness_version: env!("CARGO_PKG_VERSION").to_string(),
        parent: None,
    })?;
    log.append(EventKind::UserInput {
        text: prompt.to_string(),
    })?;
    // One call can't be stopped midway: refuse when its worst case
    // (whole prompt fresh + a full answer) would exceed the cap.
    let profile = crate::profile::lookup(&route.model);
    let worst = (crate::tokens::count_tokens(prompt, &route.model) as f64 * profile.price.input
        + f64::from(MAX_TOKENS) * profile.price.output)
        / 1_000_000.0;
    if worst > cap {
        let message = format!("consult could cost up to ${worst:.4}, above its ${cap:.4} cap");
        log.append(EventKind::Error {
            message: message.clone(),
        })?;
        log.flush()?;
        return Ok(Attempt::failed(message));
    }
    let msgs = [Message::user_text(prompt)];
    let req = Request {
        model: &route.model,
        system: &[],
        tools: &[],
        messages: &msgs,
        max_tokens: MAX_TOKENS,
        thinking_budget: None,
        effort: route.effort,
        cache_breakpoints: false,
        cache_key: None,
    };
    let attempt = match provider.complete(&req) {
        Ok(r) => {
            let cost = profile.cost_usd(&r.usage);
            ledger.record(UsageRecord::from_usage(
                &route.model,
                &r.usage,
                r.request_bytes,
                r.latency_ms,
                0,
                cost,
            ))?;
            let text: String = r
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            log.append(EventKind::ModelResponse {
                blocks: r.blocks,
                usage: r.usage,
                stop_reason: r.stop_reason.as_str().to_string(),
                latency_ms: r.latency_ms,
                cost_usd: cost,
            })?;
            let outcome = if text.trim().is_empty() {
                RunOutcome::EmptyResponse {
                    steps: 1,
                    cost_usd: cost,
                }
            } else {
                RunOutcome::Completed {
                    steps: 1,
                    cost_usd: cost,
                }
            };
            Attempt {
                text,
                outcome: Ok(outcome),
                cost,
                verdict: None,
                notes: Vec::new(),
            }
        }
        Err(e) => {
            log.append(EventKind::Error {
                message: e.to_string(),
            })?;
            Attempt {
                outcome: Ok(RunOutcome::Provider(e.to_string())),
                ..Attempt::failed(String::new())
            }
        }
    };
    log.flush()?;
    Ok(attempt)
}
