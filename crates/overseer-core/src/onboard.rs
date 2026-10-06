//! Onboarding & persona files (P6-5, playbook Ch.11 §5.8).
//!
//! Four persona files — `identity.md`, `relationships.md`, `preferences.md`,
//! `SOUL.md` — are drafted from a guided interview. A draft is inert by
//! construction, and the gate has TWO halves (R2-F8):
//!
//! 1. **Prompt half** (`prompt::assemble`): the persona segment carries the
//!    approved bodies only; while anything is still a draft the segment is a
//!    one-line pending notice. No draft text ever enters context.
//! 2. **File half** (`perm::Policy::draft_deny`): an unapproved persona dir
//!    is closed to every file tool — *including the read tools*, which the
//!    read early-allow in `check()` would otherwise wave through. Hiding a
//!    draft from the prompt is not enough if `read` can pull it into context.
//!
//! The interview writer therefore runs engine-side on the filesystem: it is
//! the only writer of a draft, and it does not need the tool gate.
//!
//! Provenance: every insight carries a `<!-- source: answer-N -->` trailer
//! naming the interview answer it came from, and `verify_trace` refuses a
//! persona whose insights don't resolve — an orphaned claim is a
//! hallucination with a paper trail.

use std::path::{Path, PathBuf};

/// The four persona files, in load order. `SOUL.md` is the line that must
/// survive every compaction.
pub const PERSONA_FILES: [&str; 4] = [
    "identity.md",
    "relationships.md",
    "preferences.md",
    "SOUL.md",
];

/// Soft cap on the assembled persona segment (static, cacheable bytes).
pub const PERSONA_CAP: usize = 8_000;

/// Transcript label prefix for answers: `answer-<n>`, 1-based.
pub const ANSWER_PREFIX: &str = "answer-";

/// Tag on the interview's question nudges — provably harness-authored.
pub const ONBOARD_TAG: &str = "onboarding";

/// The onboarding interview: five questions, asked in order. Answer `n`
/// (1-based) is what a `<!-- source: answer-n -->` trailer names.
pub const QUESTIONS: [&str; 5] = [
    "Who are you, and what should I know about how you work? (role, expertise, current focus)",
    "Which people and teams do you work with, and what should I know about each?",
    "How do you like to work — tone, detail level, when to ask instead of act?",
    "What are your hard preferences and anti-preferences (tools, formats, things to never do)?",
    "What belongs in SOUL.md — the one paragraph that should survive every compaction?",
];

/// The transcript label for answer `n` (1-based).
pub fn answer_id(n: usize) -> String {
    format!("{ANSWER_PREFIX}{n}")
}

/// Approval status of one persona file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    /// Written but unapproved: invisible to the prompt, closed to tools.
    #[default]
    Draft,
    /// User-approved: readable and rendered in the prompt.
    Approved,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Draft => "draft",
            Status::Approved => "approved",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "draft" => Ok(Status::Draft),
            "approved" => Ok(Status::Approved),
            other => Err(format!(
                "onboard: bad status `{other}` — want draft|approved"
            )),
        }
    }
}

/// Persona frontmatter: approval status + how many interview answers the
/// file was drafted from (its provenance claim, checked by `verify_trace`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PersonaMeta {
    pub status: Status,
    /// Answers this file claims to be sourced from.
    pub source_answers: usize,
}

