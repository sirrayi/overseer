//! Provider construction and API-key resolution.

use overseer_core::provider::anthropic::Anthropic;
use overseer_core::provider::openai::OpenAiCompatible;
use overseer_core::provider::Provider;

use crate::flags::ExecFlags;

/// opencode subscription (Go tier) — OpenAI-compatible chat/completions.
/// Requires the `x-opencode-session` routing header on every call.
const OPENCODE_URL: &str = "https://opencode.ai/zen/go/v1";

/// Build the provider from flags + env. Key resolution order:
/// OVERSEER_API_KEY → provider-specific env, then the same names inside
/// the resolved credential payload (`apply_credentials` runs first — env
/// stays authoritative; the keychain/env payload only fills names env
/// never set).
pub(crate) fn build_provider(
    flags: &ExecFlags,
    broker: &overseer_core::cred::Broker,
) -> Result<Box<dyn Provider>, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let cred = |k: &str| broker.real_for(k).map(str::to_string);
    let provider_key = |get: &dyn Fn(&str) -> Option<String>| match flags.provider.as_str() {
        "anthropic" => get("ANTHROPIC_API_KEY"),
        "gemini" => get("GOOGLE_API_KEY").or_else(|| get("GEMINI_API_KEY")),
        "opencode" => get("OPENCODE_API_KEY"),
        _ => get("OPENAI_API_KEY"),
    };
    let key = env("OVERSEER_API_KEY")
        .or_else(|| provider_key(&env))
        .or_else(|| cred("OVERSEER_API_KEY"))
        .or_else(|| provider_key(&cred))
        .ok_or_else(|| {
            format!(
                "no API key for provider '{}' — set OVERSEER_API_KEY \
                 (or ANTHROPIC_API_KEY / OPENAI_API_KEY), or store one in \
                 the credential payload (--credential-store)",
                flags.provider
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
        other => {
            return Err(format!(
                "unknown provider '{other}' (anthropic|openai|opencode|gemini)"
            ))
        }
    })
}
