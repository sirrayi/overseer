//! edit tool — anchored search/replace (playbook Ch.4 §2.3):
//! exact-match + uniqueness + read-before-edit, validated at apply time.
//! The result returns the applied hunk location, never the whole file.

use serde_json::{json, Value};

use super::{need_str, resolve, schema, ToolCtx, ToolOutput, ToolRegistry};

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "edit".into(),
        description: concat!(
            "Replace an exact string in a file (verbatim, including indentation; ",
            "unique unless replace_all), or apply `patch`, a unified diff for ",
            "this file alone. Read or write the file first this session. ",
            "On failure, re-read the lines and retry with more context."
        )
        .into(),
        input_schema: schema(
            json!({
                "path": {"type": "string", "description": "File to edit."},
                "old_string": {"type": "string", "description": "Exact text to replace. Omit with `patch`."},
                "new_string": {"type": "string", "description": "Replacement text. Omit with `patch`."},
                "replace_all": {"type": "boolean", "description": "Every occurrence (default false)."},
                "patch": {"type": "string", "description": "`@@ -a,b +c,d @@` hunks with context, '-' and '+' lines."}
            }),
            // Only `path` is structurally required: the edit *variant*
            // (anchor pair vs patch) is validated in `run`, so the error
            // names the missing half of whichever form was attempted.
            &["path"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx, reg: &mut ToolRegistry) -> ToolOutput {
    let path_str = match need_str(input, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let path = resolve(ctx, path_str);

    // P8-B mode edit_globs (roo pattern): a mode may bound the file tools
    // to a path set. Enforced here, in the tool, so a caller that skips
    // the CLI cannot bypass it.
    if !reg.edit_allowed(path_str) {
        let globs = reg
            .mode
            .map(|m| m.edit_globs.join(", "))
            .unwrap_or_default();
        return ToolOutput::err(format!(
            "Refusing to edit {}: the active mode allows only {globs}.",
            path.display()
        ));
    }

    // Read-before-edit: harness-enforced anti-hallucination invariant.
    if !reg.was_read(&path) {
        return ToolOutput::err(format!(
            "Refusing to edit {}: it has not been read this session. \
             Call `read` on it first — editing ungrounded content corrupts files.",
            path.display()
        ));
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("Cannot read {}: {e}", path.display())),
    };

    // P8-B patch form (aider edit-format port): a unified diff for this
    // file applies through the same validate/snapshot/write path.
    if let Some(patch) = input.get("patch").and_then(Value::as_str) {
        if input.get("old_string").is_some() {
            return ToolOutput::err(
                "Pass either `patch` or `old_string`/`new_string`, not both — \
                 the two forms encode the same change differently.",
            );
        }
        // Fail-closed pre-verification: hunk line numbers in range and
        // context matching, checked before `apply_unified_diff` runs.
        if let Err(e) = preverify_patch(&content, patch) {
            return ToolOutput::err(format!(
                "Edit rejected: the patch failed pre-verification for {} ({e}). \
                 Re-read the file and regenerate the diff.",
                path.display()
            ));
        }
        let replaced = match apply_unified_diff(&content, patch) {
            Ok(r) => r,
            Err(e) => {
                return ToolOutput::err(format!(
                    "Edit rejected: the patch does not apply to {} ({e}). \
                     Re-read the file and regenerate the diff.",
                    path.display()
                ))
            }
        };
        let added = replaced.lines().count() as i64 - content.lines().count() as i64;
        // The reported hunk is the first line the patch changed.
        let first_change = content
            .lines()
            .zip(replaced.lines())
            .position(|(a, b)| a != b)
            .map(|i| i + 1)
            .unwrap_or(1);
        return commit_edit(
            &path,
            replaced,
            1,
            first_change,
            added.max(0) as usize + 1,
            ctx,
            &reg.policy.root,
            &format!("applied patch ({added:+} lines)"),
        );
    }

    let old = match need_str(input, "old_string") {
        Ok(s) => s,
        Err(_) => {
            return ToolOutput::err(
                "Missing `old_string`/`new_string` (or `patch`): the edit needs the \
                 exact text to replace and its replacement.",
            )
        }
    };
    let new = match need_str(input, "new_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let replace_all = input
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if old == new {
        return ToolOutput::err("old_string and new_string are identical — nothing to change.");
    }

    // The model's edit dialect (aider edit-format registry): anchors for
    // the Claude family, whitespace-tolerant line blocks for diff-dialect
    // models, whole-file rewrites pointed at `write`.
    let format = ctx
        .agent_config
        .as_ref()
        .map(|c| crate::profile::edit_format(&c.model))
        .unwrap_or(crate::profile::EditFormat::SearchReplace);

    let count = content.matches(old).count();
    if count == 0 {
        // Diff-format models routinely re-indent an anchor; retry as a
        // trimmed line block before failing the edit.
        if format == crate::profile::EditFormat::Diff {
            if let Some((start, end)) = find_ws_tolerant(&content, old) {
                let replacement = terminate_block(new, &content[start..end]);
                let mut replaced = String::with_capacity(content.len() + replacement.len());
                replaced.push_str(&content[..start]);
                replaced.push_str(&replacement);
                replaced.push_str(&content[end..]);
                let new_lines = replacement.lines().count();
                let line_no = replaced[..start.min(replaced.len())].matches('\n').count() + 1;
                return commit_edit(
                    &path,
                    replaced,
                    1,
                    line_no,
                    new_lines,
                    ctx,
                    &reg.policy.root,
                    "whitespace-tolerant match (diff format)",
                );
            }
        }
        let line_count = content.lines().count();
        let advice = match format {
            crate::profile::EditFormat::Diff => {
                "This model's profile uses diff edits — send a `patch` (unified \
                 diff) instead of re-indenting the anchor."
            }
            crate::profile::EditFormat::WholeFile => {
                "This model's profile uses whole-file edits — rewrite the file \
                 with `write` instead of anchoring."
            }
            crate::profile::EditFormat::SearchReplace => {
                "Re-read the file around the target lines; whitespace and \
                 indentation must match verbatim."
            }
        };
        let fallback = if line_count <= WHOLE_FILE_FALLBACK_LINES {
            format!(
                " The file is only {line_count} lines — rewriting it wholesale with \
                 `write` is an acceptable fallback."
            )
        } else {
            String::new()
        };
        return ToolOutput::err(format!(
            "old_string not found in {}. {advice}{fallback}",
            path.display()
        ));
    }
    if count > 1 && !replace_all {
        return ToolOutput::err(format!(
            "old_string occurs {count} times in {} — the edit must be unique. \
             Include more surrounding context, or set replace_all to change all {count}.",
            path.display()
        ));
    }

    let replaced = if replace_all {
        content.replacen(old, new, usize::MAX)
    } else {
        content.replacen(old, new, 1)
    };
    let upto = content.find(old).unwrap_or(0);
    let new_lines = if new.is_empty() {
        0
    } else {
        new.matches('\n').count() + 1
    };
    let line_no = replaced[..upto.min(replaced.len())].matches('\n').count() + 1;
    commit_edit(
        &path,
        replaced,
        count,
        line_no,
        new_lines,
        ctx,
        &reg.policy.root,
        "",
    )
}

/// Validate, snapshot, write, and report one applied edit. The lint gate
/// (B1-1 ACI) runs first: refuse to write content we know is broken.
/// `line_no` locates the reported hunk in the *new* content; `note`
/// appends a provenance hint (e.g. which match strategy fired).
#[allow(clippy::too_many_arguments)]
fn commit_edit(
    path: &std::path::Path,
    replaced: String,
    count: usize,
    line_no: usize,
    new_lines: usize,
    ctx: &mut ToolCtx,
    root: &std::path::Path,
    note: &str,
) -> ToolOutput {
    if let Some(diag) = edit_lint_gate(path, &replaced) {
        return ToolOutput::err(format!(
            "Edit rejected: it would produce invalid {} in {} ({diag}). \
             The file was not modified — check the replacement syntax.",
            path.extension().and_then(|e| e.to_str()).unwrap_or("file"),
            path.display()
        ));
    }
    let target = match super::contained_target(path, root) {
        Ok(t) => t,
        Err(e) => return ToolOutput::err(e),
    };
    super::snapshot(ctx, &target);
    if let Err(e) = super::write_no_follow(&target, replaced.as_bytes()) {
        return ToolOutput::err(e);
    }
    // Return the applied hunk (±3 lines context) — never the whole file.
    let suffix = if note.is_empty() {
        String::new()
    } else {
        format!(" [{note}]")
    };
    ToolOutput::ok(format!(
        "Edited {}: {} replacement{} at line {}{suffix}.\n{}",
        path.display(),
        count,
        if count == 1 { "" } else { "s" },
        line_no,
        hunk(&replaced, line_no, new_lines)
    ))
}

/// Keep a line-aligned replacement line-aligned: when the matched block
/// ended with a newline and the replacement does not, add one — otherwise
/// the last replaced line fuses onto the line that followed the block.
/// (The exact-anchor `replace_all` path deliberately does NOT do this: a
/// character-precise anchor means the model is editing characters, not
/// lines, and its `new_string` is taken verbatim.)
fn terminate_block(replacement: &str, matched_block: &str) -> String {
    if replacement.is_empty() {
        return String::new();
    }
    if replacement.ends_with('\n') {
        return replacement.to_string();
    }
    if matched_block.ends_with('\n') {
        return format!("{replacement}\n");
    }
    replacement.to_string()
}

/// Whitespace-tolerant block match: slide `needle`'s trimmed lines over
/// `content`'s trimmed lines and return the byte span covering the first
/// full block that matches. `None` when the shapes differ or no window
/// matches. Only leading/trailing whitespace is forgiven — never content.
fn find_ws_tolerant(content: &str, needle: &str) -> Option<(usize, usize)> {
    let needle_lines: Vec<&str> = needle
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if needle_lines.is_empty() {
        return None;
    }
    // (byte offset of line start, line text)
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut off = 0usize;
    for l in content.split_inclusive('\n') {
        lines.push((off, l.trim_end_matches(['\n', '\r'])));
        off += l.len();
    }
    if lines.is_empty() {
        return None;
    }
    let hay: Vec<&str> = lines.iter().map(|(_, t)| t.trim()).collect();
    for start in 0..hay.len() {
        if hay[start].is_empty() {
            continue;
        }
        let end = start + needle_lines.len();
        if end > hay.len() {
            break;
        }
        if hay[start..end] == needle_lines[..] {
            let byte_start = lines[start].0;
            let byte_end = if end < lines.len() {
                lines[end].0
            } else {
                content.len()
            };
            return Some((byte_start, byte_end));
        }
    }
    None
}

/// Pre-verify a unified diff against `content` without writing: parse every
/// `@@` hunk, check its old line numbers sit in range, and check its old
/// block (context plus `-` lines) matches the file where
/// `apply_unified_diff` would locate it (`locate_hunk`: declared position
/// first, then a forward scan that never reaches back before the previous
/// hunk). Returns the affected 1-based old-file line numbers,
/// sorted and deduplicated; a pure-insertion (zero-length) hunk covers no
/// old lines and contributes none. Any failure names the hunk in its
/// message — fail-closed: `run` refuses the patch on `Err` before
/// `apply_unified_diff` runs, so a stale or out-of-range patch can never
/// splice into the wrong place.
pub fn preverify_patch(content: &str, patch: &str) -> Result<Vec<usize>, String> {
    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();
    let patch_lines: Vec<&str> = patch.lines().collect();
    let mut idx = 0usize;
    let mut hunks = 0usize;
    let mut floor = 0usize;
    let mut affected: Vec<usize> = Vec::new();
    while idx < patch_lines.len() {
        let header = patch_lines[idx];
        if !header.starts_with("@@") {
            idx += 1;
            continue;
        }
        let (old_start, old_len) = parse_hunk_header(header)?;
        idx += 1;
        let mut body: Vec<&str> = Vec::new();
        while idx < patch_lines.len() && !patch_lines[idx].starts_with("@@") {
            body.push(patch_lines[idx]);
            idx += 1;
        }
        let (old_block, _) = hunk_blocks(&body)?;
        if old_block.len() != old_len {
            return Err(format!(
                "hunk {hunks} declares {old_len} old line(s) but carries {}",
                old_block.len()
            ));
        }
        // Old line numbers are 1-based. A zero-length hunk is a pure
        // insertion anchored after line `old_start` (`0` = before line 1),
        // so its anchor must sit in `0..=n`; any other hunk must sit
        // wholly inside `1..=n`.
        if old_len == 0 {
            if old_start > n {
                return Err(format!(
                    "hunk {hunks} inserts after line {old_start} but the file has only {n} line(s)"
                ));
            }
        } else if old_start == 0 || old_start + old_len - 1 > n {
            return Err(format!(
                "hunk {hunks} covers old lines {old_start}..{} but the file has only {n} line(s)",
                old_start + old_len - 1
            ));
        }
        // Context must match where `apply_unified_diff` would put the
        // hunk: the same `locate_hunk` over the original content (hunk
        // headers name old-file lines, so every hunk is checked against
        // the original, never a half-patched buffer).
        let Some(start) = locate_hunk(&lines, old_start, &old_block, floor) else {
            return Err(format!(
                "hunk {hunks} (declared at line {old_start}) does not match the file \
                 after the previous hunk"
            ));
        };
        affected.extend(start..start + old_block.len());
        floor = start + old_block.len();
        hunks += 1;
    }
    if hunks == 0 {
        return Err("no `@@` hunk headers found".to_string());
    }
    let mut affected: Vec<usize> = affected.into_iter().map(|s| s + 1).collect();
    affected.sort();
    affected.dedup();
    Ok(affected)
}

/// Apply a unified diff to `content` (one file's diff — no `---`/`+++`
/// headers required, they are skipped). Hunks are located by their
/// content, not by line number alone: a hunk whose context does not match
/// is an error naming the hunk, so a stale patch can never splice into the
/// wrong place. `\ No newline at end of file` markers are ignored.
pub fn apply_unified_diff(content: &str, patch: &str) -> Result<String, String> {
    let ends_nl = content.ends_with('\n');
    let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
    if ends_nl {
        lines.pop();
    }
    let patch_lines: Vec<&str> = patch.lines().collect();
    let mut idx = 0usize;
    let mut hunks = 0usize;
    let mut floor = 0usize;
    let mut splices: Vec<(usize, usize, Vec<String>)> = Vec::new();
    while idx < patch_lines.len() {
        let header = patch_lines[idx];
        if !header.starts_with("@@") {
            idx += 1;
            continue;
        }
        let (old_start, old_len) = parse_hunk_header(header)?;
        idx += 1;
        let mut body: Vec<&str> = Vec::new();
        while idx < patch_lines.len() && !patch_lines[idx].starts_with("@@") {
            body.push(patch_lines[idx]);
            idx += 1;
        }
        let (old_block, new_block) = hunk_blocks(&body)?;
        if old_block.len() != old_len {
            return Err(format!(
                "hunk {hunks} declares {old_len} old line(s) but carries {}",
                old_block.len()
            ));
        }
        let Some(start) = locate_hunk(&lines, old_start, &old_block, floor) else {
            return Err(format!(
                "hunk {hunks} (declared at line {old_start}) does not match the file \
                 after the previous hunk"
            ));
        };
        floor = start + old_block.len();
        splices.push((start, old_block.len(), new_block));
        hunks += 1;
    }
    if hunks == 0 {
        return Err("no `@@` hunk headers found".to_string());
    }
    // Splices are ordered and disjoint in old-file coordinates.
    let mut patched: Vec<String> = Vec::with_capacity(lines.len());
    let mut cursor = 0usize;
    for (start, len, new_block) in splices {
        patched.extend(lines[cursor..start].iter().cloned());
        patched.extend(new_block);
        cursor = start + len;
    }
    patched.extend(lines[cursor..].iter().cloned());
    let mut out = patched.join("\n");
    if ends_nl {
        out.push('\n');
    }
    Ok(out)
}

/// Where a hunk lands in the original lines (0-based splice index), shared
/// by `preverify_patch` and `apply_unified_diff` so the two never disagree.
/// A pure insertion (`@@ -a,0 …`) splices at index `a` — after old line
/// `a`, `0` = top of file. Any other hunk matches its old block by content:
/// the declared position first, then a forward scan. Hunks are ordered, so
/// no hunk may land before `floor` (the previous hunk's old end) — two
/// hunks with identical context edit two regions, never one twice.
fn locate_hunk<S: AsRef<str>>(
    lines: &[S],
    old_start: usize,
    old_block: &[String],
    floor: usize,
) -> Option<usize> {
    let n = lines.len();
    if old_block.is_empty() {
        return (floor <= old_start && old_start <= n).then_some(old_start);
    }
    let from = old_start.saturating_sub(1).max(floor);
    if from + old_block.len() > n {
        return None;
    }
    (from..=n - old_block.len()).find(|&s| {
        lines[s..s + old_block.len()]
            .iter()
            .zip(old_block)
            .all(|(a, b)| a.as_ref() == b.as_str())
    })
}

/// `@@ -a[,b] +c[,d] @@` → (old start, old length). Length defaults to 1.
fn parse_hunk_header(line: &str) -> Result<(usize, usize), String> {
    let inner = line
        .trim_start_matches('@')
        .trim()
        .split("@@")
        .next()
        .unwrap_or("")
        .trim();
    let old_part = inner
        .split_whitespace()
        .find(|t| t.starts_with('-'))
        .ok_or_else(|| format!("unparsable hunk header `{line}` — want `@@ -a,b +c,d @@`"))?;
    let spec = old_part.trim_start_matches('-');
    let (start, len) = match spec.split_once(',') {
        Some((s, l)) => (
            s.parse::<usize>()
                .map_err(|_| format!("bad hunk start `{s}`"))?,
            l.parse::<usize>()
                .map_err(|_| format!("bad hunk length `{l}`"))?,
        ),
        None => (
            spec.parse::<usize>()
                .map_err(|_| format!("bad hunk start `{spec}`"))?,
            1,
        ),
    };
    Ok((start, len))
}

/// Split a hunk body into its (old, new) line blocks. Context and `-`
/// lines form the old block; context and `+` lines form the new one.
fn hunk_blocks(body: &[&str]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut old = Vec::new();
    let mut new = Vec::new();
    for l in body {
        if l.starts_with('\\') {
            continue; // `\ No newline at end of file`
        }
        match l.chars().next() {
            Some(' ') => {
                let t = l[1..].to_string();
                old.push(t.clone());
                new.push(t);
            }
            Some('-') => old.push(l[1..].to_string()),
            Some('+') => new.push(l[1..].to_string()),
            // A bare empty line inside a hunk is a context line for an
            // empty source line (some generators drop the leading space).
            None => {
                old.push(String::new());
                new.push(String::new());
            }
            Some(c) => return Err(format!("unparsable hunk line `{c}…`")),
        }
    }
    Ok((old, new))
}

/// Parse-only lint gate over post-edit content (B1-1). Returns `Some(diag)`
/// when the content is unparseable, `None` when it parses or the gate does
/// not apply (unknown extension, gate binary missing). Pure function of
/// (path, content) — no fs writes outside a temp file, no network.
pub fn edit_lint_gate(path: &std::path::Path, content: &str) -> Option<String> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => serde_json::from_str::<Value>(content)
            .err()
            .map(|e| e.to_string()),
        Some("py") => probe_tempfile(content, "py", &["python3", "-m", "py_compile"], false),
        Some("rs") => probe_tempfile(
            content,
            "rs",
            &["rustfmt", "--edition", "2021", "--check"],
            true,
        ),
        _ => None,
    }
}