/// Parse `---` frontmatter. A file with no frontmatter reads as the
/// fail-closed default (draft, 0 answers) — an unmarked persona is a draft.
/// Malformed frontmatter is an error naming the fault.
pub fn parse_frontmatter(text: &str) -> Result<(PersonaMeta, String), String> {
    let all: Vec<&str> = text.lines().collect();
    if all.first().map(|l| l.trim()) != Some("---") {
        return Ok((PersonaMeta::default(), text.to_string()));
    }
    let mut close = None;
    for (i, l) in all.iter().enumerate().skip(1) {
        if l.trim() == "---" {
            close = Some(i);
            break;
        }
    }
    let Some(end) = close else {
        return Err("onboard: unterminated frontmatter — missing closing `---`".into());
    };
    let mut meta = PersonaMeta::default();
    for line in &all[1..end] {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| format!("onboard: bad frontmatter line `{line}` — want `key: value`"))?;
        let v = v.trim();
        match k.trim() {
            "status" => meta.status = Status::parse(v)?,
            "source_answers" => {
                meta.source_answers = v
                    .parse()
                    .map_err(|_| format!("onboard: bad source_answers `{v}` — want a number"))?
            }
            // Unknown keys are ignored (forward-compatible headers).
            _ => {}
        }
    }
    let mut body = all[end + 1..].join("\n");
    if text.ends_with('\n') {
        body.push('\n');
    }
    Ok((meta, body))
}

/// Render frontmatter + body. `parse_frontmatter(render(m, b))` round-trips.
pub fn render(meta: &PersonaMeta, body: &str) -> String {
    format!(
        "---\nstatus: {}\nsource_answers: {}\n---\n\n{}\n",
        meta.status.as_str(),
        meta.source_answers,
        body.trim_matches('\n')
    )
}

/// Title-cased heading for a persona file.
fn heading(file: &str) -> &'static str {
    match file {
        "identity.md" => "Identity",
        "relationships.md" => "Relationships",
        "preferences.md" => "Preferences",
        _ => "SOUL",
    }
}

/// Seed body for a new persona file: a heading plus the trailer contract.
fn seed_body(file: &str) -> String {
    format!(
        "# {}\n\n<!-- Filled in by `overseer onboard`. Every insight line carries a \
         `<!-- source: answer-N -->` trailer naming the interview answer it came from. -->",
        heading(file)
    )
}

/// Create the persona dir and seed any missing file as an empty draft.
/// Returns the dir path.
pub fn ensure_persona_dir(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for file in PERSONA_FILES {
        let path = dir.join(file);
        if !path.exists() {
            // temp + rename: a dangling symlink is replaced, never
            // written through.
            crate::memory::store_write(
                dir,
                file,
                render(&PersonaMeta::default(), &seed_body(file)).as_bytes(),
            )
            .map_err(std::io::Error::other)?;
        }
    }
    Ok(dir.to_path_buf())
}

/// Per-file metadata, in `PERSONA_FILES` order. A missing or unreadable file
/// reads as the fail-closed default.
pub fn statuses(dir: &Path) -> Vec<(&'static str, PersonaMeta)> {
    PERSONA_FILES
        .iter()
        .map(|file| {
            let meta = crate::tools::read_no_follow(&dir.join(file))
                .ok()
                .and_then(|t| parse_frontmatter(&t).ok().map(|(m, _)| m))
                .unwrap_or_default();
            (*file, meta)
        })
        .collect()
}

/// True when every persona file is present and approved — the prompt gate.
pub fn all_approved(dir: &Path) -> bool {
    statuses(dir)
        .iter()
        .all(|(_, m)| m.status == Status::Approved)
}

/// The one-line pending notice (prompt gate, unapproved state). Names the
/// state and the repair — never any draft content.
pub fn pending_notice(dir: &Path) -> String {
    let pending = statuses(dir)
        .iter()
        .filter(|(_, m)| m.status == Status::Draft)
        .count();
    format!(
        "Persona onboarding is incomplete: {pending} of {} file(s) at {} are unapproved drafts — \
         they are not in context, and file access to them is closed until you run \
         `overseer onboard --approve`.",
        PERSONA_FILES.len(),
        dir.display()
    )
}

