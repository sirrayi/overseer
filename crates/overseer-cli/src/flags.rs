//! `ExecFlags`: the flag set shared by exec, tui, web and consolidate.

use std::path::PathBuf;

use crate::args::{self, Arg, Flag};

pub(crate) struct ExecFlags {
    pub(crate) json: bool,
    pub(crate) resume: Option<PathBuf>,
    pub(crate) session: Option<PathBuf>,
    /// `--continue`: resume the most recent session for the cwd.
    pub(crate) cont: bool,
    /// `--last`: resume the most recent session anywhere.
    pub(crate) last: bool,
    /// `--bare`: hermetic CI mode — JSONL out, fresh session in a temp
    /// dir, no persisted rules, resume flags rejected.
    pub(crate) bare: bool,
    pub(crate) cwd: PathBuf,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) base_url: Option<String>,
    pub(crate) max_steps: u32,
    pub(crate) max_cost: f64,
    pub(crate) thinking: Option<u32>,
    pub(crate) effort: Option<overseer_core::provider::Effort>,
    pub(crate) small_model: Option<String>,
    pub(crate) heavy_model: Option<String>,
    pub(crate) full_access: bool,
    pub(crate) policy: overseer_core::perm::Preset,
    pub(crate) auto_compact: bool,
    pub(crate) compact_at: Option<f32>,
    pub(crate) keep_results: usize,
    pub(crate) verify: Option<String>,
    pub(crate) verify_cap: u32,
    pub(crate) reflect: overseer_core::agent::ReflectMode,
    /// `--best-of N`: N parallel attempts in isolated git worktrees;
    /// first attempt whose verify command exits 0 wins.
    pub(crate) best_of: u32,
    pub(crate) sandbox: bool,
    /// `--runtime <name>` (P8-C gVisor port): pin the bash sandbox backend.
    /// `None` = the platform default; a pinned runtime that is unavailable
    /// fails the bash call instead of silently running unsandboxed.
    pub(crate) runtime: Option<String>,
    pub(crate) memory: bool,
    /// `--no-memory`: no user or project store (`--bare` implies it).
    pub(crate) no_memory: bool,
    /// `--no-learn`: the memory v3 review pass never runs (`learn` off).
    pub(crate) no_learn: bool,
    /// `--learn-every <n>`: user-turn cadence for the review trigger.
    pub(crate) learn_every: u32,
    /// `--learn-stage`: review ops go to `pending/` instead of applying.
    pub(crate) learn_stage: bool,
    /// `--no-tools a,b,c` — P4.3 ablation: named tools are removed from the
    /// spec list and refused at dispatch.
    pub(crate) no_tools: Vec<String>,
    /// `--autonomy external=suggest` — P5-B per-domain autonomy overrides
    /// (repeatable). Domains: internal, external, money, identity. Levels:
    /// observe, suggest, approve (act-with-approval), report (act+report),
    /// silent (act-silently).
    pub(crate) autonomy: Vec<(String, String)>,
    /// `--credential-store env|keychain|auto` — P6-4: where secrets are
    /// read from. Auto (default) tries the OS keychain and falls back to
    /// the environment; the effective store lands in the manifest.
    pub(crate) credential_store: overseer_core::cred::CredentialStore,
    pub(crate) prompt: Option<String>,
}

/// Every flag the engine-backed commands (exec, tui, web, consolidate) take.
pub(crate) const EXEC_FLAGS: &[Flag] = &[
    Flag::switch(&["--json"]),
    Flag::switch(&["--bare"]),
    Flag::value(&["--resume"]),
    Flag::switch(&["--continue", "-c"]),
    Flag::switch(&["--last"]),
    Flag::value(&["--session"]),
    Flag::value(&["--cwd"]),
    Flag::value(&["--model"]),
    Flag::value(&["--provider"]),
    Flag::value(&["--base-url"]),
    Flag::value(&["--max-steps"]),
    Flag::value(&["--max-cost"]),
    Flag::value(&["--thinking"]),
    Flag::value(&["--effort"]),
    Flag::value(&["--small-model"]),
    Flag::value(&["--heavy-model"]),
    Flag::switch(&["--full-access"]),
    Flag::value(&["--policy"]),
    Flag::value(&["--compact-at"]),
    Flag::switch(&["--no-compact"]),
    Flag::value(&["--keep-results"]),
    Flag::value(&["--verify"]),
    Flag::value(&["--best-of"]),
    Flag::value(&["--verify-cap"]),
    Flag::value(&["--reflect"]),
    Flag::switch(&["--no-sandbox"]),
    Flag::value(&["--runtime"]),
    Flag::switch(&["--memory"]),
    Flag::switch(&["--no-memory"]),
    Flag::switch(&["--no-learn"]),
    Flag::value(&["--learn-every"]),
    Flag::switch(&["--learn-stage"]),
    Flag::value(&["--autonomy"]),
    Flag::value(&["--credential-store"]),
    Flag::value(&["--no-tools"]),
];

