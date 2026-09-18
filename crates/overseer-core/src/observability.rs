//! Observability patterns ported from the prompt batch (arsenal B2).
//!
//! Two ports, both pure mappings:
//!
//! - **OTLP endpoint resolution** (phoenix's `--otlp-endpoint` flag): the
//!   exporter target a run should point at, with each known backend's default
//!   path — Phoenix, Langfuse, Jaeger, or a generic collector. `event::
//!   otel_spans` already produces the attribute map; this is the "where to
//!   send it" half, and it is deliberately data (an enum + a parser) so the
//!   CLI can resolve the flag without the engine learning about exporters.
//! - **NeMo rail → L4 map**: NeMo Guardrails classifies rails by *stage*
//!   (input/output/dialog/retrieval/execution); our gate has layers L0–L5.
//!   The map says which layer enforces an equivalent check and what our gate
//!   would do — so a rail policy can be read in our terms without adopting
//!   the runtime.
//!
//! `// DEFERRED(owner): an actual OTLP exporter (batch/retry/backpressure)
//! and NeMo rail execution — both would add a runtime this batch excludes;
//! the mappings land so the CLI flag and any future rail translation have a
//! single source of truth.`

/// Known OTLP receivers, for default path + label purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpBackend {
    Phoenix,
    Langfuse,
    Jaeger,
    /// Any other OTLP/HTTP collector.
    Generic,
}

impl OtlpBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            OtlpBackend::Phoenix => "phoenix",
            OtlpBackend::Langfuse => "langfuse",
            OtlpBackend::Jaeger => "jaeger",
            OtlpBackend::Generic => "generic",
        }
    }

    /// The traces path this receiver exposes by default.
    pub const fn traces_path(self) -> &'static str {
        match self {
            OtlpBackend::Phoenix => "/v1/traces",
            OtlpBackend::Langfuse => "/api/public/otel/v1/traces",
            OtlpBackend::Jaeger => "/v1/traces",
            OtlpBackend::Generic => "/v1/traces",
        }
    }

    /// Default local port.
    pub const fn default_port(self) -> u16 {
        match self {
            OtlpBackend::Phoenix => 6006,
            OtlpBackend::Langfuse => 3000,
            OtlpBackend::Jaeger => 4318,
            OtlpBackend::Generic => 4318,
        }
    }
}

/// A resolved exporter target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpEndpoint {
    pub url: String,
    pub backend: OtlpBackend,
}

impl OtlpEndpoint {
    /// The full traces URL for a backend's default local port.
    pub fn local(backend: OtlpBackend) -> Self {
        OtlpEndpoint {
            url: format!(
                "http://localhost:{}{}",
                backend.default_port(),
                backend.traces_path()
            ),
            backend,
        }
    }
}

/// Resolve `--otlp-endpoint <value>`. The value may be a backend name
/// (`phoenix`, `langfuse`, `jaeger`) for that backend's local default, or a
/// full `http(s)://…` URL (in which case the backend is *inferred* from the
/// URL so the manifest records what was actually targeted). Anything else is
/// an error naming the accepted forms — a silently-ignored endpoint is a run
/// whose traces go nowhere.
pub fn parse_otlp_endpoint(value: &str) -> Result<OtlpEndpoint, String> {
    let v = value.trim();
    if v.is_empty() {
        return Err("otlp: empty --otlp-endpoint — pass a backend name or a full URL".into());
    }
    let lower = v.to_ascii_lowercase();
    for backend in [
        OtlpBackend::Phoenix,
        OtlpBackend::Langfuse,
        OtlpBackend::Jaeger,
    ] {
        if lower == backend.as_str() {
            return Ok(OtlpEndpoint::local(backend));
        }
    }
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err(format!(
            "otlp: `{v}` is neither a known backend (phoenix|langfuse|jaeger) nor an \
             http(s):// URL"
        ));
    }
    if v.contains(char::is_whitespace) {
        return Err(format!("otlp: `{v}` contains whitespace — a URL cannot"));
    }
    let backend = if lower.contains(":6006") || lower.contains("phoenix") {
        OtlpBackend::Phoenix
    } else if lower.contains("/api/public/otel") || lower.contains("langfuse") {
        OtlpBackend::Langfuse
    } else if lower.contains(":4318") || lower.contains("jaeger") {
        OtlpBackend::Jaeger
    } else {
        OtlpBackend::Generic
    };
    // A bare host:port without a path gets the backend's traces path; an
    // explicit path is taken as-is (an operator may front the collector).
    let url = if v.len() - v.trim_end_matches('/').len() > 0 {
        v.trim_end_matches('/').to_string()
    } else {
        v.to_string()
    };
    let has_path = url
        .split_once("://")
        .map(|(_, rest)| rest.contains('/'))
        .unwrap_or(false);
    let url = if has_path {
        url
    } else {
        format!("{url}{}", backend.traces_path())
    };
    Ok(OtlpEndpoint { url, backend })
}