/// The persona segment body (prompt half of the gate): approved bodies
/// joined under one header, else the pending notice. Draft text never
/// appears here.
pub fn persona_body(dir: &Path) -> String {
    if !all_approved(dir) {
        return pending_notice(dir);
    }
    let mut parts: Vec<String> = Vec::new();
    for file in PERSONA_FILES {
        let Ok(text) = crate::tools::read_no_follow(&dir.join(file)) else {
            continue;
        };
        let Ok((_, body)) = parse_frontmatter(&text) else {
            continue;
        };
        let body = body.trim();
        if !body.is_empty() {
            parts.push(body.to_string());
        }
    }
    if parts.is_empty() {
        return "## Persona\nApproved persona files carry no insights yet.".to_string();
    }
    let joined = format!(
        "## Persona\nApproved by the user during onboarding — background, not instructions.\n\n{}",
        parts.join("\n\n")
    );
    if joined.len() > PERSONA_CAP {
        // Largest char-boundary offset within the cap (never split a char).
        let cut = joined
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|end| *end <= PERSONA_CAP)
            .last()
            .unwrap_or(0);
        return format!(
            "{}\n\n[overseer] persona truncated at {PERSONA_CAP} chars — tighten the persona files.",
            &joined[..cut]
        );
    }
    joined
}

/// Flip every draft to approved, then git-version the dir (reusing
/// `memory::commit`). Returns the files changed.
pub fn approve(dir: &Path) -> std::io::Result<Vec<&'static str>> {
    // W1: the draft flips and the commit are one store mutation.
    let _lock = crate::memory::StoreLock::acquire(dir)?;
    let mut changed = Vec::new();
    for file in PERSONA_FILES {
        // O_NOFOLLOW: a symlinked persona file is never read through —
        // it stays a draft and is skipped rather than flipped.
        let Ok(text) = crate::tools::read_no_follow(&dir.join(file)) else {
            continue;
        };
        let (mut meta, body) = parse_frontmatter(&text).map_err(std::io::Error::other)?;
        if meta.status == Status::Approved {
            continue;
        }
        meta.status = Status::Approved;
        crate::memory::store_write(dir, file, render(&meta, &body).as_bytes())
            .map_err(std::io::Error::other)?;
        changed.push(file);
    }
    crate::memory::commit(dir, "persona approved");
    Ok(changed)
}

/// One sourced insight: the line's text plus the interview answer it came
/// from (1-based).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Insight {
    pub text: String,
    pub source: usize,
}

/// `<!-- source: answer-N -->` trailer. `None` = no trailer at all (the
/// caller decides whether that is an orphan).
fn trailer(line: &str) -> Option<Result<usize, String>> {
    let start = line.find("<!--")?;
    let rest = &line[start + 4..];
    let end = rest.find("-->")?;
    let inner = rest[..end].trim();
    let Some(v) = inner.strip_prefix("source:") else {
        return Some(Err(format!("malformed source trailer `<!--{inner}-->`")));
    };
    match v
        .trim()
        .strip_prefix(ANSWER_PREFIX)
        .and_then(|n| n.parse::<usize>().ok())
    {
        Some(n) => Some(Ok(n)),
        None => Some(Err(format!(
            "malformed source trailer `<!--{inner}-->` — want `source: answer-N`"
        ))),
    }
}

/// Split an insight line into (text, answer index), requiring a well-formed
/// trailer. Used by the draft parser; `verify_trace` handles the orphans.
fn split_insight(line: &str) -> Result<Insight, String> {
    let item = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .unwrap_or(line)
        .trim();
    match trailer(item) {
        None => Err("insight has no `<!-- source: answer-N -->` trailer".into()),
        Some(Err(e)) => Err(e),
        Some(Ok(n)) => {
            let cut = item.find("<!--").unwrap_or(item.len());
            let text = item[..cut].trim().to_string();
            if text.is_empty() {
                return Err("insight has no text before its source trailer".into());
            }
            Ok(Insight { text, source: n })
        }
    }
}

/// Render one insight as a bullet line with its trailer.
fn insight_line(i: &Insight) -> String {
    format!(
        "- {} <!-- source: {}{} -->",
        i.text, ANSWER_PREFIX, i.source
    )
}