pub(crate) fn parse_exec(args: &[String]) -> Result<ExecFlags, String> {
    exec_from(args::parse(args, EXEC_FLAGS)?.args)
}

/// Build `ExecFlags` from already-parsed args, in order: a later value
/// overrides an earlier one, `--autonomy` accumulates, the last positional
/// is the prompt and `-` reads it from stdin.
pub(crate) fn exec_from(args: Vec<Arg>) -> Result<ExecFlags, String> {
    let mut f = ExecFlags {
        json: false,
        resume: None,
        session: None,
        cont: false,
        last: false,
        bare: false,
        cwd: std::env::current_dir().map_err(|e| e.to_string())?,
        model: "claude-sonnet-5".into(),
        provider: "anthropic".into(),
        base_url: None,
        max_steps: 100,
        max_cost: 5.0,
        thinking: None,
        effort: None,
        small_model: None,
        heavy_model: None,
        full_access: false,
        policy: overseer_core::perm::Preset::WorkspaceWrite,
        auto_compact: true,
        compact_at: None,
        keep_results: 5,
        verify: None,
        verify_cap: 8,
        reflect: overseer_core::agent::ReflectMode::Reflexion,
        best_of: 0,
        sandbox: true,
        runtime: None,
        memory: false,
        no_memory: false,
        no_learn: false,
        learn_every: 6,
        learn_stage: false,
        no_tools: Vec::new(),
        autonomy: Vec::new(),
        credential_store: overseer_core::cred::CredentialStore::Auto,
        prompt: None,
    };
    for arg in args {
        let (name, v) = match arg {
            Arg::Pos(p) if p == "-" => {
                // Read the prompt from stdin (CI-friendly).
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .map_err(|e| e.to_string())?;
                f.prompt = Some(buf);
                continue;
            }
            Arg::Pos(p) => {
                f.prompt = Some(p);
                continue;
            }
            Arg::Flag { name, value } => (name, value.unwrap_or_default()),
        };
        match name {
            "--json" => f.json = true,
            "--bare" => {
                f.bare = true;
                f.json = true;
            }
            "--resume" => f.resume = Some(PathBuf::from(&v)),
            "--continue" => f.cont = true,
            "--last" => f.last = true,
            "--session" => f.session = Some(PathBuf::from(&v)),
            "--cwd" => f.cwd = PathBuf::from(&v),
            "--model" => f.model = v.clone(),
            "--provider" => {
                crate::provider::check_provider(&v)?;
                f.provider = v.clone();
            }
            "--base-url" => f.base_url = Some(v.clone()),
            "--max-steps" => f.max_steps = v.parse().map_err(|_| "bad --max-steps")?,
            "--max-cost" => f.max_cost = v.parse().map_err(|_| "bad --max-cost")?,
            "--thinking" => f.thinking = Some(v.parse().map_err(|_| "bad --thinking")?),
            "--effort" => {
                f.effort = Some(
                    overseer_core::provider::Effort::parse(&v)
                        .ok_or("bad --effort (min|low|medium|high|max)")?,
                )
            }
            "--small-model" => f.small_model = Some(v.clone()),
            "--heavy-model" => f.heavy_model = Some(v.clone()),
            "--full-access" => f.full_access = true,
            "--policy" => {
                f.policy = match v.as_str() {
                    "workspace" => overseer_core::perm::Preset::WorkspaceWrite,
                    "readonly" => overseer_core::perm::Preset::ReadOnly,
                    "plan" => overseer_core::perm::Preset::Plan,
                    other => {
                        return Err(format!("bad --policy '{other}' (workspace|readonly|plan)"))
                    }
                }
            }
            "--compact-at" => f.compact_at = Some(v.parse().map_err(|_| "bad --compact-at")?),
            "--no-compact" => f.auto_compact = false,
            "--keep-results" => f.keep_results = v.parse().map_err(|_| "bad --keep-results")?,
            "--verify" => f.verify = Some(v.clone()),
            "--best-of" => f.best_of = v.parse().map_err(|_| "bad --best-of")?,
            "--verify-cap" => f.verify_cap = v.parse().map_err(|_| "bad --verify-cap")?,
            "--reflect" => {
                f.reflect = match v.as_str() {
                    "off" => overseer_core::agent::ReflectMode::Off,
                    "reflexion" => overseer_core::agent::ReflectMode::Reflexion,
                    other => return Err(format!("bad --reflect '{other}' (off|reflexion)")),
                }
            }
            "--no-sandbox" => f.sandbox = false,
            "--runtime" => {
                // Validate at parse time so a typo fails before a run, and
                // store the canonical spelling the core parses back.
                let rt = overseer_core::backends::SandboxRuntime::parse(&v)
                    .map_err(|e| format!("bad --runtime ({e})"))?;
                f.runtime = Some(rt.as_str().to_string());
            }
            "--memory" => f.memory = true,
            "--no-memory" => f.no_memory = true,
            "--no-learn" => f.no_learn = true,
            "--learn-every" => {
                f.learn_every = v.parse().map_err(|_| "bad --learn-every")?;
                if f.learn_every == 0 {
                    return Err("bad --learn-every (want ≥ 1)".into());
                }
            }
            "--learn-stage" => f.learn_stage = true,
            "--autonomy" => {
                let (domain, level) = v
                    .split_once('=')
                    .ok_or("bad --autonomy (want domain=level, e.g. external=suggest)")?;
                if !["internal", "external", "money", "identity"].contains(&domain) {
                    return Err(format!(
                        "bad --autonomy domain '{domain}' (internal|external|money|identity)"
                    ));
                }
                match level {
                    "observe" | "suggest" | "approve" | "report" | "silent" => {}
                    _ => {
                        return Err(format!(
                        "bad --autonomy level '{level}' (observe|suggest|approve|report|silent)"
                    ))
                    }
                }
                f.autonomy.push((domain.to_string(), level.to_string()));
            }
            "--credential-store" => {
                f.credential_store = overseer_core::cred::CredentialStore::parse(&v)
                    .map_err(|e| format!("bad --credential-store ({e})"))?
            }
            "--no-tools" => {
                for name in v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    if !overseer_core::tools::TOOL_NAMES.contains(&name) {
                        return Err(format!(
                            "unknown tool '{name}' (--no-tools expects one of: {})",
                            overseer_core::tools::TOOL_NAMES.join(",")
                        ));
                    }
                    f.no_tools.push(name.to_string());
                }
            }
            other => return Err(format!("unknown flag '{other}'")),
        }
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{agent_config, resolve_session};

    #[test]
    fn bare_implies_json_and_hermetic_session() {
        let f = parse_exec(&["--bare".into(), "do it".into()]).unwrap();
        assert!(f.bare);
        assert!(f.json, "--bare must imply --json");
        let (dir, resume) = resolve_session(&f);
        assert!(!resume, "--bare never resumes");
        // Throwaway session — never under ~/.overseer.
        assert!(
            dir.starts_with(std::env::temp_dir()),
            "bare session must live in temp: {}",
            dir.display()
        );
    }

    #[test]
    fn heavy_model_flag_reaches_agent_config() {
        let f =
            parse_exec(&["--heavy-model".into(), "claude-opus-4-8".into(), "x".into()]).unwrap();
        assert_eq!(f.heavy_model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(
            agent_config(&f).heavy_model.as_deref(),
            Some("claude-opus-4-8")
        );
    }

    #[test]
    fn bare_still_enforces_sandbox_and_omits_rules() {
        let f = parse_exec(&["--bare".into(), "x".into()]).unwrap();
        let cfg = agent_config(&f);
        assert!(cfg.sandbox_bash, "--bare must not weaken the sandbox");
        assert!(cfg.rules_path.is_none(), "--bare loads no user rules");
    }

    #[test]
    fn runtime_flag_is_validated_and_canonicalized_into_the_config() {
        // P8-C accept (gVisor `--runtime` flag): the alias parses, the
        // canonical spelling reaches the config, and a typo fails before a
        // run instead of at the first bash call.
        let f = parse_exec(&["--runtime".into(), "runsc".into(), "x".into()]).unwrap();
        assert_eq!(f.runtime.as_deref(), Some("gvisor"));
        let cfg = agent_config(&f);
        assert_eq!(cfg.sandbox_runtime.as_deref(), Some("gvisor"));
        assert!(
            cfg.sandbox_bash,
            "pinning a runtime never disables the sandbox"
        );
        // Unset stays unset: the platform default, byte-identical to before.
        let bare = agent_config(&parse_exec(&["--bare".into(), "x".into()]).unwrap());
        assert_eq!(bare.sandbox_runtime, None);
        let err = match parse_exec(&["--runtime".into(), "firecracker".into(), "x".into()]) {
            Ok(_) => panic!("a bogus --runtime must not parse"),
            Err(e) => e,
        };
        assert!(err.contains("bad --runtime"), "{err}");
        assert!(err.contains("gvisor"), "names the valid set: {err}");
    }

    #[test]
    fn learn_flags_reach_agent_config() {
        // Memory v3 §10: the review pass is on when memory is; the flags
        // gate it and steer its cadence/staging.
        let f = parse_exec(&["--no-learn".into(), "x".into()]).unwrap();
        assert!(f.no_learn);
        assert!(
            !agent_config(&f).learn,
            "--no-learn disables the review pass"
        );
        let f = parse_exec(&[
            "--learn-every".into(),
            "9".into(),
            "--learn-stage".into(),
            "x".into(),
        ])
        .unwrap();
        assert_eq!(f.learn_every, 9);
        assert!(f.learn_stage);
        let cfg = agent_config(&f);
        assert_eq!(cfg.learn_every, 9);
        assert!(cfg.learn_stage);
        for (argv, want) in [
            (
                vec!["--learn-every", "0", "x"],
                "bad --learn-every (want ≥ 1)",
            ),
            (vec!["--learn-every", "x", "y"], "bad --learn-every"),
        ] {
            match parse_exec(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>()) {
                Err(e) => assert_eq!(e, want),
                Ok(_) => panic!("{argv:?} must not parse"),
            }
        }
        // --bare is hermetic: learning is off regardless of stores.
        let bare = agent_config(&parse_exec(&["--bare".into(), "x".into()]).unwrap());
        assert!(!bare.learn);
    }
}

#[cfg(test)]
mod syntax_tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exec_flags_accept_both_value_syntaxes() {
        let a = parse_exec(&argv(&["--max-steps", "7", "--model", "m", "-c", "go"])).unwrap();
        let b = parse_exec(&argv(&["--max-steps=7", "--model=m", "--continue", "go"])).unwrap();
        for f in [&a, &b] {
            assert_eq!(f.max_steps, 7);
            assert_eq!(f.model, "m");
            assert!(f.cont);
            assert_eq!(f.prompt.as_deref(), Some("go"));
        }
    }

    #[test]
    fn exec_missing_empty_and_unknown_are_distinct() {
        let e = parse_exec(&argv(&["x", "--model"])).err().unwrap();
        assert_eq!(e, "flag '--model' needs a value");
        // An explicit empty value reaches the flag's own validation.
        let e = parse_exec(&argv(&["--max-steps=", "x"])).err().unwrap();
        assert_eq!(e, "bad --max-steps");
        let e = parse_exec(&argv(&["--nope", "x"])).err().unwrap();
        assert_eq!(e, "unknown flag '--nope'");
    }
}

