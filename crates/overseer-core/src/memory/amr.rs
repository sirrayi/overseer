//! AMR-compatible `MEMORY.md` entry point (decision record §9.1), per the
//! Agent Memory Repo spec (`amr/SPEC.md`, MIT): `# Memory: <label>`, the
//! store's CORE.md lines as top bullets, then `## Index` with a
//! `[[layer/name]]` link per live pointer — the file is itself excluded
//! from the note index. The engine regenerates it on every commit;
//! foreign (human/tool) edits are detected against the recorded sha of
//! the last generated file and their extra bullet lines are imported
//! into `semantic/amr-import-<date>.md` before regeneration.

use super::stores;
use std::collections::HashSet;
use std::path::Path;

/// The AMR entry-point file at each store's root. Never indexed as a
/// note (see `index::topic_files`).
pub const MEMORY_MD: &str = "MEMORY.md";
/// `.index/` record of the sha256 of the last generated file.
const SHA_NAME: &str = "memory_md.sha";
/// One imported-bullet note per day.
const IMPORT_SLUG: &str = "amr-import";

/// The `Memory: <label>` label for a store dir: `user` for the user
/// store, `project <slug>` for a project store (the key's slug), else
/// the dir's own name for anything ad-hoc.
pub fn label(dir: &Path) -> String {
    label_with(
        dir,
        stores::overseer_home()
            .map(|h| stores::user_store(&h))
            .as_deref(),
    )
}

/// `label` with the user-store path injected — the user check runs
/// FIRST, on canonicalized paths (raw compare as fallback), because
/// the user store's own dir is named `memory` and would otherwise be
/// labelled `project …` by the dir-name branch below.
fn label_with(dir: &Path, user_store: Option<&Path>) -> String {
    if let Some(user) = user_store {
        let same = match (std::fs::canonicalize(dir), std::fs::canonicalize(user)) {
            (Ok(a), Ok(b)) => a == b,
            _ => dir == user,
        };
        if same {
            return "user".into();
        }
    }
    let parent = dir.parent().map(Path::to_path_buf).unwrap_or_default();
    // The canonical project store: <home>/projects/<slug>-<hash8>/memory.
    let projects_child = parent
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "projects");
    if projects_child {
        if let Some(name) = parent.file_name().and_then(|n| n.to_str()) {
            if let Some((slug, hash)) = name.rsplit_once('-') {
                if hash.len() == 8 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return format!("project {}", if slug.is_empty() { name } else { slug });
                }
            }
        }
    }
    if dir.file_name().and_then(|n| n.to_str()) == Some("memory") {
        // The `--memory` workspace store lives at <cwd>/memory; its key
        // is the parent dir's project key.
        let key = stores::project_key(&parent);
        let slug = key
            .file_name()
            .map(|n| stores::slugify(&n.to_string_lossy(), 32))
            .unwrap_or_default();
        return format!("project {}", if slug.is_empty() { "store" } else { &slug });
    }
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "store".into())
}

/// The MEMORY.md text for store `dir` labelled `label` at clock `now`:
/// header, CORE.md lines as bullets, then `## Index` with one
/// `- [[layer/name]] — title` per live pointer (links drop the `.md`).
pub fn render(label: &str, dir: &Path, now: u64) -> String {
    let mut out = format!("# Memory: {label}\n");
    let core = crate::tools::read_no_follow(&dir.join(super::CORE_NAME)).unwrap_or_default();
    for line in core.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let line = line.strip_prefix("- ").unwrap_or(line);
        out.push_str(&format!("\n- {line}"));
    }
    out.push_str("\n\n## Index\n");
    for (rel, rest) in super::live_pointers(dir, now) {
        let link = rel.trim_end_matches(".md");
        out.push_str(&format!("- [[{link}]]"));
        // The pointer tail may already carry its `—` separator.
        let rest = rest.trim_start_matches(['—', '–', '-', ' ']).trim();
        if !rest.is_empty() {
            out.push_str(&format!(" — {rest}"));
        }
        out.push('\n');
    }
    out
}

/// Bullet texts (`- ` lines) of a MEMORY.md body.
fn bullets(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim_start().strip_prefix("- "))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The MEMORY.md bytes the import pass may read at most — the rest is
/// ignored with one warning (F-large-memory-md).
const MEMORY_MD_READ_MAX: u64 = 256 * 1024;

