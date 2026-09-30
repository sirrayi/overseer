//! Memory v2 notice rendering: one quiet meta line per `MemoryNotice`.

/// `  ⋯ recalled a.md, b.md` for a recall, `  ⋯ reminder: <first 60
/// chars of the body>` for a reminder.
pub fn memory_line(kind: &str, notes: &[String], text: &str) -> String {
    if kind == "reminder" {
        let body = text.strip_prefix("[reminder] ").unwrap_or(text);
        let body = body.rsplit_once(" (memory: ").map_or(body, |(b, _)| b);
        let short: String = body
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(60)
            .collect();
        return format!("  ⋯ reminder: {short}");
    }
    let names: Vec<&str> = notes
        .iter()
        .map(|n| n.rsplit('/').next().unwrap_or(n))
        .map(|n| n.rsplit(':').next().unwrap_or(n))
        .collect();
    format!("  ⋯ recalled {}", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_names_the_files() {
        let notes = vec!["project:semantic/deploy.md".into(), "user:prefs.md".into()];
        assert_eq!(
            memory_line("recall", &notes, "…"),
            "  ⋯ recalled deploy.md, prefs.md"
        );
    }

    #[test]
    fn reminder_shows_the_body_head() {
        let text = format!("[reminder] {} (memory: prospective/r.md)", "x".repeat(70));
        assert_eq!(
            memory_line("reminder", &[], &text),
            format!("  ⋯ reminder: {}", "x".repeat(60))
        );
        let text = "[reminder] bump the\nversion (memory: prospective/v.md)";
        assert_eq!(
            memory_line("reminder", &[], text),
            "  ⋯ reminder: bump the version"
        );
    }
}
