//! `overseer onboard` — the persona interview (P6-5).

use std::io::Write;
use std::path::{Path, PathBuf};

use overseer_core::agent::Agent;
use overseer_core::provider::Provider;

use crate::args::{self, Arg, Flag};
use crate::flags::parse_exec;
use crate::provider::build_provider;
use crate::session::{agent_config, apply_credentials, dirs_home};

/// `overseer onboard [--dir <d>] [--approve]` — guided persona onboarding
/// (P6-5). Without `--approve` it runs the interview over a real session,
/// records the answers in the event log (durable before anything is
/// written), then drafts the four persona files engine-side with
/// `<!-- source: answer-N -->` trailers. `--approve` trace-verifies and
/// flips every file to approved — the step that makes the persona visible
/// in the prompt and readable by tools.
pub(crate) fn cmd_onboard(args: &[String]) -> i32 {
    let flags = match parse_onboard(args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer onboard: {e}");
            return 2;
        }
    };
    let dir = match flags.dir {
        Some(d) => d,
        None => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("persona"),
    };
    if let Err(e) = overseer_core::onboard::ensure_persona_dir(&dir) {
        eprintln!("overseer onboard: {} — {e}", dir.display());
        return 1;
    }

    if flags.approve {
        return match approve_persona(&dir) {
            Ok(msg) => {
                println!("onboard: {msg}");
                0
            }
            Err(e) => {
                eprintln!("overseer onboard: {e}");
                1
            }
        };
    }

    // The onboarding session uses the default provider/key resolution (the
    // `onboard` surface takes no provider flags by design).
    let mut base_flags = match parse_exec(&[]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer onboard: {e}");
            return 2;
        }
    };
    let model =
        std::env::var("OVERSEER_ONBOARD_MODEL").unwrap_or_else(|_| "claude-sonnet-5".into());
    base_flags.model = model.clone();
    let mut cred_cfg = agent_config(&base_flags);
    apply_credentials(&mut cred_cfg);
    let provider: std::sync::Arc<dyn Provider> = match build_provider(&base_flags, &cred_cfg.broker)
    {
        Ok(p) => p.into(),
        Err(msg) => {
            eprintln!("overseer onboard: {msg}");
            return 2;
        }
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let session_dir = dirs_home().join("sessions").join(format!("onboard-{ts}"));
    let config = overseer_core::agent::AgentConfig {
        cwd: dir.parent().unwrap_or(&dir).to_path_buf(),
        model: model.clone(),
        // Drafting is engine-side; a model tool path into the draft dir
        // does not exist (and the gate would deny it anyway).
        persona_dir: Some(dir.clone()),
        disabled_tools: vec!["write".into(), "edit".into()],
        ..overseer_core::agent::AgentConfig::default()
    };
    let mut agent = match Agent::start(
        provider.clone(),
        config,
        session_dir.clone(),
        format!("onboard-{ts}"),
    ) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("overseer onboard: session {} — {e}", session_dir.display());
            return 1;
        }
    };

    let mut ask = |q: &str| -> Option<String> {
        println!("\n{q}\n");
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    };
    let mut sink = |_: &overseer_core::event::Event| {};
    let interview = match overseer_core::onboard::run_interview(&mut agent, &mut ask, &mut sink) {
        Ok(iv) => iv,
        Err(e) => {
            eprintln!("overseer onboard: {e}");
            return 1;
        }
    };
    println!(
        "onboard: {} answer(s) recorded in {}",
        interview.answers.len(),
        session_dir.display()
    );
    if interview.aborted {
        eprintln!("onboard: interview incomplete — answers kept; re-run to continue");
    }
    if interview.answers.is_empty() {
        return 1;
    }
    match overseer_core::onboard::draft_from_answers(
        provider.as_ref(),
        &model,
        &dir,
        &interview.answers,
    ) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// The `--approve` step (P6-5): trace-verify every insight against the
/// dir's own provenance claim, then flip the drafts to approved. Refuses on
/// orphans — an unsourced persona claim is exactly what the validator
/// exists to catch.
fn approve_persona(dir: &Path) -> Result<String, String> {
    let claimed = overseer_core::onboard::statuses(dir)
        .iter()
        .map(|(_, m)| m.source_answers)
        .max()
        .unwrap_or(0);
    if let Err(orphans) = overseer_core::onboard::verify_trace(dir, claimed) {
        return Err(format!(
            "refusing to approve — {} untraceable insight(s):\n  {}",
            orphans.len(),
            orphans.join("\n  ")
        ));
    }
    match overseer_core::onboard::approve(dir).map_err(|e| e.to_string())? {
        changed if changed.is_empty() => Ok(format!("already approved — {}", dir.display())),
        changed => Ok(format!(
            "approved {} file(s) in {}",
            changed.len(),
            dir.display()
        )),
    }
}

/// Parsed `overseer onboard` flags.
struct OnboardFlags {
    dir: Option<PathBuf>,
    approve: bool,
}

