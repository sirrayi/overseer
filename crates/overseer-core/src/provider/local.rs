//! Local inference backends (llama.cpp / mlx-lm / outlines, arsenal B2+B3).
//!
//! `llama-server` (llama.cpp), `mlx_lm.server` (mlx-lm) and `outlines serve`
//! (P8-C) expose an OpenAI-compatible `/v1/chat/completions` endpoint on
//! localhost, so the *provider* is the existing OpenAI adapter pointed at
//! the loopback URL — no new transport, no new dependency, no CUDA build.
//! What this module adds is the naming, the default ports, and the
//! lifecycle probe.
//!
//! The outlines arm is the constrained-decoding one: `outlines serve`
//! enforces a JSON schema on generation, so `enforces_json_schema` is the
//! routing fact a structured call reads before choosing a backend (a
//! backend that only *accepts* `response_format` would return free text).
//!
//! The ollama probe is the lifecycle half: before routing to a local model,
//! ask whether the daemon is up and which models it holds. It fails open
//! (`Unavailable`) — a probe must never be the reason a turn dies — and its
//! parser is pure, so the "is it up?" decision is testable without a server.
//! `// DEFERRED(owner): managed server lifecycle (start/stop/health-watch for
//! llama-server, mlx_lm.server and outlines serve) — this batch points at an
//! already-running server; process supervision belongs with the ops surface,
//! not the engine. CLI/provider-selection wiring for the local backends
//! (`--backend outlines`, port flags) is likewise still open — the naming,
//! ports, routing facts and probe land here; the flag rides the CLI work.
//! CUDA-only stacks (TabbyAPI/EXL2, TensorRT-LLM) stay PARKed: no weights,
//! no servers, docs-only discipline.`

use std::time::Duration;

use serde_json::Value;

use super::openai::OpenAiCompatible;
use super::{Provider, ProviderError, Request, Response};

/// Which local server a `LocalProvider` talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalBackend {
    /// `llama-server` from llama.cpp.
    LlamaServer,
    /// `mlx_lm.server` — the mlx-lm OpenAI-compatible server.
    MlxLmServer,
    /// `outlines serve` (P8-C port): the structured-generation server.
    ///
    /// Outlines' `serve --model <repo> --port <port>` exposes the same
    /// OpenAI-compatible chat route, but enforces a JSON *schema* on
    /// generation via constrained decoding — so a call that needs a
    /// guaranteed-shape reply can be routed here instead of being asked
    /// for and hoped about.
    Outlines,
}

impl LocalBackend {
    pub const ALL: [LocalBackend; 3] = [
        LocalBackend::LlamaServer,
        LocalBackend::MlxLmServer,
        LocalBackend::Outlines,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            LocalBackend::LlamaServer => "llama-server",
            LocalBackend::MlxLmServer => "mlx_lm.server",
            LocalBackend::Outlines => "outlines",
        }
    }

    /// Command an operator runs to serve this backend. The flags are the
    /// port: `--port` is where the backend's endpoint is pinned, and
    /// `outlines serve` additionally takes the model repo.
    pub const fn serve_command(self) -> &'static str {
        match self {
            LocalBackend::LlamaServer => "llama-server -m <model.gguf> --port <port>",
            LocalBackend::MlxLmServer => "mlx_lm.server --model <repo> --port <port>",
            LocalBackend::Outlines => "outlines serve --model <repo> --port <port>",
        }
    }

    /// Default port each server ships with (outlines serves on 8000).
    pub const fn default_port(self) -> u16 {
        match self {
            LocalBackend::LlamaServer => 8080,
            LocalBackend::MlxLmServer => 8080,
            LocalBackend::Outlines => 8000,
        }
    }

    /// Parse a backend name as the CLI/`--backend` spelling.
    pub fn parse(s: &str) -> Result<Self, String> {
        let want = s.trim().to_ascii_lowercase().replace('_', "-");
        LocalBackend::ALL
            .into_iter()
            .find(|b| {
                let name = b.as_str().to_ascii_lowercase().replace('_', "-");
                name == want || (b == &LocalBackend::Outlines && want == "outlines-serve")
            })
            .ok_or_else(|| {
                let names: Vec<&str> = LocalBackend::ALL.iter().map(|b| b.as_str()).collect();
                format!("local backend: unknown `{s}` — want {}", names.join("|"))
            })
    }

    /// Whether the server enforces a JSON *schema* on generation.
    ///
    /// This is the routing fact that matters: `outlines serve` exists for
    /// constrained decoding, `llama-server` enforces schemas through GBNF
    /// grammars, and `mlx_lm.server` has no schema path — so a structured
    /// request must never be routed to a backend that only *accepts*
    /// `response_format` and would return free text anyway.
    pub const fn enforces_json_schema(self) -> bool {
        matches!(self, LocalBackend::LlamaServer | LocalBackend::Outlines)
    }
}

