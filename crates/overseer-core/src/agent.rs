//! The minimal ReAct loop (deliverable 0.3; playbook Ch.4).
//! thought → action → observation, stateless tool exec, engine-enforced
//! budgets, hard exit conditions. Deliberately mini-SWE-agent-shaped:
//! every later subsystem must earn its tokens over this floor.

use std::path::PathBuf;

use crate::event::{rehydrate_messages, Event, EventKind, EventLog};
use crate::ir::{Block, Message};
use crate::ledger::{Ledger, UsageRecord};
use crate::profile;
use crate::provider::{Provider, Request, StopReason};
use crate::stuck::StuckDetector;
use crate::tools::{ToolCtx, ToolRegistry};

/// Static system prompt — assembled per turn by `prompt::assemble` as an
/// ordered section pipeline with an explicit STATIC/DYNAMIC boundary
/// (Invariant 2: nothing volatile lives above it).

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    /// Hard turn cap — engine-enforced, not model-requested (Invariant 6).
    pub max_steps: u32,
    /// Session spend cap in USD.
    pub max_cost_usd: f64,
    pub max_output_tokens: u32,
    /// Anthropic thinking budget; None disables.
    pub thinking_budget: Option<u32>,
    pub cwd: PathBuf,
    /// Benchmark mode: disable the permission gate entirely. Only valid when
    /// the environment itself is the sandbox (per-task container).
    pub full_access: bool,
    /// Context-engine master switch (benchmark/debug escape hatch).
    pub auto_compact: bool,
    /// Compaction trigger as a fraction of the model's context window
    /// (playbook Ch.3 §9.2: budget to the *effective* window, not the
    /// advertised one). `None` → the model profile's `compact_at` default.
    pub compact_at: Option<f32>,
    /// File-based memory dir (playbook Ch.3 §9.4): INDEX.md is injected at
    /// the end of the static prompt region each turn. None = memory off.
    /// Must sit under `cwd` for the permission gate to allow writes.
    pub memory_dir: Option<PathBuf>,
    /// P1.2 stale tool-result clearing: this many most-recent ToolResult
    /// blocks stay verbatim; older ones render as a placeholder in the view
    /// (events untouched). 0 disables clearing.
    pub keep_tool_results: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            model: "claude-sonnet-5".into(),
            max_steps: 100,
            max_cost_usd: 5.0,
            max_output_tokens: 16_384,
            thinking_budget: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            full_access: false,
            auto_compact: true,
            compact_at: None,
            memory_dir: None,
            keep_tool_results: 5,
        }
    }
}

#[derive(Debug)]
pub enum RunOutcome {
    Completed {
        steps: u32,
        cost_usd: f64,
    },
    StepBudgetExceeded {
        steps: u32,
        cost_usd: f64,
    },
    CostBudgetExceeded {
        steps: u32,
        cost_usd: f64,
    },
    /// Stuck detector tripped twice (nudge once, then terminate).
    Stuck {
        pattern: String,
        steps: u32,
        cost_usd: f64,
    },
    /// Three consecutive responses with no text and no tool calls.
    EmptyResponse {
        steps: u32,
        cost_usd: f64,
    },
    Provider(String),
}

/// A running agent: owns the event log, ledger, tool registry, and the
/// message view rehydrated from the log.
pub struct Agent<'a> {
    provider: &'a dyn Provider,
    config: AgentConfig,
    log: EventLog,
    ledger: Ledger,
    tools: ToolRegistry,
    messages: Vec<Message>,
    session_dir: PathBuf,
    stuck: StuckDetector,
    /// One free course-correction per run; second trip terminates.
    stuck_nudged: bool,
    /// Consecutive responses with no text and no tool calls (silent-END).
    empty_responses: u8,
    /// Compact at the next loop boundary (set by the context-budget trigger
    /// or a provider context-window stop).
    pending_compact: bool,
    /// Last response hit the provider's context wall — if compaction can't
    /// shrink the view further, the run ends rather than looping forever.
    ctx_wall_stop: bool,
    /// Monotonic spill-file counter for the whole session — kept on the
    /// agent (not per-turn ToolCtx) so output-N.txt names never collide
    /// across turns.
    spill_seq: u64,
}

