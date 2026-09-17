//! Daemon configuration — file/CLI only, never a model-reachable surface.
//!
//! Playbook Ch.11 §5.2 (OpenClaw CVE-2026-45001): "the trust boundary
//! between the model and the host operator collapses at the configuration
//! layer." There is deliberately no config.patch endpoint; changes happen
//! by editing `config.json` or via `overseer daemon` CLI flags, followed
//! by a daemon reload (SIGHUP-style: the loop re-reads the file each tick
//! when its mtime changes).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Root daemon dir: `~/.overseer/daemon` (or `--daemon-dir` override).
#[derive(Debug, Clone)]
pub struct DaemonDirs {
    pub root: PathBuf,
}

impl DaemonDirs {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("config.json")
    }
    pub fn journal(&self) -> PathBuf {
        self.root.join("daemon.jsonl")
    }
    pub fn inbox(&self) -> PathBuf {
        self.root.join("inbox")
    }
    pub fn push_log(&self) -> PathBuf {
        self.root.join("push.jsonl")
    }
    pub fn socket(&self) -> PathBuf {
        self.root.join("daemon.sock")
    }
    pub fn pidfile(&self) -> PathBuf {
        self.root.join("daemon.pid")
    }
    pub fn killswitch(&self) -> PathBuf {
        self.root.join("STOP")
    }
    pub fn runs(&self) -> PathBuf {
        self.root.join("runs")
    }
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.inbox())?;
        std::fs::create_dir_all(self.runs())
    }
}

/// A trigger the daemon owns (playbook §2.1 trigger classes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSpec {
    /// Fire every `every_s` seconds. v1 granularity is seconds — the tick
    /// loop is the scheduler; wall-clock cron syntax comes with the
    /// messaging frontends.
    Interval {
        id: String,
        every_s: u64,
        /// Prompt passed to the act tier, or body of the notification.
        body: String,
        #[serde(default)]
        class: String,
    },
    /// Fire when a glob-matched file under `dir` changes mtime.
    Watch {
        id: String,
        dir: PathBuf,
        /// Substring match on file name (glob comes later; keep v1 simple).
        #[serde(default)]
        name_contains: String,
        body: String,
        #[serde(default)]
        class: String,
    },
    /// Periodic check-in turn: the daemon emits a heartbeat event at
    /// `every_s`; whether it becomes anything is the triage layer's call.
    Heartbeat { id: String, every_s: u64 },
}

/// One deterministic triage rule: match on event class/source → decision.
/// First match wins; the catch-all default is `ClassPolicy::default_class`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageRule {
    /// Event class to match ("git.dirty", "ci.failed", …). "*" matches all.
    pub class: String,
    pub decision: TriageDecision,
    /// Benefit score 0–100 feeding the gate when decision is notify/draft.
    #[serde(default = "default_benefit")]
    pub benefit: u8,
    /// For `act`: prompt template (`{payload}` is substituted).
    #[serde(default)]
    pub act_prompt: Option<String>,
}

fn default_benefit() -> u8 {
    50
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriageDecision {
    Ignore,
    Notify,
    DraftForReview,
    Act,
}

/// User attention state for the EV-of-interruption gate (§2.3 Horvitz).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateConfig {
    /// Minimum (benefit − cost) to justify a push. Higher = quieter.
    #[serde(default = "default_theta")]
    pub theta: i16,
    /// Cost of interrupting right now, 0–100. Static v1; quiet-hours and
    /// focus signals raise it automatically.
    #[serde(default = "default_cost")]
    pub cost: u8,
    /// Quiet hours (local clock, HH:MM–HH:MM, may wrap midnight). During
    /// quiet hours only `always_push` classes escape the inbox.
    #[serde(default)]
    pub quiet_hours: Option<QuietHours>,
    /// Classes that bypass the gate entirely (e.g. "security.alert").
    #[serde(default)]
    pub always_push: Vec<String>,
}