#[cfg(test)]
mod provider_flag_tests {
    use super::*;

    #[test]
    fn unknown_provider_is_rejected_at_parse_time() {
        let e = parse_exec(&["--provider".into(), "bogus".into(), "x".into()])
            .err()
            .expect("an unknown --provider must not parse");
        assert_eq!(
            e,
            "unknown provider 'bogus' — expected one of: anthropic, openai, opencode, gemini"
        );
        assert!(!e.contains("OPENAI_API_KEY"), "{e}");
    }

    #[test]
    fn unknown_provider_exits_2_before_any_key_lookup() {
        let code = crate::cmd::exec::cmd_exec(&["--provider".into(), "bogus".into(), "x".into()]);
        assert_eq!(code, 2);
    }

    #[test]
    fn every_supported_provider_parses() {
        for p in ["anthropic", "openai", "opencode", "gemini"] {
            let f = parse_exec(&["--provider".into(), p.into(), "x".into()]).unwrap();
            assert_eq!(f.provider, p);
        }
        assert_eq!(parse_exec(&["x".into()]).unwrap().provider, "anthropic");
    }
}

#[cfg(test)]
mod credential_flag_tests {
    use super::*;
    use crate::session::agent_config;
    use overseer_core::cred::CredentialStore;

    #[test]
    fn credential_store_flag_parses_and_defaults_to_auto() {
        // Default: Auto — keychain first, env fallback.
        let f = parse_exec(&["x".into()]).unwrap();
        assert_eq!(f.credential_store, CredentialStore::Auto);
        assert_eq!(agent_config(&f).credential_store, CredentialStore::Auto);

        for (arg, want) in [
            ("env", CredentialStore::Env),
            ("keychain", CredentialStore::Keychain),
            ("auto", CredentialStore::Auto),
        ] {
            let f = parse_exec(&["--credential-store".into(), arg.into(), "x".into()]).unwrap();
            assert_eq!(f.credential_store, want);
            // The flag flows through to the run config (and the manifest).
            assert_eq!(agent_config(&f).credential_store, want);
        }
    }