/// OpenAI-compatible base URL for a local server (`…/v1`; the adapter
/// appends `/chat/completions`). Loopback only — a "local" provider that
/// can be pointed at a remote host is a footgun, so the host is not a
/// parameter. Both supported backends serve the same shape, so the backend
/// does not enter the URL.
pub fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{}/v1", port)
}

/// A `Provider` backed by a local OpenAI-compatible server. The API key is
/// a formality these servers ignore; `none` is sent unless one is supplied.
pub struct LocalProvider {
    inner: OpenAiCompatible,
    backend: LocalBackend,
    port: u16,
}

impl LocalProvider {
    pub fn new(backend: LocalBackend, port: u16, api_key: Option<&str>) -> Self {
        let inner = OpenAiCompatible::new(api_key.unwrap_or("none"), base_url(port));
        LocalProvider {
            inner,
            backend,
            port,
        }
    }

    /// llama-server on its default port.
    pub fn llama_server(port: u16) -> Self {
        Self::new(LocalBackend::LlamaServer, port, None)
    }

    /// mlx_lm.server on the given port.
    pub fn mlx_lm_server(port: u16) -> Self {
        Self::new(LocalBackend::MlxLmServer, port, None)
    }

    /// `outlines serve` on the given port — the constrained-decoding arm.
    pub fn outlines(port: u16) -> Self {
        Self::new(LocalBackend::Outlines, port, None)
    }

    /// Whether this provider's server enforces a JSON schema on
    /// generation (see `LocalBackend::enforces_json_schema`).
    pub fn enforces_json_schema(&self) -> bool {
        self.backend.enforces_json_schema()
    }

    pub fn backend(&self) -> LocalBackend {
        self.backend
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn endpoint(&self) -> String {
        base_url(self.port)
    }
}

impl Provider for LocalProvider {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        // Same wire shape as any OpenAI-compatible gateway — that is the
        // whole point of the port (one adapter, N runtimes).
        self.inner.complete(req)
    }

    fn name(&self) -> &'static str {
        "local"
    }
}

/// Ollama lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeState {
    /// The daemon answered `/api/tags`.
    Running,
    /// No usable answer (down, wrong port, not ollama, malformed reply).
    Unavailable,
}

/// One probe result: state, the models the daemon holds, and its version
/// when `/api/version` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaProbe {
    pub state: ProbeState,
    pub models: Vec<String>,
    pub version: Option<String>,
}