impl<'a> Agent<'a> {
    /// Start a fresh session in `session_dir` (must exist / be creatable).
    pub fn start(
        provider: &'a dyn Provider,
        config: AgentConfig,
        session_dir: PathBuf,
        session_id: String,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&session_dir)?;
        let log = EventLog::create(session_dir.join("events.jsonl"))?;
        let ledger = Ledger::create(session_dir.join("ledger.jsonl"))?;
        let tools = ToolRegistry::core(Self::policy(&config));
        let mut agent = Agent {
            provider,
            config,
            log,
            ledger,
            tools,
            messages: Vec::new(),
            session_dir,
            stuck: StuckDetector::new(),
            stuck_nudged: false,
            empty_responses: 0,
            pending_compact: false,
            ctx_wall_stop: false,
            spill_seq: 0,
        };
        if let Some(dir) = agent.config.memory_dir.clone() {
            crate::memory::ensure(&dir)?;
        }
        let cwd = agent.config.cwd.display().to_string();
        agent.log.append(EventKind::SessionStart {
            session_id,
            cwd,
            model: agent.config.model.clone(),
            harness_version: env!("CARGO_PKG_VERSION").to_string(),
        })?;
        agent.log.flush()?;
        Ok(agent)
    }

    /// Resume an existing session dir: replay events, rebuild the message view.
    pub fn resume(
        provider: &'a dyn Provider,
        config: AgentConfig,
        session_dir: PathBuf,
    ) -> std::io::Result<Self> {
        let events = EventLog::replay(session_dir.join("events.jsonl"))?;
        let messages = rehydrate_messages(&events);
        let log = EventLog::open(session_dir.join("events.jsonl"))?;
        let ledger = Ledger::open(session_dir.join("ledger.jsonl"))?;
        let tools = ToolRegistry::core(Self::policy(&config));
        // Seed the spill counter past existing files so resume can't
        // overwrite earlier spilled output.
        let spill_seq = std::fs::read_dir(session_dir.join("tool-outputs"))
            .map(|d| d.count() as u64)
            .unwrap_or(0);
        Ok(Agent {
            provider,
            config,
            log,
            ledger,
            tools,
            messages,
            session_dir,
            stuck: StuckDetector::new(),
            stuck_nudged: false,
            empty_responses: 0,
            pending_compact: false,
            ctx_wall_stop: false,
            spill_seq,
        })
    }

    /// Swap the tool registry — used to spawn read-only subagents with a
    /// quarantined toolset (playbook Ch.3 §9.6).
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Run one user turn through the ReAct loop until the model stops calling
    /// tools or a budget trips. `on_event` sees every logged event (TUI/JSONL
    /// frontends render from this stream).
    pub fn run_turn(
        &mut self,
        input: &str,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<RunOutcome> {
        self.messages.push(Message::user_text(input));
        self.emit(EventKind::UserInput { text: input.into() }, on_event)?;

        let mut steps = 0u32;
        let profile = profile::lookup(&self.config.model);

        loop {
            if steps >= self.config.max_steps {
                let out = RunOutcome::StepBudgetExceeded {
                    steps,
                    cost_usd: self.ledger.total_cost_usd,
                };
                self.end_run("max_steps", steps, on_event)?;
                return Ok(out);
            }
            if self.ledger.total_cost_usd >= self.config.max_cost_usd {
                let out = RunOutcome::CostBudgetExceeded {
                    steps,
                    cost_usd: self.ledger.total_cost_usd,
                };
                self.end_run("max_cost", steps, on_event)?;
                return Ok(out);
            }

            // Compaction boundary: the loop only ever compacts here — after a
            // complete tool-result batch (or a nudge), never mid-batch, so
            // the Anthropic pairing rule survives the cut.
            if self.pending_compact {
                self.pending_compact = false;
                let shrunk = self.compact(on_event)?;
                if !shrunk && self.ctx_wall_stop {
                    // Provider already refuses this context and there's
                    // nothing left to drop — end cleanly, don't spin.
                    self.end_run("context_window", steps, on_event)?;
                    return Ok(RunOutcome::Completed {
                        steps,
                        cost_usd: self.ledger.total_cost_usd,
                    });
                }
            }

            // P1.2: clear stale tool results in the view (events untouched).
            // In-place on the view — idempotent, so resume and live runs
            // render identically.
            if self.config.keep_tool_results > 0 {
                crate::event::clear_stale_tool_results(
                    &mut self.messages,
                    self.config.keep_tool_results,
                );
            }

            // System segments assemble per turn via the section pipeline:
            // the static sections are byte-stable; the memory index sits in
            // the last static slot so an edit only invalidates cache from
            // that segment onward — tools+prompt stay warm.
            let system = crate::prompt::assemble(&self.config);

            let req = Request {
                model: &self.config.model,
                system: &system,
                tools: &self.tools.specs,
                messages: &self.messages,
                max_tokens: self.config.max_output_tokens,
                thinking_budget: self.config.thinking_budget,
                cache_breakpoints: true,
            };

            let resp = match self.provider.complete(&req) {
                Ok(r) => r,
                Err(e) => {
                    let msg = e.to_string();
                    self.emit(
                        EventKind::Error {
                            message: msg.clone(),
                        },
                        on_event,
                    )?;
                    self.end_run("provider_error", steps, on_event)?;
                    return Ok(RunOutcome::Provider(msg));
                }
            };
            steps += 1;

            let cost = profile.cost_usd(&resp.usage);
            self.ledger.record(UsageRecord::from_usage(
                &self.config.model,
                &resp.usage,
                resp.request_bytes,
                resp.latency_ms,
                count_tool_calls(&resp.blocks) as u32,
                cost,
            ))?;

            // Effective-window budget (playbook Ch.3 §9.2): trigger on the
            // *measured* prompt size from the last call, not an estimate.
            // A provider context-window stop also forces compaction.
            self.ctx_wall_stop = resp.stop_reason == StopReason::ContextWindowExceeded;
            if self.config.auto_compact {
                let frac = self.config.compact_at.unwrap_or(profile.compact_at);
                let budget = frac as f64 * f64::from(profile.context_in);
                if self.ctx_wall_stop || resp.usage.total_input() as f64 > budget {
                    self.pending_compact = true;
                }
            }

            self.messages.push(Message {
                role: crate::ir::Role::Assistant,
                content: resp.blocks.clone(),
            });
            self.emit(
                EventKind::ModelResponse {
                    blocks: resp.blocks,
                    usage: resp.usage,
                    stop_reason: resp.stop_reason.as_str().to_string(),
                    latency_ms: resp.latency_ms,
                    cost_usd: cost,
                },
                on_event,
            )?;

            // Zero-tool-call turn = done (handles end_turn, empty content,
            // pause_turn, refusal — the canonical silent-END failure class).
            let calls: Vec<(String, String, serde_json::Value)> = self
                .messages
                .last()
                .map(|m| {
                    m.tool_calls()
                        .map(|(id, n, i)| (id.to_string(), n.to_string(), i.clone()))
                        .collect()
                })
                .unwrap_or_default();

            // Silent-END guard (playbook failure matrix): a response with no
            // tool calls AND no visible text is a malformed finish — the model
            // "thought" but produced nothing. Nudge and continue; three
            // consecutive empties terminate the run.
            if calls.is_empty() {
                let has_text = self
                    .messages
                    .last()
                    .map(|m| {
                        m.content
                            .iter()
                            .any(|b| matches!(b, Block::Text { text } if !text.trim().is_empty()))
                    })
                    .unwrap_or(false);
                if !has_text {
                    self.empty_responses += 1;
                    if self.empty_responses >= 3 {
                        self.end_run("empty_response", steps, on_event)?;
                        return Ok(RunOutcome::EmptyResponse {
                            steps,
                            cost_usd: self.ledger.total_cost_usd,
                        });
                    }
                    let text = "[overseer] Your previous turn produced no visible \
                                output and no tool calls. Continue working — act, \
                                or explain what is blocking you."
                        .to_string();
                    self.messages.push(Message::user_text(text.clone()));
                    self.emit(EventKind::Nudge { text }, on_event)?;
                    continue;
                }
                self.empty_responses = 0;
                // A context-window stop isn't a finish — the model was cut
                // off mid-generation. Compact and let it continue (the loop
                // exits via ctx_wall_stop if nothing can be dropped).
                if resp.stop_reason == StopReason::ContextWindowExceeded {
                    self.pending_compact = true;
                    continue;
                }
                self.end_run(resp.stop_reason.as_str(), steps, on_event)?;
                return Ok(RunOutcome::Completed {
                    steps,
                    cost_usd: self.ledger.total_cost_usd,
                });
            }
            self.empty_responses = 0;

            // Stuck check on the response itself (context-window errors);
            // any trip is handled AFTER the tool batch so every tool_use
            // still gets its tool_result (Anthropic's pairing rule).
            let mut stuck_hit = self
                .stuck
                .observe_response(true, resp.stop_reason == StopReason::ContextWindowExceeded);

            // Execute tool calls; results merge into one user message.
            let mut ctx = ToolCtx {
                cwd: self.config.cwd.clone(),
                session_dir: self.session_dir.clone(),
                spill_seq: self.spill_seq,
                provider: Some(self.provider),
                agent_config: Some(self.config.clone()),
                subagent_seq: 0,
            };
            let mut results = Vec::new();
            for (call_id, name, input) in calls {
                self.emit(
                    EventKind::ToolCallStart {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    },
                    on_event,
                )?;

                let out = self.tools.call(&name, &input, &mut ctx);
                self.emit(
                    EventKind::ToolResult {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        content: out.text.clone(),
                        is_error: out.is_error,
                        raw_bytes: out.raw_bytes,
                        spilled_to: out.spilled_to.clone(),
                        denied: out.denied,
                    },
                    on_event,
                )?;

                if stuck_hit.is_none() {
                    stuck_hit = self
                        .stuck
                        .observe_step(&name, &input, out.is_error, &out.text);
                }

                results.push(Block::ToolResult {
                    tool_use_id: call_id,
                    content: out.text,
                    is_error: out.is_error,
                });
            }
            self.messages.push(Message::tool_results(results));
            // Carry the session-monotonic spill counter forward.
            self.spill_seq = ctx.spill_seq;

            if let Some(pattern) = stuck_hit {
                if let Some(outcome) = self.on_stuck(pattern, steps, on_event)? {
                    return Ok(outcome);
                }
            }

            self.emit(EventKind::TurnEnd { step: steps }, on_event)?;
            self.log.flush()?;
            // Git-version memory at the durable-tail point (playbook: free
            // history/diff/rollback). Engine-made commit, best-effort.
            if let Some(dir) = &self.config.memory_dir {
                crate::memory::commit(dir, &format!("turn {steps}"));
            }
        }
    }

    /// Handle a detector trip: log it, inject one course-correction nudge,
    /// terminate on the second trip. Never leaves a partial tool batch.
    fn on_stuck(
        &mut self,
        pattern: crate::stuck::StuckPattern,
        steps: u32,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<Option<RunOutcome>> {
        self.emit(
            EventKind::StuckDetected {
                pattern: pattern.describe().to_string(),
            },
            on_event,
        )?;
        if self.stuck_nudged {
            self.end_run("stuck", steps, on_event)?;
            return Ok(Some(RunOutcome::Stuck {
                pattern: pattern.describe().to_string(),
                steps,
                cost_usd: self.ledger.total_cost_usd,
            }));
        }
        self.stuck_nudged = true;
        let text = format!(
            "[overseer] Stuck detector: {}. Change approach — re-read the actual \
             error output, inspect real state with a different tool, or stop and \
             report what's blocking. Repeating this pattern will end the run.",
            pattern.describe()
        );
        self.messages.push(Message::user_text(text.clone()));
        self.emit(EventKind::Nudge { text }, on_event)?;
        Ok(None)
    }

    /// Compact the message view (playbook Ch.3 §9.5): derive a fixed-schema
    /// summary from the raw event log, record a `Compaction` boundary event,
    /// then rebuild the view by replaying the log — so the live context is
    /// byte-identical to what `--resume` would produce. Returns false when
    /// there was nothing droppable (caller decides what that means).
    fn compact(&mut self, on_event: &mut dyn FnMut(&Event)) -> std::io::Result<bool> {
        let events = EventLog::replay(self.log.path())?;
        let floor = crate::compact::latest(&events).map(|(_, t)| t).unwrap_or(0);
        let Some(anchor) = crate::compact::tail_anchor(&events, crate::compact::TAIL_TURNS, floor)
        else {
            return Ok(false);
        };
        let summary = crate::compact::summarize(&events, anchor);
        self.emit(
            EventKind::Compaction {
                summary,
                tail_from: anchor,
            },
            on_event,
        )?;
        self.log.flush()?;
        // Rebuild the view from the durable log — the summary + recency
        // tail now in effect are exactly what resume replays.
        let events = EventLog::replay(self.log.path())?;
        self.messages = rehydrate_messages(&events);
        Ok(true)
    }

    fn policy(config: &AgentConfig) -> crate::perm::Policy {
        if config.full_access {
            crate::perm::Policy::allow_all()
        } else {
            crate::perm::Policy::headless(config.cwd.clone())
        }
    }

    /// Append an event and hand it to the sink.
    fn emit(&mut self, kind: EventKind, on_event: &mut dyn FnMut(&Event)) -> std::io::Result<()> {
        self.log.append(kind)?;
        if let Some(e) = self.log.last() {
            on_event(e);
        }
        Ok(())
    }

    fn end_run(
        &mut self,
        stop: &str,
        steps: u32,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<()> {
        self.emit(
            EventKind::RunEnd {
                stop_reason: stop.into(),
                steps,
                total_cost_usd: self.ledger.total_cost_usd,
            },
            on_event,
        )?;
        self.log.flush()
    }
}

fn count_tool_calls(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .filter(|b| matches!(b, Block::ToolCall { .. }))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventLog;
    use crate::ir::Usage;
    use crate::provider::{ProviderError, Response};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted provider: pops one response per call, then keeps returning
    /// the last one forever. Records the system segments it was shown.
    struct Mock {
        responses: Mutex<VecDeque<Response>>,
        seen_systems: Mutex<Vec<Vec<String>>>,
    }

    impl Mock {
        fn new(responses: Vec<Response>) -> Self {
            Mock {
                responses: Mutex::new(VecDeque::from(responses)),
                seen_systems: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for Mock {
        fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
            self.seen_systems
                .lock()
                .unwrap()
                .push(req.system.iter().map(|s| s.text.clone()).collect());
            let mut q = self.responses.lock().unwrap();
            if q.len() > 1 {
                Ok(q.pop_front().unwrap())
            } else {
                Ok(q.front().unwrap().clone())
            }
        }
        fn name(&self) -> &'static str {
            "mock"
        }
    }

    fn tool_turn(n: usize) -> Response {
        Response {
            blocks: vec![Block::ToolCall {
                id: format!("c{n}"),
                name: "bash".into(),
                // Distinct input + output per turn so the stuck detector's
                // identical-loop pattern stays quiet.
                input: serde_json::json!({"command": format!("echo turn{n}")}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                fresh_input: 20_000,
                ..Usage::default()
            },
            request_bytes: 0,
            latency_ms: 0,
        }
    }

    fn done() -> Response {
        Response {
            blocks: vec![Block::Text {
                text: "all done".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                fresh_input: 20_000,
                ..Usage::default()
            },
            request_bytes: 0,
            latency_ms: 0,
        }
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-agent-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 5 tool turns with usage far above a tiny compact threshold → the
    /// engine must compact mid-run, record the boundary event, and the
    /// live view must equal the resume view (playbook Ch.3 §9.1 invariant).
    #[test]
    fn compacts_when_context_budget_trips() {
        let dir = tmpdir();
        let mut cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            // FALLBACK profile: 200K window × 0.05 = 10K → 10,001 trips it.
            compact_at: Some(0.05),
            ..AgentConfig::default()
        };
        cfg.max_steps = 50;
        let provider = Mock::new(vec![
            tool_turn(1),
            tool_turn(2),
            tool_turn(3),
            tool_turn(4),
            tool_turn(5),
            done(),
        ]);
        let mut agent = Agent::start(&provider, cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        let out = agent.run_turn("do the thing", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Completed { .. }));

        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Compaction { .. })));

        // Live view == resume view, and it opens with the summary message.
        let resumed = rehydrate_messages(&events);
        assert_eq!(agent.messages(), resumed.as_slice());
        assert!(resumed[0].text().contains("compaction v1"));
        // The verbatim tail starts on an assistant boundary — never an
        // orphan tool_result user message (pairing invariant).
        assert!(matches!(resumed[1].role, crate::ir::Role::Assistant));
    }

    /// A context-window stop is not a finish: the loop compacts and
    /// continues instead of returning Completed mid-thought.
    #[test]
    fn context_window_stop_retries_after_compaction() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        let mut responses: Vec<Response> = (0..4).map(tool_turn).collect();
        responses.push(Response {
            blocks: vec![Block::Text {
                text: "truncated mid-th".into(),
            }],
            stop_reason: StopReason::ContextWindowExceeded,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        });
        responses.push(done());
        let provider = Mock::new(responses);
        let mut agent = Agent::start(&provider, cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        let out = agent.run_turn("work", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Completed { steps: 6, .. }));

        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Compaction { .. })));
    }

    /// With memory enabled, the provider sees a second system segment
    /// carrying INDEX.md — appended *after* the static prompt (end of the
    /// static region), updated across turns, and git-versioned.
    #[test]
    fn memory_index_reaches_provider() {
        let dir = tmpdir();
        let memdir = dir.join("memory");
        std::fs::create_dir_all(&memdir).unwrap();
        std::fs::write(memdir.join("INDEX.md"), "facts.md — user facts\n").unwrap();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            memory_dir: Some(memdir.clone()),
            ..AgentConfig::default()
        };
        let provider = Mock::new(vec![tool_turn(1), done()]);
        let mut agent = Agent::start(&provider, cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        agent.run_turn("hi", &mut sink).unwrap();

        let seen = provider.seen_systems.lock().unwrap();
        // identity + contract + safety + memory index (last static slot).
        assert_eq!(seen[0].len(), 4);
        assert!(seen[0][3].contains("## Memory index"));
        assert!(seen[0][3].contains("facts.md — user facts"));
        // Static sections stay first and byte-stable.
        assert!(seen[0][0].contains("Overseer"));
        // Git-versioned: a commit landed at the turn boundary.
        assert!(memdir.join(".git").exists());
    }

    /// The `task` tool spawns a read-only subagent in an isolated context:
    /// the parent receives a bounded digest + trace path, the subagent gets
    /// its own event log under <session>/subagents/, and it cannot write.
    #[test]
    fn task_tool_spawns_quarantined_subagent() {
        let dir = tmpdir();
        std::fs::write(dir.join("target.txt"), "needle-data").unwrap();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        // Queue order: parent's task call → sub's read call → sub's answer
        // → parent's final answer (one provider feeds both agents).
        let task_call = Response {
            blocks: vec![Block::ToolCall {
                id: "t1".into(),
                name: "task".into(),
                input: serde_json::json!({"prompt": "find the needle in target.txt"}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        };
        let sub_read = Response {
            blocks: vec![Block::ToolCall {
                id: "s1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "target.txt"}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        };
        let provider = Mock::new(vec![
            task_call,
            sub_read,
            Response {
                blocks: vec![Block::Text {
                    text: "the needle is needle-data".into(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                request_bytes: 0,
                latency_ms: 0,
            },
            done(),
        ]);
        let mut agent = Agent::start(&provider, cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        let out = agent.run_turn("use a subagent", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Completed { .. }));

        // Parent got the digest + trace pointer, not the raw transcript.
        // (The digest rides in a ToolResult block, not Text.)
        let result_text: String = agent.messages()[2]
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert!(result_text.contains("the needle is needle-data"));
        assert!(result_text.contains("full trace:"));

        // The subagent's isolated session log exists and is self-contained.
        let sub_log = dir.join("subagents/task-1/events.jsonl");
        assert!(sub_log.exists());
        let sub_events = EventLog::replay(&sub_log).unwrap();
        assert!(sub_events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::UserInput { text } if text.contains("needle"))));

        // Quarantine: the read-only registry has exactly the read tools —
        // no write/bash/task, so a subagent can neither mutate nor recurse.
        let ro = crate::tools::ToolRegistry::readonly(crate::perm::Policy::allow_all());
        let names: Vec<&str> = ro.specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["read", "grep", "glob"]);
    }
}