const ONBOARD_FLAGS: &[Flag] = &[Flag::value(&["--dir", "-d"]), Flag::switch(&["--approve"])];

fn parse_onboard(argv: &[String]) -> Result<OnboardFlags, String> {
    const WANT: &str = "want --dir <d> | --approve";
    let parsed = args::parse(argv, ONBOARD_FLAGS).map_err(|e| format!("{e} ({WANT})"))?;
    let mut f = OnboardFlags {
        dir: None,
        approve: false,
    };
    for arg in parsed.args {
        match arg {
            Arg::Flag {
                name: "--dir",
                value: Some(v),
            } => f.dir = Some(PathBuf::from(v)),
            Arg::Flag {
                name: "--approve", ..
            } => f.approve = true,
            Arg::Flag { name, .. } => return Err(format!("unknown flag '{name}' ({WANT})")),
            Arg::Pos(p) => return Err(format!("unexpected argument '{p}' ({WANT})")),
        }
    }
    Ok(f)
}

#[cfg(test)]
mod onboard_flag_tests {
    use super::*;
    use crate::flags::parse_exec;
    use crate::session::agent_config;

    #[test]
    fn onboard_parses_dir_and_approve() {
        let f = parse_onboard(&[]).unwrap();
        assert!(f.dir.is_none(), "default dir is <cwd>/persona");
        assert!(!f.approve);
        let f = parse_onboard(&["--dir".into(), "/tmp/p".into(), "--approve".into()]).unwrap();
        assert_eq!(f.dir, Some(PathBuf::from("/tmp/p")));
        assert!(f.approve);
        // Order-independent, and the short form works.
        let f = parse_onboard(&["--approve".into(), "-d".into(), "p".into()]).unwrap();
        assert!(f.approve);
        assert_eq!(f.dir, Some(PathBuf::from("p")));
    }

    #[test]
    fn onboard_rejects_unknown_flags_and_missing_dir_value() {
        assert!(parse_onboard(&["--bogus".into()]).is_err());
        assert!(parse_onboard(&["--dir".into()]).is_err());
    }

    /// P6-5 wiring: a run whose cwd has a persona dir carries it on the
    /// config, so the prompt gate and the draft gate are live.
    #[test]
    fn exec_session_picks_up_an_existing_persona_dir() {
        let dir = std::env::temp_dir().join(format!("overseer-cli-persona-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = parse_exec(&[
            "--cwd".into(),
            dir.to_string_lossy().to_string(),
            "x".into(),
        ])
        .unwrap();
        assert!(
            agent_config(&f).persona_dir.is_none(),
            "no persona dir → no gate armed"
        );
        overseer_core::onboard::ensure_persona_dir(&dir.join("persona")).unwrap();
        let cfg = agent_config(&f);
        let p = cfg
            .persona_dir
            .expect("an existing persona dir joins the session");
        assert_eq!(
            p.canonicalize().unwrap(),
            dir.join("persona").canonicalize().unwrap()
        );
        // Drafts by default → the dir is closed until approval.
        assert!(!overseer_core::onboard::all_approved(&p));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P6-5 CLI accept: `--approve` refuses to unblind a persona whose
    /// insights don't trace, then approves once they do. Exercises the CLI's
    /// own gate (`approve_persona`), not just the library beneath it.
    #[test]
    fn approve_refuses_untraceable_persona() {
        let dir = std::env::temp_dir().join(format!("overseer-cli-onboard-{}", std::process::id()));
        let persona = dir.join("persona");
        let _ = std::fs::remove_dir_all(&dir);
        overseer_core::onboard::ensure_persona_dir(&persona).unwrap();
        overseer_core::onboard::write_drafts(
            &persona,
            &[(
                "identity.md".to_string(),
                vec![overseer_core::onboard::Insight {
                    text: "engineer".to_string(),
                    source: 4,
                }],
            )],
            2,
        )
        .unwrap();
        // An insight sourced from answer-4 when 2 were recorded: refused.
        let err = approve_persona(&persona).unwrap_err();
        assert!(err.contains("refusing to approve"), "{err}");
        assert!(err.contains("identity.md:"), "{err}");
        assert!(err.contains("out of range"), "{err}");
        assert!(overseer_core::onboard::statuses(&persona)
            .iter()
            .all(|(_, m)| m.status == overseer_core::onboard::Status::Draft));

        // Repair the source, then the same call approves every file.
        overseer_core::onboard::write_drafts(
            &persona,
            &[(
                "identity.md".to_string(),
                vec![overseer_core::onboard::Insight {
                    text: "engineer".to_string(),
                    source: 2,
                }],
            )],
            2,
        )
        .unwrap();
        let msg = approve_persona(&persona).unwrap();
        assert!(msg.contains("approved 4 file(s)"), "{msg}");
        assert!(overseer_core::onboard::all_approved(&persona));
        // Idempotent: a second approve reports the already-approved state.
        assert!(approve_persona(&persona)
            .unwrap()
            .contains("already approved"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