/// Stage `content` as a temp file and run a parse probe. When
/// `stderr_error_gate` is set, only stderr lines starting with `error` block
/// (rustfmt `--check` formatting Diffs go to stdout and must NOT gate).
/// Binary missing or temp unwritable → None (fail-open). No new deps.
fn probe_tempfile(
    content: &str,
    ext: &str,
    cmd: &[&str],
    stderr_error_gate: bool,
) -> Option<String> {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("overseer-lint-{}", uuid::Uuid::now_v7()));
    tmp.set_extension(ext);
    if std::fs::write(&tmp, content).is_err() {
        return None;
    }
    let out = std::process::Command::new(cmd[0])
        .args(&cmd[1..])
        .arg(&tmp)
        .output();
    let _ = std::fs::remove_file(&tmp);
    match out {
        Err(_) => None,
        Ok(o) if !stderr_error_gate && o.status.success() => None,
        Ok(o) if !stderr_error_gate => Some(first_lines(&String::from_utf8_lossy(&o.stderr), 3)),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.lines().any(|l| l.starts_with("error")) {
                Some(first_lines(&stderr, 3))
            } else {
                None
            }
        }
    }
}

/// First `n` lines of a diagnostic, single-line-joined for tool output.
fn first_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join(" | ")
}

/// Files at or under this many lines get the whole-file `write` fallback
/// hint when an anchor fails (playbook: ~400 lines).
const WHOLE_FILE_FALLBACK_LINES: usize = 400;