/// Parse an aux-tier draft reply: `== <file> ==` sections, one bullet
/// insight per line, each carrying a source trailer. Unknown section names
/// and trailer-less bullets are errors — the writer must not guess.
pub fn parse_draft_response(text: &str) -> Result<Vec<(String, Vec<Insight>)>, String> {
    let mut out: Vec<(String, Vec<Insight>)> = Vec::new();
    let mut cur: Option<usize> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("==") {
            let name = rest.trim_end_matches('=').trim();
            if !PERSONA_FILES.contains(&name) {
                return Err(format!(
                    "onboard: draft reply names unknown persona file `{name}`"
                ));
            }
            out.push((name.to_string(), Vec::new()));
            cur = Some(out.len() - 1);
            continue;
        }
        let Some(idx) = cur else {
            continue; // preamble prose is ignored
        };
        if !(line.starts_with("- ") || line.starts_with("* ")) {
            continue; // prose inside a section is ignored
        }
        let insight = split_insight(line)?;
        out[idx].1.push(insight);
    }
    if out.is_empty() {
        return Err("onboard: draft reply had no `== <file> ==` sections".into());
    }
    Ok(out)
}

/// Engine-side draft writer: writes each file's insights (with trailers)
/// under draft frontmatter claiming `source_answers`. Returns insights
/// written. This is the one writer of a draft — the model has no tool path
/// here, by design.
pub fn write_drafts(
    dir: &Path,
    drafts: &[(String, Vec<Insight>)],
    source_answers: usize,
) -> std::io::Result<usize> {
    // W1: the draft writes and the commit are one store mutation.
    let _lock = crate::memory::StoreLock::acquire(dir)?;
    let mut wrote = 0usize;
    for (file, insights) in drafts {
        if !PERSONA_FILES.contains(&file.as_str()) {
            return Err(std::io::Error::other(format!(
                "onboard: unknown persona file `{file}`"
            )));
        }
        let mut body = format!("# {}\n", heading(file));
        for i in insights {
            body.push('\n');
            body.push_str(&insight_line(i));
        }
        let meta = PersonaMeta {
            status: Status::Draft,
            source_answers,
        };
        // temp + rename over the target: a symlinked persona file is
        // replaced by a regular file, never written through.
        crate::memory::store_write(dir, file, render(&meta, &body).as_bytes())
            .map_err(std::io::Error::other)?;
        wrote += insights.len();
    }
    crate::memory::commit(dir, "onboard draft");
    Ok(wrote)
}

/// Verify every insight trailer in the persona files resolves (P6-5):
/// each bullet must carry `<!-- source: answer-N -->` with `1 <= N <= answers`
/// (and no file may claim more answers than were recorded). Orphans come
/// back as `<file>:<line> — <reason>`; `Ok(n)` is the count of resolved
/// trailers.
pub fn verify_trace(dir: &Path, answers: usize) -> Result<usize, Vec<String>> {
    let mut orphans: Vec<String> = Vec::new();
    let mut traced = 0usize;
    for file in PERSONA_FILES {
        let Ok(text) = crate::tools::read_no_follow(&dir.join(file)) else {
            continue; // a missing or symlinked file has nothing to trace
        };
        let (meta, body) = match parse_frontmatter(&text) {
            Ok(v) => v,
            Err(e) => {
                orphans.push(format!("{file}:1 — {e}"));
                continue;
            }
        };
        if meta.source_answers > answers {
            orphans.push(format!(
                "{file}:2 — frontmatter claims {} answer(s) but only {answers} were recorded",
                meta.source_answers
            ));
        }
        // Body line numbers are file line numbers (frontmatter included).
        let offset = text.lines().count() - body.lines().count();
        for (i, raw) in body.lines().enumerate() {
            let line = raw.trim();
            if !(line.starts_with("- ") || line.starts_with("* ")) {
                continue;
            }
            let n = offset + i + 1;
            match trailer(line) {
                None => orphans.push(format!(
                    "{file}:{n} — insight has no `<!-- source: answer-N -->` trailer"
                )),
                Some(Err(e)) => orphans.push(format!("{file}:{n} — {e}")),
                Some(Ok(k)) => {
                    if k == 0 || k > answers {
                        orphans.push(format!(
                            "{file}:{n} — source answer-{k} out of range ({answers} answer(s) recorded)"
                        ));
                    } else {
                        traced += 1;
                    }
                }
            }
        }
    }
    if orphans.is_empty() {
        Ok(traced)
    } else {
        Err(orphans)
    }
}

