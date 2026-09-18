//! Browser-session, fixture-selection, and validate-flag patterns ported from
//! the browser/agent-surface batch (arsenal B2).
//!
//! Three ports, all pure data-plus-decision logic — no browser, no server, no
//! runtime, no model call, no new dependency:
//!
//! - **steel-browser session pool** (Apache-2.0) — a bounded pool of browser
//!   sessions: reuse an idle session before paying to create one, never
//!   exceed the cap, and close the oldest idle sessions under a tick budget.
//! - **WebArena fixture regression selection** (Apache-2.0) — the required
//!   keys of a task fixture (the oracle included) and the set of fixtures a
//!   site change invalidates.
//! - **Skyvern `--validate` flag** (AGPL-3.0) — the CLI spelling of "check
//!   the finished run against `url`/`text`/`element`, then retry at most
//!   `max_retries` times", plus the retry decision.
//!
//! Skyvern is **AGPL-3.0**: only the *semantics* are ported, from the
//! published CLI surface and docs; no code was taken, and nothing is vendored
//! or linked — the same treatment this repo gives restate and inngest in
//! `orchestration.rs`. That licence is also why the validate loop itself is
//! deferred rather than reimplemented here.
//!
//! `// DEFERRED(owner): a real browser pool backed by a steel-browser server
//! or a CDP endpoint, fetching WebArena fixtures from a live site registry,
//! and Skyvern's own validate-then-retry loop (a browser plus a model call) —
//! the pool arithmetic, the fixture gate, and the flag contract land here;
//! the runtimes stay out by licence policy and by scope.`

use serde_json::{Map, Value};

// ── steel-browser session pool ───────────────────────────────────────────

/// Upper bound on a pool's `max`, and the cap the gate enforces: a browser
/// session is a process tree, not a handle.
pub const MAX_POOL: usize = 64;

/// One pooled browser session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Stable id the pool hands to a caller; unique within the pool.
    pub id: String,
    /// Page the session is parked on, if any (`None` for a fresh session).
    /// The pool never guesses a page: reuse leaves this field alone.
    pub url: Option<String>,
    /// True while a caller holds the session; an in-use session is never
    /// reused and never reaped.
    pub in_use: bool,
    /// Creation time, on the caller's clock.
    pub created_ms: u64,
    /// Time of the last acquire, on the caller's clock. This single field
    /// drives both the reuse order and the reap order (older = reused sooner,
    /// reaped sooner).
    pub last_used_ms: u64,
}

/// The pool's configuration: just its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pool {
    /// Maximum number of live sessions, in `1..=MAX_POOL`.
    pub max: usize,
}

/// The one gate on a pool.
///
/// A cap of 0 is refused because `acquire` can never succeed under it — the
/// pool would be dead weight that fails at the call site instead of at
/// configuration time. A cap above [`MAX_POOL`] is refused rather than
/// clamped: a clamped cap would leave the operator believing they configured
/// 1000 browsers.
pub fn validate_pool(p: &Pool) -> Result<(), String> {
    if p.max == 0 {
        return Err(
            "pool: max is 0 — a pool that can never hold a session fails every acquire; want \
             1..=64"
                .to_string(),
        );
    }
    if p.max > MAX_POOL {
        return Err(format!(
            "pool: max {} exceeds the cap of {MAX_POOL} — a browser session is a process tree; \
             want 1..={MAX_POOL}",
            p.max
        ));
    }
    Ok(())
}

/// Take a session for a caller, returning the id now checked out.
///
/// Invariants (all asserted by the tests):
/// - the pool is validated first, so a bad cap errors before anything is
///   touched;
/// - `new_id` must be non-empty and must not already exist in `sessions`: an
///   unnamed session could never be released, and a colliding id would alias
///   two callers onto one browser. The collision is refused even when this
///   call could have been served by reuse, so the error points at the
///   argument the caller passed;
/// - otherwise the least-recently-used **idle** session is reused when one
///   exists (ties on `last_used_ms` go to the earlier entry in `sessions`, so
///   the choice is reproducible). The reused session gets `in_use = true` and
///   `last_used_ms = now_ms`; its id — never `new_id` — is returned, and its
///   `url` and `created_ms` are left alone;
/// - otherwise a fresh session is created only while the pool is under `max`,
///   with `url: None` and `created_ms == last_used_ms == now_ms`;
/// - otherwise the cap is an error naming both the cap and the live count.
///   The cap is never silently exceeded — no eviction, no queueing, no "just
///   this once".
pub fn acquire(
    p: &Pool,
    sessions: &mut Vec<Session>,
    now_ms: u64,
    new_id: &str,
) -> Result<String, String> {
    validate_pool(p)?;
    let new_id = new_id.trim();
    if new_id.is_empty() {
        return Err(
            "pool: new_id is empty — an unnamed session could never be released or reaped; pass \
             the id the caller will use"
                .to_string(),
        );
    }
    if sessions.iter().any(|s| s.id == new_id) {
        return Err(format!(
            "pool: session `{new_id}` already exists — pool ids must be unique; pass a fresh \
             new_id (or reuse the pool's own id) and the idle session will be reused instead"
        ));
    }
    if let Some(idx) = idle_lru_index(sessions) {
        let s = &mut sessions[idx];
        s.in_use = true;
        s.last_used_ms = now_ms;
        return Ok(s.id.clone());
    }
    if sessions.len() >= p.max {
        return Err(format!(
            "pool: cap reached ({}/{}) and no session is idle — wait for a release or raise the \
             cap (the cap is never exceeded silently)",
            sessions.len(),
            p.max
        ));
    }
    sessions.push(Session {
        id: new_id.to_string(),
        url: None,
        in_use: true,
        created_ms: now_ms,
        last_used_ms: now_ms,
    });
    Ok(new_id.to_string())
}