fn default_theta() -> i16 {
    30
}
fn default_cost() -> u8 {
    40
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuietHours {
    /// "22:00"
    pub start: String,
    /// "08:00"
    pub end: String,
}

/// Policy for spawned agent runs (the "act" tier).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnConfig {
    /// Hard cap on concurrent agent runs (dead-man discipline).
    #[serde(default = "default_max_runs")]
    pub max_concurrent: usize,
    /// Step budget passed to `overseer exec --max-steps`.
    #[serde(default = "default_steps")]
    pub max_steps: u32,
    /// Per-run wall-clock cap in seconds — watchdog kills the child.
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// Working directory for spawned runs.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

fn default_max_runs() -> usize {
    2
}
fn default_steps() -> u32 {
    40
}
fn default_timeout() -> u64 {
    600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub triggers: Vec<TriggerSpec>,
    #[serde(default)]
    pub triage: Vec<TriageRule>,
    /// Fallback decision for unmatched event classes.
    #[serde(default = "default_class_decision")]
    pub default_decision: TriageDecision,
    #[serde(default)]
    pub gate: GateConfig,
    #[serde(default)]
    pub spawn: SpawnConfig,
    /// Main-loop tick in milliseconds (scheduler granularity).
    #[serde(default = "default_tick")]
    pub tick_ms: u64,
    /// Dedup window: identical event keys inside this window are dropped.
    #[serde(default = "default_dedup")]
    pub dedup_window_s: u64,
    /// Heartbeat/pidfile write interval in seconds (watchdog reads it).
    #[serde(default = "default_heartbeat")]
    pub heartbeat_s: u64,
}

fn default_class_decision() -> TriageDecision {
    TriageDecision::Notify
}
fn default_tick() -> u64 {
    500
}
fn default_dedup() -> u64 {
    300
}
fn default_heartbeat() -> u64 {
    5
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            triggers: Vec::new(),
            triage: Vec::new(),
            default_decision: default_class_decision(),
            gate: GateConfig {
                theta: default_theta(),
                cost: default_cost(),
                quiet_hours: None,
                always_push: Vec::new(),
            },
            spawn: SpawnConfig {
                max_concurrent: default_max_runs(),
                max_steps: default_steps(),
                timeout_s: default_timeout(),
                cwd: None,
            },
            tick_ms: default_tick(),
            dedup_window_s: default_dedup(),
            heartbeat_s: default_heartbeat(),
        }
    }
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            theta: default_theta(),
            cost: default_cost(),
            quiet_hours: None,
            always_push: Vec::new(),
        }
    }
}

impl Default for SpawnConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_max_runs(),
            max_steps: default_steps(),
            timeout_s: default_timeout(),
            cwd: None,
        }
    }
}

/// Load config; missing file → defaults. Malformed JSON is a hard error:
/// the daemon refuses to run on a config it can't fully parse rather
/// than silently dropping rules (config integrity is a trust boundary).
pub fn load(path: &Path) -> Result<DaemonConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DaemonConfig::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "overseer-gateway-config-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create tmpdir");
        dir
    }

    fn assert_defaults(cfg: &DaemonConfig) {
        assert_eq!(cfg.tick_ms, 500);
        assert_eq!(cfg.dedup_window_s, 300);
        assert_eq!(cfg.gate.theta, 30);
        assert_eq!(cfg.gate.cost, 40);
        assert_eq!(cfg.spawn.max_concurrent, 2);
        assert_eq!(cfg.spawn.max_steps, 40);
        assert_eq!(cfg.spawn.timeout_s, 600);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = tmpdir("missing").join("no-such-config.json");
        let _ = std::fs::remove_file(&path);
        let cfg = load(&path).expect("missing file loads");
        assert_defaults(&cfg);
    }

    #[test]
    fn malformed_json_is_hard_error() {
        let path = tmpdir("malformed").join("config.json");
        std::fs::write(&path, "{ not valid json").expect("write config");
        assert!(load(&path).is_err());
    }

    #[test]
    fn empty_object_yields_defaults() {
        let path = tmpdir("empty").join("config.json");
        std::fs::write(&path, "{}").expect("write config");
        let cfg = load(&path).expect("{} loads");
        assert_defaults(&cfg);
    }
}