/// Render the applied hunk with line numbers: the new content plus 3 lines
/// of context each side. Long replacements are head/tail-trimmed so the
/// result stays a hunk, not a file dump.
fn hunk(content: &str, line_no: usize, new_lines: usize) -> String {
    const CONTEXT: usize = 3;
    const MAX_HUNK: usize = 60;

    let lines: Vec<&str> = content.lines().collect();
    let first = line_no.saturating_sub(CONTEXT).max(1);
    let last = (line_no + new_lines - 1 + CONTEXT).min(lines.len());
    let mut out = String::new();
    let mut shown = 0usize;
    let mut i = first;
    while i <= last {
        let in_new = i >= line_no && i < line_no + new_lines;
        // Trim the middle of large inserted regions — context rows stay.
        if in_new && new_lines > MAX_HUNK && shown >= MAX_HUNK / 2 {
            let remaining = (line_no + new_lines) - i;
            if remaining > MAX_HUNK / 2 {
                out.push_str(&format!(
                    "      [...{} inserted lines omitted...]\n",
                    remaining - MAX_HUNK / 2
                ));
                i = line_no + new_lines - MAX_HUNK / 2;
                continue;
            }
        }
        out.push_str(&format!("{i:>6}\t{}\n", lines[i - 1]));
        shown += 1;
        i += 1;
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lint_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(name)
    }

    #[test]
    fn lint_gate_blocks_invalid_json() {
        let d = edit_lint_gate(&lint_path("a.json"), r#"{"a": }"#);
        assert!(d.is_some(), "invalid JSON must block");
        assert!(edit_lint_gate(&lint_path("a.json"), r#"{"a": 1}"#).is_none());
    }

    #[test]
    fn lint_gate_blocks_invalid_python() {
        let d = edit_lint_gate(&lint_path("a.py"), "def f(:\n  pass\n");
        assert!(d.is_some(), "invalid Python must block");
    }

    #[test]
    fn lint_gate_allows_valid_python_with_imports() {
        let ok = "import os\n\n\ndef f():\n    return os.getcwd()\n";
        assert!(edit_lint_gate(&lint_path("a.py"), ok).is_none());
    }

    #[test]
    fn lint_gate_blocks_invalid_rust() {
        let d = edit_lint_gate(&lint_path("a.rs"), "fn main( { broken\n");
        assert!(d.is_some(), "unparseable Rust must block");
    }

    #[test]
    fn lint_gate_allows_valid_rust_with_imports() {
        let ok = "use std::collections::HashMap;\n\nfn main() {\nlet m = HashMap::new();\nprintln!(\"{:?}\", m.len());\n}\n";
        assert!(
            edit_lint_gate(&lint_path("a.rs"), ok).is_none(),
            "valid Rust with imports must apply"
        );
    }

    #[test]
    fn lint_gate_allows_valid_but_unformatted_rust() {
        // rustfmt --check exits 1 with a Diff on stdout for this file, but
        // stderr carries no `error` — the gate signal is stderr, not exit code.
        let ok = "fn main() { let x = 1; println!(\"{x}\"); }\n";
        assert!(
            edit_lint_gate(&lint_path("a.rs"), ok).is_none(),
            "valid-but-unformatted Rust must apply"
        );
    }

    #[test]
    fn lint_gate_ignores_unknown_extensions() {
        assert!(edit_lint_gate(&lint_path("a.md"), "{{{broken").is_none());
    }

    #[test]
    fn unified_diff_applies_hunks_by_content() {
        let content = "one\ntwo\nthree\nfour\nfive\nsix\n";
        // Two hunks: change `two`, insert after `five`.
        let patch = "\
--- a/f
+++ b/f
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
@@ -4,3 +4,4 @@
 four
 five
+SIX-AND-A-HALF
 six
";
        let out = apply_unified_diff(content, patch).unwrap();
        assert_eq!(out, "one\nTWO\nthree\nfour\nfive\nSIX-AND-A-HALF\nsix\n");
        // A stale patch (context that no longer exists) is refused, and the
        // error names the hunk.
        let err = apply_unified_diff(&out, patch).unwrap_err();
        assert!(err.contains("hunk 0"), "{err}");
        // A patch with no hunks is an error, not a silent no-op.
        assert!(apply_unified_diff(content, "--- a\n+++ b\n")
            .unwrap_err()
            .contains("no `@@`"));
        // Deleting a line works, and a trailing-newline-less file stays so.
        let del = apply_unified_diff("a\nb\nc\n", "@@ -1,3 +1,2 @@\n a\n-b\n c\n").unwrap();
        assert_eq!(del, "a\nc\n");
        let no_nl = apply_unified_diff("a\nb", "@@ -1,2 +1,2 @@\n a\n-b\n+B\n").unwrap();
        assert_eq!(no_nl, "a\nB");
    }

    #[test]
    fn block_replacement_keeps_line_alignment() {
        assert_eq!(terminate_block("BETA\nGAMMA", "x\ny\n"), "BETA\nGAMMA\n");
        assert_eq!(terminate_block("BETA\n", "x\ny\n"), "BETA\n");
        // A block that did not end with a newline (EOF) stays unterminated.
        assert_eq!(terminate_block("BETA", "x\ny"), "BETA");
        // Deleting the block stays a deletion.
        assert_eq!(terminate_block("", "x\ny\n"), "");
    }

    #[test]
    fn whitespace_tolerant_match_is_bounded_by_content() {
        let content = "fn main() {\n    let x = 1;\n    x\n}\n";
        // Same block, re-indented and with a trailing space difference.
        let found = find_ws_tolerant(content, "  let x = 1;\n  x\n");
        assert!(found.is_some(), "trimmed lines must match");
        let (s, e) = found.unwrap();
        assert_eq!(&content[s..e], "    let x = 1;\n    x\n");
        // Content differences are never forgiven.
        assert!(find_ws_tolerant(content, "let y = 1;\n").is_none());
        assert!(find_ws_tolerant(content, "let x = 2;\n").is_none());
    }

    #[test]
    fn edit_format_selects_the_anchor_strategy() {
        use crate::tools::ToolCtx;
        let dir = std::env::temp_dir().join(format!("overseer-editfmt-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        let body = "alpha\n    beta\n    gamma\ndelta\n";
        std::fs::write(&file, body).unwrap();
        let mut reg = crate::tools::ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ToolCtx {
            cwd: dir.clone(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagents: Default::default(),
            checkpoint: None,
            sandbox: false,
            broker: None,
        };
        reg.call("read", &json!({"path": "f.txt"}), &mut c);

        // SearchReplace profile (the default ctx has no agent config):
        // a re-indented anchor misses and the hint names re-reading.
        let miss = reg.call(
            "edit",
            &json!({"path": "f.txt", "old_string": "  beta\n  gamma", "new_string": "x"}),
            &mut c,
        );
        assert!(miss.is_error);
        assert!(
            miss.text.contains("whitespace and indentation"),
            "{}",
            miss.text
        );

        // Diff profile (deepseek-v4.1-flash): the same anchor applies as a
        // trimmed block, and the result notes which strategy fired.
        c.agent_config = Some(crate::agent::AgentConfig {
            model: "deepseek-v4.1-flash".into(),
            cwd: dir.clone(),
            ..Default::default()
        });
        let ok = reg.call(
            "edit",
            &json!({"path": "f.txt", "old_string": "  beta\n  gamma", "new_string": "BETA\nGAMMA"}),
            &mut c,
        );
        assert!(!ok.is_error, "{}", ok.text);
        assert!(ok.text.contains("whitespace-tolerant match"), "{}", ok.text);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nBETA\nGAMMA\ndelta\n"
        );

        // The patch form works regardless of the profile, and mixing forms
        // is refused rather than guessed.
        let patched = reg.call(
            "edit",
            &json!({"path": "f.txt", "patch": "@@ -1,2 +1,2 @@\n alpha\n-BETA\n+beta\n"}),
            &mut c,
        );
        assert!(!patched.is_error, "{}", patched.text);
        assert!(patched.text.contains("applied patch"), "{}", patched.text);
        assert!(std::fs::read_to_string(&file)
            .unwrap()
            .starts_with("alpha\nbeta\n"));
        let mixed = reg.call(
            "edit",
            &json!({"path": "f.txt", "patch": "@@ -1 +1 @@\n-a\n+b\n", "old_string": "a", "new_string": "b"}),
            &mut c,
        );
        assert!(mixed.is_error);
        assert!(mixed.text.contains("not both"), "{}", mixed.text);

        // Neither form → a field-level error naming both halves.
        let neither = reg.call("edit", &json!({"path": "f.txt"}), &mut c);
        assert!(neither.is_error);
        assert!(neither.text.contains("old_string"), "{}", neither.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preverify_accepts_a_good_patch_with_sorted_affected_lines() {
        let content = "one\ntwo\nthree\nfour\nfive\nsix\n";
        let patch = "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        assert_eq!(preverify_patch(content, patch).unwrap(), vec![1, 2, 3]);
        // A pure insertion (zero-length hunk) covers no old lines.
        let ins = preverify_patch(content, "@@ -2,0 +3,1 @@\n+NEW\n").unwrap();
        assert!(ins.is_empty(), "{ins:?}");
        // No write happened: preverify is read-only.
        assert_eq!(content, "one\ntwo\nthree\nfour\nfive\nsix\n");
    }

    #[test]
    fn preverify_refuses_stale_out_of_range_and_hunkless_patches() {
        let content = "one\ntwo\nthree\n";
        let patch = "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let applied = apply_unified_diff(content, patch).unwrap();
        // The same patch against the already-patched file is stale, and
        // the error names the hunk — the same message apply reports.
        let err = preverify_patch(&applied, patch).unwrap_err();
        assert!(err.contains("hunk 0"), "{err}");
        // Old line numbers past EOF are out of range, naming the hunk.
        let err = preverify_patch(content, "@@ -9,1 +9,1 @@\n-x\n+X\n").unwrap_err();
        assert!(err.contains("hunk 0") && err.contains("only 3"), "{err}");
        // No hunks is an error, not a silent no-op.
        assert!(preverify_patch(content, "--- a\n+++ b\n")
            .unwrap_err()
            .contains("no `@@`"));
    }

    #[test]
    fn pure_insertion_hunk_inserts_after_the_anchor_line() {
        let content = "a\nb\nc\n";
        // `-0,0`: insert before line 1 (top of file).
        let top = "@@ -0,0 +1,1 @@\n+TOP\n";
        assert!(preverify_patch(content, top).is_ok());
        assert_eq!(apply_unified_diff(content, top).unwrap(), "TOP\na\nb\nc\n");
        // `-2,0`: insert AFTER old line 2.
        let mid = "@@ -2,0 +3,1 @@\n+MID\n";
        assert!(preverify_patch(content, mid).is_ok());
        assert_eq!(apply_unified_diff(content, mid).unwrap(), "a\nb\nMID\nc\n");
        // `-3,0`: insert after the last line (EOF).
        let eof = "@@ -3,0 +4,1 @@\n+END\n";
        assert!(preverify_patch(content, eof).is_ok());
        assert_eq!(apply_unified_diff(content, eof).unwrap(), "a\nb\nc\nEND\n");
        // Past EOF is out of range in both halves.
        let past = "@@ -4,0 +5,1 @@\n+X\n";
        assert!(preverify_patch(content, past).is_err());
        assert!(apply_unified_diff(content, past).is_err());
    }

    #[test]
    fn identical_context_hunks_edit_two_different_regions() {
        let content = "x\nkeep\ny\nmid\nx\nkeep\ny\n";
        // Both hunks carry the same old block and a loose declared line:
        // hunks are ordered, so the second must land after the first.
        let patch = "\
@@ -1,3 +1,3 @@
 x
-keep
+ONE
 y
@@ -1,3 +1,3 @@
 x
-keep
+TWO
 y
";
        let affected = preverify_patch(content, patch).unwrap();
        assert_eq!(affected, vec![1, 2, 3, 5, 6, 7]);
        assert_eq!(
            apply_unified_diff(content, patch).unwrap(),
            "x\nONE\ny\nmid\nx\nTWO\ny\n"
        );
        // A hunk whose context only exists before the previous hunk's end
        // is refused, never re-applied to the earlier region.
        let twice = "\
@@ -5,3 +5,3 @@
 x
-keep
+TWO
 y
@@ -1,3 +1,3 @@
 x
-keep
+ONE
 y
";
        assert!(preverify_patch(content, twice).is_err());
        assert!(apply_unified_diff(content, twice).is_err());
        // Hunks that grow the buffer still locate later hunks correctly.
        let grow = "\
@@ -1,2 +1,3 @@
 x
+NEW
 keep
@@ -5,2 +6,2 @@
 x
-keep
+KEPT
";
        assert!(preverify_patch(content, grow).is_ok());
        assert_eq!(
            apply_unified_diff(content, grow).unwrap(),
            "x\nNEW\nkeep\ny\nmid\nx\nKEPT\ny\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn edit_through_a_symlink_to_an_outside_file_is_refused() {
        use crate::tools::ToolCtx;
        let dir = std::env::temp_dir().join(format!("overseer-editlink-{}", uuid::Uuid::now_v7()));
        let outside = dir.with_extension("outside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let dir = dir.canonicalize().unwrap();
        std::fs::write(outside.join("v.txt"), "alpha\n").unwrap();
        std::os::unix::fs::symlink(outside.join("v.txt"), dir.join("l.txt")).unwrap();
        let mut reg = crate::tools::ToolRegistry::core(crate::perm::Policy::headless(dir.clone()));
        reg.mark_read(&dir.join("l.txt"));
        let mut c = ToolCtx {
            cwd: dir.clone(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagents: Default::default(),
            checkpoint: None,
            sandbox: false,
            broker: None,
        };
        let out = run(
            &json!({"path": "l.txt", "old_string": "alpha", "new_string": "pwned"}),
            &mut c,
            &mut reg,
        );
        assert!(out.is_error, "{}", out.text);
        assert!(
            out.text.contains("outside the working directory"),
            "{}",
            out.text
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("v.txt")).unwrap(),
            "alpha\n"
        );
        // A normal in-workspace edit is unchanged.
        std::fs::write(dir.join("n.txt"), "alpha\n").unwrap();
        reg.mark_read(&dir.join("n.txt"));
        let out = run(
            &json!({"path": "n.txt", "old_string": "alpha", "new_string": "beta"}),
            &mut c,
            &mut reg,
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            std::fs::read_to_string(dir.join("n.txt")).unwrap(),
            "beta\n"
        );
    }

    #[test]
    fn run_rejects_a_patch_that_fails_preverification_without_writing() {
        use crate::tools::ToolCtx;
        let dir = std::env::temp_dir().join(format!("overseer-preverify-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "a\nb\nc\n").unwrap();
        let mut reg = crate::tools::ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ToolCtx {
            cwd: dir.clone(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagents: Default::default(),
            checkpoint: None,
            sandbox: false,
            broker: None,
        };
        reg.call("read", &json!({"path": "f.txt"}), &mut c);
        let out = reg.call(
            "edit",
            &json!({"path": "f.txt", "patch": "@@ -9,1 +9,1 @@\n-x\n+X\n"}),
            &mut c,
        );
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("pre-verification"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nb\nc\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