/// Index of the idle session with the smallest `last_used_ms`, earliest entry
/// winning a tie; `None` when every session is in use.
fn idle_lru_index(sessions: &[Session]) -> Option<usize> {
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.in_use)
        .min_by_key(|(i, s)| (s.last_used_ms, *i))
        .map(|(i, _)| i)
}

/// Hand a session back to the pool.
///
/// An unknown id is an error: the caller and the pool disagree about what the
/// caller holds, and ignoring it would leak the session the caller believes it
/// released. Releasing an already-idle session is **idempotent** — it is
/// `Ok(())` and changes nothing — so a `finally`-style double release, or a
/// retry after a timeout, is harmless rather than a second failure.
///
/// `last_used_ms` is deliberately not touched: idle age is measured from the
/// last acquire, which is what reuse order reads.
pub fn release(sessions: &mut [Session], id: &str) -> Result<(), String> {
    let s = sessions.iter_mut().find(|s| s.id == id).ok_or_else(|| {
        format!("pool: unknown session `{id}` — release takes an id this pool owns")
    })?;
    s.in_use = false;
    Ok(())
}

/// Close up to `max_reap` sessions that have been idle for at least `idle_ms`,
/// oldest first, returning the ids that closed.
///
/// Invariants (all asserted by the tests):
/// - only sessions with `in_use == false` are candidates — a checked-out
///   session is somebody's live browser, and reaping it would pull the page
///   out from under them;
/// - candidates are ordered by `last_used_ms` ascending, ties broken by
///   position in `sessions`, so a reap is reproducible;
/// - at most `max_reap` sessions close per call: a reap is a tick, and its
///   cost must not scale with the pool. `max_reap == 0` closes none — that is
///   the explicit "reap nothing" spelling, which is legal (a caller may reap
///   nothing this tick without special-casing the call);
/// - closed sessions are removed from `sessions`, so their ids can never be
///   resolved by a later `release`, and `acquire` under a stale id is caught
///   as a duplicate;
/// - a `now_ms` behind `last_used_ms` (clock skew) counts as zero idle time:
///   only `idle_ms == 0` reaps then, never a negative age.
pub fn reap(
    sessions: &mut Vec<Session>,
    now_ms: u64,
    idle_ms: u64,
    max_reap: usize,
) -> Vec<String> {
    if max_reap == 0 {
        return Vec::new();
    }
    let mut idx: Vec<usize> = sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.in_use && now_ms.saturating_sub(s.last_used_ms) >= idle_ms)
        .map(|(i, _)| i)
        .collect();
    idx.sort_by_key(|&i| (sessions[i].last_used_ms, i));
    idx.truncate(max_reap);
    let ids: Vec<String> = idx.iter().map(|&i| sessions[i].id.clone()).collect();
    for &i in idx.iter().rev() {
        sessions.remove(i);
    }
    ids
}

// ── WebArena fixture regression selection ────────────────────────────────

/// One WebArena-style task fixture: where to start, what to do, and how the
/// run is judged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFixture {
    /// Fixture id, non-empty; the key a regression selection reports.
    pub id: String,
    /// URL the task starts from.
    pub start_url: String,
    /// The natural-language task.
    pub intent: String,
    /// The oracle script. Required and non-empty: a fixture whose eval script
    /// is missing cannot fail, so it could never regress anything.
    pub eval_script: String,
    /// Sites the fixture touches. Empty when the fixture declares none (see
    /// [`regress_set`]: matching then rests on the eval script alone).
    pub sites: Vec<String>,
}

/// Parse one fixture object.
///
/// Required, non-empty strings: `id`, `start_url`, `intent`, `eval_script`.
/// `sites` defaults to empty (absent or `null`; a serialized optional field
/// must not become an error). Unknown fields are ignored, so a fixture file
/// can carry fields this batch does not model yet. Every error names the
/// offending field, and the missing `eval_script` says *why* it is required.
pub fn parse_fixture(v: &Value) -> Result<TaskFixture, String> {
    let obj = v.as_object().ok_or_else(|| {
        format!(
            "fixture: expected a JSON object, got {} — one fixture is one task",
            kind_of(v)
        )
    })?;
    let sites = match obj.get("sites") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                let s = item.as_str().ok_or_else(|| {
                    format!(
                        "fixture: `sites[{i}]` must be a string, got {}",
                        kind_of(item)
                    )
                })?;
                let s = s.trim();
                if s.is_empty() {
                    return Err(format!(
                        "fixture: `sites[{i}]` is empty — a site name is what the regression \
                         selector matches on"
                    ));
                }
                out.push(s.to_string());
            }
            out
        }
        Some(other) => {
            return Err(format!(
                "fixture: `sites` must be an array of strings, got {}",
                kind_of(other)
            ))
        }
    };
    Ok(TaskFixture {
        id: required_str(obj, "id")?,
        start_url: required_str(obj, "start_url")?,
        intent: required_str(obj, "intent")?,
        eval_script: required_str(obj, "eval_script")?,
        sites,
    })
}