/// NeMo Guardrails rail stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailStage {
    /// Checks the user's message before the model sees it.
    Input,
    /// Checks the model's reply before the user sees it.
    Output,
    /// Mid-conversation flow checks (canonical forms, dialog rails).
    Dialog,
    /// Checks retrieved context before it is used.
    Retrieval,
    /// Checks a tool/side effect before it runs.
    Execution,
}

impl RailStage {
    pub const ALL: [RailStage; 5] = [
        RailStage::Input,
        RailStage::Output,
        RailStage::Dialog,
        RailStage::Retrieval,
        RailStage::Execution,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            RailStage::Input => "input",
            RailStage::Output => "output",
            RailStage::Dialog => "dialog",
            RailStage::Retrieval => "retrieval",
            RailStage::Execution => "execution",
        }
    }
}

/// Which of our layers enforces the equivalent check.
///
/// L0–L3 are probabilistic (prompt, lint, model self-check) and L4 is the
/// deterministic boundary; L5 is the human. The important property is that
/// nothing *side-effecting* maps below L4: a rail that guards an execution
/// must land on the deterministic gate, or it is advice rather than a
/// control.
pub fn l4_layer(stage: RailStage) -> u8 {
    match stage {
        RailStage::Input => 3,
        RailStage::Dialog => 3,
        RailStage::Retrieval => 2,
        RailStage::Output => 4,
        RailStage::Execution => 4,
    }
}

/// What our gate does for the equivalent rail: `observe` (record only),
/// `lint` (refuse to send/accept), `ask` (human), `deny`.
pub fn l4_action(stage: RailStage) -> &'static str {
    match stage {
        RailStage::Input => "lint",
        RailStage::Dialog => "lint",
        RailStage::Retrieval => "observe",
        RailStage::Output => "lint",
        RailStage::Execution => "ask",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otlp_endpoint_resolves_backends_urls_and_defaults() {
        // Backend names resolve to their local default including the path.
        let phoenix = parse_otlp_endpoint("phoenix").unwrap();
        assert_eq!(phoenix.backend, OtlpBackend::Phoenix);
        assert_eq!(phoenix.url, "http://localhost:6006/v1/traces");
        assert_eq!(
            parse_otlp_endpoint(" Langfuse ").unwrap().url,
            "http://localhost:3000/api/public/otel/v1/traces"
        );
        assert_eq!(
            parse_otlp_endpoint("jaeger").unwrap().url,
            "http://localhost:4318/v1/traces"
        );
        // A bare host:port gets the backend's traces path appended…
        let bare = parse_otlp_endpoint("http://collector.internal:6006").unwrap();
        assert_eq!(bare.backend, OtlpBackend::Phoenix);
        assert_eq!(bare.url, "http://collector.internal:6006/v1/traces");
        // …and an explicit path is taken as written.
        let explicit = parse_otlp_endpoint("https://phoenix.example.com/otel/v1/traces").unwrap();
        assert_eq!(explicit.url, "https://phoenix.example.com/otel/v1/traces");
        assert_eq!(explicit.backend, OtlpBackend::Phoenix);
        // An unknown host with no port is a generic collector.
        let generic = parse_otlp_endpoint("http://otel.internal").unwrap();
        assert_eq!(generic.backend, OtlpBackend::Generic);
        assert_eq!(generic.url, "http://otel.internal/v1/traces");
        // A trailing slash does not produce a doubled separator.
        assert_eq!(
            parse_otlp_endpoint("http://otel.internal/").unwrap().url,
            "http://otel.internal/v1/traces"
        );
    }

    #[test]
    fn otlp_endpoint_rejects_values_that_would_go_nowhere() {
        for bad in ["", "   "] {
            let err = parse_otlp_endpoint(bad).unwrap_err();
            assert!(err.contains("empty"), "{err}");
        }
        let err = parse_otlp_endpoint("datadog").unwrap_err();
        assert!(err.contains("neither a known backend"), "{err}");
        assert!(err.contains("phoenix"), "lists the backends: {err}");
        assert!(parse_otlp_endpoint("http://has space/")
            .unwrap_err()
            .contains("whitespace"));
        assert!(parse_otlp_endpoint("localhost:6006")
            .unwrap_err()
            .contains("http(s)://"));
    }

    #[test]
    fn nemo_rail_stages_map_to_our_layers_with_side_effects_at_l4() {
        assert_eq!(l4_layer(RailStage::Input), 3);
        assert_eq!(l4_layer(RailStage::Dialog), 3);
        assert_eq!(l4_layer(RailStage::Retrieval), 2);
        assert_eq!(l4_layer(RailStage::Output), 4);
        assert_eq!(l4_layer(RailStage::Execution), 4);
        // The invariant that matters: anything guarding a side effect sits
        // on the deterministic layer, and its action involves a human or a
        // refusal — never "observe".
        for stage in RailStage::ALL {
            if stage == RailStage::Execution {
                assert!(l4_layer(stage) >= 4, "{stage:?} must be gated at L4+");
                assert_ne!(l4_action(stage), "observe");
            }
            assert!(!l4_action(stage).is_empty());
            assert!(!stage.as_str().is_empty());
        }
        assert_eq!(l4_action(RailStage::Execution), "ask");
        assert_eq!(l4_action(RailStage::Output), "lint");
    }
}
