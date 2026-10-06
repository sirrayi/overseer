//! The last line of every `task` result: one engine-written record of what
//! ran, parsed back by the drain (for `SubagentDone`) and the TUI label.
//! `[task-3 · read · light (claude-haiku-4-5) · $0.0123 · completed · trace: …]`

#[derive(Debug, Clone, PartialEq)]
pub struct Footer {
    pub id: String,
    pub mode: String,
    pub tier: String,
    pub model: String,
    /// None on a background ack (nothing spent yet).
    pub cost_usd: Option<f64>,
    pub status: String,
    pub verdict: Option<String>,
    /// What a verifier changed in the tree it reviewed (forces `fail`).
    pub tampered: Option<String>,
    pub trace: String,
}

const SEP: &str = " · ";

impl Footer {
    pub fn render(&self) -> String {
        let mut parts = vec![
            self.id.clone(),
            self.mode.clone(),
            format!("{} ({})", self.tier, self.model),
        ];
        if let Some(c) = self.cost_usd {
            parts.push(format!("${c:.4}"));
        }
        parts.push(self.status.clone());
        if let Some(v) = &self.verdict {
            parts.push(format!("verdict {v}"));
        }
        if let Some(t) = &self.tampered {
            parts.push(format!("tampered: {t}"));
        }
        parts.push(format!("trace: {}", self.trace));
        format!("[{}]", parts.join(SEP))
    }

    /// The last footer line in `text`, if any.
    pub fn parse(text: &str) -> Option<Self> {
        let line = text
            .lines()
            .rev()
            .find(|l| l.starts_with("[task-") && l.ends_with(']'))?;
        let mut parts = line[1..line.len() - 1].split(SEP);
        let id = parts.next()?.to_string();
        let mode = parts.next()?.to_string();
        let (tier, model) = parts.next()?.split_once(" (")?;
        let mut f = Footer {
            id,
            mode,
            tier: tier.to_string(),
            model: model.strip_suffix(')')?.to_string(),
            cost_usd: None,
            status: String::new(),
            verdict: None,
            tampered: None,
            trace: String::new(),
        };
        for p in parts {
            if let Some(c) = p.strip_prefix('$') {
                f.cost_usd = c.parse().ok();
            } else if let Some(v) = p.strip_prefix("verdict ") {
                f.verdict = Some(v.to_string());
            } else if let Some(t) = p.strip_prefix("tampered: ") {
                f.tampered = Some(t.to_string());
            } else if let Some(t) = p.strip_prefix("trace: ") {
                f.trace = t.to_string();
            } else {
                f.status = p.to_string();
            }
        }
        Some(f)
    }

    /// `read · light (claude-haiku-4-5) · $0.0123 · verdict pass` — the
    /// task cell label.
    pub fn label(&self) -> String {
        let mut s = format!("{}{SEP}{} ({})", self.mode, self.tier, self.model);
        if let Some(c) = self.cost_usd {
            s.push_str(&format!("{SEP}${c:.4}"));
        }
        if let Some(v) = &self.verdict {
            s.push_str(&format!("{SEP}verdict {v}"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_round_trips_as_the_last_line() {
        let f = Footer {
            id: "task-3".into(),
            mode: "verify".into(),
            tier: "standard".into(),
            model: "claude-sonnet-5".into(),
            cost_usd: Some(0.0123),
            status: "completed".into(),
            verdict: Some("fail".into()),
            tampered: Some("HEAD abc→def".into()),
            trace: "/s/subagents/task-3".into(),
        };
        let text = format!("[task-1 · read · light (x) · early]\nbody\n{}", f.render());
        assert_eq!(Footer::parse(&text), Some(f.clone()));
        assert_eq!(
            f.label(),
            "verify · standard (claude-sonnet-5) · $0.0123 · verdict fail"
        );
        assert_eq!(Footer::parse("no footer"), None);
    }
}