impl OllamaProbe {
    fn unavailable() -> Self {
        OllamaProbe {
            state: ProbeState::Unavailable,
            models: Vec::new(),
            version: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.state == ProbeState::Running
    }

    pub fn has_model(&self, name: &str) -> bool {
        self.models.iter().any(|m| m == name)
    }
}

/// Default ollama endpoint (its own daemon port, not the server port).
pub const OLLAMA_PORT: u16 = 11_434;

/// Ollama's base URL on `port`.
pub fn ollama_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Parse `/api/tags` (and optionally `/api/version`) into a probe result.
/// Pure: the "is it up?" decision is testable without a daemon.
pub fn parse_tags(tags_body: &str, version_body: Option<&str>) -> OllamaProbe {
    let Ok(v) = serde_json::from_str::<Value>(tags_body) else {
        return OllamaProbe::unavailable();
    };
    let Some(list) = v.get("models").and_then(Value::as_array) else {
        return OllamaProbe::unavailable();
    };
    let models = list
        .iter()
        .filter_map(|m| {
            // `name` is the documented field; `model` is the older spelling.
            m.get("name")
                .or_else(|| m.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let version = version_body
        .and_then(|b| serde_json::from_str::<Value>(b).ok())
        .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string));
    OllamaProbe {
        state: ProbeState::Running,
        models,
        version,
    }
}

/// Ask a live daemon. Every failure mode (connection refused, timeout,
/// garbage) is `Unavailable` — the probe reports, it never propagates.
pub fn probe_ollama(port: u16) -> OllamaProbe {
    let get = |path: &str| -> Option<String> {
        let url = format!("{}{path}", ollama_base_url(port));
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(1_500)))
            .build();
        let agent = ureq::Agent::new_with_config(agent);
        match agent.get(&url).call() {
            Ok(mut resp) => resp.body_mut().read_to_string().ok(),
            Err(_) => None,
        }
    };
    match get("/api/tags") {
        Some(tags) => parse_tags(&tags, get("/api/version").as_deref()),
        None => OllamaProbe::unavailable(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Message;

    #[test]
    fn backends_map_to_loopback_openai_urls() {
        assert_eq!(base_url(8080), "http://127.0.0.1:8080/v1");
        assert_eq!(base_url(9000), "http://127.0.0.1:9000/v1");
        let p = LocalProvider::llama_server(8081);
        assert_eq!(p.name(), "local");
        assert_eq!(p.backend(), LocalBackend::LlamaServer);
        assert_eq!(p.port(), 8081);
        assert_eq!(p.endpoint(), "http://127.0.0.1:8081/v1");
        let m = LocalProvider::mlx_lm_server(8082);
        assert_eq!(m.backend().as_str(), "mlx_lm.server");
        assert!(LocalBackend::MlxLmServer
            .serve_command()
            .contains("mlx_lm.server"));
        assert_eq!(LocalBackend::default_port(LocalBackend::LlamaServer), 8080);
    }

    #[test]
    fn outlines_is_the_constrained_decoding_arm() {
        // P8-C accept: the outlines backend is named, ported, and carries
        // the routing fact a structured call reads.
        let o = LocalProvider::outlines(8000);
        assert_eq!(o.backend().as_str(), "outlines");
        assert_eq!(o.port(), 8000);
        assert_eq!(o.endpoint(), "http://127.0.0.1:8000/v1");
        assert_eq!(LocalBackend::Outlines.default_port(), 8000);
        assert!(LocalBackend::Outlines
            .serve_command()
            .starts_with("outlines serve --model"));
        assert!(LocalBackend::Outlines.serve_command().contains("--port"));
        assert!(o.enforces_json_schema(), "outlines constrains the decode");
        assert!(LocalProvider::llama_server(8080).enforces_json_schema());
        assert!(
            !LocalProvider::mlx_lm_server(8080).enforces_json_schema(),
            "mlx_lm.server has no schema path: a structured call must not \
             be routed there on the assumption that it does"
        );
        // Parsing accepts the CLI spellings and refuses an unknown name.
        assert_eq!(
            LocalBackend::parse("outlines").unwrap(),
            LocalBackend::Outlines
        );
        assert_eq!(
            LocalBackend::parse("Llama-Server").unwrap(),
            LocalBackend::LlamaServer
        );
        assert_eq!(
            LocalBackend::parse("mlx_lm.server").unwrap(),
            LocalBackend::MlxLmServer
        );
        let err = LocalBackend::parse("vllm").unwrap_err();
        assert!(err.contains("vllm"), "{err}");
        assert!(
            err.contains("outlines"),
            "the error names the valid set: {err}"
        );
        assert_eq!(LocalBackend::ALL.len(), 3);
    }

    #[test]
    fn local_provider_delegates_the_wire_and_surfaces_transport_errors() {
        // Nothing is listening on this port: the delegation must return a
        // transport error, never panic and never fabricate a response.
        let p = LocalProvider::new(LocalBackend::LlamaServer, 1, None);
        let msgs = [Message::user_text("hi")];
        let req = Request {
            model: "local-model",
            system: &[],
            tools: &[],
            messages: &msgs,
            max_tokens: 16,
            thinking_budget: None,
            effort: None,
            cache_breakpoints: false,
        };
        assert!(
            p.complete(&req).is_err(),
            "a closed port must be an error, not a response"
        );
    }

    #[test]
    fn ollama_probe_parses_lifecycle_and_fails_open() {
        let tags = r#"{"models":[
            {"name":"llama3:8b","size":4700000000},
            {"model":"qwen3:4b"}
        ]}"#;
        let up = parse_tags(tags, Some(r#"{"version":"0.3.5"}"#));
        assert_eq!(up.state, ProbeState::Running);
        assert!(up.is_running());
        assert!(up.has_model("llama3:8b"));
        assert!(up.has_model("qwen3:4b"), "the older `model` spelling works");
        assert!(!up.has_model("nope:1b"));
        assert_eq!(up.version.as_deref(), Some("0.3.5"));
        // A running daemon with no models is still running.
        let empty = parse_tags(r#"{"models":[]}"#, None);
        assert_eq!(empty.state, ProbeState::Running);
        assert!(empty.models.is_empty());
        assert_eq!(empty.version, None, "a missing version is not an error");
        // Garbage, wrong shape, and a non-daemon reply all read as down.
        for bad in ["", "not json", "{}", r#"{"models":"nope"}"#, "null"] {
            let p = parse_tags(bad, Some("junk"));
            assert_eq!(p.state, ProbeState::Unavailable, "input: {bad:?}");
            assert!(p.models.is_empty());
            assert!(!p.is_running());
        }
        assert_eq!(ollama_base_url(OLLAMA_PORT), "http://127.0.0.1:11434");
    }

    #[test]
    fn probing_a_closed_port_reports_unavailable() {
        // Port 1 has nothing on it: the probe must report, not panic.
        let p = probe_ollama(1);
        assert_eq!(p.state, ProbeState::Unavailable);
    }
}
