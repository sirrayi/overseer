//! Audit (memory v2): ranking behaviour of `index::Index::search`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use overseer_core::memory::{self, activation, index::Index, Scope};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn store(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ov-audit-m2i-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    memory::ensure(&d).unwrap();
    d
}

/// RRF gives activation and confidence two of three equal votes over
/// every candidate, including notes that matched one common query term.
/// The only note matching both query terms (unused, default confidence)
/// is pushed out of the top 3 by recently used, high-confidence notes
/// that match just `port`.
#[test]
fn the_only_full_match_ranks_first_over_popular_partial_matches() {
    let dir = store("fusion");
    let t = now();
    let old = UNIX_EPOCH + Duration::from_secs(t - 200 * 86_400);
    let gold = dir.join("semantic/kestrel-port.md");
    std::fs::write(&gold, "# kestrel port\nkestrel listens on port 8443.\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&gold)
        .unwrap()
        .set_modified(old)
        .unwrap();
    for i in 0..30 {
        let rel = format!("semantic/svc{i}-port.md");
        std::fs::write(
            dir.join(&rel),
            format!(
                "---\nconfidence: 0.9\n---\n# svc{i} port\nsvc{i} listens on port {}.\n",
                9000 + i
            ),
        )
        .unwrap();
        for k in 0..5 {
            activation::record(&dir, &rel, t - k * 3600).unwrap();
        }
    }
    let idx = Index::build(&[(Scope::Project, dir)], t);
    let top: Vec<String> = idx
        .search("kestrel port", t)
        .iter()
        .take(3)
        .map(|h| {
            format!(
                "{} bm25={:.2} matched={}",
                idx.docs[h.doc].id(),
                h.bm25,
                h.matched
            )
        })
        .collect();
    assert!(
        top.first()
            .is_some_and(|s| s.starts_with("project:semantic/kestrel-port.md")),
        "top 3: {top:#?}"
    );
}
