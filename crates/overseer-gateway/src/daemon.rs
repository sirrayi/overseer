//! The daemon loop (playbook 5.1): thin, safe, always-on.
//!
//! Per tick: check kill switch → drain control requests → poll triggers
//! → dedup → triage → gate → notify/spawn → reap children → heartbeat.
//! One thread owns all state; the socket thread only forwards requests.
//!
//! Safety posture:
//!   - holds no secrets (env keys are the child's problem, not ours)
//!   - runs no dangerous tools (spawns `overseer exec`, nothing else)
//!   - config is file/CLI-only — reload on mtime change, never a socket
//!     write surface
//!   - kill switch reachable three ways: `overseer daemon kill` (socket),
//!     `touch <dir>/STOP` (file), SIGTERM/SIGINT
//!   - watchdog: heartbeat in the pidfile; a parent monitor (or
//!     launchd KeepAlive) can detect a silently-dead daemon

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};

use crate::config::{load, DaemonConfig, DaemonDirs, TriageDecision};
use crate::ctl::{listen, CtlRequest, CtlResponse};
use crate::event::{now_ms, Dedup, TriggerEvent};
use crate::gate::{in_quiet_hours, route, Route};
use crate::inbox::{Inbox, InboxItem, ItemState};
use crate::journal::Journal;
use crate::notify::PushQueue;
use crate::spawn::{reap, spawn_run_from, Spawned};
use crate::triage::classify;
use crate::channels;
use crate::channels::threads::ThreadRoutes;
use crate::outbox::{LogSender, Outbox, Sender as OutboxSender};
use crate::spawn::Origin;
use crate::trigger::Trigger;

/// Control-channel pair between the socket thread and the daemon loop.
type CtlChannel = (
    Sender<(CtlRequest, Sender<CtlResponse>)>,
    Receiver<(CtlRequest, Sender<CtlResponse>)>,
);

static SIG_RECEIVED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sig(_sig: i32) {
    SIG_RECEIVED.store(true, Ordering::SeqCst);
}

/// Install SIGTERM/SIGINT → flag (libc-free: raw signal(2) via std is
/// not exposed; we register with `signal` through libc only on unix —
/// overseer-tui already carries the libc dep pattern).
#[cfg(unix)]
fn install_signal_flag() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            on_sig as extern "C" fn(i32) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            on_sig as extern "C" fn(i32) as libc::sighandler_t,
        );
    }
}
#[cfg(not(unix))]
fn install_signal_flag() {}

pub struct Daemon {
    /// P7-4: thread → session routing for messaging channels.
    routes: ThreadRoutes,
    /// P7-5: outbound drafts (draft → approve → send).
    outbox: Outbox,
    dirs: DaemonDirs,
    cfg: DaemonConfig,
    cfg_mtime: u64,
    journal: Journal,
    inbox: Inbox,
    push: PushQueue,
    triggers: Vec<Trigger>,
    dedup: Dedup,
    spawned: Vec<Spawned>,
    overseer_bin: PathBuf,
    started_ms: u64,
    counts: Counters,
}

#[derive(Default)]
struct Counters {
    fired: u64,
    deduped: u64,
    triaged: u64,
    pushed: u64,
    inboxed: u64,
    silenced: u64,
    spawned: u64,
    reaped: u64,
}

