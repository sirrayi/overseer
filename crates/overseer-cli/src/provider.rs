//! Provider construction and API-key resolution.

use overseer_core::provider::anthropic::Anthropic;
use overseer_core::provider::openai::OpenAiCompatible;
use overseer_core::provider::Provider;

use crate::flags::ExecFlags;

/// opencode subscription (Go tier) — OpenAI-compatible chat/completions.
/// Requires the `x-opencode-session` routing header on every call.
const OPENCODE_URL: &str = "https://opencode.ai/zen/go/v1";

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Every `--provider` value [`build_provider`] can construct.
pub(crate) const PROVIDERS: &[&str] = &["anthropic", "openai", "opencode", "gemini"];

/// `Ok` for a name in [`PROVIDERS`], else the user-facing error.
pub(crate) fn check_provider(name: &str) -> Result<(), String> {
    if PROVIDERS.contains(&name) {
        Ok(())
    } else {
        Err(format!(
            "unknown provider '{name}' — expected one of: {}",
            PROVIDERS.join(", ")
        ))
    }
}

/// The env/credential names that hold `provider`'s key, in lookup order.
/// An unknown provider has none.
fn key_names(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "gemini" => &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
        "opencode" => &["OPENCODE_API_KEY"],
        _ => &[],
    }
}

/// The provider-specific names from env, then the same names in the
/// credential payload. The harness has no key of its own.
fn resolve_key(provider: &str, env: Lookup, cred: Lookup) -> Option<String> {
    let names = key_names(provider);
    names
        .iter()
        .find_map(|n| env(n))
        .or_else(|| names.iter().find_map(|n| cred(n)))
}

/// Build the provider from flags + env. The key comes from
/// [`resolve_key`]: `apply_credentials` runs first, env stays
/// authoritative, and the keychain/env payload only fills names env never
/// set.
pub(crate) fn build_provider(
    flags: &ExecFlags,
    broker: &overseer_core::cred::Broker,
) -> Result<Box<dyn Provider>, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let cred = |k: &str| broker.real_for(k).map(str::to_string);
    check_provider(&flags.provider)?;
    let key = resolve_key(&flags.provider, &env, &cred).ok_or_else(|| {
        format!(
            "no API key for provider '{}' — set {}, or store it in the \
             credential payload (--credential-store)",
            flags.provider,
            key_names(&flags.provider).join(" or ")
        )
    })?;
    Ok(match flags.provider.as_str() {
        "anthropic" => Box::new(Anthropic::new(key)),
        "openai" => Box::new(OpenAiCompatible::new(
            key,
            flags
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".into()),
        )),
        "gemini" => Box::new(overseer_core::provider::gemini::Gemini::new(
            key,
            flags
                .base_url
                .clone()
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".into()),
        )),
        "opencode" => {
            // Go routes on x-opencode-session; any stable per-process tag
            // gives session-affinity routing (verified live 2026-09).
            let tag = format!(
                "overseer-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            );
            let base = flags
                .base_url
                .clone()
                .unwrap_or_else(|| OPENCODE_URL.into());
            // muse-spark-* is Responses-API-only on Go (chat/completions
            // 500s upstream) — those models take the responses adapter.
            // The adapter choice follows --model; aux calls (consolidate,
            // reflect) reuse this provider with --small-model as the
            // request model. Warn when they disagree in transport family:
            // e.g. --model deepseek --small-model muse-* would send muse
            // to chat/completions and 500 every time.
            if let Some(sm) = &flags.small_model {
                if sm.starts_with("muse-") != flags.model.starts_with("muse-") {
                    eprintln!(
                        "overseer: warning — --small-model '{sm}' and --model '{}' \
                         use different opencode transports (Responses vs chat); \
                         aux calls may fail upstream",
                        flags.model
                    );
                }
            }
            if flags.model.starts_with("muse-") {
                Box::new(
                    overseer_core::provider::responses::ResponsesApi::new(key, base)
                        .with_header("x-opencode-session", tag),
                )
            } else {
                Box::new(OpenAiCompatible::new(key, base).with_header("x-opencode-session", tag))
            }
        }
        other => return Err(check_provider(other).unwrap_err()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| owned.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn keys_resolve_by_provider_specific_name_env_first() {
        let none = table(&[]);
        for (provider, name) in [
            ("anthropic", "ANTHROPIC_API_KEY"),
            ("openai", "OPENAI_API_KEY"),
            ("gemini", "GOOGLE_API_KEY"),
            ("gemini", "GEMINI_API_KEY"),
            ("opencode", "OPENCODE_API_KEY"),
        ] {
            let hit = table(&[(name, "k")]);
            assert_eq!(resolve_key(provider, &hit, &none).as_deref(), Some("k"));
            assert_eq!(resolve_key(provider, &none, &hit).as_deref(), Some("k"));
        }
        let env = table(&[("ANTHROPIC_API_KEY", "from-env")]);
        let cred = table(&[("ANTHROPIC_API_KEY", "from-cred")]);
        assert_eq!(
            resolve_key("anthropic", &env, &cred).as_deref(),
            Some("from-env")
        );
        let both = table(&[("GOOGLE_API_KEY", "g"), ("GEMINI_API_KEY", "m")]);
        assert_eq!(resolve_key("gemini", &both, &none).as_deref(), Some("g"));
    }

    #[test]
    fn every_supported_provider_has_its_own_key_names() {
        for (p, want) in [
            ("anthropic", &["ANTHROPIC_API_KEY"][..]),
            ("openai", &["OPENAI_API_KEY"][..]),
            ("opencode", &["OPENCODE_API_KEY"][..]),
            ("gemini", &["GOOGLE_API_KEY", "GEMINI_API_KEY"][..]),
        ] {
            assert!(PROVIDERS.contains(&p));
            assert_eq!(key_names(p), want, "{p}");
        }
        assert_eq!(PROVIDERS.len(), 4);
    }

    #[test]
    fn an_unknown_provider_borrows_no_other_providers_key() {
        assert!(key_names("bogus").is_empty());
        let openai = table(&[("OPENAI_API_KEY", "k")]);
        assert_eq!(resolve_key("bogus", &openai, &openai), None);
        assert_eq!(
            check_provider("bogus").unwrap_err(),
            "unknown provider 'bogus' — expected one of: anthropic, openai, opencode, gemini"
        );
        for p in PROVIDERS {
            assert!(check_provider(p).is_ok(), "{p}");
        }
    }

    #[test]
    fn a_project_level_key_is_not_a_provider_key() {
        for p in ["anthropic", "openai", "gemini", "opencode", "other"] {
            assert!(key_names(p).iter().all(|n| !n.starts_with("OVERSEER")));
        }
        let generic = table(&[("HARNESS_API_KEY", "harness-key")]);
        let none = table(&[]);
        assert_eq!(resolve_key("anthropic", &generic, &none), None);
        assert_eq!(resolve_key("opencode", &none, &generic), None);
        // Nor does it shadow the real one.
        let env = table(&[("HARNESS_API_KEY", "x"), ("OPENAI_API_KEY", "real")]);
        assert_eq!(resolve_key("openai", &env, &none).as_deref(), Some("real"));
    }
}
