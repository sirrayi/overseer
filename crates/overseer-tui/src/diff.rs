//! Line-based unified diff for `/diff` (P2.9). View-only — the diff is
//! computed from checkpoint snapshots vs the working tree, never stored.
//!
//! `hunks` returns structured hunks (the per-hunk accept/reject surface
//! and the +/− counts); `unified` renders them to display lines.

/// One contiguous change region, with its surrounding context for
/// display and the index ranges needed to revert just this hunk.
#[derive(Debug, Clone)]
pub struct Hunk {
    /// First changed index into the OLD file's lines.
    pub old_start: usize,
    /// Old lines the hunk replaced (empty for a pure insertion).
    pub old: Vec<String>,
    /// First changed index into the NEW file's lines.
    pub new_start: usize,
    /// New lines the hunk produced (empty for a pure deletion).
    pub new: Vec<String>,
    /// Display lines: `@@ hunk @@` then ` `/`-`/`+` rows.
    pub lines: Vec<String>,
    pub added: usize,
    pub deleted: usize,
}

/// Split `old`→`new` into hunks: change runs separated by more than
/// `ctx` unchanged lines stay distinct (context windows never merge).
pub fn hunks(old: &str, new: &str, ctx: usize) -> Vec<Hunk> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = lcs_ops(&a, &b);

    // (i,j) consumed *before* each op — group ranges index into a/b.
    let mut pos = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0usize, 0usize);
    for op in &ops {
        pos.push((i, j));
        match op {
            Op::Keep(_) | Op::Del(_) => i += 1,
            _ => {}
        }
        match op {
            Op::Keep(_) | Op::Add(_) => j += 1,
            _ => {}
        }
    }
    let is_change = |o: &Op| !matches!(o, Op::Keep(_));

    // Group change-op indices: a run stays one hunk while consecutive
    // changes are ≤ 2*ctx ops apart (their context windows would touch).
    let mut groups: Vec<(usize, usize)> = Vec::new(); // op-index ranges
    for (k, op) in ops.iter().enumerate() {
        if !is_change(op) {
            continue;
        }
        match groups.last_mut() {
            Some(g) if k - g.1 <= 2 * ctx => g.1 = k,
            _ => groups.push((k, k)),
        }
    }

    groups
        .into_iter()
        .map(|(k0, k1)| {
            let (i0, j0) = pos[k0];
            let (i1, j1) = match ops[k1] {
                Op::Del(_) => (pos[k1].0 + 1, pos[k1].1),
                Op::Add(_) => (pos[k1].0, pos[k1].1 + 1),
                Op::Keep(_) => unreachable!(),
            };
            let mut lines = vec!["@@ hunk @@".to_string()];
            // Context before the change region.
            for l in &a[i0.saturating_sub(ctx)..i0] {
                lines.push(format!(" {l}"));
            }
            for op in &ops[k0..=k1] {
                lines.push(match op {
                    Op::Keep(x) => format!(" {}", a[*x]),
                    Op::Del(x) => format!("-{}", a[*x]),
                    Op::Add(y) => format!("+{}", b[*y]),
                });
            }
            for l in &a[i1..(i1 + ctx).min(a.len())] {
                lines.push(format!(" {l}"));
            }
            let old_l: Vec<String> = a[i0..i1].iter().map(|s| s.to_string()).collect();
            let new_l: Vec<String> = b[j0..j1].iter().map(|s| s.to_string()).collect();
            Hunk {
                old_start: i0,
                old: old_l.clone(),
                new_start: j0,
                new: new_l.clone(),
                lines,
                added: new_l.len(),
                deleted: old_l.len(),
            }
        })
        .collect()
}

/// Unified-diff body lines across all hunks. Empty vec = identical.
/// Kept for callers that only need display text.
pub fn unified(old: &str, new: &str, ctx: usize) -> Vec<String> {
    hunks(old, new, ctx)
        .into_iter()
        .flat_map(|h| h.lines)
        .collect()
}

/// Reject selected hunks: rebuild `current` with each rejected hunk's
/// NEW range replaced by its OLD lines. Applies bottom-up so earlier
/// `new_start` indices stay valid.
pub fn apply_rejects(current: &str, file_hunks: &[Hunk], rejected: &[bool]) -> String {
    let mut lines: Vec<String> = current.lines().map(|s| s.to_string()).collect();
    for (idx, h) in file_hunks.iter().enumerate().rev() {
        if !rejected.get(idx).copied().unwrap_or(false) {
            continue;
        }
        let end = (h.new_start + h.new.len()).min(lines.len());
        lines.splice(h.new_start..end, h.old.iter().cloned());
    }
    let mut out = lines.join("\n");
    if current.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

enum Op {
    Keep(usize),
    Del(usize),
    Add(usize),
}

/// Classic DP LCS → ordered op stream (del preferred on ties so `-`
/// lines print before `+` lines like real diff).
fn lcs_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Keep(i));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Del(i));
            i += 1;
        } else {
            ops.push(Op::Add(j));
            j += 1;
        }
    }
    ops.extend((i..n).map(Op::Del));
    ops.extend((j..m).map(Op::Add));
    ops
}

#[cfg(test)]
mod tests {
    use super::{apply_rejects, hunks, unified};

    #[test]
    fn identical_is_empty() {
        assert!(unified("a\nb\n", "a\nb\n", 3).is_empty());
    }

    #[test]
    fn change_marks_minus_plus() {
        let d = unified("a\nb\nc\n", "a\nx\nc\n", 1);
        assert_eq!(d[0], "@@ hunk @@");
        assert!(d.iter().any(|l| l == "-b"));
        assert!(d.iter().any(|l| l == "+x"));
        assert!(d.iter().any(|l| l == " a"));
    }

    #[test]
    fn new_file_is_all_plus() {
        let d = unified("", "l1\nl2\n", 3);
        assert!(d.iter().all(|l| l.starts_with('+') || l.starts_with('@')));
        assert_eq!(d.len(), 3); // marker + 2 lines
    }

    #[test]
    fn deleted_file_is_all_minus() {
        let d = unified("l1\nl2\n", "", 3);
        assert!(d.iter().any(|l| l == "-l1") && d.iter().any(|l| l == "-l2"));
    }

    #[test]
    fn distant_changes_split_into_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        let hs = hunks(old, new, 1);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].old, vec!["b"]);
        assert_eq!(hs[0].new, vec!["B"]);
        assert_eq!(hs[1].old, vec!["j"]);
        assert_eq!(hs[1].new, vec!["J"]);
    }

    #[test]
    fn near_changes_merge_into_one_hunk() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\nD\ne\n";
        assert_eq!(hunks(old, new, 2).len(), 1);
    }

    #[test]
    fn reject_one_hunk_restores_its_old_lines() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        let hs = hunks(old, new, 1);
        // Reject hunk 1 only: B→b restored, J stays.
        let out = apply_rejects(new, &hs, &[true, false]);
        assert_eq!(out, "a\nb\nc\nd\ne\nf\ng\nh\ni\nJ\n");
        // Reject both → identical to old.
        let out = apply_rejects(new, &hs, &[true, true]);
        assert_eq!(out, old);
    }

    #[test]
    fn reject_pure_insert_removes_the_added_lines() {
        let hs = hunks("a\nc\n", "a\nb\nc\n", 1);
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].new, vec!["b"]);
        let out = apply_rejects("a\nb\nc\n", &hs, &[true]);
        assert_eq!(out, "a\nc\n");
    }
}