fn file_mtime(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Daemon {
    pub fn new(dirs: DaemonDirs, overseer_bin: PathBuf) -> Result<Self, String> {
        dirs.ensure().map_err(|e| e.to_string())?;
        let cfg_path = dirs.config();
        let cfg = load(&cfg_path)?;
        let journal = Journal::new(dirs.journal());
        let inbox = Inbox::new(dirs.inbox()).map_err(|e| e.to_string())?;
        let push = PushQueue::new(dirs.push_log());
        let dedup = Dedup::new(cfg.dedup_window_s);
        let triggers: Vec<Trigger> = cfg
            .triggers
            .iter()
            // Webhook spools default to the daemon's own directory; a spec
            // that names its own dir is taken as written.
            .map(|spec| Trigger::from_spec(&resolve_spec(spec, &dirs)))
            .collect();
        // A channel that cannot run (unset token, bad cron) is journaled
        // once at startup: misconfiguration stays visible.
        for trigger in &triggers {
            if let Some(reason) = trigger.unconfigured_reason() {
                journal.log("channel.unconfigured", serde_json::json!({"reason": reason}));
            }
        }
        let routes = ThreadRoutes::open(&dirs.channels()).map_err(|e| e.to_string())?;
        let outbox = Outbox::new(dirs.outbox()).map_err(|e| e.to_string())?;
        Ok(Self {
            routes,
            outbox,
            cfg_mtime: file_mtime(&cfg_path),
            dirs,
            cfg,
            journal,
            inbox,
            push,
            triggers,
            dedup,
            spawned: Vec::new(),
            overseer_bin,
            started_ms: now_ms(),
            counts: Counters::default(),
        })
    }

    /// Write pid + heartbeat timestamp — external watchdogs read this.
    fn write_heartbeat(&self) -> std::io::Result<()> {
        let mut f = std::fs::File::create(self.dirs.pidfile())?;
        writeln!(f, "{}", std::process::id())?;
        writeln!(f, "{}", now_ms())?;
        Ok(())
    }

    fn reload_if_changed(&mut self) {
        let m = file_mtime(&self.dirs.config());
        if m == self.cfg_mtime {
            return;
        }
        match load(&self.dirs.config()) {
            Ok(cfg) => {
                self.dedup = Dedup::new(cfg.dedup_window_s);
                self.triggers = cfg
                    .triggers
                    .iter()
                    .map(|spec| Trigger::from_spec(&resolve_spec(spec, &self.dirs)))
                    .collect();
                self.cfg_mtime = m;
                self.cfg = cfg;
                self.journal.log("config_reload", serde_json::json!({}));
            }
            Err(e) => self
                .journal
                .log("config_reload_failed", serde_json::json!({"error": e})),
        }
    }

    /// The pipeline for one event: dedup → triage → gate → notify/spawn.
    fn process(&mut self, ev: TriggerEvent) {
        self.counts.fired += 1;
        self.journal.log(
            "trigger_fired",
            serde_json::json!({
                "id": ev.id, "source": ev.source, "class": ev.class,
            }),
        );
        if !self.dedup.admit(&ev) {
            self.counts.deduped += 1;
            self.journal
                .log("dedup_drop", serde_json::json!({"id": ev.id}));
            return;
        }
        let mut triage = classify(&self.cfg, &ev);
        // P7-4: inbound channel content is never acted on, whatever the
        // rule table says. An `Act` rule is downgraded to DraftForReview
        // (the template survives for an approval to run), and the session
        // below starts armed with the external-approval floor. Notify
        // stays Notify — a notification is not an execution.
        if ev.untrusted_source && triage.decision == TriageDecision::Act {
            self.journal.log(
                "channel.act_downgraded",
                serde_json::json!({
                    "id": ev.id,
                    "source": ev.source,
                    "class": ev.class,
                }),
            );
            triage.decision = TriageDecision::DraftForReview;
        }
        self.counts.triaged += 1;
        self.journal.log(
            "triage",
            serde_json::json!({
                "id": ev.id, "decision": format!("{:?}", triage.decision)
                    .to_lowercase(),
            }),
        );

        match triage.decision {
            TriageDecision::Ignore => {
                self.counts.silenced += 1;
            }
            TriageDecision::Act => {
                let prompt = triage
                    .act_prompt
                    .clone()
                    .unwrap_or_else(|| ev.payload.clone());
                // Defensive: an untrusted event must never reach the act
                // tier (the downgrade above is the only path here).
                let origin = self.origin_for(&ev);
                self.spawn_for(prompt, None, origin);
            }
            TriageDecision::Notify | TriageDecision::DraftForReview => {
                if let Some(origin) = &ev.origin {
                    // Route the thread to its own session dir (creating it
                    // on first sight) — two conversations never share one.
                    let dir = self.routes.route(&origin.channel, origin.thread.as_deref());
                    self.journal.log(
                        "channel.route",
                        serde_json::json!({
                            "id": ev.id,
                            "channel": origin.channel,
                            "sender": origin.sender,
                            "thread": origin.thread,
                            "intent": origin.intent,
                            "session_dir": dir.display().to_string(),
                        }),
                    );
                }
                let quiet = in_quiet_hours(&self.cfg.gate);
                let r = route(&self.cfg.gate, &ev.class, &triage, quiet);
                self.journal.log(
                    "route",
                    serde_json::json!({"id": ev.id, "route": format!("{r:?}").to_lowercase()}),
                );
                match r {
                    Route::Silent => self.counts.silenced += 1,
                    Route::Inbox => {
                        self.counts.inboxed += 1;
                        self.open_item(&ev, &triage, false);
                    }
                    Route::Push => {
                        self.counts.pushed += 1;
                        let ok = self.push.push(&ev.class, &ev.source, &ev.payload);
                        self.journal
                            .log("push", serde_json::json!({"id": ev.id, "delivered": ok}));
                        // Pushes also leave an inbox trace for audit.
                        self.open_item(&ev, &triage, true);
                    }
                }
            }
        }
    }

    fn open_item(&self, ev: &TriggerEvent, triage: &crate::triage::Triage, pushed: bool) {
        let item = InboxItem {
            id: uuid::Uuid::now_v7().to_string(),
            created_ms: now_ms(),
            class: ev.class.clone(),
            source: ev.source.clone(),
            title: format!("{} · {}", ev.class, ev.source),
            body: ev.payload.clone(),
            act_prompt: triage.act_prompt.clone(),
            state: ItemState::Open,
            until_ms: None,
        };
        if let Err(e) = self.inbox.open(&self.journal, item) {
            self.journal
                .log("inbox_error", serde_json::json!({"error": e.to_string()}));
        }
        if pushed {
            self.journal
                .log("push_inbox_trace", serde_json::json!({"id": ev.id}));
        }
    }

    /// The transport for one draft. `local` is the daemon's own outbox log
    /// (frontends tail it); Telegram is a real send when its token is
    /// configured. Anything else fails honestly — an unconfigured channel
    /// never silently "succeeds".
    fn sender_for(&self, id: &str) -> Box<dyn OutboxSender> {
        let channel = self
            .outbox
            .get(id)
            .map(|d| d.channel)
            .unwrap_or_else(|| "local".to_string());
        match channel.as_str() {
            "telegram" => {
                let spec = self.cfg.triggers.iter().find_map(|t| match t {
                    crate::config::TriggerSpec::Telegram(s) => Some(s.clone()),
                    _ => None,
                });
                match spec.and_then(|s| {
                    crate::channels::telegram::Telegram::from_env(&s.token_env)
                        .ok()
                        .map(|c| c.with_base(s.base.clone()))
                }) {
                    Some(tg) => Box::new(TelegramSender { tg }),
                    None => Box::new(UnconfiguredSender {
                        channel,
                        detail: "no telegram trigger with a configured token".into(),
                    }),
                }
            }
            "local" => Box::new(LogSender::new(self.dirs.outbox().join("sent.jsonl"))),
            other => Box::new(UnconfiguredSender {
                channel: other.to_string(),
                detail: "no transport for this channel".into(),
            }),
        }
    }

    /// The origin a spawn inherits from its event: a channel event carries
    /// the untrusted marker (and therefore the autonomy floor); everything
    /// else is a local run.
    fn origin_for(&self, ev: &TriggerEvent) -> Origin {
        match (&ev.origin, ev.untrusted_source) {
            (Some(origin), _) => Origin::Untrusted(channels::untrusted_marker(origin)),
            (None, true) => Origin::Untrusted(format!("trigger:{}", ev.source)),
            (None, false) => Origin::Local,
        }
    }

    /// Spawn an `overseer exec` run (the only way the daemon does work).
    fn spawn_for(&mut self, prompt: String, inbox_id: Option<String>, origin: Origin) {
        if self.spawned.len() >= self.cfg.spawn.max_concurrent {
            self.journal.log(
                "spawn_deferred",
                serde_json::json!({"prompt": prompt, "reason": "at_cap"}),
            );
            // Deferral becomes an inbox item — capacity is an offer,
            // not a silent drop.
            let item = InboxItem {
                id: uuid::Uuid::now_v7().to_string(),
                created_ms: now_ms(),
                class: "spawn.deferred".into(),
                source: "daemon".into(),
                title: "deferred run (at capacity)".into(),
                body: prompt.clone(),
                act_prompt: Some(prompt),
                state: ItemState::Open,
                until_ms: None,
            };
            let _ = self.inbox.open(&self.journal, item);
            return;
        }
        match spawn_run_from(
            &self.cfg.spawn,
            self.dirs.runs().as_path(),
            &prompt,
            inbox_id,
            &self.overseer_bin,
            &origin,
        ) {
            Ok(sp) => {
                self.counts.spawned += 1;
                self.journal.log(
                    "spawn",
                    serde_json::json!({
                        "id": sp.id, "inbox_id": sp.inbox_id,
                        "prompt": sp.prompt, "log": sp.log_path,
                        "origin": match &origin {
                            Origin::Local => "local".to_string(),
                            Origin::Untrusted(m) => format!("untrusted:{m}"),
                        },
                    }),
                );
                self.spawned.push(sp);
            }
            Err(e) => self
                .journal
                .log("spawn_error", serde_json::json!({"error": e})),
        }
    }

    fn reap_children(&mut self) {
        let mut i = 0;
        while i < self.spawned.len() {
            if let Some(outcome) = reap(&mut self.spawned[i]) {
                self.counts.reaped += 1;
                self.journal.log(
                    "spawn_exit",
                    serde_json::to_value(&outcome).unwrap_or_default(),
                );
                if let Some(iid) = &outcome.inbox_id {
                    let _ = self.inbox.mark_acted(&self.journal, iid);
                }
                self.spawned.remove(i);
            } else {
                i += 1;
            }
        }
    }

    fn handle_ctl(&mut self, req: CtlRequest) -> CtlResponse {
        match req {
            CtlRequest::Status => CtlResponse::ok(serde_json::json!({
                "pid": std::process::id(),
                "up_ms": now_ms() - self.started_ms,
                "triggers": self.triggers.len(),
                "spawned": self.spawned.len(),
                "inbox_open": self.inbox.list().iter()
                    .filter(|i| i.state == ItemState::Open).count(),
                "counts": {
                    "fired": self.counts.fired,
                    "deduped": self.counts.deduped,
                    "pushed": self.counts.pushed,
                    "inboxed": self.counts.inboxed,
                    "silenced": self.counts.silenced,
                    "spawned": self.counts.spawned,
                    "reaped": self.counts.reaped,
                },
            })),
            CtlRequest::Kill => {
                self.journal
                    .log("kill_switch", serde_json::json!({"via": "socket"}));
                std::fs::write(self.dirs.killswitch(), b"socket\n").ok();
                CtlResponse::ok(serde_json::json!({"stopping": true}))
            }
            CtlRequest::InboxList => {
                let items: Vec<serde_json::Value> = self
                    .inbox
                    .list()
                    .into_iter()
                    .map(|i| {
                        serde_json::json!({
                            "id": i.id, "state": i.state, "class": i.class,
                            "source": i.source, "title": i.title, "body": i.body,
                            "act_prompt": i.act_prompt, "created_ms": i.created_ms,
                            "until_ms": i.until_ms,
                        })
                    })
                    .collect();
                CtlResponse::ok(serde_json::json!({"items": items}))
            }
            CtlRequest::InboxDecide {
                id,
                decision,
                snooze_ms,
            } => match self.inbox.decide(&self.journal, &id, &decision, snooze_ms) {
                Ok(item) => {
                    // P7-5: approving a channel draft is the send trigger —
                    // the ladder's external → Ask is what put it there.
                    if item.class == "channel.draft" && decision == "approve" {
                        let outcome = self.outbox.approve_and_send(
                            &self.journal,
                            &item.id,
                            &self.sender_for(&item.id),
                        );
                        return match outcome {
                            Ok(o) => CtlResponse::ok(serde_json::json!({
                                "id": o.draft.id, "state": o.draft.state,
                                "sent": o.sent, "retried": o.retried,
                            })),
                            Err(e) => CtlResponse::err(e),
                        };
                    }
                    CtlResponse::ok(serde_json::json!({
                        "id": item.id, "state": item.state,
                    }))
                }
                Err(e) => CtlResponse::err(e),
            },
            CtlRequest::InboxAct { id } => match self.inbox.get(&id) {
                // A channel draft is not agent work: acting on it sends the
                // message (still through the outbox, still once).
                Some(item) if item.class == "channel.draft" => {
                    match self
                        .outbox
                        .approve_and_send(&self.journal, &item.id, &self.sender_for(&item.id))
                    {
                        Ok(o) => {
                            let _ = self.inbox.mark_acted(&self.journal, &item.id);
                            CtlResponse::ok(serde_json::json!({
                                "id": o.draft.id, "state": o.draft.state, "sent": o.sent,
                            }))
                        }
                        Err(e) => CtlResponse::err(e),
                    }
                }
                Some(item) => {
                    let prompt = item.act_prompt.clone().unwrap_or_else(|| item.body.clone());
                    // An approval is the human's decision, but an item
                    // that came from a channel keeps its untrusted origin
                    // (and therefore the autonomy floor).
                    let origin = match item.class.as_str() {
                        c if c.starts_with("msg.inbound") => {
                            Origin::Untrusted(format!("channel:approved:{}", item.source))
                        }
                        _ => Origin::Local,
                    };
                    self.spawn_for(prompt, Some(item.id.clone()), origin);
                    let _ = self.inbox.mark_acted(&self.journal, &item.id);
                    CtlResponse::ok(serde_json::json!({"id": item.id, "spawned": true}))
                }
                None => CtlResponse::err(format!("inbox: no item '{id}'")),
            },
            CtlRequest::TriggerFire {
                source,
                class,
                payload,
            } => {
                self.process(TriggerEvent::new(source, class, payload));
                CtlResponse::ok(serde_json::json!({"fired": true}))
            }
            CtlRequest::ChannelSend {
                to,
                thread,
                text,
                channel,
            } => {
                // Draft only: the message waits for an inbox approval.
                let channel = channel.unwrap_or_else(|| "local".to_string());
                match self
                    .outbox
                    .draft(&self.journal, &channel, &to, thread.as_deref(), &text)
                {
                    Ok(draft) => {
                        let item = InboxItem {
                            id: draft.id.clone(),
                            created_ms: draft.created_ms,
                            class: "channel.draft".into(),
                            source: format!("{channel}:{to}"),
                            title: format!("outbound {channel} → {to}"),
                            body: draft.text.clone(),
                            // No act_prompt: approval *sends*, it never runs
                            // a prompt on the model's behalf.
                            act_prompt: None,
                            state: ItemState::Open,
                            until_ms: None,
                        };
                        if let Err(e) = self.inbox.open(&self.journal, item) {
                            return CtlResponse::err(e.to_string());
                        }
                        CtlResponse::ok(serde_json::json!({
                            "id": draft.id, "state": draft.state,
                            "channel": draft.channel, "to": draft.to,
                        }))
                    }
                    Err(e) => CtlResponse::err(e),
                }
            }
            CtlRequest::DigestGet => {
                let cards = crate::notify::build_digest(
                    &self.inbox.list(),
                    now_ms(),
                    in_quiet_hours(&self.cfg.gate),
                );
                let cards = crate::notify::expire(cards, now_ms());
                CtlResponse::ok(crate::notify::digest_view(&cards))
            }
            CtlRequest::Reload => {
                let m = file_mtime(&self.dirs.config());
                match load(&self.dirs.config()) {
                    Ok(cfg) => {
                        self.dedup = Dedup::new(cfg.dedup_window_s);
                        self.triggers = cfg
                            .triggers
                            .iter()
                            .map(|spec| Trigger::from_spec(&resolve_spec(spec, &self.dirs)))
                            .collect();
                        self.cfg_mtime = m;
                        self.cfg = cfg;
                        CtlResponse::ok(serde_json::json!({"reloaded": true}))
                    }
                    Err(e) => CtlResponse::err(e),
                }
            }
        }
    }

    /// Run the loop until the kill switch fires. Returns the exit code.
    pub fn run(&mut self) -> i32 {
        install_signal_flag();
        self.dirs.ensure().ok();
        if let Err(e) = self.write_heartbeat() {
            eprintln!("overseer daemon: heartbeat write failed: {e}");
            return 1;
        }

        let (tx, rx): CtlChannel = channel();
        let _listener = match listen(self.dirs.socket(), tx) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("overseer daemon: socket bind: {e}");
                return 1;
            }
        };
        self.journal.log(
            "daemon_start",
            serde_json::json!({
                "pid": std::process::id(),
                "triggers": self.triggers.len(),
            }),
        );

        let tick = std::time::Duration::from_millis(self.cfg.tick_ms.max(50));
        let mut last_heartbeat = 0u64;
        loop {
            // Kill switch: file or socket (both leave the STOP marker).
            if self.dirs.killswitch().exists() || SIG_RECEIVED.load(Ordering::SeqCst) {
                break;
            }
            // Drain control requests.
            while let Ok((req, reply)) = rx.try_recv() {
                let resp = self.handle_ctl(req);
                let _ = reply.send(resp);
                // A Kill request writes the STOP file; next loop exits.
            }
            self.reload_if_changed();

            // Poll triggers → process events.
            let events: Vec<TriggerEvent> =
                self.triggers.iter_mut().flat_map(|t| t.poll()).collect();
            for ev in events {
                self.process(ev);
            }

            self.reap_children();

            // Watchdog heartbeat (dead-man's switch surface).
            let now = now_ms();
            if now - last_heartbeat >= self.cfg.heartbeat_s * 1000 {
                if self.write_heartbeat().is_err() {
                    // Can't prove liveness → stop (dead-man's switch).
                    break;
                }
                last_heartbeat = now;
            }

            std::thread::sleep(tick);
        }

        // Shutdown: kill children, journal, clean the socket + STOP file.
        for sp in self.spawned.iter_mut() {
            let _ = sp.child.kill();
            let _ = sp.child.wait();
        }
        self.journal.log(
            "daemon_stop",
            serde_json::json!({"counts": {
                "fired": self.counts.fired,
                "spawned": self.counts.spawned,
                "reaped": self.counts.reaped,
            }}),
        );
        let _ = std::fs::remove_file(self.dirs.socket());
        let _ = std::fs::remove_file(self.dirs.killswitch());
        let _ = std::fs::remove_file(self.dirs.pidfile());
        0
    }
}