    #[test]
    fn credential_store_flag_rejects_unknown_and_missing_value() {
        let e = parse_exec(&["--credential-store".into(), "bogus".into(), "x".into()])
            .err()
            .expect("bogus store must be rejected");
        assert!(e.contains("bad --credential-store"), "{e}");
        assert!(parse_exec(&["--credential-store".into()]).is_err());
    }

    /// P6-4: the resolved (effective) store is what the manifest records —
    /// a keychain fallback must not be reported as a keychain run.
    #[test]
    fn resolved_store_replaces_the_configured_one_on_fallback() {
        let flags =
            parse_exec(&["--credential-store".into(), "keychain".into(), "x".into()]).unwrap();
        let mut cfg = agent_config(&flags);
        assert_eq!(cfg.credential_store, CredentialStore::Keychain);
        let kc = overseer_core::cred::Keychain::new(
            Some("overseer-definitely-not-a-keychain-binary"),
            &["find-generic-password"],
            &["delete-generic-password"],
        );
        let res = overseer_core::cred::resolve_store_with(
            cfg.credential_store,
            &kc,
            kc.fetch(),
            Some("API_TOKEN=from-env".into()),
        );
        cfg.credential_store = res.store;
        assert_eq!(cfg.credential_store, CredentialStore::Env);
        // The env payload the fallback carried is usable as-is.
        let payload = overseer_core::cred::parse_payload(res.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload.secrets.len(), 1);
    }
}

