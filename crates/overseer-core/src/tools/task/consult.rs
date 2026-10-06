//! Consult mode: one request to the heavy tier — no tools, no loop. The
//! task dir still gets an event log and a ledger so tracing and
//! settlement work exactly as for looping subagents.

use std::path::Path;

use super::route::Route;
use super::Attempt;
use crate::agent::RunOutcome;
use crate::event::{EventKind, EventLog};
use crate::ir::{Block, Message};
use crate::ledger::Ledger;
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
        harness_version: env!("CARGO_PKG_VERSION").into(),
        parent: None,
    })?;
    log.append(EventKind::UserInput {
        text: prompt.to_string(),
    })?;
    // One call can't be stopped midway: the spend gate lowers its
    // `max_tokens` to what the cap affords and refuses below its floor.
    let msgs = [Message::user_text(prompt)];
    let mut req = Request {
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
    let gated = crate::ledger::Gate {
        provider,
        ledger: &mut ledger,
        cap_usd: cap,
        reserved_usd: 0.0,
    }
    .call(&mut req, Some("consult"));
    let attempt = match gated {
        Err(crate::ledger::GateError::Budget { worst_usd, .. }) => {
            let message =
                format!("consult could cost up to ${worst_usd:.4}, above its ${cap:.4} cap");
            log.append(EventKind::Error {
                message: message.clone(),
            })?;
            log.flush()?;
            return Ok(Attempt::refused(message));
        }
        Err(crate::ledger::GateError::Io(e)) => return Err(e),
        Ok((r, cost)) => {
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
        Err(crate::ledger::GateError::Provider(e)) => {
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