/// Read a required non-empty string field, naming the field and the repair.
fn required_str(obj: &Map<String, Value>, field: &str) -> Result<String, String> {
    let why = if field == "eval_script" {
        "a fixture with no oracle cannot regress anything"
    } else {
        "the field is required and must be non-empty"
    };
    let raw = obj
        .get(field)
        .ok_or_else(|| format!("fixture: missing `{field}` — {why}"))?;
    let s = raw
        .as_str()
        .ok_or_else(|| format!("fixture: `{field}` must be a string, got {}", kind_of(raw)))?;
    let s = s.trim();
    if s.is_empty() {
        return Err(format!("fixture: `{field}` is empty — {why}"));
    }
    Ok(s.to_string())
}

/// JSON type of a value, for error messages.
fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a bool",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Which fixtures a change to `changed_sites` invalidates.
///
/// A fixture is selected when either
/// - one of its `sites` equals a changed site, compared case-insensitively
///   after trimming; or
/// - its `eval_script` *mentions* a changed site: a case-insensitive substring
///   test, because an eval script is Python and parsing it is not this
///   module's job. The mention test can only add fixtures, so a script that
///   names a site its `sites` list forgot still regresses.
///
/// Output is the selected ids in fixture order, deduplicated (a fixture
/// declared twice runs once), so two runs over the same inputs produce the
/// same plan. An empty `changed_sites` — or one whose entries all trim to
/// empty — selects **nothing**: a regression run with no change is empty by
/// definition, and returning every fixture would silently redefine "the
/// regression set" as "the whole suite".
pub fn regress_set(fixtures: &[TaskFixture], changed_sites: &[String]) -> Vec<String> {
    let changed: Vec<String> = changed_sites
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for f in fixtures {
        let sites: Vec<String> = f
            .sites
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .collect();
        let script = f.eval_script.to_ascii_lowercase();
        let hit = changed
            .iter()
            .any(|c| sites.iter().any(|s| s == c) || script.contains(c.as_str()));
        if hit && !out.contains(&f.id) {
            out.push(f.id.clone());
        }
    }
    out
}

// ── Skyvern `--validate` flag (pattern only, AGPL-3.0) ───────────────────

/// One check a `--validate` run performs against the finished task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The `none` token: the explicit "check nothing" spelling. It is legal
    /// input only in the sense that it is *named* in an error — the gate
    /// refuses it ([`validate_flag`]), because "validate, but check nothing"
    /// is a no-op, not a configuration.
    None,
    /// The page URL must match.
    Url,
    /// Expected text must appear on the page.
    Text,
    /// An expected element must exist.
    Element,
}

impl Check {
    /// The vocabulary, in the order error messages list it.
    pub const ALL: [Check; 4] = [Check::Url, Check::Text, Check::Element, Check::None];

    /// The CLI token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Check::None => "none",
            Check::Url => "url",
            Check::Text => "text",
            Check::Element => "element",
        }
    }

    /// Parse one token, case-insensitively; an unknown token errors naming
    /// the vocabulary.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim().to_ascii_lowercase();
        Check::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| {
                let names: Vec<&str> = Check::ALL.iter().map(|c| c.as_str()).collect();
                format!("validate: unknown check `{s}` — want {}", names.join("|"))
            })
    }
}

/// A parsed `--validate` flag: whether validation runs, what it checks, and
/// the attempt budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validation {
    /// False when the flag was absent (or the gate found a contradiction).
    pub enabled: bool,
    /// The checks to run; non-empty exactly when `enabled` (see
    /// [`validate_flag`]).
    pub checks: Vec<Check>,
    /// Attempt budget, `0..=MAX_RETRIES`.
    pub max_retries: u32,
}

/// Attempt budget when `--max-retries` is absent. One: each retry re-runs the
/// whole task, so the default spends at most one extra run.
pub const DEFAULT_MAX_RETRIES: u32 = 1;

/// Upper bound on `--max-retries`: a retry is a task re-run, and an unbounded
/// budget is an unbounded spend discovered on the invoice.
pub const MAX_RETRIES: u32 = 5;

