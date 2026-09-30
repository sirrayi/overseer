//! The minimal ReAct loop (deliverable 0.3; playbook Ch.4).
//! thought → action → observation, stateless tool exec, engine-enforced
//! budgets, hard exit conditions. Deliberately mini-SWE-agent-shaped:
//! every later subsystem must earn its tokens over this floor.

use std::path::PathBuf;
use std::sync::Arc;

use crate::control::Control;
use crate::event::{rehydrate_messages, Event, EventKind, EventLog};
use crate::ir::{Block, Message};
use crate::ledger::{Ledger, UsageRecord};
use crate::profile;
use crate::provider::{Provider, Request, StopReason};
use crate::stuck::StuckDetector;
use crate::tools::{ToolCtx, ToolRegistry};

// Static system prompt — assembled ONCE per Agent (start/resume, and on a
// model switch) by `prompt::assemble` as an ordered section pipeline with an
// explicit STATIC/DYNAMIC boundary (Invariant 2: nothing volatile lives
// above it). Every request of the Agent reuses the exact bytes.

/// P7-4: the marker an untrusted-originated spawn exports
/// (`channel:<channel>:<sender>`). Absent/empty = a locally-originated run.
pub const UNTRUSTED_ENV: &str = "OVERSEER_UNTRUSTED_SOURCE";

/// Read the untrusted-origin marker through an injected getter so tests
/// never have to touch the process environment (which is shared by every
/// test thread in the binary).
pub fn untrusted_origin_from(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    get(UNTRUSTED_ENV).filter(|s| !s.trim().is_empty())
}

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
    /// Cross-provider effort knob (P3.2); adapters map it per-API.
    /// `thinking_budget` wins where the API takes tokens.
    pub effort: Option<crate::provider::Effort>,
    /// Small-tier model for aux calls (P3.2): titles, consolidation,
    /// guardrails. None → aux calls use the main model. Also the light
    /// subagent tier when it shares the main model's transport.
    pub small_model: Option<String>,
    /// Heavy subagent tier (consult, escalation) when it shares the main
    /// model's transport. None → the family's priciest profile row.
    pub heavy_model: Option<String>,
    /// Background subagents allowed in flight at once. Bounded because
    /// every one shares the provider's rate limit and each returns a
    /// digest the lead must review — unbounded fan-out buys neither.
    pub max_bg_subagents: usize,
    pub cwd: PathBuf,
    /// Benchmark mode: disable the permission gate entirely. Only valid when
    /// the environment itself is the sandbox (per-task container).
    pub full_access: bool,
    /// Permission preset (P1.4): workspace-write is the default; read-only
    /// denies all side effects; plan additionally removes mutating tools
    /// from the spec list (capability removal). Ignored under full_access.
    pub policy_preset: crate::perm::Preset,
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
    /// P6-2 sensitivity ceiling for the quarantined subagent memory view:
    /// `task` subagents see only entries at or below this tier (Secret
    /// hidden by default). The parent always sees the full index.
    pub memory_filter: crate::memory::Sensitivity,
    /// Memory v2 user store (`<overseer home>/memory`); `memory_dir` is
    /// the project store. None = no user store.
    pub user_memory_dir: Option<PathBuf>,
    /// Memory v2 recall: search each user input and inject the admitted
    /// notes as a `MemoryNotice`.
    pub memory_recall: bool,
    /// Set by `task` for subagents: memory is read-only, and recall,
    /// reminders and the episode note are off.
    pub is_subagent: bool,
    /// P6-3 credential broker: process-side secret store. The agent
    /// hands it to each turn's ToolCtx for bash injection + result
    /// sanitization. Default-empty (no creds); P6-4 adds persistence.
    pub broker: crate::cred::Broker,
    /// P1.2 stale tool-result clearing: this many most-recent ToolResult
    /// blocks stay verbatim; older ones render as a placeholder in the view
    /// (events untouched). 0 disables clearing.
    pub keep_tool_results: usize,
    /// P1.10 verification gate: a runnable definition-of-done check (e.g.
    /// `cargo test`). When the model tries to finish, the engine runs it —
    /// a failure blocks the stop and the failing output goes back into
    /// context. None disables the gate.
    pub verify_cmd: Option<String>,
    /// Consecutive verify-block cap before the run ends anyway (~8 per
    /// the playbook). Counted per session, not reset between blocks.
    pub verify_block_cap: u32,
    /// P1.5 sandbox v1: wrap bash calls in sandbox-exec (macOS) or bwrap
    /// (Linux) when available — deny-by-default network, writes confined
    /// to the workspace. Falls back to unsandboxed exec with a warning
    /// when no backend exists.
    pub sandbox_bash: bool,
    /// P2.5 human-verdict channel: consulted when the permission gate
    /// returns Ask. `None` (headless) collapses Ask to Deny — fail-closed.
    /// The handler runs on the agent thread; frontends block it on a UI
    /// response channel.
    pub ask_handler: Option<crate::perm::AskHandler>,
    /// Persisted-allow rules file (`~/.overseer/rules` by convention).
    /// `AllowAlways` decisions append keys here; every new Policy loads
    /// them. None disables persistence.
    pub rules_path: Option<PathBuf>,
    /// P4.3 ablation: tool names removed from the spec list and refused at
    /// dispatch (`--no-tools`). Validated against tools::TOOL_NAMES.
    pub disabled_tools: Vec<String>,
    /// P5-B per-domain autonomy overrides (domain → level). Empty = lane
    /// defaults (internal: existing rules decide; external/money/identity:
    /// approval). Merged into the Policy at agent start.
    pub autonomy: std::collections::HashMap<String, crate::perm::Autonomy>,
    /// P7-1 computer-use containment (appended at struct end; Default at end).
    /// `takeover_pause`: credential-field focus or watch-mode suppresses
    /// pixel capture (metadata-only obs). `watch_mode`: treat every capture
    /// as takeover-suppressed. `egress_deny`: block networked sends from
    /// computer-driven turns (defense alongside the no-creds invariant —
    /// child envs never carry `*_KEY`/`*_TOKEN` secrets).
    pub computer: ComputerConfig,
    /// B1-7 Reflexion hook (Reflexion post-episode pattern): on a verify
    /// block, ask the aux tier for a ≤300-token self-critique appended as a
    /// `[reflection]`-tagged Nudge. `Off` disables; `Reflexion` reflects on
    /// verify blocks only (never on success). Default: Reflexion.
    pub reflect: ReflectMode,
    /// P6-4 credential store: where secrets are read from at session start.
    /// The CLI resolves the configured value against the machine (keychain
    /// backend present? entry usable?) and stores the *effective* store
    /// here, so the manifest records what actually held the secret.
    pub credential_store: crate::cred::CredentialStore,
    /// P6-5 persona dir (onboarding). Some = the persona segment renders
    /// (approved bodies, or a one-line pending notice) and the draft gate
    /// closes file tools on an unapproved dir.
    pub persona_dir: Option<PathBuf>,
    /// P8-C `--runtime` sandbox selector (the gVisor port's flag surface):
    /// which backend bash calls run under. `None` keeps the pre-P8-C
    /// behavior exactly — sandbox-exec on macOS, bwrap on Linux, unsandboxed
    /// with a visible note when neither exists. `Some(name)` PINS the
    /// backend: an unknown or unavailable runtime is reported to the model
    /// by name and the call FAILS, because a run that asked for gVisor must
    /// never end up executing unsandboxed just because `runsc` was missing.
    /// Parsed/validated with `crate::backends::SandboxRuntime::parse`.
    pub sandbox_runtime: Option<String>,
    /// R7 MCP servers this session may drive (`~/.overseer/mcp.json`). Empty
    /// (the default) means no MCP at all: the registry gets no `mcp` spec and
    /// the prompt gets no MCP segment, so the session is byte-identical to
    /// the pre-MCP engine. The registry installs one op tool (see
    /// `ToolRegistry::with_mcp`) and the discovered tool definitions never
    /// reach the advertised spec array.
    pub mcp_servers: Vec<crate::mcp_config::McpServer>,
}

/// P7-1 computer-use containment flags. All default off except
/// `takeover_pause` (fail-closed: a cred-field capture suppresses pixels
/// unless explicitly disabled).
#[derive(Debug, Clone)]
pub struct ComputerConfig {
    /// Suppress pixel capture on credential-field focus (metadata-only obs).
    pub takeover_pause: bool,
    /// Treat every capture as takeover-suppressed (metadata-only obs).
    pub watch_mode: bool,
    /// Deny networked sends from computer-driven turns.
    pub egress_deny: bool,
}

impl Default for ComputerConfig {
    fn default() -> Self {
        ComputerConfig {
            takeover_pause: true,
            watch_mode: false,
            egress_deny: false,
        }
    }
}

impl ComputerConfig {
    /// P7-1 containment: child envs never carry secrets. `computer`-driven
    /// turns (and untrusted-originated spawns) strip `*_KEY`/`*_TOKEN`
    /// plus the known provider-key names — the no-creds invariant.
    /// Pure predicate over one env key (the bash spawn filters on it).
    pub fn env_allowed(key: &str) -> bool {
        let upper = key.to_ascii_uppercase();
        if upper.ends_with("_KEY") || upper.ends_with("_TOKEN") {
            return false;
        }
        !matches!(
            upper.as_str(),
            "ANTHROPIC_API_KEY"
                | "OPENAI_API_KEY"
                | "GOOGLE_API_KEY"
                | "GEMINI_API_KEY"
                | "OVERSEER_API_KEY"
        )
    }
}

/// B1-7: when the Reflexion hook fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReflectMode {
    /// No reflection critiques.
    Off,
    /// Reflect on verify-gate blocks only (default).
    #[default]
    Reflexion,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            model: "claude-sonnet-5".into(),
            max_steps: 100,
            max_cost_usd: 5.0,
            max_output_tokens: 16_384,
            thinking_budget: None,
            effort: None,
            small_model: None,
            heavy_model: None,
            max_bg_subagents: 4,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            full_access: false,
            policy_preset: crate::perm::Preset::WorkspaceWrite,
            auto_compact: true,
            compact_at: None,
            memory_dir: None,
            memory_filter: crate::memory::Sensitivity::Personal,
            user_memory_dir: None,
            memory_recall: true,
            is_subagent: false,
            broker: crate::cred::Broker::new(),
            keep_tool_results: 5,
            verify_cmd: None,
            verify_block_cap: 8,
            sandbox_bash: true,
            ask_handler: None,
            rules_path: None,
            disabled_tools: Vec::new(),
            autonomy: Default::default(),
            computer: ComputerConfig::default(),
            reflect: ReflectMode::Reflexion,
            credential_store: crate::cred::CredentialStore::Auto,
            persona_dir: None,
            // P8-C: unset = the platform default sandbox, exactly as before.
            sandbox_runtime: None,
            // R7: no MCP unless the CLI's config loader found servers.
            mcp_servers: Vec::new(),
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
    /// Verification gate blocked the finish `verify_block_cap` times and
    /// the check was still failing — the run ends without a clean bill.
    VerifyFailed {
        steps: u32,
        cost_usd: f64,
    },
    /// The user interrupted the run (Esc). Lands at a tool-launch
    /// boundary — a tool already executing is never killed mid-flight.
    Interrupted {
        steps: u32,
        cost_usd: f64,
    },
    Provider(String),
}

/// A running agent: owns the event log, ledger, tool registry, and the
/// message view rehydrated from the log.
pub struct Agent {
    provider: Arc<dyn Provider>,
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
    /// Adaptive-effort escalations (P3.2): each stuck trip bumps the
    /// request effort one notch for the rest of the run.
    effort_boost: u8,
    /// Background subagent id → last run already noticed (P3.4).
    bg_noticed: std::collections::HashMap<String, u32>,
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
    /// Session-monotonic `task-N` counter — same reason as `spill_seq`:
    /// a per-batch counter reused ids across steps.
    subagent_seq: u64,
    /// This agent's cap composed with its subagents': settled spend is in
    /// the ledger, in-flight caps are reserved here. Shared with the
    /// `task` tool and its background threads.
    spend: Arc<crate::tools::task::SpendAccount>,
    /// Consecutive verification-gate blocks this session (P1.10).
    verify_blocks: u32,
    /// Active checkpoint for the current user prompt (P1.9): created at
    /// each `run_turn` boundary so file snapshots group per prompt.
    checkpoint: Option<crate::tools::Checkpoint>,
    /// Frontend steering handle (P2.4): interrupt + queued input, checked
    /// at safe boundaries only. Default = headless, never fires.
    control: Control,
    /// The frozen static system prefix (invariant 2). Mid-session edits to
    /// memory INDEX/CORE, skills or persona reach the model through its
    /// tools, not by rewriting these bytes (which would bust the cache).
    system: Vec<crate::provider::SystemSegment>,
    /// Provider prompt-cache routing key: the session id, stable across
    /// resumes of the same session.
    cache_key: Option<String>,
    /// CacheStats snapshot at the current run's start — RunEnd reports
    /// the delta so a run summary shows ITS hit rate, not the session's.
    run_cache_start: crate::ledger::CacheStats,
}

/// A stop gate's verdict: let the stop through, block it (a nudge was
/// injected; loop again), or end the run.
enum Gate {
    Pass,
    Blocked,
    Stop(RunOutcome),
}

impl Agent {
    /// Start a fresh session in `session_dir` (must exist / be creatable).
    pub fn start(
        provider: Arc<dyn Provider>,
        config: AgentConfig,
        session_dir: PathBuf,
        session_id: String,
    ) -> std::io::Result<Self> {
        Self::start_with_env(provider, config, session_dir, session_id, |k| {
            std::env::var(k).ok()
        })
    }