/// The Telegram transport: one plain-text send per approved draft.
struct TelegramSender {
    tg: crate::channels::telegram::Telegram,
}

impl OutboxSender for TelegramSender {
    fn send(&self, draft: &crate::outbox::Draft) -> Result<(), String> {
        self.tg.send(&draft.to, &draft.text)
    }
}

/// A channel with no usable transport. Refusing is the honest answer: a
/// draft that cannot be delivered must stay approved-and-unsent.
struct UnconfiguredSender {
    channel: String,
    detail: String,
}

impl OutboxSender for UnconfiguredSender {
    fn send(&self, _draft: &crate::outbox::Draft) -> Result<(), String> {
        Err(format!(
            "channel '{}' is unconfigured: {}",
            self.channel, self.detail
        ))
    }
}

/// Resolve daemon-relative paths inside a trigger spec (P7-4): a relative
/// webhook spool dir is anchored at the daemon root, so the same config
/// works from any cwd.
fn resolve_spec(spec: &crate::config::TriggerSpec, dirs: &DaemonDirs) -> crate::config::TriggerSpec {
    match spec {
        crate::config::TriggerSpec::Webhook(w) if w.dir.is_relative() => {
            let mut w = w.clone();
            w.dir = dirs.root.join(&w.dir);
            crate::config::TriggerSpec::Webhook(w)
        }
        other => other.clone(),
    }
}