/// What one interview produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interview {
    /// Answer texts in question order.
    pub answers: Vec<String>,
    /// The human stopped early (quit or blank answer).
    pub aborted: bool,
}

/// Drive the onboarding interview over a live agent (P6-5): the question is
/// recorded as a harness Nudge (provably not user-typed) and the answer as a
/// `UserInput` labelled `answer-<n>` — so the transcript, not the persona
/// files, is where the answers become durable first. `ask` is the
/// frontend's prompt; returning None quits. Every question must be answered
/// (blank counts as quitting) so `answer-N` numbering stays dense.
pub fn run_interview(
    agent: &mut crate::agent::Agent,
    ask: &mut dyn FnMut(&str) -> Option<String>,
    on_event: &mut dyn FnMut(&crate::event::Event),
) -> std::io::Result<Interview> {
    let mut answers = Vec::new();
    let mut aborted = false;
    for (i, q) in QUESTIONS.iter().enumerate() {
        agent.record_nudge(&format!("[{ONBOARD_TAG}] {q}"), on_event)?;
        let Some(a) = ask(q) else {
            aborted = true;
            break;
        };
        let a = a.trim();
        if a.is_empty() {
            aborted = true;
            break;
        }
        agent.record_user_input(&format!("{}: {a}", answer_id(i + 1)), on_event)?;
        answers.push(a.to_string());
    }
    Ok(Interview { answers, aborted })
}