#[cfg(test)]
mod autonomy_flag_tests {
    use super::*;
    use crate::session::agent_config;

    #[test]
    fn autonomy_flag_parses_domains_and_levels() {
        let f = parse_exec(&["--autonomy".into(), "external=suggest".into(), "x".into()]).unwrap();
        assert_eq!(
            f.autonomy,
            vec![("external".to_string(), "suggest".to_string())]
        );
        let f = parse_exec(&[
            "--autonomy".into(),
            "money=observe".into(),
            "--autonomy".into(),
            "identity=silent".into(),
            "x".into(),
        ])
        .unwrap();
        assert_eq!(f.autonomy.len(), 2);
    }

    #[test]
    fn autonomy_flag_rejects_bad_domain_and_level() {
        assert!(parse_exec(&["--autonomy".into(), "bogus=approve".into(), "x".into()]).is_err());
        assert!(parse_exec(&["--autonomy".into(), "external=bogus".into(), "x".into()]).is_err());
        assert!(parse_exec(&["--autonomy".into(), "external".into(), "x".into()]).is_err());
    }

    #[test]
    fn autonomy_flag_flows_into_agent_config() {
        let f = parse_exec(&["--autonomy".into(), "external=suggest".into(), "x".into()]).unwrap();
        let cfg = agent_config(&f);
        assert_eq!(
            cfg.autonomy.get("external"),
            Some(&overseer_core::perm::Autonomy::Suggest)
        );
    }

    #[test]
    fn memory_flags_resolve_the_stores() {
        use crate::session::memory_stores;
        let cwd = std::path::Path::new("/w");
        for off in [&["--bare", "x"][..], &["--no-memory", "x"]] {
            let args: Vec<String> = off.iter().map(|s| s.to_string()).collect();
            let f = parse_exec(&args).unwrap();
            assert_eq!(memory_stores(&f, cwd), (None, None), "{off:?}");
            let cfg = agent_config(&f);
            assert!(
                cfg.memory_dir.is_none() && cfg.user_memory_dir.is_none(),
                "{off:?}"
            );
        }
        let home = overseer_core::memory::overseer_home();
        let (user, project) = memory_stores(&parse_exec(&["x".into()]).unwrap(), cwd);
        assert_eq!(user.is_some(), home.is_some(), "on by default");
        assert_eq!(project.is_some(), home.is_some());
        let (_, legacy) =
            memory_stores(&parse_exec(&["--memory".into(), "x".into()]).unwrap(), cwd);
        assert_eq!(
            legacy,
            Some(cwd.join("memory")),
            "--memory keeps v1's in-workspace store"
        );
    }
}