/// The one gate on a [`Validation`].
///
/// Exported because `Validation` is a public struct: the parser is not the
/// only way to build one, and these are exactly the malformed states a
/// hand-built value reaches. Each refusal names the field and the repair:
/// - `max_retries` outside `0..=MAX_RETRIES`;
/// - `enabled` with no checks, or with a `none` entry — "a validate flag with
///   nothing to check is a no-op"; accepting it would let a run report
///   "validated" while nothing was ever checked, the same silent downgrade
///   `tools/computer.rs` refuses for an unconfigured backend;
/// - `!enabled` with checks — a contradiction: the flag says off, the check
///   list says what to check. Guessing which one the operator meant would be
///   a silent downgrade in one direction or the other, so it is refused.
pub fn validate_flag(v: &Validation) -> Result<(), String> {
    if v.max_retries > MAX_RETRIES {
        return Err(format!(
            "validate: max_retries {} exceeds the cap of {MAX_RETRIES} — each retry re-runs the \
             task; want 0..={MAX_RETRIES}",
            v.max_retries
        ));
    }
    if v.enabled {
        if v.checks.is_empty() {
            return Err(
                "validate: enabled with no checks — a validate flag with nothing to check is a \
                 no-op; want url|text|element, or omit the flag to disable validation"
                    .to_string(),
            );
        }
        if v.checks.contains(&Check::None) {
            let names = check_names(&v.checks);
            return Err(format!(
                "validate: enabled with checks [{names}] — `none` names no check, and a validate \
                 flag with nothing to check is a no-op; want url|text|element, or omit the flag \
                 to disable validation"
            ));
        }
    } else if !v.checks.is_empty() {
        let names = check_names(&v.checks);
        return Err(format!(
            "validate: disabled with {} check(s) [{names}] — contradiction: either drop the \
             checks or enable the flag",
            v.checks.len()
        ));
    }
    Ok(())
}

/// The check tokens of a list, comma-joined, for error messages.
fn check_names(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<&str>>()
        .join(",")
}

/// Parse a CLI-shaped argument list — `["--validate", "url,text",
/// "--max-retries", "2"]`, and the `--validate=url,text` spelling of the same
/// flag. Arguments that are not this flag (the task URL, unrelated options)
/// are ignored: a flag parser that rejected them would be a CLI, and this
/// module ports the flag.
///
/// Invariants (all asserted by the tests):
/// - the last occurrence of each flag wins, as a CLI does;
/// - a value is required after the spaced spelling; `--max-retries` must be a
///   whole number in `0..=MAX_RETRIES`;
/// - the check list is comma-separated, trimmed, case-insensitive, and
///   deduplicated first-wins; every unknown token is reported together with
///   the vocabulary, so one message fixes a typo'd list;
/// - no `--validate` at all yields `enabled: false` with no checks — that is
///   how a run says "do not validate", and it is not an error.
///   `--max-retries` without `--validate` is parsed and inert: it is the
///   budget of a disabled flag;
/// - a flag whose checks are empty, only `none`, or `none` mixed with a real
///   check is an error, because [`validate_flag`] runs on the result before
///   it is returned — the parser cannot hand back a state the gate rejects.
pub fn parse_validate_flag(args: &[&str]) -> Result<Validation, String> {
    let mut raw_checks: Option<String> = None;
    let mut max_retries: Option<u32> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--validate" {
            let value = args.get(i + 1).ok_or_else(missing_value)?;
            raw_checks = Some((*value).to_string());
            i += 2;
        } else if let Some(value) = arg.strip_prefix("--validate=") {
            raw_checks = Some(value.to_string());
            i += 1;
        } else if arg == "--max-retries" {
            let value = args.get(i + 1).ok_or_else(missing_value)?;
            max_retries = Some(parse_retries(value)?);
            i += 2;
        } else if let Some(value) = arg.strip_prefix("--max-retries=") {
            max_retries = Some(parse_retries(value)?);
            i += 1;
        } else {
            i += 1;
        }
    }

    let (enabled, checks) = match raw_checks {
        None => (false, Vec::new()),
        Some(raw) => {
            let tokens: Vec<&str> = raw
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect();
            if tokens.is_empty() {
                return Err(format!(
                    "validate: `--validate {raw}` names no check — a validate flag with nothing \
                     to check is a no-op; want url|text|element"
                ));
            }
            let mut checks: Vec<Check> = Vec::with_capacity(tokens.len());
            let mut unknown: Vec<&str> = Vec::new();
            for t in tokens {
                match Check::parse(t) {
                    Ok(c) if !checks.contains(&c) => checks.push(c),
                    Ok(_) => {}
                    Err(_) => {
                        if !unknown.contains(&t) {
                            unknown.push(t);
                        }
                    }
                }
            }
            if !unknown.is_empty() {
                let names: Vec<&str> = Check::ALL.iter().map(|c| c.as_str()).collect();
                return Err(format!(
                    "validate: unknown check(s) {} — want {}",
                    unknown.join(", "),
                    names.join("|")
                ));
            }
            (true, checks)
        }
    };

    let v = Validation {
        enabled,
        checks,
        max_retries: max_retries.unwrap_or(DEFAULT_MAX_RETRIES),
    };
    validate_flag(&v)?;
    Ok(v)
}

