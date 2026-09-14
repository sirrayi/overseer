//! The minimal ReAct loop (deliverable 0.3; playbook Ch.4).
//! thought → action → observation, stateless tool exec, engine-enforced
//! budgets, hard exit conditions. Deliberately mini-SWE-agent-shaped:
//! every later subsystem must earn its tokens over this floor.

use std::path::PathBuf;

use crate::event::{rehydrate_messages, Event, EventKind, EventLog};
use crate::ir::{Block, Message};
use crate::ledger::{Ledger, UsageRecord};
use crate::profile;
use crate::provider::{Provider, Request, StopReason, SystemSegment};
use crate::stuck::StuckDetector;
use crate::tools::{ToolCtx, ToolRegistry};

/// Static system prompt — kept minimal and byte-stable (Invariant 2:
/// nothing volatile lives here; per-session data goes below the boundary
/// once the prompt assembler lands in Phase 1).
const SYSTEM_PROMPT: &str = "\
You are Overseer, an agentic coding engine running as a CLI on the user's machine.\n\
Use the tools to accomplish the task. Prefer dedicated tools over bash for file operations.\n\
Keep prose between tool calls under 25 words. Verify work with builds/tests when available.\n\
Large tool outputs are spilled to files — read or grep them by path for more.";

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
        }
    }
}

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
        };
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
        })
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
        let system = [SystemSegment {
            text: SYSTEM_PROMPT.into(),
            cacheable: true,
        }];
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

            if calls.is_empty() || resp.stop_reason == StopReason::EndTurn {
                self.end_run(resp.stop_reason.as_str(), steps, on_event)?;
                return Ok(RunOutcome::Completed {
                    steps,
                    cost_usd: self.ledger.total_cost_usd,
                });
            }

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
                spill_seq: 0,
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

            if let Some(pattern) = stuck_hit {
                if let Some(outcome) = self.on_stuck(pattern, steps, on_event)? {
                    return Ok(outcome);
                }
            }

            self.emit(EventKind::TurnEnd { step: steps }, on_event)?;
            self.log.flush()?;
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
