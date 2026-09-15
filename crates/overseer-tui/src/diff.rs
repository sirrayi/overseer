//! Line-based unified diff for `/diff` (P2.9). View-only — the diff is
//! computed from checkpoint snapshots vs the working tree, never stored.

/// Unified-diff body lines: ` `-context, `-`/`+` changes, one `@@` hunk
/// marker, `ctx` lines of context each side. Empty vec = identical.
/// LCS over lines — inputs are line-capped by callers.
pub fn unified(old: &str, new: &str, ctx: usize) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = lcs_ops(&a, &b);
    let is_change = |o: &Op| !matches!(o, Op::Keep(_));
    let Some(first) = ops.iter().position(is_change) else {
        return Vec::new();
    };
    let last = ops.iter().rposition(is_change).unwrap();
    let start = first.saturating_sub(ctx);
    let end = (last + ctx + 1).min(ops.len());

    let mut out = vec!["@@ hunk @@".to_string()];
    for op in &ops[start..end] {
        out.push(match op {
            Op::Keep(i) => format!(" {}", a[*i]),
            Op::Del(i) => format!("-{}", a[*i]),
            Op::Add(j) => format!("+{}", b[*j]),
        });
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
    use super::unified;

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
}