/// Resolve the overseer binary: same dir as an exe (installed
/// together), else PATH lookup.
pub fn overseer_binary() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("overseer")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("overseer"))
}

#[cfg(test)]
mod daemon_pipeline_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::config::{GateConfig, SpawnConfig, TriageRule};

    static N: AtomicU64 = AtomicU64::new(0);

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "overseer-gw-{}-{}-{}-{}",
            tag,
            std::process::id(),
            now_ms(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        dir
    }

    fn journal_records(dir: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("daemon.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn has_kind(recs: &[serde_json::Value], kind: &str) -> bool {
        recs.iter()
            .any(|r| r.get("kind").and_then(|k| k.as_str()) == Some(kind))
    }

    /// Triage table covering every pipeline branch; gate theta=30/cost=40.
    /// `max_concurrent: 0` forces the at-cap deferral branch so no child
    /// process is ever spawned.
    fn pipeline_config() -> DaemonConfig {
        DaemonConfig {
            triggers: Vec::new(),
            triage: vec![
                TriageRule {
                    class: "spam.noise".into(),
                    decision: TriageDecision::Ignore,
                    benefit: 0,
                    act_prompt: None,
                },
                TriageRule {
                    class: "note.low".into(),
                    decision: TriageDecision::Notify,
                    benefit: 10,
                    act_prompt: None,
                },
                // Low benefit on purpose: only `always_push` can route
                // this to Push (5 - 40 < 30).
                TriageRule {
                    class: "security.alert".into(),
                    decision: TriageDecision::Notify,
                    benefit: 5,
                    act_prompt: None,
                },
                TriageRule {
                    class: "job.heavy".into(),
                    decision: TriageDecision::Act,
                    benefit: 90,
                    act_prompt: Some("do {payload}".into()),
                },
            ],
            default_decision: TriageDecision::Notify,
            gate: GateConfig {
                theta: 30,
                cost: 40,
                quiet_hours: None,
                always_push: vec!["security.alert".into()],
            },
            spawn: SpawnConfig {
                max_concurrent: 0,
                max_steps: 5,
                timeout_s: 5,
                cwd: None,
            },
            tick_ms: 50,
            dedup_window_s: 300,
            heartbeat_s: 5,
        }
    }

    fn new_daemon(root: PathBuf, cfg: Option<&DaemonConfig>) -> Daemon {
        if let Some(c) = cfg {
            std::fs::write(
                root.join("config.json"),
                serde_json::to_string_pretty(c).unwrap(),
            )
            .unwrap();
        }
        Daemon::new(DaemonDirs::new(root), PathBuf::from("overseer-test-unused"))
            .expect("daemon new")
    }

    #[test]
    fn new_creates_dirs_and_loads_defaults() {
        let root = tmpdir("new");
        let d = new_daemon(root.clone(), None);
        assert!(root.join("inbox").is_dir());
        assert!(root.join("runs").is_dir());
        assert_eq!(d.dirs.journal(), root.join("daemon.jsonl"));
        assert_eq!(d.cfg.tick_ms, 500);
        assert!(d.triggers.is_empty());
    }

    #[test]
    fn process_ignore_silences_without_inbox() {
        let root = tmpdir("ignore");
        let mut d = new_daemon(root.clone(), Some(&pipeline_config()));
        d.process(TriggerEvent::new("src", "spam.noise", "junk"));
        assert_eq!(d.counts.silenced, 1);
        assert_eq!(d.counts.inboxed, 0);
        assert!(d.inbox.list().is_empty());
        let recs = journal_records(&root);
        assert!(has_kind(&recs, "triage"));
        // Ignore never reaches the gate.
        assert!(!has_kind(&recs, "route"));
    }

    #[test]
    fn process_notify_low_benefit_goes_to_inbox() {
        let root = tmpdir("inbox");
        let mut d = new_daemon(root.clone(), Some(&pipeline_config()));
        d.process(TriggerEvent::new("watch", "note.low", "hello"));
        assert_eq!(d.counts.inboxed, 1);
        assert_eq!(d.inbox.list().len(), 1);
        let recs = journal_records(&root);
        let route = recs
            .iter()
            .find(|r| r.get("kind").and_then(|k| k.as_str()) == Some("route"))
            .expect("route journal");
        assert_eq!(route.get("route").and_then(|v| v.as_str()), Some("inbox"));
    }

    #[test]
    fn process_duplicate_dedups_without_second_item() {
        let root = tmpdir("dedup");
        let mut d = new_daemon(root.clone(), Some(&pipeline_config()));
        d.process(TriggerEvent::new("watch", "note.low", "same-payload"));
        d.process(TriggerEvent::new("watch", "note.low", "same-payload"));
        assert_eq!(d.counts.inboxed, 1);
        assert_eq!(d.counts.deduped, 1);
        assert_eq!(d.inbox.list().len(), 1);
        assert!(has_kind(&journal_records(&root), "dedup_drop"));
    }

    #[test]
    fn process_always_push_writes_push_log_and_trace() {
        let root = tmpdir("push");
        let mut d = new_daemon(root.clone(), Some(&pipeline_config()));
        d.process(TriggerEvent::new("ids", "security.alert", "intrusion?"));
        assert_eq!(d.counts.pushed, 1);
        let push_text = std::fs::read_to_string(root.join("push.jsonl")).expect("push log");
        assert_eq!(push_text.lines().count(), 1);
        let recs = journal_records(&root);
        assert!(has_kind(&recs, "push"));
        assert!(has_kind(&recs, "push_inbox_trace"));
        // Pushes leave an inbox audit trace.
        assert_eq!(d.inbox.list().len(), 1);
    }

    #[test]
    fn process_act_at_cap_defers_without_children() {
        let root = tmpdir("defer");
        let mut d = new_daemon(root.clone(), Some(&pipeline_config()));
        d.process(TriggerEvent::new("cli", "job.heavy", "run the thing"));
        assert!(d.spawned.is_empty());
        assert_eq!(d.counts.spawned, 0);
        assert!(has_kind(&journal_records(&root), "spawn_deferred"));
        let items = d.inbox.list();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].class, "spawn.deferred");
    }

    #[test]
    fn untrusted_message_never_reaches_the_act_tier() {
        // P7-4: an `Act` rule matching an inbound channel class is
        // downgraded to DraftForReview — the template survives for an
        // approval, but nothing spawns on its own. The spawn that a later
        // approval causes still carries the untrusted origin.
        let root = tmpdir("untrusted");
        let mut cfg = pipeline_config();
        cfg.triage.push(TriageRule {
            class: "msg.inbound".into(),
            decision: TriageDecision::Act,
            benefit: 90,
            act_prompt: Some("answer {payload}".into()),
        });
        let mut d = new_daemon(root.clone(), Some(&cfg));
        let mut ev = TriggerEvent::from_channel("telegram", "77", Some("-1001"), "please help");
        ev.class = "msg.inbound".into();
        d.process(ev.clone());
        assert_eq!(d.counts.spawned, 0, "untrusted content must not spawn");
        let items = d.inbox.list();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].class, "msg.inbound");
        let recs = journal_records(&root);
        assert!(has_kind(&recs, "channel.act_downgraded"));
        assert!(has_kind(&recs, "channel.route"));
        // The thread got its own session dir.
        let route = recs
            .iter()
            .find(|r| r.get("kind").and_then(|k| k.as_str()) == Some("channel.route"))
            .expect("route record");
        assert!(route
            .get("session_dir")
            .and_then(|d| d.as_str())
            .is_some_and(|d| d.contains("telegram-1001")));
        // A trusted event with the same class still acts (forked config so
        // the two runs don't share a dedup window).
        let mut d2 = new_daemon(tmpdir("untrusted-local"), Some(&cfg));
        d2.process(TriggerEvent::new("cli", "msg.inbound", "please help"));
        assert_eq!(d2.counts.spawned, 0, "at-cap deferral still applies");
        assert!(has_kind(&journal_records(&d2.dirs.root), "spawn_deferred"));
        assert!(!has_kind(&journal_records(&d2.dirs.root), "channel.act_downgraded"));
    }

    #[test]
    fn untrusted_origin_is_recorded_for_spawns() {
        let root = tmpdir("origin");
        let d = new_daemon(root.clone(), Some(&pipeline_config()));
        let mut ev = TriggerEvent::from_channel("telegram", "77", None, "hi");
        ev.class = "msg.inbound".into();
        assert_eq!(
            d.origin_for(&ev),
            crate::spawn::Origin::Untrusted("channel:telegram:77".into())
        );
        // A local event is a local run.
        assert_eq!(
            d.origin_for(&TriggerEvent::new("cli", "note.low", "x")),
            crate::spawn::Origin::Local
        );
        // An event that is untrusted without a channel envelope still gets
        // a floor (defensive: the flag alone must never mean "act").
        let mut bare = TriggerEvent::new("relay", "note.low", "x");
        bare.untrusted_source = true;
        assert_eq!(
            d.origin_for(&bare),
            crate::spawn::Origin::Untrusted("trigger:relay".into())
        );
    }

    #[test]
    fn channel_triggers_are_journaled_when_unconfigured() {
        // A channel trigger that cannot run says so once at startup —
        // never a silent no-op.
        let root = tmpdir("unconfigured");
        let mut cfg = pipeline_config();
        cfg.triggers.push(crate::config::TriggerSpec::Telegram(
            crate::config::TelegramSpec {
                id: "tg".into(),
                token_env: "OVERSEER_TELEGRAM_TOKEN_UNSET_4ab".into(),
                allow_senders: vec!["77".into()],
                rate_per_min: 5,
                base: "http://127.0.0.1:1".into(),
            },
        ));
        let _d = new_daemon(root.clone(), Some(&cfg));
        let recs = journal_records(&root);
        assert!(has_kind(&recs, "channel.unconfigured"));
    }

    #[test]
    fn channel_send_drafts_then_approval_sends_exactly_once() {
        // P7-5: the socket call drafts; approval sends; a second approval
        // does not send again. External comms never bypass the ladder.
        let root = tmpdir("channel-send");
        let mut d = new_daemon(root.clone(), Some(&pipeline_config()));
        let resp = d.handle_ctl(CtlRequest::ChannelSend {
            to: "ops".into(),
            thread: Some("t1".into()),
            text: "deploy is done".into(),
            channel: None,
        });
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["data"]["state"], "draft");
        let id = v["data"]["id"].as_str().unwrap().to_string();
        assert!(root.join("outbox").join(format!("{id}.json")).exists());

        // The draft is an inbox approval with no act prompt: approving
        // sends, it never runs a prompt as agent work.
        let items = d.inbox.list();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].class, "channel.draft");
        assert!(items[0].act_prompt.is_none());
        assert!(root.join("outbox").join("sent.jsonl").exists() == false);

        let first = d.handle_ctl(CtlRequest::InboxDecide {
            id: id.clone(),
            decision: "approve".into(),
            snooze_ms: None,
        });
        let first = serde_json::to_value(&first).unwrap();
        assert_eq!(first["data"]["sent"], true, "{first}");
        let second = d.handle_ctl(CtlRequest::InboxDecide {
            id: id.clone(),
            decision: "approve".into(),
            snooze_ms: None,
        });
        let second = serde_json::to_value(&second).unwrap();
        assert_eq!(second["data"]["sent"], false, "double approve sends once");
        let sent = std::fs::read_to_string(root.join("outbox").join("sent.jsonl")).unwrap();
        assert_eq!(sent.lines().count(), 1);
        let rec: serde_json::Value = serde_json::from_str(sent.lines().next().unwrap()).unwrap();
        assert_eq!(rec["text"], "deploy is done");
        assert_eq!(rec["channel"], "local");
        assert!(rec["idempotency_key"].as_str().is_some_and(|k| !k.is_empty()));

        // An unconfigured transport refuses honestly and stays retryable.
        let resp = d.handle_ctl(CtlRequest::ChannelSend {
            to: "ops".into(),
            thread: None,
            text: "escalate".into(),
            channel: Some("carrier-pigeon".into()),
        });
        let v = serde_json::to_value(&resp).unwrap();
        let pigeon = v["data"]["id"].as_str().unwrap().to_string();
        let r = d.handle_ctl(CtlRequest::InboxDecide {
            id: pigeon.clone(),
            decision: "approve".into(),
            snooze_ms: None,
        });
        let r = serde_json::to_value(&r).unwrap();
        assert_eq!(r["ok"], false);
        assert!(r["error"]
            .as_str()
            .is_some_and(|e| e.contains("unconfigured") && e.contains("carrier-pigeon")));
        assert_eq!(
            d.outbox.get(&pigeon).unwrap().state,
            crate::outbox::DraftState::Approved,
            "a failed send stays approved, never lost or faked"
        );

        // The digest is a protocol view over the same inbox: resolved items
        // leave no cards…
        let digest = serde_json::to_value(&d.handle_ctl(CtlRequest::DigestGet)).unwrap();
        assert_eq!(digest["data"]["source"], "inbox");
        assert!(
            digest["data"]["cards"]
                .as_array()
                .is_some_and(|c| c.is_empty()),
            "decided items are not pending: {digest}"
        );
        // …and a fresh draft shows up as a pending card.
        let _ = d.handle_ctl(CtlRequest::ChannelSend {
            to: "ops".into(),
            thread: None,
            text: "still pending".into(),
            channel: None,
        });
        let digest = serde_json::to_value(&d.handle_ctl(CtlRequest::DigestGet)).unwrap();
        let cards = digest["data"]["cards"].as_array().unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0]["class"], "channel.draft");
        assert_eq!(cards[0]["count"], 1);
    }

    #[test]
    fn reload_malformed_keeps_old_config() {
        let root = tmpdir("reload");
        let mut d = new_daemon(root.clone(), Some(&pipeline_config()));
        assert!(d.triggers.is_empty());
        std::fs::write(root.join("config.json"), "{ malformed json").unwrap();
        // Force the mtime check deterministically (same-ms writes flake).
        d.cfg_mtime = 0;
        d.reload_if_changed();
        assert!(d.triggers.is_empty());
        assert_eq!(d.cfg.triage.len(), pipeline_config().triage.len());
        assert!(has_kind(&journal_records(&root), "config_reload_failed"));
    }
}