/// Read `dir/MEMORY.md` for the import pass: `O_NOFOLLOW` (a symlinked
/// file is never read — it is simply replaced at write time), bounded
/// to [`MEMORY_MD_READ_MAX`] bytes with a one-line warning past that.
fn read_memory_md(dir: &Path) -> String {
    use std::io::Read;
    let path = dir.join(MEMORY_MD);
    let oversized = path
        .metadata()
        .map(|m| m.len() > MEMORY_MD_READ_MAX)
        .unwrap_or(false);
    let f = match crate::tools::open_read_no_follow(&path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = Vec::new();
    if f.take(MEMORY_MD_READ_MAX + 1)
        .read_to_end(&mut buf)
        .is_err()
    {
        return String::new();
    }
    if oversized || buf.len() as u64 > MEMORY_MD_READ_MAX {
        eprintln!("memory: MEMORY.md exceeds 256 KiB — importing from the first 256 KiB only");
        buf.truncate(MEMORY_MD_READ_MAX as usize);
    }
    String::from_utf8(buf).unwrap_or_default()
}

/// Regenerate `dir/MEMORY.md`, importing foreign bullet additions first
/// when the on-disk file's sha differs from the recorded generated one.
/// Returns the imported bullet count (0 when the file is engine-owned).
/// Caller is expected to hold the store's [`super::StoreLock`].
pub(crate) fn regen(dir: &Path, now: u64) -> std::io::Result<usize> {
    let label = label(dir);
    let index_dir = dir.join(".index");
    let current = read_memory_md(dir);
    let recorded = crate::tools::read_no_follow(&index_dir.join(SHA_NAME)).unwrap_or_default();
    let foreign = !current.is_empty() && super::sha_hex(current.as_bytes(), 64) != recorded.trim();
    if foreign {
        let fresh = render(&label, dir, now);
        let known: HashSet<String> = bullets(&fresh).into_iter().collect();
        let mut new: Vec<String> = Vec::new();
        for b in bullets(&current) {
            if known.contains(&b) || new.contains(&b) {
                continue;
            }
            // F-edited-bullet-import: a `[[…]]` line is generated
            // content (an index link, edited or not) — never imported.
            if b.contains("[[") {
                continue;
            }
            if super::threat::strict_refusal(&b).is_some() {
                continue; // a hostile "edit" is dropped, never imported
            }
            new.push(b);
        }
        if !new.is_empty() {
            import(dir, &new, now)?;
        }
    }
    let text = render(&label, dir, now);
    // Temp file in the store root, renamed over MEMORY.md: a symlinked
    // entry is REPLACED by a regular file, never written through
    // (F-symlink-memory-md).
    super::store_write(dir, MEMORY_MD, text.as_bytes()).map_err(std::io::Error::other)?;
    crate::harden::ensure_private_dir(&index_dir)?;
    super::real_dir(dir, ".index").map_err(std::io::Error::other)?;
    super::store_write(
        dir,
        &format!(".index/{SHA_NAME}"),
        format!("{}\n", super::sha_hex(text.as_bytes(), 64)).as_bytes(),
    )
    .map_err(std::io::Error::other)?;
    Ok(usize::from(foreign))
}

/// Append `bullets` to `semantic/amr-import-<date>.md` (created with
/// `provenance: amr-import`, `confidence: 0.5` when new), then ensure it
/// has an INDEX pointer.
fn import(dir: &Path, bullets: &[String], now: u64) -> std::io::Result<()> {
    let date = &super::rfc3339(now)[..10];
    let rel = format!("semantic/{IMPORT_SLUG}-{date}.md");
    if let Ok(mut body) = super::store_read(dir, &rel) {
        let have: HashSet<String> = body
            .lines()
            .map(str::trim_end)
            .map(str::to_string)
            .collect();
        let extra: Vec<&String> = bullets
            .iter()
            .filter(|b| !have.contains(&format!("- {b}")))
            .collect();
        if extra.is_empty() {
            return Ok(());
        }
        if !body.ends_with('\n') {
            body.push('\n');
        }
        for b in extra {
            body.push_str(&format!("- {b}\n"));
        }
        super::store_write(dir, &rel, body.as_bytes()).map_err(std::io::Error::other)?;
    } else {
        crate::harden::ensure_private_dir(&dir.join("semantic"))?;
        super::real_dir(dir, "semantic").map_err(std::io::Error::other)?;
        let mut body = format!(
            "---\nprovenance: amr-import\nconfidence: 0.5\nsource: overseer:{MEMORY_MD}\nadded: {date}\n---\n# Imported MEMORY.md bullets\n"
        );
        for b in bullets {
            body.push_str(&format!("- {b}\n"));
        }
        super::store_write(dir, &rel, body.as_bytes()).map_err(std::io::Error::other)?;
        super::append_pointer(dir, &format!("{rel} — imported MEMORY.md bullets"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const NOW: u64 = 1_900_000_000;

    fn store() -> PathBuf {
        let d = std::env::temp_dir().join(format!("ov-amr-{}", uuid::Uuid::now_v7()));
        super::super::ensure(&d).unwrap();
        d
    }

    fn note(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_user_store_is_labelled_user_not_project() {
        // The user store's dir is literally named `memory` — before the
        // canonicalized user-store check ran first it fell through to
        // the dir-name branch as `project <home>`.
        let home = store();
        let user = home.join("memory");
        super::super::ensure(&user).unwrap();
        assert_eq!(label_with(&user, Some(&user)), "user");
        // …through a non-canonical spelling of the same dir.
        let dotted = user.join(".");
        assert_eq!(label_with(&dotted, Some(&user)), "user");
        // A real project store under `projects/` still labels project.
        let proj = home.join("projects").join("demo-0123abcd").join("memory");
        super::super::ensure(&proj).unwrap();
        assert_eq!(label_with(&proj, Some(&user)), "project demo");
        // And a `--memory` workspace store (dir named `memory`, not the
        // user store) keeps its project label.
        let ws = home.join("ws").join("memory");
        super::super::ensure(&ws).unwrap();
        assert_eq!(label_with(&ws, Some(&user)), "project ws");
    }

    #[test]
    fn render_is_amr_shaped() {
        let dir = store();
        std::fs::write(dir.join("CORE.md"), "- always run tests\nkeep it small\n").unwrap();
        note(&dir, "semantic/deploy.md", "# Deploy\nhow we ship\n");
        super::super::append_pointer(&dir, "semantic/deploy.md — how we ship").unwrap();
        let text = render("project x", &dir, NOW);
        assert!(text.starts_with("# Memory: project x\n"));
        assert!(text.contains("\n- always run tests\n"), "{text}");
        assert!(text.contains("## Index\n"));
        assert!(
            text.contains("- [[semantic/deploy]] — how we ship"),
            "{text}"
        );
        assert!(!text.contains(".md]]"), "{text}");
        // Every [[link]] resolves to a topic file.
        for link in super::super::links_of(&text) {
            assert!(dir.join(format!("{link}.md")).is_file(), "{link}");
        }
    }

    #[test]
    fn regen_writes_and_tracks_sha() {
        let dir = store();
        assert_eq!(regen(&dir, NOW).unwrap(), 0);
        let path = dir.join(MEMORY_MD);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# Memory:"));
        let sha = std::fs::read_to_string(dir.join(".index").join(SHA_NAME)).unwrap();
        assert_eq!(
            sha.trim(),
            super::super::sha_hex(text.as_bytes(), 64),
            "recorded sha covers the generated file"
        );
        // Idempotent: an unedited file is never "foreign".
        assert_eq!(regen(&dir, NOW).unwrap(), 0);
    }

    #[test]
    fn foreign_bullets_are_imported_then_regenerated() {
        let dir = store();
        regen(&dir, NOW).unwrap();
        let path = dir.join(MEMORY_MD);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("- coffee over tea\n- evil \u{200b}bullet\n");
        std::fs::write(&path, &text).unwrap();
        assert_eq!(regen(&dir, NOW).unwrap(), 1);
        let date = &super::super::rfc3339(NOW)[..10];
        let import =
            std::fs::read_to_string(dir.join(format!("semantic/{IMPORT_SLUG}-{date}.md"))).unwrap();
        assert!(import.contains("provenance: amr-import"), "{import}");
        assert!(import.contains("confidence: 0.5"), "{import}");
        assert!(import.contains("- coffee over tea"), "{import}");
        assert!(!import.contains("evil"), "{import}");
        // The regenerated file dropped the foreign lines and links the
        // import note.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("coffee over tea"), "{text}");
        assert!(
            text.contains(&format!("[[semantic/{IMPORT_SLUG}-{date}]]")),
            "{text}"
        );
        // The INDEX pointer was added too.
        assert!(std::fs::read_to_string(dir.join("INDEX.md"))
            .unwrap()
            .contains("amr-import"));
        // A second pass imports nothing new.
        assert_eq!(regen(&dir, NOW).unwrap(), 0);
    }
}