    /// `start` with the environment injected — the untrusted-origin marker
    /// is read through `get` so tests can exercise the arming path without
    /// mutating the process environment (shared by every test thread).
    pub fn start_with_env(
        provider: Arc<dyn Provider>,
        config: AgentConfig,
        session_dir: PathBuf,
        session_id: String,
        get: impl Fn(&str) -> Option<String>,
    ) -> std::io::Result<Self> {
        crate::harden::ensure_private_dir(&session_dir)?;
        let log = EventLog::create(session_dir.join("events.jsonl"))?;
        let ledger = Ledger::create(session_dir.join("ledger.jsonl"))?;
        let subagents_dir = session_dir.join("subagents");
        let max_cost_usd = config.max_cost_usd;
        let tools = Self::registry(&config);
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
            effort_boost: 0,
            bg_noticed: std::collections::HashMap::new(),
            pending_compact: false,
            ctx_wall_stop: false,
            spill_seq: 0,
            subagent_seq: crate::tools::task::sidecar::max_seq(&subagents_dir),
            spend: Arc::new(crate::tools::task::SpendAccount::new(max_cost_usd, 0.0)),
            verify_blocks: 0,
            checkpoint: None,
            control: Control::default(),
            system: Vec::new(),
            cache_key: Some(session_id.clone()),
            run_cache_start: crate::ledger::CacheStats::default(),
        };
        agent.reconcile_subagents()?;
        if let Some(dir) = agent.config.memory_dir.clone() {
            crate::memory::ensure(&dir)?;
        }
        if let Some(dir) = agent.config.user_memory_dir.clone() {
            crate::memory::ensure(&dir)?;
        }
        // P6-5: the persona dir is seeded as drafts (never overwritten) so
        // the interview has files to write and the gate has a target.
        if let Some(dir) = agent.config.persona_dir.clone() {
            crate::onboard::ensure_persona_dir(&dir)?;
        }
        agent.system = crate::prompt::assemble(&agent.config);
        let cwd = agent.config.cwd.display().to_string();
        agent.log.append(EventKind::SessionStart {
            session_id: session_id.clone(),
            cwd,
            model: agent.config.model.clone(),
            harness_version: env!("CARGO_PKG_VERSION").to_string(),
            parent: None,
        })?;
        agent.log.flush()?;
        // P6-4: consent grants loaded into the broker are audited at
        // session start — audit-only events carrying client/scope metadata,
        // never secret material (Invariant 1: the log is the paper trail).
        for g in agent.config.broker.grants() {
            agent.log.append(EventKind::ConsentGranted {
                client: g.client.clone(),
                scopes: g.scopes.clone(),
                expires_ms: g.expires_ms,
                actor: g.actor.clone(),
                approved_by: g.approved_by.clone(),
            })?;
        }
        agent.log.flush()?;
        // P7-4 messaging env-arm (disjoint from P6 fields): an untrusted-
        // originated spawn exports OVERSEER_UNTRUSTED_SOURCE=channel:<sender>;
        // `start` pre-arms taint.untrusted and emits an auditable Tainted
        // event so the session begins already distrusting its own inputs —
        // no tool use is needed to earn the latch. Value format is
        // informational only (never parsed).
        if let Some(src) = untrusted_origin_from(get) {
            if let Some(notice) = agent.tools.policy.arm_untrusted(&src) {
                agent.log.append(EventKind::Tainted { detail: notice })?;
            }
        }
        // Run manifest (P4.5): provenance record for the reporting
        // standard — written once, never rewritten by resume.
        crate::manifest::write(
            &agent.session_dir,
            &session_id,
            &agent.config,
            agent.provider.as_ref(),
            &agent.tools,
        )?;
        Ok(agent)
    }

    /// Resume an existing session dir: replay events, rebuild the message view.
    pub fn resume(
        provider: Arc<dyn Provider>,
        config: AgentConfig,
        session_dir: PathBuf,
    ) -> std::io::Result<Self> {
        let events = EventLog::replay(session_dir.join("events.jsonl"))?;
        let messages = rehydrate_messages(&events);
        // Background tasks already noticed before the resume must not be
        // re-injected (their SubagentDone rehydrated above).
        let mut bg_noticed = std::collections::HashMap::new();
        for e in &events {
            if let EventKind::SubagentDone { task_id, run, .. } = &e.kind {
                let seen = bg_noticed.entry(task_id.clone()).or_insert(0);
                *seen = (*seen).max(*run);
            }
        }
        // The LAST SessionStart is this session's own id — a fork's log
        // begins with the parent's SessionStart, so first-match would
        // hand the resumed fork its parent's prompt-cache key.
        let cache_key = events
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                EventKind::SessionStart { session_id, .. } => Some(session_id.clone()),
                _ => None,
            })
            .or_else(|| {
                session_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            });
        let log = EventLog::open(session_dir.join("events.jsonl"))?;
        let ledger = Ledger::open(session_dir.join("ledger.jsonl"))?;
        let tools = Self::registry(&config);
        let system = crate::prompt::assemble(&config);
        // Seed the spill counter past existing files so resume can't
        // overwrite earlier spilled output.
        let spill_seq = std::fs::read_dir(session_dir.join("tool-outputs"))
            .map(|d| d.count() as u64)
            .unwrap_or(0);
        let subagent_seq = crate::tools::task::sidecar::max_seq(&session_dir.join("subagents"));
        let spend = Arc::new(crate::tools::task::SpendAccount::new(
            config.max_cost_usd,
            ledger.total_cost_usd,
        ));
        let mut agent = Agent {
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
            effort_boost: 0,
            bg_noticed,
            pending_compact: false,
            ctx_wall_stop: false,
            spill_seq,
            subagent_seq,
            spend,
            verify_blocks: 0,
            checkpoint: None,
            control: Control::default(),
            system,
            cache_key,
            run_cache_start: crate::ledger::CacheStats::default(),
        };
        agent.reconcile_subagents()?;
        Ok(agent)
    }

    /// Swap the tool registry — used to spawn read-only subagents with a
    /// quarantined toolset (playbook Ch.3 §9.6).
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    /// Attach a frontend steering handle (P2.4). Frontends mint a fresh
    /// `Control` per run so a consumed interrupt can't leak into the next
    /// turn; queued steering survives an interrupt (stop ≠ clear-queue).
    pub fn set_control(&mut self, control: Control) {
        self.control = control;
    }

    /// The current steering handle (queue-strip introspection).
    pub fn control(&self) -> &Control {
        &self.control
    }

    /// Swap the permission preset live (TUI mode badge, P2.7). Rebuilds
    /// the registry — plan mode removes mutating tools from the spec list
    /// (capability removal). Session-scoped approvals reset with the new
    /// policy: changing modes is a trust-boundary change.
    pub fn set_preset(&mut self, preset: crate::perm::Preset) {
        self.config.policy_preset = preset;
        self.tools = Self::registry(&self.config);
    }

    /// P8-B (crush `set_model`): switch the run's model mid-session. The
    /// audit event carries the from/to ids plus the new model's list
    /// prices (`profile::lookup`), so a session's cost steps are
    /// attributable without re-deriving them from the table later.
    /// Switching to the current model is a no-op (no event). The static
    /// system prefix re-assembles: the edit-format contract is per model,
    /// and a new model has no cache to keep warm.
    pub fn set_model(
        &mut self,
        model: &str,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<()> {
        if model == self.config.model {
            return Ok(());
        }
        let p = profile::lookup(model);
        let from = std::mem::replace(&mut self.config.model, model.to_string());
        self.system = crate::prompt::assemble(&self.config);
        self.emit(
            EventKind::ModelSwitch {
                from,
                to: model.to_string(),
                price_in: p.price.input,
                price_out: p.price.output,
            },
            on_event,
        )
    }

    /// P8-B (roo modes): switch the session's posture — toolset by
    /// capability removal, optional model, `edit_globs`, and the mode's
    /// framing line as a logged Nudge. The frozen prompt ORDER is never
    /// touched (that is why the framing is a Nudge, not a segment), and a
    /// switch rebuilds the toolset from the resident set so `--no-tools`
    /// ablations survive it.
    pub fn set_mode(
        &mut self,
        mode: &'static crate::modes::Mode,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<()> {
        self.tools.set_mode(mode, &self.config.disabled_tools);
        if let Some(m) = mode.model {
            self.set_model(m, on_event)?;
        }
        let text = format!("[mode {}] {}", mode.name, mode.prompt_frag);
        self.messages.push(Message::user_text(text.clone()));
        self.emit(EventKind::Nudge { text }, on_event)
    }

    /// The active session mode (None = default posture).
    pub fn mode(&self) -> Option<&'static crate::modes::Mode> {
        self.tools.mode
    }

    /// Session token totals by cache class (see [`crate::ledger::CacheStats`]).
    pub fn cache_stats(&self) -> crate::ledger::CacheStats {
        self.ledger.cache_stats()
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Record a user-role message without running a turn (P6-5): interview
    /// answers land in the log — written, flushed, fsynced — before any
    /// persona file is drafted, so a crash mid-interview loses nothing.
    pub fn record_user_input(
        &mut self,
        text: &str,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<()> {
        self.messages.push(Message::user_text(text));
        self.emit(EventKind::UserInput { text: text.into() }, on_event)?;
        self.log.flush()
    }

    /// Record harness-authored user-role text without running a turn
    /// (P6-5): the onboarding questions. `Nudge` is provably not user-typed
    /// (same durability contract as `record_user_input`).
    pub fn record_nudge(
        &mut self,
        text: &str,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<()> {
        self.messages.push(Message::user_text(text));
        self.emit(EventKind::Nudge { text: text.into() }, on_event)?;
        self.log.flush()
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
        // Per-run cache baseline — RunEnd emits the delta against this.
        self.run_cache_start = self.ledger.cache_stats();

        // P1.9 checkpointing: open a fresh checkpoint per user prompt,
        // named by the input's event id — that id is also the conversation
        // boundary a rewind truncates to.
        if let Some(boundary) = self.log.last().map(|e| e.id) {
            self.checkpoint = Some(crate::tools::Checkpoint {
                dir: self
                    .session_dir
                    .join("checkpoints")
                    .join(format!("e{boundary}")),
                done: Default::default(),
            });
        }

        // P8-B microagents (OpenHands microagent pattern): repo-authored
        // instructions whose triggers match this prompt ride in as a
        // harness Nudge — logged, provenance-wrapped, and replayed
        // identically on resume. Only the turns they were written for.
        let micros = crate::microagent::matching(&self.config.cwd, input);
        if !micros.is_empty() {
            let text = crate::microagent::render(&micros);
            self.messages.push(Message::user_text(text.clone()));
            self.emit(EventKind::Nudge { text }, on_event)?;
        }
        self.memory_notices(Some(input), on_event)?;

        let mut steps = 0u32;

        loop {
            if let Some(out) = self.loop_boundary(steps, on_event)? {
                return Ok(out);
            }
            let resp = match self.request_with_retry(steps, on_event)? {
                Ok(r) => r,
                Err(out) => return Ok(out),
            };
            steps += 1;
            let stop_reason = self.record_response(resp, on_event)?;

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
                match self.on_final_response(&stop_reason, steps, on_event)? {
                    Some(out) => return Ok(out),
                    None => continue,
                }
            }
            self.empty_responses = 0;

            // Stuck check on the response itself (context-window errors);
            // any trip is handled AFTER the tool batch so every tool_use
            // still gets its tool_result (Anthropic's pairing rule).
            let stuck_hit = self
                .stuck
                .observe_response(true, stop_reason == StopReason::ContextWindowExceeded);
            let stuck_hit = self.dispatch_tools(&calls, stuck_hit, on_event)?;
            if let Some(pattern) = stuck_hit {
                if let Some(outcome) = self.on_stuck(pattern, steps, on_event)? {
                    return Ok(outcome);
                }
            }
            self.end_turn(steps, on_event)?;
        }
    }

    /// Loop-top boundary: steering (interrupt, queued input), background
    /// notices, the step and cost budgets, the compaction point and stale
    /// tool-result clearing. `Some` ends the run.
    fn loop_boundary(
        &mut self,
        steps: u32,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<Option<RunOutcome>> {
        // Steering boundary (P2.4): interrupt ends the run; queued
        // user input lands here as ordinary user messages — after the
        // previous batch's tool_results, so provider pairing holds.
        if self.control.interrupted() {
            self.end_run("interrupted", steps, on_event)?;
            return Ok(Some(RunOutcome::Interrupted {
                steps,
                cost_usd: self.ledger.total_cost_usd,
            }));
        }
        for text in self.control.take_steer() {
            self.messages.push(Message::user_text(text.clone()));
            self.emit(EventKind::UserInput { text: text.clone() }, on_event)?;
            self.memory_notices(Some(&text), on_event)?;
        }
        self.drain_bg_notices(on_event)?;
        self.memory_notices(None, on_event)?;

        if steps >= self.config.max_steps {
            let out = RunOutcome::StepBudgetExceeded {
                steps,
                cost_usd: self.ledger.total_cost_usd,
            };
            self.end_run("max_steps", steps, on_event)?;
            return Ok(Some(out));
        }
        // Own + settled subagent spend (both in the ledger) + caps still
        // reserved for subagents in flight.
        if self.ledger.total_cost_usd + self.spend.reserved_usd() >= self.config.max_cost_usd {
            let out = RunOutcome::CostBudgetExceeded {
                steps,
                cost_usd: self.ledger.total_cost_usd,
            };
            self.end_run("max_cost", steps, on_event)?;
            return Ok(Some(out));
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
                return Ok(Some(RunOutcome::Completed {
                    steps,
                    cost_usd: self.ledger.total_cost_usd,
                }));
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
        // F5: keep only the last two computer images in the view —
        // each capture's envelope text stays on its ToolResult, so
        // eliding pixels never breaks tool pairing. Same view-only,
        // idempotent contract as the clearing above.
        crate::event::cap_image_blocks(&mut self.messages, 2);
        Ok(None)
    }

    /// Build the request and send it, re-issuing once on a Malformed
    /// response. A provider error is logged and ends the run (`Err`).
    fn request_with_retry(
        &mut self,
        steps: u32,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<Result<crate::provider::Response, RunOutcome>> {
        // The frozen static prefix (assembled at start/resume): every
        // request of this Agent sends identical system bytes.
        let req = Request {
            model: &self.config.model,
            system: &self.system,
            tools: &self.tools.specs,
            messages: &self.messages,
            max_tokens: self.config.max_output_tokens,
            thinking_budget: self.config.thinking_budget,
            effort: Some(self.effort_now()),
            cache_breakpoints: true,
            cache_key: self.cache_key.clone(),
        };

        // B1-2 (Instructor retry): on a Malformed response only, re-issue
        // the same request once per provider call (flag scoped to this
        // invocation, reset before every call — multi-Malformed sequences
        // retry each call once, never loop). The retry is a normal step:
        // it increments `steps` and records ledger usage on success.
        // Malformed responses are prompt-adjacent, so the retry request
        // stays in the dynamic segment — the static prefix is untouched.
        let mut malformed_retried = false;
        let resp = loop {
            match self.provider.complete(&req) {
                Ok(r) => break r,
                Err(crate::provider::ProviderError::Malformed(_msg)) if !malformed_retried => {
                    malformed_retried = true;
                    continue;
                }
                Err(e) => {
                    let msg = e.to_string();
                    self.emit(
                        EventKind::Error {
                            message: msg.clone(),
                        },
                        on_event,
                    )?;
                    self.end_run("provider_error", steps, on_event)?;
                    return Ok(Err(RunOutcome::Provider(msg)));
                }
            }
        };
        Ok(Ok(resp))
    }

    /// Ledger usage, the compaction triggers, then the assistant message
    /// and its ModelResponse event. Returns the raw stop reason.
    fn record_response(
        &mut self,
        resp: crate::provider::Response,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<StopReason> {
        // Re-read the profile every iteration: `set_model` can switch
        // the model mid-session (P8-B), which changes both the cost
        // math and the compaction trigger.
        let profile = profile::lookup(&self.config.model);
        let cost = profile.cost_usd(&resp.usage);
        self.ledger.record(UsageRecord::from_usage(
            &self.config.model,
            &resp.usage,
            resp.request_bytes,
            resp.latency_ms,
            count_tool_calls(&resp.blocks) as u32,
            cost,
        ))?;
        self.spend.sync(self.ledger.total_cost_usd);

        // Effective-window budget (playbook Ch.3 §9.2): trigger on the
        // *measured* prompt size from the last call, not an estimate.
        // A provider context-window stop also forces compaction.
        self.ctx_wall_stop = resp.stop_reason == StopReason::ContextWindowExceeded;
        if self.config.auto_compact {
            let frac = self.config.compact_at.unwrap_or(profile.compact_at);
            let budget = frac as f64 * f64::from(profile.context_in);
            // B1-9a: estimator pre-trigger — when measured usage is not
            // yet over budget but the token estimate is, compact early
            // rather than risk a context-wall stop mid-turn.
            let est_tokens = crate::tokens::count_tokens(
                &prompt_text_for_estimate(&self.messages),
                &self.config.model,
            ) as f64;
            if self.ctx_wall_stop || resp.usage.total_input() as f64 > budget || est_tokens > budget
            {
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
        Ok(resp.stop_reason)
    }

    /// A response with no tool calls: the silent-END guard, the
    /// context-window continue, then the stop gates (verify command, stop
    /// hook). `None` loops again; `Some` ends the run.
    fn on_final_response(
        &mut self,
        stop_reason: &StopReason,
        steps: u32,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<Option<RunOutcome>> {
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
                return Ok(Some(RunOutcome::EmptyResponse {
                    steps,
                    cost_usd: self.ledger.total_cost_usd,
                }));
            }
            let text = "[overseer] Your previous turn produced no visible \
                        output and no tool calls. Continue working — act, \
                        or explain what is blocking you."
                .to_string();
            self.messages.push(Message::user_text(text.clone()));
            self.emit(EventKind::Nudge { text }, on_event)?;
            return Ok(None);
        }
        self.empty_responses = 0;
        // A context-window stop isn't a finish — the model was cut
        // off mid-generation. Compact and let it continue (the loop
        // exits via ctx_wall_stop if nothing can be dropped).
        if *stop_reason == StopReason::ContextWindowExceeded {
            self.pending_compact = true;
            return Ok(None);
        }
        match self.verify_gate(steps, on_event)? {
            Gate::Pass => {}
            Gate::Blocked => return Ok(None),
            Gate::Stop(out) => return Ok(Some(out)),
        }
        // Stop hook (ECC `stop` event): a data rule may veto the
        // stop itself on the final text. Blocks share the verify
        // budget — one cap covers every stop-blocker — and hooks
        // fail open at the cap (guardrail, not the gate): the
        // stop is then allowed through.
        if let Some(reason) =
            crate::hooks::maybe_block_stop(&self.tools.hooks, &last_text(&self.messages))
        {
            self.verify_blocks += 1;
            if self.verify_blocks < self.config.verify_block_cap {
                let text = format!(
                    "[overseer] Stop blocked by hooks rule — {reason} \
                     (block {}/{}). Address it, then finish.",
                    self.verify_blocks, self.config.verify_block_cap
                );
                self.messages.push(Message::user_text(text.clone()));
                self.emit(EventKind::Nudge { text }, on_event)?;
                return Ok(None);
            }
        }
        self.end_run(stop_reason.as_str(), steps, on_event)?;
        Ok(Some(RunOutcome::Completed {
            steps,
            cost_usd: self.ledger.total_cost_usd,
        }))
    }

    /// P1.10 verification gate: run the definition-of-done command before
    /// the model may stop. A failure blocks the stop (nudge + reflection);
    /// at the block cap the run ends.
    fn verify_gate(
        &mut self,
        steps: u32,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<Gate> {
        // P1.10 verification gate: the model wants to stop — run the
        // definition-of-done check first. A failure blocks the stop;
        // the failing output goes back into context as a nudge.
        if let Some(cmd) = self.config.verify_cmd.clone() {
            // I4: verify runs through the bash sandbox wrapper with the
            // bash child-env allowlist — no provider/broker/checkpoint.
            let ctx = ToolCtx {
                cwd: self.config.cwd.clone(),
                session_dir: self.session_dir.clone(),
                spill_seq: 0,
                provider: None,
                agent_config: Some(self.config.clone()),
                subagents: Default::default(),
                checkpoint: None,
                sandbox: self.config.sandbox_bash,
                broker: None,
            };
            if let Err(tail) = run_verify(&cmd, &ctx) {
                self.verify_blocks += 1;
                if self.verify_blocks >= self.config.verify_block_cap {
                    self.end_run("verify_failed", steps, on_event)?;
                    return Ok(Gate::Stop(RunOutcome::VerifyFailed {
                        steps,
                        cost_usd: self.ledger.total_cost_usd,
                    }));
                }
                let text = format!(
                    "[overseer] Verification failed — `{cmd}` did not pass \
                     (block {}/{}). Output:\n{tail}\nFix the failures, \
                     then finish.",
                    self.verify_blocks, self.config.verify_block_cap
                );
                self.messages.push(Message::user_text(text.clone()));
                self.emit(EventKind::Nudge { text }, on_event)?;
                // B1-7: post-episode verbal-RL hook — a bounded
                // aux-tier critique of this failed attempt, tagged
                // so the keep-last-1 eviction never touches
                // verify-tail/empty/stuck Nudges. small_model None
                // → skip (zero spend change by default).
                self.reflect(&tail, on_event)?;
                return Ok(Gate::Blocked);
            }
        }
        Ok(Gate::Pass)
    }

    /// Execute one tool batch; every tool_use gets a tool_result (synthetic
    /// when an interrupt or queued input skips the rest). Returns the first
    /// stuck pattern seen, if any.
    fn dispatch_tools(
        &mut self,
        calls: &[(String, String, serde_json::Value)],
        mut stuck_hit: Option<crate::stuck::StuckPattern>,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<Option<crate::stuck::StuckPattern>> {
        // Execute tool calls; results merge into one user message.
        // The checkpoint is taken out of self for the batch so ctx can
        // borrow it while emit() still has &mut self.
        let mut checkpoint = self.checkpoint.take();
        let mut ctx = ToolCtx {
            cwd: self.config.cwd.clone(),
            session_dir: self.session_dir.clone(),
            spill_seq: self.spill_seq,
            provider: Some(self.provider.clone()),
            agent_config: Some(self.config.clone()),
            subagents: crate::tools::task::SubagentCtx {
                seq: self.subagent_seq,
                spend: Some(self.spend.clone()),
            },
            checkpoint: checkpoint.as_mut(),
            sandbox: self.config.sandbox_bash,
            broker: Some(self.config.broker.clone()),
        };
        self.tools.set_control(self.control.clone());
        let mut results = Vec::new();
        for (idx, (call_id, name, input)) in calls.iter().enumerate() {
            // Tool-launch boundary (P2.4): an interrupt or a queued
            // steer skips every remaining call with a synthetic
            // result — every tool_use still gets its tool_result, so
            // the provider pairing rule survives the truncation.
            // The loop-top check then ends the run or injects input.
            if self.control.interrupted() || self.control.steer_pending() {
                let skipped = if self.control.interrupted() {
                    "[skipped: interrupted by user]"
                } else {
                    "[skipped: new user input arrived]"
                };
                for (call_id, name, _) in &calls[idx..] {
                    self.emit(
                        EventKind::ToolCallStart {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            input: serde_json::Value::Null,
                        },
                        on_event,
                    )?;
                    self.emit(
                        EventKind::ToolResult {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            content: skipped.to_string(),
                            is_error: true,
                            raw_bytes: skipped.len() as u64,
                            spilled_to: None,
                            denied: false,
                        },
                        on_event,
                    )?;
                    results.push(Block::ToolResult {
                        tool_use_id: call_id.clone(),
                        content: crate::tools::provenance_wrap(name, skipped),
                        is_error: true,
                    });
                }
                break;
            }

            self.emit(
                EventKind::ToolCallStart {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
                on_event,
            )?;

            let out = self.tools.call(name, input, &mut ctx);
            // Behaviour keyed on a tool name follows the tool that ran
            // (`tools op=call` unwraps); the events keep the outer call.
            let (ran, ran_input) = crate::tools::effective_call(name, input);
            // P7-3: computer-use acts are auditable — the tool's
            // envelope carries the serving tier and the pre/post
            // observation digests (audit-only; never rehydrated).
            // Only this tool's results are parsed (a bash echo of a
            // similar object must not forge an audit record), and
            // error/unconfigured results skip.
            if ran == "computer" {
                if let Some(kind) = crate::tools::computer::audit_event(&out.text) {
                    self.emit(kind, on_event)?;
                }
            }
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
            // Audit-only record of each `run_code` sub-call.
            for rec in self.tools.take_script_calls() {
                self.emit(
                    EventKind::ScriptCall {
                        parent_call_id: call_id.clone(),
                        name: rec.name,
                        input_digest: rec.input_digest,
                        is_error: rec.is_error,
                        denied: rec.denied,
                        raw_bytes: rec.raw_bytes,
                    },
                    on_event,
                )?;
            }

            if stuck_hit.is_none() {
                stuck_hit = self
                    .stuck
                    .observe_step(ran, ran_input, out.is_error, &out.text);
            }

            results.push(Block::ToolResult {
                tool_use_id: call_id.clone(),
                content: crate::tools::provenance_wrap(name, &out.text),
                is_error: out.is_error,
            });

            // S5/D4: computer captures ride the conversation as a
            // sibling user-level image block — pixels never inline
            // into the tool-result text (budget) or events.jsonl.
            if ran == "computer" {
                if let Some(img) = crate::tools::computer::image_block(&out.text) {
                    results.push(img);
                }
            }

            // Rule-of-Two latch flips are auditable events (P3.10).
            let notices: Vec<String> = std::mem::take(&mut self.tools.taint_notices);
            for notice in notices {
                self.emit(EventKind::Tainted { detail: notice }, on_event)?;
            }
        }
        self.messages.push(Message::tool_results(results));
        // Carry the session-monotonic spill counter forward; hand the
        // checkpoint back for the next iteration.
        self.spill_seq = ctx.spill_seq;
        self.subagent_seq = ctx.subagents.seq;
        self.checkpoint = checkpoint;
        Ok(stuck_hit)
    }

    /// Turn-end bookkeeping at the durable boundary: TurnEnd, the grant
    /// clock, the log flush, then the memory commit.
    fn end_turn(&mut self, steps: u32, on_event: &mut dyn FnMut(&Event)) -> std::io::Result<()> {
        self.emit(EventKind::TurnEnd { step: steps }, on_event)?;
        // P8-B: one turn elapsed — the clock a TTL'd session grant
        // (`AllowEntry::expires_turn`) reads. Bumped at the same
        // durable boundary as the log flush.
        self.tools.policy.tick_turn();
        self.log.flush()?;
        // Git-version memory at the durable-tail point (playbook: free
        // history/diff/rollback). Engine-made commit, best-effort.
        // P6-2 audit: a dirty worktree at the boundary emits a
        // log-only MemoryUpdated event (never injected into context).
        // The dirty set is captured BEFORE the commit — afterwards the
        // worktree is clean by construction.
        let mut files = Vec::new();
        for (scope, dir) in crate::memory::stores::of_config(&self.config) {
            let dirty = crate::memory::dirty_files(&dir);
            crate::memory::commit(&dir, &format!("turn {steps}"));
            files.extend(dirty.into_iter().map(|f| format!("{}:{f}", scope.name())));
        }
        if !files.is_empty() {
            self.emit(EventKind::MemoryUpdated { files }, on_event)?;
        }
        Ok(())
    }

    /// Memory v2 notices at a durable boundary. `Some(input)` (after the
    /// checkpoint capture): recall plus due `at:`/`kw:` reminders. `None`
    /// (loop top, after the batch's results were pushed): queued `path:`
    /// reminders — never mid-batch, so replay groups results identically.
    fn memory_notices(
        &mut self,
        input: Option<&str>,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<()> {
        self.tools.memory.init(&self.config, &self.session_dir);
        let notices = match input {
            Some(text) => self.tools.memory.on_input(text, crate::memory::now_secs()),
            None => self.tools.memory.take_queued(),
        };
        for n in notices {
            self.messages.push(Message::user_text(n.text.clone()));
            let kind = EventKind::MemoryNotice {
                kind: n.kind.into(),
                notes: n.notes,
                text: n.text,
            };
            self.emit(kind, on_event)?;
        }
        Ok(())
    }

    /// Effective effort for the next request: configured level plus one
    /// bump per stuck-detector trip (per-run adaptive effort).
    fn effort_now(&self) -> crate::provider::Effort {
        let mut e = self
            .config
            .effort
            .unwrap_or(crate::provider::Effort::Medium);
        for _ in 0..self.effort_boost {
            e = e.bumped();
        }
        e
    }

    /// Nudge tag for B1-7 reflection critiques. Stable prefix — the
    /// keep-last-1 eviction matches only this tag, so verify-tail, empty-
    /// response, and stuck Nudges are never evicted.
    pub const REFLECTION_TAG: &str = "[reflection]";

    /// B1-7 Reflexion hook: one bounded aux-tier critique of a failed
    /// verify block, appended as a tagged Nudge. Live view AND resume view
    /// stay identical: pushes to `messages` + appends the event, then prunes
    /// older tagged critiques from BOTH (keep last 1). Log bytes are never
    /// rewritten — eviction is view-only (messages drain + rehydrate rule).
    /// small_model None → Ok (skip). Respects `reflect: Off`.
    /// ≤1 aux call per verify block, ≤300-token critique, 1024-token request.
    fn reflect(
        &mut self,
        verify_tail: &str,
        on_event: &mut dyn FnMut(&Event),
    ) -> std::io::Result<()> {
        if self.config.reflect == ReflectMode::Off {
            return Ok(());
        }
        if self.config.small_model.is_none() {
            return Ok(()); // aux-tier-only: no small model → no critique
        }
        let prompt = format!(
            "The last attempt failed verification. Output tail:\n{verify_tail}\n\
             List the 2 most likely defects and the single most useful next fix. \
             Keep it under 300 tokens, actionable and specific."
        );
        // D2: small tier DIRECTLY — never aux_call (which escalates to the
        // main model on small-tier failure/empty, bypassing ledger). A
        // failed/empty small critique fail-softs to no-critique; the verify
        // Nudge remains. Zero main-model spend, by construction.
        let small = self.config.small_model.clone().expect("checked above");
        let msgs = [Message::user_text(prompt)];
        let req = Request {
            model: &small,
            system: &[],
            tools: &[],
            messages: &msgs,
            max_tokens: 1_024,
            thinking_budget: None,
            effort: Some(crate::provider::Effort::Min),
            cache_breakpoints: false,
            cache_key: None,
        };
        let critique = match self.provider.complete(&req) {
            Ok(r) => {
                let t: String = r
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
                    .chars()
                    .take(1200)
                    .collect();
                if t.trim().is_empty() {
                    return Ok(());
                }
                t
            }
            Err(_) => return Ok(()), // fail-soft: the verify Nudge remains
        };
        let text = format!("{} {critique}", Self::REFLECTION_TAG);
        self.messages.push(Message::user_text(text.clone()));
        self.emit(EventKind::Nudge { text }, on_event)?;
        // Dual eviction (keep last 1 tagged): live messages drain + the
        // rehydrate rule mirrors it. Untagged Nudges untouched.
        Self::prune_reflections(&mut self.messages);
        Ok(())
    }

    /// Prune older `[reflection]`-tagged user messages, keeping the last 1.
    /// Live-view half of the B1-7 dual eviction (rehydrate mirrors it).
    /// Superseded critiques are DRAINED (removed), not blanked — after two
    /// verify blocks the live message list holds exactly 1 tagged critique,
    /// byte-identical to what rehydrate replays. Nudges carry no
    /// tool_use/tool_result blocks, so provider pairing is unaffected.
    fn prune_reflections(messages: &mut Vec<Message>) {
        use crate::ir::Block;
        let last = messages.iter().rposition(|m| {
            m.content.iter().any(|b| match b {
                Block::Text { text } => text.starts_with(Self::REFLECTION_TAG),
                _ => false,
            })
        });
        if let Some(keep) = last {
            let mut idx = 0;
            messages.retain(|m| {
                let is_ref = m.content.iter().any(|b| match b {
                    Block::Text { text } => text.starts_with(Self::REFLECTION_TAG),
                    _ => false,
                });
                let cur = idx;
                idx += 1;
                !is_ref || cur == keep
            });
        }
    }

    /// Small-tier call (P3.2): one stateless request on `small_model`
    /// for titles/consolidation/guardrails. Escalates to the main model
    /// when the small call fails — the tier contract is "cheap first,
    /// correct always". Min effort; these calls never need reasoning.
    pub fn aux_call(&self, prompt: &str) -> Result<String, crate::provider::ProviderError> {
        let msgs = [Message::user_text(prompt)];
        if let Some(small) = &self.config.small_model {
            let req = Request {
                model: small,
                system: &[],
                tools: &[],
                messages: &msgs,
                max_tokens: 1_024,
                thinking_budget: None,
                effort: Some(crate::provider::Effort::Min),
                cache_breakpoints: false,
                cache_key: None,
            };
            // Small tier first; empty text or a provider error escalates.
            if let Ok(r) = self.provider.complete(&req) {
                let text = r
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if !text.trim().is_empty() {
                    return Ok(text);
                }
            }
        }
        let req = Request {
            model: &self.config.model,
            system: &[],
            tools: &[],
            messages: &msgs,
            max_tokens: 1_024,
            thinking_budget: None,
            effort: Some(crate::provider::Effort::Min),
            cache_breakpoints: false,
            cache_key: None,
        };
        let r = self.provider.complete(&req)?;
        Ok(r.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect())
    }

    /// Bring every finished subagent's spend into this ledger — one
    /// settlement row per unsettled delta, idempotent across resumes (the
    /// ledger replays what it settled) — and drop its reservation. Dead
    /// background tasks are reaped first so their slots free.
    fn reconcile_subagents(&mut self) -> std::io::Result<()> {
        use crate::tools::task::{budget::MIN_CAP_USD, sidecar};
        let dir = self.session_dir.join("subagents");
        sidecar::reap_dead(&dir);
        for (_, sc) in sidecar::all(&dir) {
            if sc.is_live() {
                // Still running in this process: a rebuilt agent re-holds
                // its cap (a no-op when the reservation already exists).
                self.spend.restore(&sc.id, sc.cap_usd, MIN_CAP_USD);
                continue;
            }
            if sc.state == sidecar::State::Running {
                continue;
            }
            self.ledger.settle(&sc.id, &sc.model, sc.cost_usd)?;
            self.spend.settle(&sc.id, self.ledger.total_cost_usd);
        }
        self.spend.sync(self.ledger.total_cost_usd);
        Ok(())
    }

    /// Fire-and-notify delivery (P3.4): settle finished subagents, then
    /// turn each background run's new done marker into a user message +
    /// a SubagentDone event so resumes replay it identically.
    fn drain_bg_notices(&mut self, on_event: &mut dyn FnMut(&Event)) -> std::io::Result<()> {
        use crate::tools::task::{done_marker, sidecar, Footer};
        self.reconcile_subagents()?;
        for (path, sc) in sidecar::all(&self.session_dir.join("subagents")) {
            if !sc.background || self.bg_noticed.get(&sc.id).is_some_and(|r| *r >= sc.run) {
                continue;
            }
            let marker = path.join(done_marker(sc.run));
            let digest = match std::fs::read_to_string(&marker) {
                Ok(d) => d,
                Err(_) if sc.state == sidecar::State::Running || sidecar::delivering(&path) => {
                    continue;
                }
                Err(_) => {
                    let lost = format!("[subagent {} finished but its digest was lost]", sc.id);
                    let _ = std::fs::write(&marker, &lost);
                    lost
                }
            };
            self.bg_noticed.insert(sc.id.clone(), sc.run);
            let footer = Footer::parse(&digest);
            let text = format!("[subagent {} finished]\n{digest}", sc.id);
            self.messages.push(Message::user_text(text));
            self.emit(
                EventKind::SubagentDone {
                    cost_usd: footer
                        .as_ref()
                        .and_then(|f| f.cost_usd)
                        .unwrap_or(sc.cost_usd),
                    tier: footer
                        .as_ref()
                        .map_or(sc.tier.as_str().to_string(), |f| f.tier.clone()),
                    model: footer
                        .as_ref()
                        .map_or(sc.model.clone(), |f| f.model.clone()),
                    verdict: footer.and_then(|f| f.verdict),
                    run: sc.run,
                    task_id: sc.id,
                    trace: path.display().to_string(),
                },
                on_event,
            )?;
        }
        Ok(())
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
        // Failure signal ⇒ next steps run one notch harder (P3.2
        // escalation). Deterministic, bounded at Max.
        self.effort_boost = self.effort_boost.saturating_add(1);
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

    /// Registry for the configured preset — plan mode removes mutating
    /// tools from the spec list entirely (capability removal, P1.4).
    /// R7: the configured MCP servers join the **core** registry only. Plan
    /// mode is a read-only posture with its own spec set, and the subagent
    /// registries are quarantined by design — neither gets the `mcp` op tool,
    /// so neither can spawn a third-party program.
    fn registry(config: &AgentConfig) -> ToolRegistry {
        let policy = Self::policy(config);
        let plan_mode = !config.full_access && config.policy_preset == crate::perm::Preset::Plan;
        let mut reg = if plan_mode {
            ToolRegistry::plan_mode(policy)
        } else {
            // Skills are detected under the session cwd — the root
            // `prompt::assemble` indexes — so the `skill` spec and the
            // skills segment always agree (full access's policy root is `/`).
            ToolRegistry::core_in(policy, &config.cwd)
        };
        if !config.disabled_tools.is_empty() {
            reg.disable(&config.disabled_tools);
        }
        if !plan_mode {
            // `--no-tools mcp` survives this: `with_mcp` rebuilds the
            // advertised list from the resident set minus the disabled set.
            reg = reg.with_mcp(config.mcp_servers.clone());
        }
        reg
    }

    fn policy(config: &AgentConfig) -> crate::perm::Policy {
        if config.full_access {
            crate::perm::Policy::allow_all()
        } else {
            let mut p = crate::perm::Policy::preset(config.policy_preset, config.cwd.clone());
            p.ask_handler = config.ask_handler.clone();
            p.autonomy = config.autonomy.clone();
            p.memory_dir = config.memory_dir.clone();
            p.memory_readonly = config.is_subagent;
            // P6-5: the draft gate needs the dir and the approval verdict —
            // computed once here so a mid-session `--approve` is a restart
            // (approval is a trust-boundary change, like a preset swap).
            p.persona_dir = config.persona_dir.clone();
            p.persona_approved = config
                .persona_dir
                .as_deref()
                .is_some_and(crate::onboard::all_approved);
            if let Some(path) = &config.rules_path {
                p.load_rules(path.clone());
            }
            p
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
        self.reconcile_subagents()?;
        self.emit(
            EventKind::RunEnd {
                stop_reason: stop.into(),
                steps,
                total_cost_usd: self.ledger.total_cost_usd,
                subagent_cost_usd: self.ledger.subagent_cost_usd,
                cache: self
                    .ledger
                    .cache_stats()
                    .saturating_delta(&self.run_cache_start),
            },
            on_event,
        )?;
        self.log.flush()?;
        self.write_episode();
        Ok(())
    }

    /// Memory v2 episode note, rewritten from the log at each run end
    /// (project store; never in subagents). Best-effort, like the commit.
    fn write_episode(&self) {
        let Some(dir) = self
            .config
            .memory_dir
            .as_deref()
            .filter(|_| !self.config.is_subagent)
        else {
            return;
        };
        if let Ok(events) = EventLog::replay(self.log.path()) {
            if let Ok(Some(_)) = crate::memory::episode::write(dir, &events) {
                crate::memory::commit(dir, "episode");
            }
        }
    }
}

/// Serialize the current message view for the B1-9 token estimate.
/// Runs once per turn at the budget checkpoint — never in hot loops.
fn prompt_text_for_estimate(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        for b in &m.content {
            match b {
                Block::Text { text } => out.push_str(text),
                Block::ToolResult { content, .. } => out.push_str(content),
                Block::ToolCall { name, input, .. } => {
                    out.push_str(name);
                    out.push_str(&input.to_string());
                }
                // Screenshots are pixels, not tokens of text — the estimate
                // counts nothing for them (image cost lands in P7-2 usage).
                Block::Image { .. } => {}
                Block::Reasoning { .. } => {}
            }
        }
    }
    out
}

fn count_tool_calls(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .filter(|b| matches!(b, Block::ToolCall { .. }))
        .count()
}

/// Text of the last assistant message — what a stop hook observes as the
/// session's "final result" (the `tool` side of the rule never matches a
/// real dispatch, so payload = the text the model ended on).
fn last_text(messages: &[Message]) -> String {
    messages
        .last()
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// P1.10 verification gate runner: execute the definition-of-done command
/// in the session cwd via `sh -c`. Output goes to a temp file (not a pipe)
/// so a verbose suite can't deadlock on a full buffer; a 120s watchdog
/// kills runaway checks. `Ok(())` = exit 0; `Err(tail)` = nonzero exit,
/// spawn failure, or timeout — the tail keeps the last ~6K chars of output
/// so the failure stays reviewable when injected back into context.
// I4: `verify_cmd` goes through the same sandbox bash uses (pinned
// --runtime when set, else the platform default — an unavailable pinned
// backend fails closed, never a silent downgrade) and inherits only
// bash's child-env allowlist, so parent secrets can't reach repo checks.
fn run_verify(cmd: &str, ctx: &crate::tools::ToolCtx) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let argv = vec!["sh".to_string(), "-c".to_string(), cmd.to_string()];
    let inv = crate::tools::bash::sandboxed(&argv, ctx)?;

    let log_path =
        std::env::temp_dir().join(format!("overseer-verify-{}.log", uuid::Uuid::now_v7()));
    let file =
        std::fs::File::create(&log_path).map_err(|e| format!("cannot create verify log: {e}"))?;
    let err_file = file
        .try_clone()
        .map_err(|e| format!("cannot clone verify log handle: {e}"))?;
    let mut child = Command::new(&inv.program)
        .args(&inv.args)
        .current_dir(&ctx.cwd)
        .env_clear()
        .envs(crate::tools::bash::child_env())
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err_file)
        .spawn()
        .map_err(|e| format!("cannot run `{cmd}`: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => break None,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&log_path);
                return Err(format!("verify wait failed: {e}"));
            }
        }
    };
    let tail = read_tail(&log_path, 6_000);
    let _ = std::fs::remove_file(&log_path);

    match status {
        Some(s) if s.success() => Ok(()),
        Some(s) => Err(format!("exit {s}\n{tail}")),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("timed out after 120s\n{tail}"))
        }
    }
}

/// Last `cap` chars of a file — best-effort, char-boundary safe.
fn read_tail(path: &std::path::Path, cap: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(cap as u64 * 2);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let s = String::from_utf8_lossy(&buf);
    s.chars()
        .skip(s.chars().count().saturating_sub(cap))
        .collect::<String>()
        .trim_end()
        .to_string()
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
        seen_messages: Mutex<Vec<Vec<Message>>>,
        seen_models: Mutex<Vec<String>>,
        seen_cache_keys: Mutex<Vec<Option<String>>>,
        /// Models that always error — drives the aux-call escalation path.
        fail_models: Vec<String>,
        /// Fail the next N calls with Malformed, then serve responses.
        /// Drives the B1-2 structured-retry path.
        malformed_first: Mutex<usize>,
    }

    impl Mock {
        fn new(responses: Vec<Response>) -> Self {
            Mock {
                responses: Mutex::new(VecDeque::from(responses)),
                seen_systems: Mutex::new(Vec::new()),
                seen_messages: Mutex::new(Vec::new()),
                seen_models: Mutex::new(Vec::new()),
                seen_cache_keys: Mutex::new(Vec::new()),
                fail_models: Vec::new(),
                malformed_first: Mutex::new(0),
            }
        }

        fn malformed_first(n: usize, responses: Vec<Response>) -> Self {
            let m = Self::new(responses);
            *m.malformed_first.lock().unwrap() = n;
            m
        }

        fn failing_on(models: &[&str], responses: Vec<Response>) -> Self {
            let mut m = Self::new(responses);
            m.fail_models = models.iter().map(|s| s.to_string()).collect();
            m
        }
    }

    impl Provider for Mock {
        fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
            self.seen_systems
                .lock()
                .unwrap()
                .push(req.system.iter().map(|s| s.text.clone()).collect());
            self.seen_models.lock().unwrap().push(req.model.to_string());
            self.seen_messages
                .lock()
                .unwrap()
                .push(req.messages.to_vec());
            self.seen_cache_keys
                .lock()
                .unwrap()
                .push(req.cache_key.clone());
            if self.fail_models.iter().any(|m| m == req.model) {
                return Err(ProviderError::Transport("mock fail".into()));
            }
            {
                let mut n = self.malformed_first.lock().unwrap();
                if *n > 0 {
                    *n -= 1;
                    return Err(ProviderError::Malformed("mock malformed".into()));
                }
            }
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
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
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
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        let out = agent.run_turn("work", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Completed { steps: 6, .. }));

        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Compaction { .. })));
    }

    /// P8-B accept (crush `set_model`): a mid-session model switch is
    /// audited with the new model's list prices, and the very next request
    /// carries the new model (the loop re-reads the profile each step, so
    /// cost math and the compaction trigger follow it).
    #[test]
    fn set_model_switches_and_audits_prices() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        let provider = Mock::new(vec![done()]);
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};

        // Same model → no event (nothing changed).
        agent.set_model("claude-sonnet-5", &mut sink).unwrap();
        agent.set_model("deepseek-v4.1-flash", &mut sink).unwrap();
        agent.run_turn("work", &mut sink).unwrap();

        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        let switches: Vec<(&str, &str, f64, f64)> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ModelSwitch {
                    from,
                    to,
                    price_in,
                    price_out,
                } => Some((from.as_str(), to.as_str(), *price_in, *price_out)),
                _ => None,
            })
            .collect();
        assert_eq!(
            switches.len(),
            1,
            "one switch, no no-op event: {switches:?}"
        );
        assert_eq!(switches[0].0, "claude-sonnet-5");
        assert_eq!(switches[0].1, "deepseek-v4.1-flash");
        // Prices come from the profile table (opencode rows are $0).
        let p = profile::lookup("deepseek-v4.1-flash");
        assert_eq!(switches[0].2, p.price.input);
        assert_eq!(switches[0].3, p.price.output);
        // The audit event never rehydrates into the model's context.
        let view = rehydrate_messages(&events);
        assert!(
            view.iter()
                .all(|m| !m.text().contains("deepseek-v4.1-flash")),
            "a model switch is provenance, not conversation"
        );
    }

    /// P8-B accept (roo modes): a mode switch removes tools from the spec
    /// list, moves the model when the mode names one, and states its
    /// posture as a logged Nudge — the static prompt ORDER is untouched.
    #[test]
    fn set_mode_removes_tools_moves_model_and_nudges() {
        static VERIFY_MODE: crate::modes::Mode = crate::modes::Mode {
            name: "verify-only",
            prompt_frag: "Only read and run checks; change nothing.",
            allowed_tools: &["read", "grep", "glob", "bash"],
            model: Some("deepseek-v4.1-flash"),
            edit_globs: &[],
        };
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        let provider = Mock::new(vec![done()]);
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        agent.set_mode(&VERIFY_MODE, &mut sink).unwrap();

        assert_eq!(agent.mode().map(|m| m.name), Some("verify-only"));
        let names: Vec<String> = agent.tools.specs.iter().map(|s| s.name.clone()).collect();
        for t in ["write", "edit", "task", "computer"] {
            assert!(!names.contains(&t.to_string()), "{t} must be removed");
        }
        assert!(names.contains(&"read".to_string()));
        assert_eq!(
            agent.config.model, "deepseek-v4.1-flash",
            "mode model override applies"
        );
        assert!(agent
            .messages()
            .iter()
            .any(|m| m.text().contains("[mode verify-only]")));

        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        assert!(events.iter().any(|e| matches!(
            &e.kind,
            EventKind::Nudge { text } if text.contains("[mode verify-only]")
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(&e.kind, EventKind::ModelSwitch { to, .. } if to == "deepseek-v4.1-flash")),
            "the mode's model rides the audited switch"
        );
    }

    /// P8-B accept (openhands microagent): a repo microagent whose trigger
    /// matches the prompt is injected as a provenance-wrapped Nudge — and
    /// only for matching prompts.
    #[test]
    fn microagent_trigger_injects_repo_guidance() {
        let dir = tmpdir();
        let md = dir.join(".overseer/microagents/style");
        std::fs::create_dir_all(&md).unwrap();
        std::fs::write(
            md.join("MICROAGENT.md"),
            "---\nname: style\ntriggers: flaky\n---\nAlways keep the flaky-test repro.\n",
        )
        .unwrap();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        let provider = Mock::new(vec![done()]);
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};

        agent
            .run_turn("the flaky test needs fixing", &mut sink)
            .unwrap();
        let nudges: Vec<String> = EventLog::replay(dir.join("events.jsonl"))
            .unwrap()
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Nudge { text } if text.contains("microagent") => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(nudges.len(), 1, "{nudges:?}");
        assert!(nudges[0].contains("Always keep the flaky-test repro."));
        assert!(
            nudges[0].contains("<microagent name=\"style\""),
            "{}",
            nudges[0]
        );

        // A prompt that does not match the trigger injects nothing.
        agent.run_turn("unrelated work", &mut sink).unwrap();
        let after = EventLog::replay(dir.join("events.jsonl"))
            .unwrap()
            .iter()
            .filter(|e| matches!(&e.kind, EventKind::Nudge { text } if text.contains("microagent")))
            .count();
        assert_eq!(after, 1, "only the matching turn carries the microagent");
    }

    /// With memory enabled, the provider sees a second system segment
    /// carrying INDEX.md — appended *after* the static prompt (end of the
    /// static region), frozen for the Agent's life, and git-versioned.
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
        let provider = Arc::new(Mock::new(vec![tool_turn(1), done()]));
        let mut agent = Agent::start(provider.clone(), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        agent.run_turn("hi", &mut sink).unwrap();

        let seen = provider.seen_systems.lock().unwrap();
        // identity + contract + safety + memory index (union ORDER). The
        // count is branch-local, so it is not pinned here — the *order* is
        // what matters.
        assert!(seen[0].len() >= 4);
        assert!(seen[0][0].contains("Overseer"));
        let idx = seen[0]
            .iter()
            .position(|s| s.contains("## Memory index"))
            .expect("memory index segment present");
        assert!(idx >= 3, "memory index sits after the fixed sections");
        assert!(seen[0][idx].contains("facts.md — user facts"));
        assert!(
            seen[0]
                .iter()
                .all(|s| !s.starts_with("Computer use is tiered")),
            "computer guidance rides the deferred tool's description"
        );
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
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
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
        assert!(result_text.contains("trace: "));

        // The subagent's isolated session log exists and is self-contained.
        let sub_log = dir.join("subagents/task-1/events.jsonl");
        assert!(sub_log.exists());
        let sub_events = EventLog::replay(&sub_log).unwrap();
        assert!(sub_events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::UserInput { text } if text.contains("needle"))));

        // Quarantine: the read-only registry has exactly the read tools —
        // no write/bash/task, so a subagent can neither mutate nor recurse
        // (`memory` is search/get-only there: the gate denies writes).
        let ro = crate::tools::ToolRegistry::readonly(crate::perm::Policy::allow_all());
        let names: Vec<&str> = ro.specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["read", "grep", "glob", "memory"]);
    }

    /// P6-2 accept (Secret hidden from subagent view): parent memory with
    /// a Secret topic → the spawned subagent's filtered dir keeps the
    /// Personal pointer and drops the Secret one (body and bytes).
    #[test]
    fn subagent_memory_view_hides_secret() {
        let dir = tmpdir();
        let mem = dir.join("memory");
        crate::memory::ensure(&mem).unwrap();
        std::fs::write(
            mem.join("episodic").join("diary.md"),
            "---\nsensitivity: personal\n---\nhad lunch\n",
        )
        .unwrap();
        std::fs::write(
            mem.join("semantic").join("token.md"),
            "---\nsensitivity: secret\n---\nsk-live-abc\n",
        )
        .unwrap();
        std::fs::write(
            mem.join("INDEX.md"),
            "# Memory Index\n\ndiary.md — lunch notes\ntoken.md — api token\n",
        )
        .unwrap();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            memory_dir: Some(mem.clone()),
            ..AgentConfig::default()
        };
        let dest = dir.join("sub-view");
        let got = crate::tools::task::filtered_memory_dir(&mem, cfg.memory_filter, &dest)
            .expect("filtered view materializes");
        assert_eq!(got, dest);
        let idx = std::fs::read_to_string(dest.join("INDEX.md")).unwrap();
        assert!(idx.contains("diary.md"), "{idx}");
        assert!(!idx.contains("token.md"), "{idx}");
        assert!(dest.join("episodic").join("diary.md").exists());
        assert!(!dest.join("semantic").join("token.md").exists());
        // Default ceiling is Personal.
        assert_eq!(cfg.memory_filter, crate::memory::Sensitivity::Personal);
    }

    /// P6-2 accept (consolidate no rewrite-smaller): a consolidation that
    /// drops a stale pointer leaves topic bodies byte-identical — only
    /// INDEX.md is rewritten.
    #[test]
    fn consolidate_never_rewrites_topic_bodies() {
        let dir = tmpdir();
        let mem = dir.join("memory");
        crate::memory::ensure(&mem).unwrap();
        let topic = mem.join("episodic").join("diary.md");
        std::fs::write(&topic, "---\nsensitivity: personal\n---\nhad lunch\n").unwrap();
        std::fs::write(
            mem.join("INDEX.md"),
            "# Memory Index\n\ndiary.md — lunch\ndupe.md — stale\n",
        )
        .unwrap();
        let before = std::fs::read(&topic).unwrap();
        struct Fixed {
            reply: String,
        }
        impl crate::provider::Provider for Fixed {
            fn complete(
                &self,
                _: &crate::provider::Request,
            ) -> Result<crate::provider::Response, crate::provider::ProviderError> {
                Ok(crate::provider::Response {
                    blocks: vec![Block::Text {
                        text: self.reply.clone(),
                    }],
                    stop_reason: crate::provider::StopReason::EndTurn,
                    usage: crate::ir::Usage::default(),
                    request_bytes: 0,
                    latency_ms: 0,
                })
            }
            fn name(&self) -> &'static str {
                "fixed"
            }
        }
        let p = Fixed {
            reply: "---INDEX---\n# Memory Index\n\ndiary.md — lunch\n---INDEX---".into(),
        };
        crate::memory::consolidate(&p, "tiny", &mem).unwrap();
        assert_eq!(std::fs::read(&topic).unwrap(), before);
        let new_idx = std::fs::read_to_string(mem.join("INDEX.md")).unwrap();
        assert!(!new_idx.contains("dupe.md"));
    }

    /// P6-2 accept (dirty→event, clean→none): `dirty_files` reports the
    /// post-commit residue and is empty on a clean tree.
    #[test]
    fn memory_audit_dirty_and_clean() {
        let dir = tmpdir();
        let mem = dir.join("memory");
        crate::memory::ensure(&mem).unwrap();
        crate::memory::commit(&mem, "seed");
        // Clean tree → no files.
        assert!(crate::memory::dirty_files(&mem).is_empty());
        // New file → porcelain names it. (Committed at the boundary in
        // run_turn; here we observe the pre-commit residue directly.)
        std::fs::write(mem.join("episodic").join("note.md"), "hi\n").unwrap();
        let dirty = crate::memory::dirty_files(&mem);
        assert!(
            dirty.iter().any(|f| f.contains("note.md")),
            "dirty files: {dirty:?}"
        );
        // The event round-trips through the log (audit-only shape).
        let log_path = dir.join("audit.jsonl");
        let mut log = EventLog::create(&log_path).unwrap();
        log.append(EventKind::MemoryUpdated {
            files: dirty.clone(),
        })
        .unwrap();
        log.flush().unwrap();
        let events = EventLog::replay(&log_path).unwrap();
        assert!(matches!(
            &events[0].kind,
            EventKind::MemoryUpdated { files } if files == &dirty
        ));
        // Audit-only: never rehydrates into messages.
        assert!(crate::event::rehydrate_messages(&events).is_empty());
    }

    /// Stop-hook dispatch (ECC `stop` event): a matching rule nudges the
    /// loop instead of letting it stop; blocks share the verify budget —
    /// at the cap the hook fails open and the stop is allowed.
    #[test]
    fn stop_hook_blocks_then_fails_open() {
        // Block-then-allow: first finish matches the rule, second doesn't.
        let dir = tmpdir();
        std::fs::create_dir_all(dir.join(".overseer")).unwrap();
        std::fs::write(
            dir.join(crate::hooks::HOOKS_FILE),
            r#"[{"event":"stop","contains":"not yet","reason":"tests must pass first"}]"#,
        )
        .unwrap();
        // WorkspaceWrite (not full_access): hooks load off the policy
        // root, which only preset policies pin to `cwd`.
        let cfg = AgentConfig {
            cwd: dir.clone(),
            verify_block_cap: 3,
            ..AgentConfig::default()
        };
        let unfinished = Response {
            blocks: vec![Block::Text {
                text: "wrapping up — not yet verified".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        };
        let provider = Mock::new(vec![unfinished, done()]);
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        let out = agent.run_turn("finish when done", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Completed { steps: 2, .. }));
        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        let nudges = events
            .iter()
            .filter(
                |e| matches!(&e.kind, EventKind::Nudge { text } if text.contains("Stop blocked")),
            )
            .count();
        assert_eq!(nudges, 1, "one stop-block nudge, then the clean finish");

        // Cap: a rule that matches every finish counts to the cap and
        // then lets the stop through (guardrail, not the gate).
        let dir2 = tmpdir();
        std::fs::create_dir_all(dir2.join(".overseer")).unwrap();
        std::fs::write(
            dir2.join(crate::hooks::HOOKS_FILE),
            r#"[{"event":"stop","contains":"all done","reason":"never"}]"#,
        )
        .unwrap();
        let cfg2 = AgentConfig {
            cwd: dir2.clone(),
            verify_block_cap: 3,
            ..AgentConfig::default()
        };
        let provider2 = Mock::new(vec![done()]);
        let mut agent2 = Agent::start(Arc::new(provider2), cfg2, dir2.clone(), "s".into()).unwrap();
        let mut sink2 = |_: &Event| {};
        let out2 = agent2.run_turn("go", &mut sink2).unwrap();
        assert!(
            matches!(out2, RunOutcome::Completed { steps: 3, .. }),
            "cap reached → stop allowed through, got {out2:?}"
        );
        let nudges2 = EventLog::replay(dir2.join("events.jsonl"))
            .unwrap()
            .iter()
            .filter(
                |e| matches!(&e.kind, EventKind::Nudge { text } if text.contains("Stop blocked")),
            )
            .count();
        assert_eq!(nudges2, 2, "cap-1 nudges; the last block opens the stop");
    }

    /// P1.10 verification gate: a failing DoD check blocks the finish and
    /// the output goes back into context; the run ends `VerifyFailed` at
    /// the block cap. A passing check lets the finish through.
    #[test]
    fn verify_gate_blocks_then_caps() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            verify_cmd: Some("false".into()),
            verify_block_cap: 3,
            ..AgentConfig::default()
        };
        let provider = Mock::new(vec![done()]);
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        let out = agent.run_turn("finish fast", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::VerifyFailed { steps: 3, .. }));

        // Each block left a nudge carrying the failure in context.
        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        let nudges = events
            .iter()
            .filter(|e| matches!(&e.kind, EventKind::Nudge { text } if text.contains("Verification failed")))
            .count();
        assert_eq!(nudges, 2); // cap-1 nudges, the last block ends the run
        assert!(agent
            .messages()
            .iter()
            .any(|m| m.text().contains("Verification failed")));

        // B1-7: small_model None → no critiques (verify Nudges only).
        let reflections = events
            .iter()
            .filter(|e| {
                matches!(&e.kind, EventKind::Nudge { text } if text.starts_with(Agent::REFLECTION_TAG))
            })
            .count();
        assert_eq!(reflections, 0, "no small model → no reflection Nudges");

        // B1-7: small_model Some → exactly 1 tagged critique in BOTH views,
        // verify-tail Nudges survive eviction, live == rehydrate.
        let dir3 = tmpdir();
        let cfg3 = AgentConfig {
            cwd: dir3.clone(),
            full_access: true,
            verify_cmd: Some("false".into()),
            verify_block_cap: 3,
            small_model: Some("tiny-1".into()),
            ..AgentConfig::default()
        };
        // done() for the 3 main turns + "critique" aux texts for 2 blocks.
        let critique = Response {
            blocks: vec![Block::Text {
                text: "defect A; defect B; fix C".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        };
        let provider3 = Mock::new(vec![done(), critique.clone(), done(), critique, done()]);
        // NOTE: aux_call uses the main model when small_model fails; here
        // tiny-1 is not in fail_models so aux serves from the queue.
        let mut agent3 = Agent::start(Arc::new(provider3), cfg3, dir3.clone(), "s".into()).unwrap();
        let mut sink3 = |_: &Event| {};
        let out3 = agent3.run_turn("finish fast", &mut sink3).unwrap();
        assert!(matches!(out3, RunOutcome::VerifyFailed { steps: 3, .. }));
        let events3 = EventLog::replay(dir3.join("events.jsonl")).unwrap();
        let tails3 = events3
            .iter()
            .filter(|e| {
                matches!(&e.kind, EventKind::Nudge { text } if text.contains("Verification failed"))
            })
            .count();
        assert_eq!(tails3, 2, "both verify tails must survive");
        let refs3: Vec<&str> = events3
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Nudge { text } if text.starts_with(Agent::REFLECTION_TAG) => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(refs3.len(), 2, "two critiques logged (log keeps both)");
        // Rehydrate keeps only the last tagged critique; live view matches
        // after the dual prune (last message holds the live critique).
        let re = crate::event::rehydrate_messages(&events3);
        let re_refs = re
            .iter()
            .filter(|m| m.text().starts_with(Agent::REFLECTION_TAG))
            .count();
        assert_eq!(re_refs, 1, "rehydrate keeps last-1 reflection");
        let live_refs = agent3
            .messages()
            .iter()
            .filter(|m| m.text().starts_with(Agent::REFLECTION_TAG))
            .count();
        assert_eq!(live_refs, 1, "live view keeps last-1 reflection");
        // D3: live == rehydrate byte-for-byte on the tagged stream — no
        // superseded stubs linger in either view.
        let live_texts: Vec<String> = agent3.messages().iter().map(|m| m.text()).collect();
        let re_texts: Vec<String> = re.iter().map(|m| m.text()).collect();
        let live_tagged: Vec<&str> = live_texts
            .iter()
            .filter(|t| t.starts_with(Agent::REFLECTION_TAG))
            .map(|s| s.as_str())
            .collect();
        let re_tagged: Vec<&str> = re_texts
            .iter()
            .filter(|t| t.starts_with(Agent::REFLECTION_TAG))
            .map(|s| s.as_str())
            .collect();
        assert_eq!(
            live_tagged, re_tagged,
            "live and rehydrate tagged streams must match exactly"
        );

        // D2: small-tier failure → no critique AND no main-model call.
        // (aux_call would escalate; reflect() must not.)
        let dir5 = tmpdir();
        let cfg5 = AgentConfig {
            cwd: dir5.clone(),
            full_access: true,
            verify_cmd: Some("false".into()),
            verify_block_cap: 2,
            small_model: Some("tiny-1".into()),
            ..AgentConfig::default()
        };
        // tiny-1 always fails; main model would serve done() if called.
        let provider5 = Arc::new(Mock::failing_on(&["tiny-1"], vec![done()]));
        let mut agent5 = Agent::start(provider5.clone(), cfg5, dir5.clone(), "s".into()).unwrap();
        let mut sink5 = |_: &Event| {};
        let _ = agent5.run_turn("finish fast", &mut sink5).unwrap();
        let models5 = provider5.seen_models.lock().unwrap();
        let main_calls = models5.iter().filter(|m| *m == "claude-sonnet-5").count();
        assert_eq!(
            main_calls, 2,
            "loop turns only (2 blocks); reflect must add zero main-model calls, saw: {models5:?}"
        );
        let events5 = EventLog::replay(dir5.join("events.jsonl")).unwrap();
        assert!(
            !events5.iter().any(|e| matches!(&e.kind, EventKind::Nudge { text } if text.starts_with(Agent::REFLECTION_TAG))),
            "failed small critique → no reflection Nudge"
        );

        // B1-7: --reflect=off disables critiques entirely.
        let dir4 = tmpdir();
        let cfg4 = AgentConfig {
            cwd: dir4.clone(),
            full_access: true,
            verify_cmd: Some("false".into()),
            verify_block_cap: 2,
            small_model: Some("tiny-1".into()),
            reflect: ReflectMode::Off,
            ..AgentConfig::default()
        };
        let provider4 = Mock::new(vec![done()]);
        let mut agent4 = Agent::start(Arc::new(provider4), cfg4, dir4.clone(), "s".into()).unwrap();
        let mut sink4 = |_: &Event| {};
        let _ = agent4.run_turn("finish fast", &mut sink4).unwrap();
        let events4 = EventLog::replay(dir4.join("events.jsonl")).unwrap();
        assert!(
            !events4.iter().any(|e| matches!(&e.kind, EventKind::Nudge { text } if text.starts_with(Agent::REFLECTION_TAG))),
            "reflect=off → no critiques"
        );

        // A passing check completes on the first attempt.
        let dir2 = tmpdir();
        let cfg2 = AgentConfig {
            cwd: dir2.clone(),
            full_access: true,
            verify_cmd: Some("true".into()),
            ..AgentConfig::default()
        };
        let provider2 = Mock::new(vec![done()]);
        let mut agent2 = Agent::start(Arc::new(provider2), cfg2, dir2.clone(), "s".into()).unwrap();
        let mut sink2 = |_: &Event| {};
        let out2 = agent2.run_turn("finish", &mut sink2).unwrap();
        assert!(matches!(out2, RunOutcome::Completed { steps: 1, .. }));
    }

    fn bash_call(id: &str, cmd: &str) -> Block {
        Block::ToolCall {
            id: id.into(),
            name: "bash".into(),
            input: serde_json::json!({"command": cmd}),
        }
    }

    fn resp(blocks: Vec<Block>) -> Response {
        Response {
            blocks,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        }
    }

    /// I4: verify_cmd runs under the bash sandbox wrapper with bash's
    /// child-env allowlist — a secret-looking parent var is invisible to
    /// the check, and pass/fail semantics are unchanged.
    #[test]
    fn verify_cmd_sandboxed_env_strips_parent_secrets() {
        let dir = tmpdir();
        let ctx = ToolCtx {
            cwd: dir.clone(),
            session_dir: dir,
            spill_seq: 0,
            provider: None,
            agent_config: Some(AgentConfig::default()),
            subagents: Default::default(),
            checkpoint: None,
            sandbox: false,
            broker: None,
        };
        std::env::set_var("FOO_API_KEY", "sk-parent-secret");
        let r = run_verify("test -z \"$FOO_API_KEY\"", &ctx);
        std::env::remove_var("FOO_API_KEY");
        assert!(r.is_ok(), "FOO_API_KEY leaked into verify child: {r:?}");
        assert!(run_verify("true", &ctx).is_ok(), "pass behaves as before");
        let e = run_verify("false", &ctx).unwrap_err();
        assert!(e.contains("exit"), "fail behaves as before: {e}");
    }

    /// P2.4 interrupt: Esc lands at a tool-launch boundary. The call
    /// already launched completes; every remaining call gets a synthetic
    /// result so tool_use/tool_result pairing survives the truncation.
    #[test]
    fn interrupt_skips_remaining_calls_with_synthetic_results() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            sandbox_bash: false,
            ..AgentConfig::default()
        };
        let provider = Mock::new(vec![resp(vec![
            bash_call("c1", "echo one"),
            bash_call("c2", "echo two"),
            bash_call("c3", "echo three"),
        ])]);
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let control = crate::control::Control::default();
        agent.set_control(control.clone());
        // Deterministic mid-batch trigger: the sink runs on the agent
        // thread — the first launched call trips the flag; the boundary
        // check then skips the rest.
        let mut sink = |e: &Event| {
            if matches!(e.kind, EventKind::ToolCallStart { .. }) {
                control.interrupt();
            }
        };
        let out = agent.run_turn("go", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Interrupted { steps: 1, .. }));

        // Pairing: the tool_results message answers all three calls —
        // one real result, two synthetic skips.
        let last = agent.messages().last().unwrap();
        assert_eq!(last.content.len(), 3);
        for (i, b) in last.content.iter().enumerate() {
            match b {
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    if i == 0 {
                        assert!(!is_error);
                    } else {
                        assert!(*is_error && content.contains("interrupted"));
                    }
                }
                _ => panic!("expected ToolResult"),
            }
        }
        // The log tells the same story — interrupted run end.
        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        assert!(events.iter().any(
            |e| matches!(&e.kind, EventKind::RunEnd { stop_reason, .. } if stop_reason == "interrupted")
        ));
    }

    /// P2.4 steering: input queued mid-run truncates the batch at the
    /// next launch boundary and lands as a user message before the next
    /// provider call.
    #[test]
    fn steer_injects_at_boundary_and_continues() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            sandbox_bash: false,
            ..AgentConfig::default()
        };
        let provider = Mock::new(vec![
            resp(vec![
                bash_call("c1", "echo one"),
                bash_call("c2", "echo two"),
            ]),
            done(),
        ]);
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let control = crate::control::Control::default();
        agent.set_control(control.clone());
        let mut sink = |e: &Event| {
            if matches!(e.kind, EventKind::ToolCallStart { .. }) {
                control.steer("stop and report instead");
            }
        };
        let out = agent.run_turn("go", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Completed { steps: 2, .. }));

        // The steer landed as a user message after the truncated batch's
        // tool_results — ordering the provider can accept.
        let msgs = agent.messages();
        let pos = msgs
            .iter()
            .position(|m| m.text().contains("stop and report instead"))
            .expect("steer message in view");
        let results = &msgs[pos - 1];
        assert!(results
            .content
            .iter()
            .all(|b| matches!(b, Block::ToolResult { .. })));
        // Event stream shows it too (audit trail).
        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        assert!(events.iter().any(
            |e| matches!(&e.kind, EventKind::UserInput { text } if text.contains("report instead"))
        ));
    }

    /// P2.5 ask channel: a human Deny through the handler surfaces as a
    /// denied ToolResult — the call never executes.
    #[test]
    fn ask_deny_denies_the_call() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            sandbox_bash: false,
            ask_handler: Some(crate::perm::AskHandler(std::sync::Arc::new(|_| {
                crate::perm::AskDecision::Deny
            }))),
            ..AgentConfig::default()
        };
        let provider = Mock::new(vec![
            resp(vec![bash_call("c1", "git push origin main")]),
            done(),
        ]);
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        let out = agent.run_turn("ship it", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Completed { .. }));
        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        let denied = events.iter().any(|e| {
            matches!(&e.kind, EventKind::ToolResult { denied, content, .. }
                if *denied && content.contains("denied by user"))
        });
        assert!(denied, "expected a denied ToolResult event");
    }

    /// P3.2: aux calls hit the small tier when configured.
    #[test]
    fn aux_call_prefers_small_model() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            small_model: Some("tiny-1".into()),
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::new(vec![done()]));
        let agent = Agent::start(provider.clone(), cfg, dir, "s".into()).unwrap();
        let out = agent.aux_call("title this").unwrap();
        assert_eq!(out.trim(), "all done");
        let models = provider.seen_models.lock().unwrap();
        assert_eq!(models.as_slice(), &["tiny-1"]);
    }

    /// P3.2: a failing small call escalates to the main model.
    #[test]
    fn aux_call_escalates_on_failure() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            small_model: Some("tiny-1".into()),
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::failing_on(&["tiny-1"], vec![done()]));
        let agent = Agent::start(provider.clone(), cfg, dir, "s".into()).unwrap();
        let out = agent.aux_call("title this").unwrap();
        assert_eq!(out.trim(), "all done");
        let models = provider.seen_models.lock().unwrap();
        assert_eq!(models.as_slice(), &["tiny-1", "claude-sonnet-5"]);
    }

    /// B1-2: a single Malformed response is retried once per provider call.
    /// The retry consumes exactly 1 step and records exactly 1 ledger call.
    #[test]
    fn malformed_response_retried_once_with_budget() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        // First provider call fails Malformed, retry succeeds with done().
        let provider = Arc::new(Mock::malformed_first(1, vec![done()]));
        let mut agent = Agent::start(provider.clone(), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        let out = agent.run_turn("finish fast", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Completed { steps: 1, .. }));
        // Two provider calls (initial + 1 retry), one recorded ledger call
        // (ledger records successes; the Malformed attempt never completes).
        assert_eq!(provider.seen_models.lock().unwrap().len(), 2);
        let ledger = Ledger::read_all(dir.join("ledger.jsonl"));
        assert_eq!(ledger.len(), 1, "retry success records exactly 1 call");
    }

    /// P3.10: tool results enter the model view provenance-wrapped, and
    /// the resumed view is byte-identical (rehydrate re-wraps).
    #[test]
    fn tool_results_are_provenance_wrapped() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::new(vec![tool_turn(1), done()]));
        let mut agent = Agent::start(provider, cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        agent.run_turn("go", &mut sink).unwrap();

        let wrapped = agent
            .messages()
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| {
                matches!(b, Block::ToolResult { content, .. }
                if content.contains("<tool_result tool=\"bash\">"))
            });
        assert!(wrapped, "tool results must be provenance-wrapped");

        // Event log stays raw; the resumed view re-wraps identically.
        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        let raw = events.iter().any(|e| {
            matches!(&e.kind,
            EventKind::ToolResult { content, .. } if !content.contains("<tool_result"))
        });
        assert!(raw, "event log stores raw output");
        let resumed = rehydrate_messages(&events);
        let rewrapped = resumed.iter().flat_map(|m| m.content.iter()).any(|b| {
            matches!(b, Block::ToolResult { content, .. }
                if content.contains("<tool_result tool=\"bash\">"))
        });
        assert!(rewrapped, "resumed view must match the live view");
    }

    /// P3.4: a finished background subagent lands as a SubagentDone
    /// event + a user message at the next step boundary; one left
    /// `running` by a dead process is reaped and noticed too.
    #[test]
    fn bg_notice_drains_into_context() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::new(vec![tool_turn(1), done()]));
        let mut agent = Agent::start(provider.clone(), cfg, dir.clone(), "s".into()).unwrap();
        // Pre-seed a finished bg task and a dead one before the turn.
        for (id, state, nonce) in [("task-7", "done", "x"), ("task-8", "running", "old")] {
            let bg = dir.join("subagents").join(id);
            std::fs::create_dir_all(&bg).unwrap();
            std::fs::write(
                bg.join("task.json"),
                serde_json::json!({"id": id, "mode": "read", "tier": "light",
                    "model": "m", "background": true, "worktree": null, "branch": null,
                    "cap_usd": 0.25, "process_nonce": nonce, "state": state, "cost_usd": 0.0})
                .to_string(),
            )
            .unwrap();
        }
        std::fs::write(dir.join("subagents/task-7/done.txt"), "bg digest here").unwrap();

        let mut events = Vec::new();
        let mut sink = |e: &Event| events.push(e.kind.clone());
        let out = agent.run_turn("go", &mut sink).unwrap();
        assert!(matches!(out, RunOutcome::Completed { .. }));
        for id in ["task-7", "task-8"] {
            assert!(
                events.iter().any(|k| matches!(
                    k,
                    EventKind::SubagentDone { task_id, .. } if task_id == id
                )),
                "{id}"
            );
        }
        assert!(agent
            .messages()
            .iter()
            .any(|m| m.text().contains("bg digest here")));
        assert!(agent
            .messages()
            .iter()
            .any(|m| m.text().contains("task-8 died with its process")));

        // Resume replays the notice identically from the event log and
        // does not drain it again.
        let events2 = EventLog::replay(dir.join("events.jsonl")).unwrap();
        let msgs = rehydrate_messages(&events2);
        assert!(msgs.iter().any(|m| m.text().contains("bg digest here")));
        let mut resumed = Agent::resume(
            provider,
            AgentConfig {
                cwd: dir.clone(),
                full_access: true,
                ..AgentConfig::default()
            },
            dir.clone(),
        )
        .unwrap();
        assert_eq!(
            resumed.subagent_seq, 8,
            "counter seeds past existing task dirs"
        );
        let mut again = Vec::new();
        resumed
            .drain_bg_notices(&mut |e: &Event| again.push(e.kind.clone()))
            .unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    /// A finished background task whose marker is missing still lands as
    /// a notice — unless its thread is still writing that marker.
    #[test]
    fn lost_digest_is_noticed_not_skipped() {
        use crate::tools::task::sidecar::Delivery;
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::new(vec![done()]));
        let mut agent = Agent::start(provider, cfg, dir.clone(), "s".into()).unwrap();
        for id in ["task-9", "task-10"] {
            let bg = dir.join("subagents").join(id);
            std::fs::create_dir_all(&bg).unwrap();
            std::fs::write(
                bg.join("task.json"),
                serde_json::json!({"id": id, "mode": "read", "tier": "light",
                    "model": "m", "background": true, "worktree": null, "branch": null,
                    "cap_usd": 0.25, "process_nonce": "x", "state": "done", "cost_usd": 0.0})
                .to_string(),
            )
            .unwrap();
        }
        let delivering = Delivery::new(&dir.join("subagents/task-10"));
        let mut events = Vec::new();
        agent
            .drain_bg_notices(&mut |e: &Event| events.push(e.kind.clone()))
            .unwrap();
        let noticed = |events: &[EventKind], id: &str| {
            events
                .iter()
                .any(|k| matches!(k, EventKind::SubagentDone { task_id, .. } if task_id == id))
        };
        assert!(noticed(&events, "task-9"), "{events:?}");
        assert!(!noticed(&events, "task-10"), "still delivering");
        assert!(agent.messages().iter().any(|m| m
            .text()
            .contains("[subagent task-9 finished but its digest was lost]")));
        drop(delivering);
        let mut again = Vec::new();
        agent
            .drain_bg_notices(&mut |e: &Event| again.push(e.kind.clone()))
            .unwrap();
        assert!(
            noticed(&again, "task-10") && !noticed(&again, "task-9"),
            "{again:?}"
        );
    }

    /// A same-process rebuild (TUI session switch / rewind) while tasks
    /// run: the new agent re-holds their caps instead of starting free.
    #[test]
    fn rebuilt_agent_re_reserves_live_tasks() {
        let dir = tmpdir();
        let cfg = || AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            max_cost_usd: 1.0,
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::new(vec![done()]));
        drop(Agent::start(provider.clone(), cfg(), dir.clone(), "s".into()).unwrap());
        let nonce = crate::tools::task::sidecar::process_nonce();
        for (id, cap, nonce) in [
            ("task-1", 0.40, nonce),
            ("task-2", 0.90, nonce),
            ("task-3", 0.25, nonce),
            ("task-4", 0.50, "gone"),
        ] {
            let bg = dir.join("subagents").join(id);
            std::fs::create_dir_all(&bg).unwrap();
            std::fs::write(
                bg.join("task.json"),
                serde_json::json!({"id": id, "mode": "read", "tier": "light",
                    "model": "m", "background": true, "worktree": null, "branch": null,
                    "cap_usd": cap, "process_nonce": nonce, "state": "running",
                    "cost_usd": 0.0})
                .to_string(),
            )
            .unwrap();
        }
        let resumed = Agent::resume(provider, cfg(), dir.clone()).unwrap();
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // $0.40 whole, $0.60 clamped, $0.25 over the top; task-4's
        // process is gone, so it is reaped and settled, not re-held.
        assert!(
            near(resumed.spend.reserved_usd(), 1.25),
            "{}",
            resumed.spend.reserved_usd()
        );
        assert!(resumed.spend.reserved_usd() >= resumed.config.max_cost_usd);
    }

    fn task_call(n: usize, input: serde_json::Value) -> Response {
        Response {
            blocks: vec![Block::ToolCall {
                id: format!("t{n}"),
                name: "task".into(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        }
    }

    fn text_costing(model: &str, text: &str, usd: f64) -> Response {
        let price = crate::profile::lookup(model).price.input;
        Response {
            blocks: vec![Block::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                fresh_input: (usd * 1_000_000.0 / price).round() as u64,
                ..Usage::default()
            },
            request_bytes: 0,
            latency_ms: 0,
        }
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Binding correction 1: the spawn counter lives on the agent, so two
    /// writers spawned in different steps get distinct dirs and branches.
    #[test]
    fn task_ids_are_session_monotonic_across_steps() {
        let repo = tmpdir();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-qm", "x", "--allow-empty"]);
        let session = repo.join("session");
        let cfg = AgentConfig {
            cwd: repo.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::new(vec![
            task_call(1, serde_json::json!({"prompt": "w1", "mode": "write"})),
            done(),
            task_call(2, serde_json::json!({"prompt": "w2", "mode": "write"})),
            done(),
            done(),
        ]));
        let mut agent = Agent::start(provider, cfg, session.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        agent.run_turn("go", &mut sink).unwrap();
        for n in [1, 2] {
            assert!(session
                .join(format!("subagents/task-{n}/task.json"))
                .exists());
            assert!(session.join(format!("subagents/wt-{n}/wt")).exists());
        }
        let branches = git(&repo, &["branch", "--list", "overseer-task-*"]);
        assert!(branches.contains("overseer-task-1") && branches.contains("overseer-task-2"));
        let results: Vec<String> = agent
            .messages()
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                Block::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert!(results.iter().all(|r| !r.contains("task: ")), "{results:?}");
    }

    /// Budget composition: subagent spend settles into the parent's ledger
    /// and stops the parent; a resume does not settle it twice.
    #[test]
    fn subagent_spend_composes_into_the_parent_cap() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            model: "claude-fable-5".into(),
            max_cost_usd: 0.10,
            ..AgentConfig::default()
        };
        let light = crate::tools::task::route::resolve(crate::profile::Tier::Light, &cfg).model;
        let provider = Arc::new(Mock::new(vec![
            task_call(1, serde_json::json!({"prompt": "a"})),
            text_costing(&light, "sub a", 0.06),
            task_call(2, serde_json::json!({"prompt": "b"})),
            text_costing(&light, "sub b", 0.06),
            done(),
        ]));
        let mut agent =
            Agent::start(provider.clone(), cfg.clone(), dir.clone(), "s".into()).unwrap();
        let mut events = Vec::new();
        let out = agent
            .run_turn("go", &mut |e: &Event| events.push(e.kind.clone()))
            .unwrap();
        // task-2 was capped at what was left ($0.04) but its one call cost
        // $0.06: spend is only known after a call, so the parent stops.
        let RunOutcome::CostBudgetExceeded { cost_usd, .. } = out else {
            panic!("{out:?}");
        };
        assert!((cost_usd - 0.12).abs() < 1e-3, "{cost_usd}");
        let caps: Vec<f64> = [1, 2]
            .iter()
            .map(|n| {
                crate::tools::task::sidecar::Sidecar::load(&dir.join(format!("subagents/task-{n}")))
                    .unwrap()
                    .cap_usd
            })
            .collect();
        assert!(
            (caps[0] - 0.10).abs() < 1e-9 && (caps[1] - 0.04).abs() < 1e-3,
            "{caps:?}"
        );
        assert!(events.iter().any(|k| matches!(k,
            EventKind::RunEnd { total_cost_usd, subagent_cost_usd, .. }
                if (total_cost_usd - 0.12).abs() < 1e-3 && (subagent_cost_usd - 0.12).abs() < 1e-3)));
        let settlements = |d: &std::path::Path| {
            Ledger::read_all(d.join("ledger.jsonl"))
                .iter()
                .filter(|r| r.subagent.is_some())
                .count()
        };
        assert_eq!(settlements(&dir), 2);
        drop(agent);
        let resumed = Agent::resume(provider, cfg, dir.clone()).unwrap();
        assert!((resumed.ledger.total_cost_usd - 0.12).abs() < 1e-3);
        assert_eq!(settlements(&dir), 2, "resume must not settle again");
        assert_eq!(resumed.subagent_seq, 2);
    }

    /// P3.2: effort bumps one notch per stuck-detector trip.
    #[test]
    fn effort_escalates_on_stuck() {
        use crate::provider::Effort;
        let mut e = Effort::Medium;
        e = e.bumped();
        assert_eq!(e, Effort::High);
        e = e.bumped().bumped();
        assert_eq!(e, Effort::Max, "bounded at Max");
        assert_eq!(Effort::parse("med"), Some(Effort::Medium));
        assert_eq!(Effort::parse("bogus"), None);
    }

    /// P6-3 accept (no-plaintext-in-events): a tool that returns a mapped
    /// real lands in events.jsonl with the sentinel, never the real.
    #[test]
    fn broker_secret_never_reaches_events_jsonl() {
        let dir = tmpdir();
        let mut broker = crate::cred::Broker::new();
        let sentinel = broker.issue_capability(
            "db",
            "DB_PASS",
            "pw-real-9",
            vec![],
            vec!["read".into()],
            None,
        );
        let mut cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            sandbox_bash: false,
            ..AgentConfig::default()
        };
        cfg.broker = broker;
        // The real enters via file bytes (not the call input — inputs log
        // raw in ToolCallStart, so a real in argv would leak by design).
        std::fs::write(dir.join("secret.txt"), "password is pw-real-9 ok\n").unwrap();
        let bash_call = crate::provider::Response {
            blocks: vec![Block::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "secret.txt"}),
            }],
            stop_reason: crate::provider::StopReason::ToolUse,
            usage: crate::ir::Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        };
        let provider = Mock::new(vec![
            bash_call,
            crate::provider::Response {
                blocks: vec![Block::Text {
                    text: "done".into(),
                }],
                stop_reason: crate::provider::StopReason::EndTurn,
                usage: crate::ir::Usage::default(),
                request_bytes: 0,
                latency_ms: 0,
            },
        ]);
        let mut agent = Agent::start(Arc::new(provider), cfg, dir.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        agent.run_turn("go", &mut sink).unwrap();
        let raw = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert!(!raw.contains("pw-real-9"), "events.jsonl leaked the real");
        assert!(
            raw.contains(&sentinel),
            "events.jsonl must carry the sentinel"
        );
    }

    /// P6-4 accept: consent grants are audited at session start and stay
    /// audit-only — rehydration must not turn them into model context, and
    /// the audit copy carries metadata only (no real, no sentinel).
    #[test]
    fn consent_grant_is_audited_and_never_rehydrates() {
        let dir = tmpdir();
        let mut cfg = AgentConfig {
            cwd: dir.clone(),
            ..AgentConfig::default()
        };
        cfg.broker.grant_book_mut().add(crate::cred::Grant {
            client: "gh".into(),
            scopes: vec!["repo".into(), "read".into()],
            expires_ms: 0,
            actor: "user".into(),
            approved_by: "alice".into(),
        });
        let sentinel = cfg.broker.issue_capability(
            "gh",
            "GH_TOKEN",
            "ghp_real_grant_1",
            vec![],
            vec!["repo".into()],
            None,
        );
        let agent =
            Agent::start(Arc::new(Mock::new(vec![])), cfg, dir.clone(), "s".into()).unwrap();
        drop(agent);

        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        let granted: Vec<(&str, &[String], &str)> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ConsentGranted {
                    client,
                    scopes,
                    approved_by,
                    ..
                } => Some((client.as_str(), scopes.as_slice(), approved_by.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(granted.len(), 1, "the grant must be on the record");
        assert_eq!(granted[0].0, "gh");
        assert_eq!(granted[0].1, ["repo".to_string(), "read".to_string()]);
        assert_eq!(granted[0].2, "alice");
        let raw = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert!(
            !raw.contains("ghp_real_grant_1"),
            "audit event leaked a real"
        );
        assert!(
            !raw.contains(&sentinel),
            "audit event must not carry sentinels"
        );

        // Audit-only: no message view entry, no context injection.
        let msgs = crate::event::rehydrate_messages(&events);
        let view = format!("{msgs:?}");
        assert!(
            !view.contains("alice"),
            "grant leaked into the view: {view}"
        );
        assert!(
            !view.contains("consent"),
            "grant leaked into the view: {view}"
        );
    }

    /// P6-5 accept, at the engine boundary: an unapproved persona dir is
    /// closed to the file tools AND invisible in the prompt; approving it
    /// flips both halves for the next session.
    #[test]
    fn persona_draft_gate_is_wired_through_registry_and_prompt() {
        let dir = tmpdir();
        let persona = dir.join("persona");
        crate::onboard::ensure_persona_dir(&persona).unwrap();
        crate::onboard::write_drafts(
            &persona,
            &[(
                "identity.md".to_string(),
                vec![crate::onboard::Insight {
                    text: "DRAFT_INSIGHT_A".to_string(),
                    source: 1,
                }],
            )],
            1,
        )
        .unwrap();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            persona_dir: Some(persona.clone()),
            ..AgentConfig::default()
        };
        let rel = serde_json::json!({"path": "persona/identity.md"});

        let agent = Agent::start(
            Arc::new(Mock::new(vec![])),
            cfg.clone(),
            dir.join("s1"),
            "s1".into(),
        )
        .unwrap();
        assert!(matches!(
            agent.tools.policy().check("read", &rel),
            crate::perm::Verdict::Deny { .. }
        ));
        let system = crate::prompt::assemble(&agent.config);
        let seg = system.iter().find(|s| s.name == "persona").unwrap();
        assert!(!seg.text.contains("DRAFT_INSIGHT_A"), "{}", seg.text);
        drop(agent);

        // Approve → the next session can read it and the prompt carries it.
        crate::onboard::approve(&persona).unwrap();
        let agent = Agent::start(
            Arc::new(Mock::new(vec![])),
            cfg,
            dir.join("s2"),
            "s2".into(),
        )
        .unwrap();
        assert_eq!(
            agent.tools.policy().check("read", &rel),
            crate::perm::Verdict::Allow
        );
        let system = crate::prompt::assemble(&agent.config);
        assert!(system
            .iter()
            .any(|s| s.name == "persona" && s.text.contains("DRAFT_INSIGHT_A")));
    }

    /// Screenshots taken through `tools op=call computer` still ride the
    /// conversation as image siblings (last-2 rule intact) and still emit
    /// the computer audit event; the events keep the outer `tools` call.
    #[cfg(feature = "code-mode")]
    #[test]
    fn run_code_sub_calls_are_audited_but_never_replayed() {
        let dir = tmpdir();
        std::fs::write(dir.join("a.txt"), "alpha\n").unwrap();
        let script = Response {
            blocks: vec![Block::ToolCall {
                id: "r1".into(),
                name: "run_code".into(),
                input: serde_json::json!({"code":
                    "tools.read({path: 'a.txt'});\nreturn tools.glob({pattern: '*.txt'}).length > 0;"}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        };
        let provider = Arc::new(Mock::new(vec![script, done()]));
        let cfg = AgentConfig {
            cwd: dir.clone(),
            ..AgentConfig::default()
        };
        let mut agent = Agent::start(provider.clone(), cfg, dir.clone(), "s".into())
            .unwrap()
            .with_tools(ToolRegistry::core(crate::perm::Policy::allow_all()));
        agent.run_turn("go", &mut |_: &Event| {}).unwrap();

        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        let calls: Vec<(&str, &str, &str)> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ScriptCall {
                    parent_call_id,
                    name,
                    input_digest,
                    ..
                } => Some((
                    parent_call_id.as_str(),
                    name.as_str(),
                    input_digest.as_str(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!((calls[0].0, calls[0].1), ("r1", "read"));
        assert_eq!((calls[1].0, calls[1].1), ("r1", "glob"));
        let result_at = events
            .iter()
            .position(|e| matches!(e.kind, EventKind::ToolResult { .. }))
            .unwrap();
        let first_audit = events
            .iter()
            .position(|e| matches!(e.kind, EventKind::ScriptCall { .. }))
            .unwrap();
        assert!(result_at < first_audit, "audit follows the paired result");

        let digest = calls[0].2;
        let replayed = format!("{:?}", crate::event::rehydrate_messages(&events));
        assert!(!replayed.contains(digest), "ScriptCall must not rehydrate");
        let seen = format!(
            "{:?}",
            provider.seen_messages.lock().unwrap().last().unwrap()
        );
        assert!(!seen.contains(digest) && seen.contains("→ true"), "{seen}");
    }

    #[test]
    fn computer_through_tools_keeps_audit_and_image_siblings() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir();
        let driver = dir.join("fake-cua.sh");
        let img = r#"{"content":[{"type":"image","data":"aGVsbG8=","mimeType":"image/png"}],"structuredContent":{"screenshot_width":64,"screenshot_height":32,"screenshot_mime_type":"image/png","window_bounds":{"x":0,"y":0,"width":64,"height":32},"window_id":1,"pid":1}}"#;
        std::fs::write(
            &driver,
            format!(
                "#!/bin/sh
                 sid() {{ printf '%s' \"$1\" | sed 's/.*\"id\":\\([0-9][0-9]*\\).*/\\1/' | head -1; }}
                 while IFS= read -r line; do
                 case \"$line\" in
                 *'\"method\":\"initialize\"'*) r='{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"f\",\"version\":\"0\"}}}}' ;;
                 *'\"method\":\"notifications/'*) continue ;;
                 *'\"method\":\"tools/list\"'*) r='{{\"tools\":[]}}' ;;
                 *) r='{img}' ;;
                 esac
                 printf '%s\\n' \"{{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":$(sid \"$line\"),\\\"result\\\":$r}}\"
                 done
"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755)).unwrap();
        let shot = |n: usize| Response {
            blocks: vec![Block::ToolCall {
                id: format!("s{n}"),
                name: "tools".into(),
                input: serde_json::json!({"op": "call", "name": "computer",
                    "args": {"action": "screenshot", "pid": 1, "window_id": n}}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        };
        let provider = Arc::new(Mock::new(vec![shot(1), shot(2), shot(3), done()]));
        let cfg = AgentConfig {
            cwd: dir.clone(),
            ..AgentConfig::default()
        };
        let mut tools = ToolRegistry::core_with(
            crate::perm::Policy::allow_all(),
            crate::tools::Optional::ALL,
        );
        tools.set_computer(crate::tools::computer::ComputerState::new(
            crate::tools::computer::Backends {
                driver: Some(driver),
                ..Default::default()
            },
        ));
        let mut agent = Agent::start(provider.clone(), cfg, dir.clone(), "s".into())
            .unwrap()
            .with_tools(tools);
        agent.run_turn("go", &mut |_: &Event| {}).unwrap();

        let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
        let acts = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ComputerAct { .. }))
            .count();
        assert_eq!(acts, 3, "one audit event per screenshot");
        assert!(events.iter().all(|e| match &e.kind {
            EventKind::ToolCallStart { name, .. } | EventKind::ToolResult { name, .. } =>
                name == "tools",
            _ => true,
        }));
        let last = provider
            .seen_messages
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let images = last
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| matches!(b, Block::Image { .. }))
            .count();
        assert_eq!(images, 2, "screenshots reach the model, last two kept");
    }

    #[test]
    fn computer_takeover_suppression_metadata_only() {
        // P7-1: cred-field focus or watch_mode suppresses pixel capture —
        // the obs carries metadata only, never pixel bytes.
        let cfg = AgentConfig::default();
        assert!(cfg.computer.takeover_pause, "fail-closed default");
        assert!(crate::computer_obs::is_suppressed(&cfg.computer, true));
        assert!(!crate::computer_obs::is_suppressed(&cfg.computer, false));
        let mut watch = cfg.computer.clone();
        watch.watch_mode = true;
        assert!(crate::computer_obs::is_suppressed(&watch, false));
        let obs = crate::computer_obs::metadata_obs(2560, 1600, 1280, 800, "cred-field focus");
        assert!(
            !obs.contains("aGVsbG8"),
            "metadata obs carries no pixel bytes"
        );
        assert!(obs.contains("2560x1600"));
    }

    #[test]
    fn computer_containment_strips_secret_env() {
        // P7-1: child envs never carry secrets (no-creds invariant).
        for k in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "SOME_SERVICE_KEY",
            "BOT_TOKEN",
            "session_token",
        ] {
            assert!(!ComputerConfig::env_allowed(k), "{k} must be stripped");
        }
        for k in ["PATH", "HOME", "OVERSEER_UNTRUSTED_SOURCE", "TMPDIR"] {
            assert!(ComputerConfig::env_allowed(k), "{k} must pass through");
        }
    }

    /// P7-4 env-arm: an untrusted-originated spawn (channel message) starts
    /// with the untrusted latch already armed, so the Rule-of-Two triangle
    /// closes the moment sensitive data is read — no tool use is needed to
    /// earn the latch, and the control session (same script, no marker)
    /// stays clean. The marker is read through the injected getter, so the
    /// process env is never touched.
    #[test]
    fn untrusted_source_arms_the_session_before_any_tool_use() {
        fn run(dir: &std::path::Path, armed: bool) -> (Vec<Event>, Agent) {
            let cfg = AgentConfig {
                cwd: dir.to_path_buf(),
                // Real policy (not the benchmark allow-all shortcut).
                full_access: false,
                ..AgentConfig::default()
            };
            let provider = Arc::new(Mock::new(vec![
                Response {
                    blocks: vec![Block::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": ".env"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        fresh_input: 20_000,
                        ..Usage::default()
                    },
                    request_bytes: 0,
                    latency_ms: 0,
                },
                Response {
                    blocks: vec![Block::ToolCall {
                        id: "c2".into(),
                        name: "write".into(),
                        input: serde_json::json!({"path": "out.txt", "content": "x"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        fresh_input: 20_000,
                        ..Usage::default()
                    },
                    request_bytes: 0,
                    latency_ms: 0,
                },
                done(),
            ]));
            let marker = armed.then(|| "channel:telegram:u1".to_string());
            let mut agent =
                Agent::start_with_env(provider, cfg, dir.to_path_buf(), "s".into(), |k| {
                    if k == UNTRUSTED_ENV {
                        marker.clone()
                    } else {
                        None
                    }
                })
                .unwrap();
            let mut sink = |_: &Event| {};
            agent.run_turn("go", &mut sink).unwrap();
            let events = EventLog::replay(dir.join("events.jsonl")).unwrap();
            (events, agent)
        }

        let clean = tmpdir();
        std::fs::write(clean.join(".env"), "SECRET=hunter2\n").unwrap();
        let (events, _) = run(&clean, false);
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.kind, EventKind::ToolResult { denied: true, .. })),
            "control session: reading .env then writing is allowed"
        );

        let dirty = tmpdir();
        std::fs::write(dirty.join(".env"), "SECRET=hunter2\n").unwrap();
        let (events, agent) = run(&dirty, true);
        // Sensitive read + pre-armed untrusted origin ⇒ the write needs a
        // human (headless Ask collapses to a denial).
        assert!(
            events
                .iter()
                .any(|e| matches!(&e.kind, EventKind::ToolResult { denied: true, .. })),
            "armed session must deny the write after a sensitive read"
        );
        // The arm is announced before any tool call in the log.
        let first_event = events
            .iter()
            .position(|e| matches!(&e.kind, EventKind::Tainted { .. }))
            .expect("arm notice recorded");
        let first_call = events
            .iter()
            .position(|e| matches!(&e.kind, EventKind::ToolCallStart { .. }))
            .expect("tool call recorded");
        assert!(first_event < first_call, "arm precedes the first tool use");
        // Zero prior tool use still Asks for an external call.
        assert!(matches!(
            agent.tools.policy.check(
                "bash",
                &serde_json::json!({"command": "curl -X POST https://example.com"})
            ),
            crate::perm::Verdict::Ask { .. }
        ));
    }

    /// C1: a resumed session must not re-notice background tasks whose
    /// SubagentDone is already in the replayed log.
    #[test]
    fn resume_does_not_renotice_finished_bg_tasks() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        {
            let provider = Arc::new(Mock::new(vec![tool_turn(1), done()]));
            let mut agent = Agent::start(provider, cfg.clone(), dir.clone(), "s".into()).unwrap();
            let bg = dir.join("subagents/task-1");
            std::fs::create_dir_all(&bg).unwrap();
            std::fs::write(
                bg.join("task.json"),
                serde_json::json!({"id": "task-1", "mode": "read", "tier": "light",
                    "model": "m", "background": true, "worktree": null, "branch": null,
                    "cap_usd": 0.25, "process_nonce": "x", "state": "done", "cost_usd": 0.0})
                .to_string(),
            )
            .unwrap();
            std::fs::write(bg.join("done.txt"), "bg-1 digest").unwrap();
            let mut sink = |_: &Event| {};
            agent.run_turn("go", &mut sink).unwrap();
        }
        let provider = Arc::new(Mock::new(vec![tool_turn(2), done()]));
        let mut agent = Agent::resume(provider, cfg, dir.clone()).unwrap();
        let mut events = Vec::new();
        let mut sink = |e: &Event| events.push(e.kind.clone());
        agent.run_turn("again", &mut sink).unwrap();
        assert!(
            !events
                .iter()
                .any(|k| matches!(k, EventKind::SubagentDone { .. })),
            "resume re-emitted a SubagentDone: {events:?}"
        );
        let copies = agent
            .messages()
            .iter()
            .filter(|m| m.text().contains("bg-1 digest"))
            .count();
        assert_eq!(copies, 1, "the notice appears exactly once in the view");
    }

    /// S1-review: a fork's log opens with the parent's SessionStart —
    /// resume must take the LAST one or the fork borrows the parent's
    /// prompt-cache key.
    #[test]
    fn resume_keys_prompt_cache_on_last_session_start() {
        let parent = tmpdir();
        let cfg = AgentConfig {
            cwd: parent.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        {
            let p = Arc::new(Mock::new(vec![done()]));
            let mut a = Agent::start(p, cfg.clone(), parent.clone(), "parent-id".into()).unwrap();
            let mut sink = |_: &Event| {};
            a.run_turn("go", &mut sink).unwrap();
        }
        let fork_dir = tmpdir();
        crate::session::fork(&parent, None, &fork_dir).unwrap();
        let a = Agent::resume(Arc::new(Mock::new(vec![])), cfg, fork_dir.clone()).unwrap();
        let want = fork_dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            a.cache_key.as_deref(),
            Some(want.as_str()),
            "resumed fork must not reuse the parent's cache key"
        );
    }

    /// C2: a dirty memory dir at the turn boundary emits MemoryUpdated
    /// naming the dirty files (captured BEFORE the commit cleans them).
    #[test]
    fn dirty_memory_emits_memory_updated() {
        let dir = tmpdir();
        let memdir = dir.join("memory");
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            memory_dir: Some(memdir.clone()),
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::new(vec![tool_turn(1), done()]));
        let mut agent = Agent::start(provider, cfg, dir.join("s"), "s".into()).unwrap();
        crate::memory::commit(&memdir, "seed");
        std::fs::write(memdir.join("episodic").join("note.md"), "hi\n").unwrap();
        let mut events = Vec::new();
        let mut sink = |e: &Event| events.push(e.kind.clone());
        agent.run_turn("go", &mut sink).unwrap();
        assert!(
            events.iter().any(|k| matches!(
                k,
                EventKind::MemoryUpdated { files } if files.iter().any(|f| f.contains("note.md"))
            )),
            "no MemoryUpdated for the dirty file: {events:?}"
        );
    }

    /// C8: the token estimate counts tool-call inputs — a 100 KB `write`
    /// body is not "5 chars".
    #[test]
    fn estimate_counts_tool_call_input() {
        let body = "x".repeat(100_000);
        let msgs = vec![Message {
            role: crate::ir::Role::Assistant,
            content: vec![Block::ToolCall {
                id: "c1".into(),
                name: "write".into(),
                input: serde_json::json!({"path": "a.txt", "content": body}),
            }],
        }];
        let text = prompt_text_for_estimate(&msgs);
        assert!(text.len() >= 100_000, "estimate saw {} chars", text.len());
    }

    /// K5 (invariant 2): the static system prefix is frozen per Agent — an
    /// INDEX.md edit between steps leaves the next request's system bytes
    /// identical (the model reads fresh memory through its tools).
    #[test]
    fn system_prefix_frozen_across_memory_edits() {
        let dir = tmpdir();
        let memdir = dir.join("memory");
        std::fs::create_dir_all(&memdir).unwrap();
        std::fs::write(memdir.join("INDEX.md"), "facts.md — v1\n").unwrap();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            memory_dir: Some(memdir.clone()),
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::new(vec![done()]));
        let mut agent = Agent::start(provider.clone(), cfg, dir.join("s"), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        agent.run_turn("one", &mut sink).unwrap();
        std::fs::write(memdir.join("INDEX.md"), "facts.md — v2 EDITED\n").unwrap();
        agent.run_turn("two", &mut sink).unwrap();
        let seen = provider.seen_systems.lock().unwrap();
        assert!(seen.len() >= 2);
        assert_eq!(
            seen[0],
            seen[seen.len() - 1],
            "system bytes moved mid-session"
        );
        assert!(seen[0].iter().any(|s| s.contains("v1")));
    }

    /// K4: every main-loop request carries the session id as its
    /// prompt-cache key, and a resume keeps the same key.
    #[test]
    fn cache_key_is_session_id_across_resume() {
        let dir = tmpdir();
        let cfg = AgentConfig {
            cwd: dir.clone(),
            full_access: true,
            ..AgentConfig::default()
        };
        let provider = Arc::new(Mock::new(vec![done()]));
        let sdir = dir.join("s");
        let mut sink = |_: &Event| {};
        {
            let mut agent = Agent::start(
                provider.clone(),
                cfg.clone(),
                sdir.clone(),
                "sess-42".into(),
            )
            .unwrap();
            agent.run_turn("one", &mut sink).unwrap();
        }
        let mut agent = Agent::resume(provider.clone(), cfg, sdir).unwrap();
        agent.run_turn("two", &mut sink).unwrap();
        let keys = provider.seen_cache_keys.lock().unwrap();
        assert!(keys.len() >= 2);
        assert!(
            keys.iter().all(|k| k.as_deref() == Some("sess-42")),
            "{keys:?}"
        );
    }

    /// Memory v2 config: home-style user + project stores outside `cwd`.
    fn v2_cfg(dir: &std::path::Path) -> AgentConfig {
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let (user, project) = (
            dir.join("home/memory"),
            dir.join("home/projects/ws-0/memory"),
        );
        crate::memory::ensure(&user).unwrap();
        crate::memory::ensure(&project).unwrap();
        AgentConfig {
            cwd: ws,
            full_access: true,
            memory_dir: Some(project),
            user_memory_dir: Some(user),
            ..AgentConfig::default()
        }
    }

    fn note(store: &std::path::Path, rel: &str, text: &str) {
        std::fs::write(store.join(rel), text).unwrap();
        crate::memory::append_pointer(store, &format!("{rel} — note")).unwrap();
    }

    fn call(id: &str, name: &str, input: serde_json::Value) -> Block {
        Block::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    fn calls(blocks: Vec<Block>) -> Response {
        Response {
            blocks,
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        }
    }

    fn notices(events: &[Event]) -> Vec<(usize, String, Vec<String>)> {
        events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| match &e.kind {
                EventKind::MemoryNotice { kind, notes, .. } => {
                    Some((i, kind.clone(), notes.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// A steered input runs the same memory hook as a turn's input:
    /// recall lands right after its UserInput, and resume is identical.
    #[test]
    fn steered_input_recalls_and_resumes_identically() {
        let dir = tmpdir();
        let cfg = v2_cfg(&dir);
        let project = cfg.memory_dir.clone().unwrap();
        note(
            &project,
            "semantic/deploy.md",
            "# Deploy\nstaging deploy uses blue green\n",
        );
        let write = calls(vec![call(
            "w1",
            "write",
            serde_json::json!({"path": "f.txt", "content": "x"}),
        )]);
        let provider = Arc::new(Mock::new(vec![write, done()]));
        let session = dir.join("s");
        let mut agent = Agent::start(provider, cfg, session.clone(), "s".into()).unwrap();
        let control = crate::control::Control::default();
        agent.set_control(control.clone());
        let mut sink = |e: &Event| {
            if matches!(e.kind, EventKind::ToolCallStart { .. }) {
                control.steer("the staging deploy");
            }
        };
        agent.run_turn("write a file", &mut sink).unwrap();
        let events = EventLog::replay(session.join("events.jsonl")).unwrap();
        let steered = events
            .iter()
            .position(
                |e| matches!(&e.kind, EventKind::UserInput { text } if text.contains("staging")),
            )
            .expect("steered UserInput");
        assert_eq!(
            notices(&events),
            [(
                steered + 1,
                "recall".into(),
                vec!["project:semantic/deploy.md".into()]
            )]
        );
        assert_eq!(agent.messages(), rehydrate_messages(&events).as_slice());
    }

    /// Recall lands right after the UserInput, the checkpoint is still
    /// named after the input's id, a note is recalled once per session,
    /// and the resumed view is byte-identical.
    #[test]
    fn recall_follows_the_checkpoint_and_resumes_identically() {
        let dir = tmpdir();
        let cfg = v2_cfg(&dir);
        let project = cfg.memory_dir.clone().unwrap();
        note(
            &project,
            "semantic/deploy.md",
            "# Deploy\nstaging deploy uses blue green\n",
        );
        let write = calls(vec![call(
            "w1",
            "write",
            serde_json::json!({"path": "f.txt", "content": "x"}),
        )]);
        let provider = Arc::new(Mock::new(vec![write, done(), done()]));
        let session = dir.join("s");
        let mut agent = Agent::start(provider, cfg, session.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        agent.run_turn("the staging deploy", &mut sink).unwrap();
        let events = EventLog::replay(session.join("events.jsonl")).unwrap();
        let input = events
            .iter()
            .position(|e| matches!(e.kind, EventKind::UserInput { .. }))
            .unwrap();
        let got = notices(&events);
        assert_eq!(
            got,
            [(
                input + 1,
                "recall".into(),
                vec!["project:semantic/deploy.md".into()]
            )]
        );
        let cp = session
            .join("checkpoints")
            .join(format!("e{}", events[input].id));
        assert!(cp.is_dir(), "checkpoint still named after the UserInput id");
        assert_eq!(agent.messages(), rehydrate_messages(&events).as_slice());

        agent
            .run_turn("the staging deploy again", &mut sink)
            .unwrap();
        let events = EventLog::replay(session.join("events.jsonl")).unwrap();
        // The run's own episode note may surface; deploy.md never again.
        let recalled: Vec<String> = notices(&events).into_iter().flat_map(|n| n.2).collect();
        assert_eq!(
            recalled.iter().filter(|n| n.ends_with("deploy.md")).count(),
            1,
            "recalled once per session: {recalled:?}"
        );
        assert_eq!(agent.messages(), rehydrate_messages(&events).as_slice());
    }

    /// A path trigger firing mid-batch is queued to the loop boundary:
    /// one tool_results message, then the reminder; resume is identical;
    /// the note fires once.
    #[test]
    fn path_reminder_waits_for_the_batch_and_fires_once() {
        let dir = tmpdir();
        let cfg = v2_cfg(&dir);
        let project = cfg.memory_dir.clone().unwrap();
        std::fs::create_dir_all(cfg.cwd.join("docs")).unwrap();
        std::fs::write(cfg.cwd.join("docs/a.md"), "hello\n").unwrap();
        note(
            &project,
            "prospective/docs.md",
            "---\ntrigger: path:docs/*.md\n---\nbump the docs version\n",
        );
        let batch = || {
            calls(vec![
                call("r1", "read", serde_json::json!({"path": "docs/a.md"})),
                call("b1", "bash", serde_json::json!({"command": "echo hi"})),
            ])
        };
        let again = calls(vec![call(
            "r2",
            "read",
            serde_json::json!({"path": "docs/a.md", "offset": 1}),
        )]);
        let provider = Arc::new(Mock::new(vec![batch(), again, done()]));
        let session = dir.join("s");
        let mut agent = Agent::start(provider, cfg, session.clone(), "s".into()).unwrap();
        let mut sink = |_: &Event| {};
        agent.run_turn("go", &mut sink).unwrap();
        let events = EventLog::replay(session.join("events.jsonl")).unwrap();
        let got = notices(&events);
        assert_eq!(got.len(), 1, "fires exactly once: {got:?}");
        assert_eq!(
            (got[0].1.as_str(), got[0].2.as_slice()),
            (
                "reminder",
                ["project:prospective/docs.md".to_string()].as_slice()
            )
        );
        let results: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.kind, EventKind::ToolResult { .. }))
            .map(|(i, _)| i)
            .collect();
        assert!(results[1] < got[0].0, "after the whole batch");
        assert_eq!(agent.messages(), rehydrate_messages(&events).as_slice());
        let text = std::fs::read_to_string(project.join("prospective/docs.md")).unwrap();
        assert!(text.contains("\nfired: "), "{text}");
    }

    /// The episode note is written at run end from the log — only past
    /// the threshold, never in subagents.
    #[test]
    fn episode_note_at_run_end() {
        for (sub, turns, want) in [(false, 3, true), (false, 1, false), (true, 3, false)] {
            let dir = tmpdir();
            let mut cfg = v2_cfg(&dir);
            cfg.is_subagent = sub;
            let project = cfg.memory_dir.clone().unwrap();
            let mut script: Vec<Response> = (1..turns).map(tool_turn).collect();
            script.push(done());
            let mut agent =
                Agent::start(Arc::new(Mock::new(script)), cfg, dir.join("s"), "s".into()).unwrap();
            agent.run_turn("do it", &mut |_: &Event| {}).unwrap();
            let episodes = std::fs::read_dir(project.join("episodic")).unwrap().count();
            assert_eq!(episodes == 1, want, "sub={sub} turns={turns}");
        }
    }

    /// A spawned subagent sees filtered copies of BOTH stores and can
    /// search them, but its writes are refused.
    #[test]
    fn subagent_memory_covers_both_stores_read_only() {
        let dir = tmpdir();
        let mut cfg = v2_cfg(&dir);
        cfg.full_access = false;
        let (user, project) = (
            cfg.user_memory_dir.clone().unwrap(),
            cfg.memory_dir.clone().unwrap(),
        );
        note(&user, "procedural/fmt.md", "# Fmt\nkiwi: run cargo fmt\n");
        note(
            &user,
            "semantic/key.md",
            "---\nsensitivity: secret\n---\nkiwi sk-live\n",
        );
        note(&project, "semantic/ci.md", "# CI\nkiwi pipeline notes\n");
        let task = calls(vec![call(
            "t1",
            "task",
            serde_json::json!({"prompt": "look", "mode": "read"}),
        )]);
        let sub = calls(vec![
            call(
                "s1",
                "memory",
                serde_json::json!({"op": "search", "query": "kiwi"}),
            ),
            call(
                "s2",
                "memory",
                serde_json::json!({"op": "remember", "layer": "semantic", "text": "x"}),
            ),
        ]);
        let provider = Arc::new(Mock::new(vec![task, sub, done(), done()]));
        let mut agent = Agent::start(provider, cfg, dir.join("s"), "s".into()).unwrap();
        agent.run_turn("delegate", &mut |_: &Event| {}).unwrap();
        let log = EventLog::replay(dir.join("s/subagents/task-1/events.jsonl")).unwrap();
        let outs: Vec<(String, bool)> = log
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::ToolResult {
                    content, is_error, ..
                } => Some((content.clone(), *is_error)),
                _ => None,
            })
            .collect();
        assert!(outs[0].0.contains("user:procedural/fmt.md"), "{outs:?}");
        assert!(outs[0].0.contains("project:semantic/ci.md"), "{outs:?}");
        assert!(!outs[0].0.contains("key.md"), "secret filtered: {outs:?}");
        assert!(
            outs[1].1 && outs[1].0.contains(crate::tools::memory_tool::SUBAGENT_DENY),
            "{outs:?}"
        );
        assert!(notices(&log).is_empty(), "no recall in subagents");
        assert!(!project.join("semantic/x.md").exists());
    }
}