/// Draft the persona files from interview answers (P6-5): an aux-tier call
/// turns the answers into sourced insight lines, the ENGINE writes them
/// (the draft dir is tool-denied until approved), and the result is
/// trace-verified before it is reported. Returns a summary line.
pub fn draft_from_answers(
    provider: &dyn crate::provider::Provider,
    model: &str,
    dir: &Path,
    answers: &[String],
) -> Result<String, String> {
    if answers.is_empty() {
        return Err("onboard: no interview answers to draft from".into());
    }
    ensure_persona_dir(dir).map_err(|e| e.to_string())?;
    let mut listed = String::new();
    for (i, a) in answers.iter().enumerate() {
        listed.push_str(&format!("{}: {a}\n", answer_id(i + 1)));
    }
    let prompt = format!(
        "You are drafting an agent's persona files from an onboarding interview.\n\
         The recorded answers are:\n{listed}\n\
         Write only durable facts the user actually stated, grouped into sections:\n\
         == identity.md == role, expertise, current focus\n\
         == relationships.md == people/teams and what to know about each\n\
         == preferences.md == working style, tone, format preferences\n\
         == SOUL.md == the one paragraph that must survive compaction\n\
         Each insight is one bullet line of the form\n\
         `- <insight> <!-- source: answer-N -->` where N is the answer it came \
         from. Never invent a source: every bullet must cite an answer that \
         exists (1..={}). Omit a section entirely when the interview gave you \
         nothing for it. Reply with the sections only — no preamble.",
        answers.len()
    );
    let msgs = [crate::ir::Message::user_text(prompt)];
    let req = crate::provider::Request {
        model,
        system: &[],
        tools: &[],
        messages: &msgs,
        max_tokens: 4_096,
        thinking_budget: None,
        effort: Some(crate::provider::Effort::Min),
        cache_breakpoints: false,
        cache_key: None,
    };
    let resp = provider
        .complete(&req)
        .map_err(|e| format!("onboard: {e}"))?;
    let text: String = resp
        .blocks
        .iter()
        .filter_map(|b| match b {
            crate::ir::Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let drafts = parse_draft_response(&text)?;
    let wrote = write_drafts(dir, &drafts, answers.len()).map_err(|e| e.to_string())?;
    let traced = verify_trace(dir, answers.len()).map_err(|orphans| {
        format!(
            "onboard: draft written but {} insight(s) do not trace to an answer:\n{}",
            orphans.len(),
            orphans.join("\n")
        )
    })?;
    Ok(format!(
        "onboard: {wrote} insight(s) drafted into {} file(s), {traced} traced to answers — \
         review, then `overseer onboard --approve`",
        drafts.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-onboard-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ensures_four_draft_files() {
        let dir = tmpdir().join("persona");
        let p = ensure_persona_dir(&dir).unwrap();
        assert_eq!(p, dir);
        for f in PERSONA_FILES {
            assert!(dir.join(f).is_file(), "{f} missing");
        }
        assert_eq!(statuses(&dir).len(), 4);
        assert!(!all_approved(&dir));
        // Idempotent: an existing file is never overwritten.
        std::fs::write(
            dir.join("identity.md"),
            "---\nstatus: approved\nsource_answers: 1\n---\nkept\n",
        )
        .unwrap();
        ensure_persona_dir(&dir).unwrap();
        assert!(std::fs::read_to_string(dir.join("identity.md"))
            .unwrap()
            .contains("kept"));
    }

    #[test]
    fn frontmatter_round_trips_and_fails_closed() {
        let meta = PersonaMeta {
            status: Status::Approved,
            source_answers: 3,
        };
        let text = render(&meta, "# Identity\n\n- a <!-- source: answer-1 -->\n");
        let (back, body) = parse_frontmatter(&text).unwrap();
        assert_eq!(back, meta);
        assert!(body.contains("source: answer-1"));
        // No frontmatter → draft default (an unmarked persona is a draft).
        let (m, body) = parse_frontmatter("# Identity\n").unwrap();
        assert_eq!(m, PersonaMeta::default());
        assert_eq!(m.status, Status::Draft);
        assert_eq!(body, "# Identity\n");
        // Malformed → error naming the fault.
        assert!(parse_frontmatter("---\nstatus: maybe\n---\nx\n").is_err());
        assert!(parse_frontmatter("---\nstatus: draft\n").is_err());
        assert!(parse_frontmatter("---\nnonsense\n---\n").is_err());
        assert!(parse_frontmatter("---\nsource_answers: many\n---\n").is_err());
        assert_eq!(Status::parse("Approved").unwrap(), Status::Approved);
    }

    /// Draft-invisible (prompt half of the gate): while any file is a draft
    /// the segment is a one-line notice and carries no draft text.
    #[test]
    fn draft_is_invisible_in_the_persona_segment() {
        let dir = tmpdir().join("persona");
        ensure_persona_dir(&dir).unwrap();
        write_drafts(
            &dir,
            &[(
                "identity.md".to_string(),
                vec![Insight {
                    text: "DRAFT_SECRET_INSIGHT".to_string(),
                    source: 1,
                }],
            )],
            1,
        )
        .unwrap();
        let body = persona_body(&dir);
        assert!(!body.contains("DRAFT_SECRET_INSIGHT"), "{body}");
        assert_eq!(body.lines().count(), 1, "the pending notice is one line");
        assert!(body.contains("unapproved drafts"), "{body}");
        assert!(body.contains("persona"), "{body}");
    }

    #[test]
    fn approval_reveals_the_persona() {
        let dir = tmpdir().join("persona");
        ensure_persona_dir(&dir).unwrap();
        write_drafts(
            &dir,
            &[
                (
                    "identity.md".to_string(),
                    vec![Insight {
                        text: "Rust systems engineer".to_string(),
                        source: 1,
                    }],
                ),
                (
                    "SOUL.md".to_string(),
                    vec![Insight {
                        text: "Prefer boring, verified work".to_string(),
                        source: 2,
                    }],
                ),
            ],
            2,
        )
        .unwrap();
        assert!(!all_approved(&dir));
        assert!(persona_body(&dir).contains("unapproved drafts"));

        let changed = approve(&dir).unwrap();
        assert_eq!(changed.len(), 4, "every draft flips, seeded files included");
        assert!(statuses(&dir)
            .iter()
            .all(|(_, m)| m.status == Status::Approved));
        let body = persona_body(&dir);
        assert!(body.contains("Rust systems engineer"), "{body}");
        assert!(body.contains("Prefer boring, verified work"), "{body}");
        assert!(body.contains("## Persona"));
        // Approving again is a no-op.
        assert!(approve(&dir).unwrap().is_empty());
    }

    #[test]
    fn trace_verifies_or_reports_orphans_with_file_line() {
        let dir = tmpdir().join("persona");
        ensure_persona_dir(&dir).unwrap();
        write_drafts(
            &dir,
            &[(
                "identity.md".to_string(),
                vec![
                    Insight {
                        text: "sourced".to_string(),
                        source: 1,
                    },
                    Insight {
                        text: "also sourced".to_string(),
                        source: 2,
                    },
                ],
            )],
            2,
        )
        .unwrap();
        assert_eq!(verify_trace(&dir, 2).unwrap(), 2);

        // An insight with no trailer is an orphan.
        let path = dir.join("identity.md");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}- unsourced claim\n")).unwrap();
        let orphans = verify_trace(&dir, 2).unwrap_err();
        assert_eq!(orphans.len(), 1, "{orphans:?}");
        assert!(orphans[0].starts_with("identity.md:"), "{orphans:?}");
        assert!(orphans[0].contains("no `<!-- source: answer-N -->` trailer"));
        let line: usize = orphans[0]
            .trim_start_matches("identity.md:")
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        // The reported line really is the orphan's line.
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .nth(line - 1),
            Some("- unsourced claim")
        );

        // A trailer naming an answer that does not exist is an orphan too.
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("- unsourced claim\n", "");
        std::fs::write(
            &path,
            format!("{text}- wild claim <!-- source: answer-9 -->\n"),
        )
        .unwrap();
        let orphans = verify_trace(&dir, 2).unwrap_err();
        assert_eq!(orphans.len(), 1, "{orphans:?}");
        assert!(orphans[0].contains("out of range"), "{orphans:?}");

        // Malformed trailer is caught, including the wrong shape.
        std::fs::write(&path, format!("{text}- x <!-- answer-1 -->\n")).unwrap();
        assert!(verify_trace(&dir, 2).unwrap_err()[0].contains("malformed"));

        // A file claiming more answers than were recorded is an orphan.
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            text.replace("source_answers: 2", "source_answers: 5"),
        )
        .unwrap();
        let orphans = verify_trace(&dir, 2).unwrap_err();
        assert!(orphans[0].contains("only 2 were recorded"), "{orphans:?}");

        // Clean again after repair (frontmatter included).
        std::fs::write(
            &path,
            render(
                &PersonaMeta {
                    status: Status::Draft,
                    source_answers: 1,
                },
                "# Identity\n\n- ok <!-- source: answer-1 -->\n",
            ),
        )
        .unwrap();
        assert_eq!(verify_trace(&dir, 1).unwrap(), 1);
    }

    #[test]
    fn parse_draft_response_requires_sources_and_known_files() {
        let text = "preamble\n== identity.md ==\n- Rust engineer <!-- source: answer-1 -->\n\
                    - interviewer <!-- source: answer-2 -->\n== SOUL.md ==\n\
                    - boring wins <!-- source: answer-3 -->\n";
        let drafts = parse_draft_response(text).unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].0, "identity.md");
        assert_eq!(drafts[0].1.len(), 2);
        assert_eq!(drafts[0].1[0].source, 1);
        assert_eq!(drafts[0].1[0].text, "Rust engineer");
        assert_eq!(drafts[1].0, "SOUL.md");
        // Unsourced bullet, unknown file, and empty reply all fail loudly.
        assert!(parse_draft_response("== identity.md ==\n- no source\n").is_err());
        assert!(parse_draft_response("== secrets.md ==\n").is_err());
        assert!(parse_draft_response("no sections here").is_err());
    }

    #[test]
    fn write_drafts_rejects_a_foreign_file_name() {
        let dir = tmpdir().join("persona");
        ensure_persona_dir(&dir).unwrap();
        let err = write_drafts(&dir, &[("evil.md".to_string(), vec![])], 1).unwrap_err();
        assert!(err.to_string().contains("unknown persona file"), "{err}");
    }

    /// The interview records answers through the agent's event log, so no
    /// model call is needed to prove durability.
    struct Stub;

    impl crate::provider::Provider for Stub {
        fn complete(
            &self,
            _: &crate::provider::Request,
        ) -> Result<crate::provider::Response, crate::provider::ProviderError> {
            unreachable!("the interview records answers without a model call")
        }
        fn name(&self) -> &'static str {
            "stub"
        }
    }

    fn interview_agent(dir: &Path) -> (crate::agent::Agent, PathBuf, PathBuf) {
        let persona = dir.join("persona");
        let session = dir.join("session");
        ensure_persona_dir(&persona).unwrap();
        let cfg = crate::agent::AgentConfig {
            cwd: dir.to_path_buf(),
            persona_dir: Some(persona.clone()),
            ..crate::agent::AgentConfig::default()
        };
        let agent =
            crate::agent::Agent::start(std::sync::Arc::new(Stub), cfg, session.clone(), "s".into())
                .unwrap();
        (agent, persona, session)
    }

    fn canned(answers: &[&str]) -> std::collections::VecDeque<String> {
        answers.iter().map(|s| s.to_string()).collect()
    }

    /// Interview durability (P6-5): the answers are on disk, labelled
    /// `answer-N`, before any persona file is written — and they rehydrate
    /// as conversation on resume.
    #[test]
    fn interview_answers_are_durable_before_any_draft() {
        let dir = tmpdir();
        let (mut agent, persona, session) = interview_agent(&dir);
        let mut qs: Vec<String> = Vec::new();
        let mut queue = canned(&[
            "systems engineer",
            "works with Ada",
            "terse answers",
            "no emoji",
            "verify everything",
        ]);
        let mut ask = |q: &str| {
            qs.push(q.to_string());
            queue.pop_front()
        };
        let mut sink = |_: &crate::event::Event| {};

        let iv = run_interview(&mut agent, &mut ask, &mut sink).unwrap();
        assert_eq!(iv.answers.len(), QUESTIONS.len());
        assert!(!iv.aborted);
        assert_eq!(qs, QUESTIONS.to_vec());

        // On disk: one `answer-N` user input per question, and the question
        // text itself recorded as a harness nudge (not user authorship).
        let raw = std::fs::read_to_string(session.join("events.jsonl")).unwrap();
        for (i, a) in iv.answers.iter().enumerate() {
            assert!(
                raw.contains(&format!("\"answer-{}: {a}\"", i + 1)),
                "answer-{} not durable: {raw}",
                i + 1
            );
        }
        assert!(raw.contains("\"type\":\"nudge\""));
        assert!(raw.contains(&QUESTIONS[0].replace('"', "")));
        // No persona file has been touched by the interview.
        assert!(!persona_body(&persona).contains("systems engineer"));

        // The answers are part of the rehydrated view (resume sees them).
        let events = crate::event::EventLog::replay(session.join("events.jsonl")).unwrap();
        let view = format!("{:?}", crate::event::rehydrate_messages(&events));
        assert!(view.contains("answer-1: systems engineer"), "{view}");
        assert!(view.contains("answer-5: verify everything"), "{view}");

        // Quitting mid-interview keeps the answers recorded so far and the
        // numbering dense — no hole for a trailer to point into.
        let mut queue = canned(&["just one"]);
        let mut ask = |_: &str| queue.pop_front();
        let iv2 = run_interview(&mut agent, &mut ask, &mut sink).unwrap();
        assert!(iv2.aborted);
        assert_eq!(iv2.answers, vec!["just one".to_string()]);
        let raw = std::fs::read_to_string(session.join("events.jsonl")).unwrap();
        assert!(raw.contains("\"answer-1: just one\""));
        assert!(
            !raw.contains("answer-2: just one"),
            "no hole after quitting early"
        );
    }
}
