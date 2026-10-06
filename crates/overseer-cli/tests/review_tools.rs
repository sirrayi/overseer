//! Hostile-review test for `overseer onboard --approve`
//! (diff e059e02..cfc56a6). `#[ignore]`d: FAILS at cfc56a6. Run with
//! `cargo test -p overseer-cli --test review_tools -- --ignored`.

use overseer_core::onboard::PERSONA_FILES;
use std::process::Command;

/// T5: `approve_persona` verifies provenance against the files' own
/// `source_answers` claim, not against the recorded interview.
///
/// cli/cmd/onboard.rs:152-158 takes `claimed = max(source_answers)` over
/// the very files being verified, then calls `verify_trace(dir, claimed)`
/// — the docstring (onboard.rs:431-435) promises "no file may claim more
/// answers than were recorded", but nothing reads the session transcript
/// where `answer-N` records are durable (onboard.rs:498-502). A draft —
/// or a hand-edited file — that declares `source_answers: 9` forgives its
/// own `<!-- source: answer-9 -->` trailers. No interview ever ran here,
/// so the recorded count is 0 and approval must be refused.
#[test]
#[ignore = "review: T5"]
fn onboard_approve_refuses_a_self_claimed_provenance_bound() {
    let dir = std::env::temp_dir().join(format!("overseer-review-onboard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for file in PERSONA_FILES {
        let body = if file == "identity.md" {
            "---\nstatus: draft\nsource_answers: 9\n---\n\n# Identity\n\n\
             - forged insight <!-- source: answer-9 -->\n"
        } else {
            "---\nstatus: draft\nsource_answers: 0\n---\n\n# X\n"
        };
        std::fs::write(dir.join(file), body).unwrap();
    }

    let out = Command::new(env!("CARGO_BIN_EXE_overseer"))
        .arg("onboard")
        .arg("--dir")
        .arg(&dir)
        .arg("--approve")
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "approve accepted a provenance bound the files claimed themselves \
         (stdout: {} stderr: {})",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