/// The error for a flag whose value is missing.
fn missing_value() -> String {
    "validate: `--validate` needs a value — e.g. `--validate url,text` (omit the flag to disable \
     validation)"
        .to_string()
}

/// Parse a retry budget, naming the band on both a bad number and a bad range.
fn parse_retries(raw: &str) -> Result<u32, String> {
    let n: u32 = raw.trim().parse().map_err(|_| {
        format!(
            "validate: `--max-retries {raw}` is not a whole number — want 0..={MAX_RETRIES} \
             (each retry re-runs the task)"
        )
    })?;
    if n > MAX_RETRIES {
        return Err(format!(
            "validate: max_retries {n} exceeds the cap of {MAX_RETRIES} — each retry re-runs the \
             task; want 0..={MAX_RETRIES}"
        ));
    }
    Ok(n)
}

/// Whether a run may be attempted again: the flag is enabled, it names at
/// least one real check, `attempts` is under the budget, and the flag itself
/// is well-formed — a contradictory [`Validation`] never buys a retry.
///
/// `attempts` is the number of attempts already made (0 = nothing has run
/// yet), so `max_retries` is the whole attempt budget: 2 permits a first run
/// plus one retry. `max_retries == 0` (legal) admits the first attempt and
/// never a retry.
pub fn retry_allowed(v: &Validation, attempts: u32) -> bool {
    validate_flag(v).is_ok() && v.enabled && attempts < v.max_retries
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sess(id: &str, in_use: bool, last_used_ms: u64) -> Session {
        Session {
            id: id.to_string(),
            url: None,
            in_use,
            created_ms: 1,
            last_used_ms,
        }
    }

    fn pool(max: usize) -> Pool {
        Pool { max }
    }

    // ── pool ────────────────────────────────────────────────────────────

    #[test]
    fn pool_cap_is_bounded_to_one_through_sixty_four() {
        assert!(validate_pool(&pool(1)).is_ok());
        assert!(validate_pool(&pool(MAX_POOL)).is_ok());
        let err = validate_pool(&pool(0)).unwrap_err();
        assert!(err.contains("max is 0"), "{err}");
        assert!(err.contains("1..=64"), "names the band: {err}");
        let err = validate_pool(&pool(65)).unwrap_err();
        assert!(err.contains("65"), "names the value: {err}");
        assert!(err.contains("cap of 64"), "{err}");
        // An acquire under a bad cap fails before touching the pool.
        let mut sessions = vec![];
        assert!(acquire(&pool(0), &mut sessions, 0, "s1")
            .unwrap_err()
            .contains("max is 0"));
        assert!(sessions.is_empty());
    }

    #[test]
    fn acquire_reuses_the_least_recently_used_idle_session() {
        let mut sessions = vec![
            sess("busy", true, 5),
            sess("old", false, 10),
            sess("new", false, 20),
        ];
        let id = acquire(&pool(4), &mut sessions, 100, "fresh").unwrap();
        assert_eq!(id, "old", "the older idle session is reused first");
        assert_eq!(sessions.len(), 3, "reuse creates nothing");
        let used = sessions.iter().find(|s| s.id == "old").unwrap();
        assert!(used.in_use);
        assert_eq!(used.last_used_ms, 100, "the acquire bumps last_used_ms");
        assert_eq!(used.created_ms, 1, "creation time is not rewritten");
        assert!(sessions.iter().find(|s| s.id == "busy").unwrap().in_use);
        assert!(!sessions.iter().find(|s| s.id == "new").unwrap().in_use);
        // The next acquire takes the other idle session, not the one just used.
        let id2 = acquire(&pool(4), &mut sessions, 101, "fresh2").unwrap();
        assert_eq!(id2, "new");
    }

    #[test]
    fn acquire_tie_breaks_by_position_so_the_choice_is_reproducible() {
        let mut sessions = vec![sess("a", false, 10), sess("b", false, 10)];
        assert_eq!(acquire(&pool(2), &mut sessions, 50, "c").unwrap(), "a");
    }

    #[test]
    fn acquire_creates_a_session_while_under_the_cap() {
        let mut sessions = vec![];
        let id = acquire(&pool(2), &mut sessions, 7, "s1").unwrap();
        assert_eq!(id, "s1");
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0],
            Session {
                id: "s1".into(),
                url: None,
                in_use: true,
                created_ms: 7,
                last_used_ms: 7,
            }
        );
        // A released session is idle, so the third acquire reuses it rather
        // than creating a second one.
        release(&mut sessions, "s1").unwrap();
        assert_eq!(acquire(&pool(2), &mut sessions, 9, "s2").unwrap(), "s1");
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn acquire_at_the_cap_with_nothing_idle_errors() {
        let mut sessions = vec![sess("s1", true, 1)];
        let err = acquire(&pool(1), &mut sessions, 2, "s2").unwrap_err();
        assert!(err.contains("cap reached (1/1)"), "{err}");
        assert!(
            err.contains("never exceeded"),
            "states the invariant: {err}"
        );
        assert_eq!(sessions.len(), 1, "nothing was evicted to make room");
    }

    #[test]
    fn acquire_rejects_a_duplicate_or_empty_new_id() {
        let mut sessions = vec![sess("s1", true, 1)];
        let err = acquire(&pool(4), &mut sessions, 2, "s1").unwrap_err();
        assert!(err.contains("`s1` already exists"), "{err}");
        assert_eq!(sessions.len(), 1);
        // The collision is refused even though an idle session could have
        // served the call: the argument itself is wrong.
        let mut mixed = vec![sess("s1", true, 1), sess("s2", false, 1)];
        assert!(acquire(&pool(4), &mut mixed, 2, "s1")
            .unwrap_err()
            .contains("already exists"));
        assert!(!mixed[1].in_use, "the idle session was not touched");
        let err = acquire(&pool(4), &mut sessions, 2, "   ").unwrap_err();
        assert!(err.contains("new_id is empty"), "{err}");
    }

    #[test]
    fn release_unknown_id_errors_and_a_second_release_is_a_no_op() {
        let mut sessions = vec![sess("s1", true, 1)];
        let err = release(&mut sessions, "nope").unwrap_err();
        assert!(err.contains("unknown session `nope`"), "{err}");
        release(&mut sessions, "s1").unwrap();
        assert!(!sessions[0].in_use);
        let before = sessions[0].last_used_ms;
        release(&mut sessions, "s1").unwrap();
        assert_eq!(sessions[0].last_used_ms, before, "a no-op changes nothing");
        assert_eq!(sessions.len(), 1);
        // And the released session is reusable.
        assert_eq!(acquire(&pool(1), &mut sessions, 3, "s2").unwrap(), "s1");
    }

    #[test]
    fn reap_skips_in_use_sessions_and_respects_idle_ms() {
        let mut sessions = vec![
            sess("busy", true, 0),
            sess("warm", false, 95),
            sess("cold", false, 10),
        ];
        let closed = reap(&mut sessions, 100, 20, 10);
        assert_eq!(closed, vec!["cold".to_string()], "only past idle_ms");
        assert_eq!(
            sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["busy", "warm"],
            "the in-use session survives a long idle and the warm one is not due"
        );
        // Exactly at the threshold counts as idle.
        assert_eq!(reap(&mut sessions, 115, 20, 10), vec!["warm".to_string()]);
    }

    #[test]
    fn reap_closes_the_oldest_first_and_honors_max_reap() {
        let mut sessions = vec![
            sess("m", false, 50),
            sess("o", false, 30),
            sess("n", false, 40),
            sess("busy", true, 0),
        ];
        let closed = reap(&mut sessions, 100, 10, 2);
        assert_eq!(
            closed,
            vec!["o".to_string(), "n".to_string()],
            "oldest first, at most max_reap"
        );
        assert_eq!(
            sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["m", "busy"],
            "only the closed ones were removed"
        );
    }

    #[test]
    fn reap_of_zero_closes_nothing() {
        let mut sessions = vec![sess("a", false, 0)];
        assert!(reap(&mut sessions, 1_000, 1, 0).is_empty());
        assert_eq!(sessions.len(), 1, "an explicit reap-nothing is a no-op");
        // And an empty pool reaps nothing without panicking.
        let mut empty: Vec<Session> = vec![];
        assert!(reap(&mut empty, 1_000, 1, 5).is_empty());
    }

    #[test]
    fn reap_counts_a_clock_behind_last_use_as_zero_idle_time() {
        let mut sessions = vec![sess("a", false, 500)];
        assert!(
            reap(&mut sessions, 100, 1, 4).is_empty(),
            "skew is never a negative age"
        );
        assert_eq!(reap(&mut sessions, 100, 0, 4), vec!["a".to_string()]);
    }

    // ── fixtures ────────────────────────────────────────────────────────

    #[test]
    fn fixture_parse_requires_the_oracle_and_names_every_missing_field() {
        let full = json!({
            "id": "t1", "start_url": "http://a/", "intent": "do it",
            "eval_script": "assert ok", "sites": ["a"]
        });
        let f = parse_fixture(&full).unwrap();
        assert_eq!(f.id, "t1");
        assert_eq!(f.sites, vec!["a".to_string()]);

        for field in ["id", "start_url", "intent", "eval_script"] {
            let mut v = full.clone();
            v.as_object_mut().unwrap().remove(field);
            let err = parse_fixture(&v).unwrap_err();
            assert!(err.contains(field), "names {field}: {err}");
        }
        let err = parse_fixture(&json!({
            "id": "t1", "start_url": "http://a/", "intent": "do it"
        }))
        .unwrap_err();
        assert!(
            err.contains("eval_script") && err.contains("oracle"),
            "a fixture with no oracle cannot regress: {err}"
        );
        let err = parse_fixture(&json!({
            "id": " ", "start_url": "http://a/", "intent": "i", "eval_script": "e"
        }))
        .unwrap_err();
        assert!(err.contains("`id` is empty"), "{err}");
        assert!(parse_fixture(&json!(["t1"]))
            .unwrap_err()
            .contains("JSON object"));
        assert!(parse_fixture(&json!({
            "id": "t1", "start_url": 3, "intent": "i", "eval_script": "e"
        }))
        .unwrap_err()
        .contains("`start_url` must be a string"));
    }

    #[test]
    fn fixture_parse_defaults_sites_and_ignores_unknown_fields() {
        let f = parse_fixture(&json!({
            "id": "t1", "start_url": "http://a/", "intent": "i",
            "eval_script": "e", "storage_state": "x", "geolocation": null
        }))
        .unwrap();
        assert!(f.sites.is_empty(), "sites defaults to empty");
        let null = parse_fixture(&json!({
            "id": "t1", "start_url": "http://a/", "intent": "i",
            "eval_script": "e", "sites": null
        }))
        .unwrap();
        assert!(null.sites.is_empty(), "a null sites list is an absent one");
        assert!(parse_fixture(&json!({
            "id": "t1", "start_url": "http://a/", "intent": "i",
            "eval_script": "e", "sites": "a"
        }))
        .unwrap_err()
        .contains("`sites` must be an array"));
        assert!(parse_fixture(&json!({
            "id": "t1", "start_url": "http://a/", "intent": "i",
            "eval_script": "e", "sites": ["a", 2]
        }))
        .unwrap_err()
        .contains("`sites[1]`"));
    }

    // ── regression selection ────────────────────────────────────────────

    #[test]
    fn regress_set_selects_by_site_and_by_eval_script_mention() {
        let fixtures = vec![
            TaskFixture {
                id: "a".into(),
                start_url: "http://a/".into(),
                intent: "i".into(),
                eval_script: "assert 1".into(),
                sites: vec!["shop".into()],
            },
            TaskFixture {
                id: "b".into(),
                start_url: "http://b/".into(),
                intent: "i".into(),
                eval_script: "page.goto('http://forum.example/x')".into(),
                sites: vec![],
            },
            TaskFixture {
                id: "a".into(),
                start_url: "http://a/".into(),
                intent: "i".into(),
                eval_script: "assert 1".into(),
                sites: vec!["shop".into()],
            },
            TaskFixture {
                id: "c".into(),
                start_url: "http://c/".into(),
                intent: "i".into(),
                eval_script: "assert 1".into(),
                sites: vec!["mail".into()],
            },
        ];
        // Intersection, case-insensitively, in fixture order, deduplicated.
        assert_eq!(
            regress_set(&fixtures, &["SHOP".to_string()]),
            vec!["a".to_string()],
            "declared site matches case-insensitively and the duplicate id runs once"
        );
        // A mention in the eval script selects too, even with no `sites`.
        assert_eq!(
            regress_set(&fixtures, &["forum".to_string()]),
            vec!["b".to_string()]
        );
        // Both rules together, still in fixture order.
        assert_eq!(
            regress_set(&fixtures, &["forum".to_string(), "shop".to_string()]),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(regress_set(&fixtures, &["unrelated".to_string()]).is_empty());
    }

    #[test]
    fn regress_set_with_no_change_selects_nothing() {
        let fixtures = vec![TaskFixture {
            id: "a".into(),
            start_url: "http://a/".into(),
            intent: "i".into(),
            eval_script: "assert 1".into(),
            sites: vec!["shop".into()],
        }];
        assert!(
            regress_set(&fixtures, &[]).is_empty(),
            "no change regresses nothing — not everything"
        );
        assert!(
            regress_set(&fixtures, &["".to_string(), "  ".to_string()]).is_empty(),
            "entries that trim to empty name no site"
        );
        assert!(regress_set(&[], &["shop".to_string()]).is_empty());
    }

    // ── validate flag ───────────────────────────────────────────────────

    #[test]
    fn validate_flag_parses_both_spellings_and_looks_past_other_args() {
        let v = parse_validate_flag(&["--validate", "url,text", "--max-retries", "2"]).unwrap();
        assert!(v.enabled);
        assert_eq!(v.checks, vec![Check::Url, Check::Text]);
        assert_eq!(v.max_retries, 2);
        let eq = parse_validate_flag(&["--validate=element", "http://task/"]).unwrap();
        assert_eq!(eq.checks, vec![Check::Element]);
        assert_eq!(eq.max_retries, DEFAULT_MAX_RETRIES);
        let eq = parse_validate_flag(&["--validate=Text,text,URL"]).unwrap();
        assert_eq!(
            eq.checks,
            vec![Check::Text, Check::Url],
            "case-insensitive, trimmed, deduplicated first-wins"
        );
        // No flag at all: validation off, and that is not an error.
        let off = parse_validate_flag(&["http://task/", "--headless"]).unwrap();
        assert!(!off.enabled);
        assert!(off.checks.is_empty());
        assert_eq!(off.max_retries, DEFAULT_MAX_RETRIES);
        // The last occurrence wins, as a CLI does.
        let last =
            parse_validate_flag(&["--validate=url", "--validate=text", "--max-retries=0"]).unwrap();
        assert_eq!(last.checks, vec![Check::Text]);
        assert_eq!(last.max_retries, 0);
        // An inert retry budget beside a disabled flag is legal.
        let inert = parse_validate_flag(&["--max-retries", "3"]).unwrap();
        assert!(!inert.enabled && inert.max_retries == 3);
    }

    #[test]
    fn validate_flag_rejects_unknown_checks_and_bad_retry_budgets() {
        let err = parse_validate_flag(&["--validate", "url,teleport"]).unwrap_err();
        assert!(err.contains("teleport"), "{err}");
        assert!(
            err.contains("url|text|element|none"),
            "lists the vocabulary: {err}"
        );
        let err = parse_validate_flag(&["--validate=teleport,smoke"]).unwrap_err();
        assert!(
            err.contains("teleport, smoke"),
            "reports every unknown token at once: {err}"
        );
        for (arg, want) in [
            (
                &["--validate", "url", "--max-retries", "6"][..],
                "exceeds the cap of 5",
            ),
            (&["--max-retries=99"][..], "exceeds the cap of 5"),
            (&["--max-retries", "two"][..], "not a whole number"),
            (&["--max-retries", "-1"][..], "not a whole number"),
        ] {
            let err = parse_validate_flag(arg).unwrap_err();
            assert!(err.contains(want), "{arg:?} → {err}");
        }
        assert!(parse_validate_flag(&["--validate"])
            .unwrap_err()
            .contains("needs a value"));
        assert!(parse_validate_flag(&["--max-retries"])
            .unwrap_err()
            .contains("needs a value"));
        // The bounds themselves are accepted.
        assert!(parse_validate_flag(&["--validate=url", "--max-retries=0"]).is_ok());
        assert!(parse_validate_flag(&["--validate=url", "--max-retries=5"]).is_ok());
    }

    #[test]
    fn validate_flag_refuses_a_no_op_or_contradictory_configuration() {
        // `none`-only and empty check lists: nothing to check.
        for arg in [
            "--validate=none",
            "--validate=",
            "--validate=,",
            "--validate=none,none",
        ] {
            let err = parse_validate_flag(&[arg]).unwrap_err();
            assert!(err.contains("nothing to check"), "{arg} → {err}");
        }
        // `none` beside a real check is a contradiction, not a silent drop.
        let err = parse_validate_flag(&["--validate=none,url"]).unwrap_err();
        assert!(err.contains("`none` names no check"), "{err}");
        // The same rules hold for a hand-built Validation.
        assert!(validate_flag(&Validation {
            enabled: false,
            checks: vec![Check::Url],
            max_retries: 1,
        })
        .unwrap_err()
        .contains("contradiction"));
        assert!(validate_flag(&Validation {
            enabled: true,
            checks: vec![],
            max_retries: 1,
        })
        .unwrap_err()
        .contains("no-op"));
        assert!(validate_flag(&Validation {
            enabled: true,
            checks: vec![Check::None],
            max_retries: 1,
        })
        .unwrap_err()
        .contains("nothing to check"));
        assert!(validate_flag(&Validation {
            enabled: true,
            checks: vec![Check::Element],
            max_retries: MAX_RETRIES + 1,
        })
        .unwrap_err()
        .contains("exceeds the cap"));
        let err = validate_flag(&Validation {
            enabled: false,
            checks: vec![Check::Url, Check::Text],
            max_retries: 2,
        })
        .unwrap_err();
        assert!(err.contains("2 check(s) [url,text]"), "{err}");
        // The honest states pass.
        assert!(validate_flag(&Validation {
            enabled: true,
            checks: vec![Check::Url],
            max_retries: 0,
        })
        .is_ok());
        assert!(validate_flag(&Validation {
            enabled: false,
            checks: vec![],
            max_retries: 5,
        })
        .is_ok());
    }

    #[test]
    fn retry_allowed_needs_an_enabled_well_formed_flag_and_budget() {
        let on = |max| Validation {
            enabled: true,
            checks: vec![Check::Url],
            max_retries: max,
        };
        // max_retries is the whole attempt budget.
        assert!(retry_allowed(&on(2), 0));
        assert!(retry_allowed(&on(2), 1));
        assert!(!retry_allowed(&on(2), 2), "budget spent");
        assert!(!retry_allowed(&on(0), 0), "no retry budget at all");
        assert!(retry_allowed(&on(5), 4));
        assert!(!retry_allowed(&on(5), 5));
        // Disabled: never.
        assert!(!retry_allowed(
            &Validation {
                enabled: false,
                checks: vec![],
                max_retries: 5,
            },
            0
        ));
        // Malformed flags never buy a retry, either direction of the
        // contradiction, and a `none`-only check list is malformed.
        for bad in [
            Validation {
                enabled: false,
                checks: vec![Check::Url],
                max_retries: 5,
            },
            Validation {
                enabled: true,
                checks: vec![],
                max_retries: 5,
            },
            Validation {
                enabled: true,
                checks: vec![Check::None],
                max_retries: 5,
            },
            Validation {
                enabled: true,
                checks: vec![Check::Url],
                max_retries: MAX_RETRIES + 1,
            },
        ] {
            assert!(!retry_allowed(&bad, 0), "{bad:?} must not retry");
        }
    }
}
