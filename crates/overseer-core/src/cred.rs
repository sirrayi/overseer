//! Credential broker (P6-3, playbook Ch.11 §5.5 lean spine).
//!
//! The broker holds real secrets process-side and hands the model only
//! opaque sentinels (`ovsent_` + 32 lowercase hex — frozen contract shared
//! with P7's channel sentinel). Real values are injected into `bash`
//! children as env vars; every `ToolResult` text is verbatim-sanitized
//! back to sentinels AFTER the taint latch sees the raw text, so the
//! latch, the model context, AND `events.jsonl` all see the safe form by
//! construction (the event append copies the same sanitized string the
//! model reads).
//!
//! Reals are never serialized: `Credential::real` has no Serialize path,
//! `Broker` has a redacting Debug, and scans redact spills before write.
//!
//! P6-4 adds the OAuth *shape* (no redirect server): consent `Grant`s
//! recorded for audit, a vault kind per credential, and the credential
//! store — the OS
//! keychain through a subprocess (one entry, prefetched) with an honest
//! env fallback that also clears a stale entry.

use std::collections::HashMap;

/// Sentinel prefix — frozen contract (both branches, R2-F6).
pub const SENTINEL_PREFIX: &str = "ovsent_";
/// Hex body length after the prefix.
pub const SENTINEL_HEX_LEN: usize = 32;

/// The single env payload variable (P6-4): one variable carrying the same
/// grammar as the keychain entry, so the two stores are interchangeable.
pub const ENV_PAYLOAD_VAR: &str = "OVERSEER_CREDENTIALS";
/// Keychain service name — the single-entry contract.
pub const KEYCHAIN_SERVICE: &str = "overseer";
/// Keychain account name within the service.
pub const KEYCHAIN_ACCOUNT: &str = "overseer";

/// Kind of secret held in the vault (P6-4, playbook Ch.11 §5.5): a
/// password, an OAuth access token, or an opaque session handle
/// (browser/session credential). Provenance for the manifest and for the
/// frontend's handle bridge — the gate itself keys on scopes, not kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VaultKind {
    #[default]
    Password,
    OAuthToken,
    SessionHandle,
}

impl VaultKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VaultKind::Password => "password",
            VaultKind::OAuthToken => "oauth_token",
            VaultKind::SessionHandle => "session_handle",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "password" => Ok(VaultKind::Password),
            "oauth_token" | "oauth" => Ok(VaultKind::OAuthToken),
            "session_handle" | "session" => Ok(VaultKind::SessionHandle),
            other => Err(format!(
                "cred: bad vault kind `{other}` — want password|oauth_token|session_handle"
            )),
        }
    }
}

/// One brokered credential: the selector names the env var the child
/// sees; the sentinel is what the model sees; `real` never leaves the
/// process except into a spawned child's env.
#[derive(Clone)]
pub struct Credential {
    pub id: String,
    pub selector: String,
    pub sentinel: String,
    pub inject_hosts: Vec<String>,
    pub scopes: Vec<String>,
    pub expires_ms: Option<u64>,
    /// P6-4: what kind of secret this is.
    pub kind: VaultKind,
    real: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Reals never render — sentinel only.
        f.debug_struct("Credential")
            .field("id", &self.id)
            .field("selector", &self.selector)
            .field("sentinel", &self.sentinel)
            .field("kind", &self.kind)
            .field("real", &"[redacted]")
            .finish()
    }
}

impl Credential {
    /// Build a credential, deriving its sentinel from (id, real) via
    /// `sentinel_for` (keyed HMAC, first 32 hex chars). Same (id, real) →
    /// same sentinel within the process (1:1 mapping the tests pin).
    pub fn new(
        id: impl Into<String>,
        selector: impl Into<String>,
        real: impl Into<String>,
        inject_hosts: Vec<String>,
        scopes: Vec<String>,
        expires_ms: Option<u64>,
    ) -> Self {
        let id = id.into();
        let real = real.into();
        let sentinel = sentinel_for(&id, &real);
        Credential {
            id,
            selector: selector.into(),
            sentinel,
            inject_hosts,
            scopes,
            expires_ms,
            kind: VaultKind::Password,
            real,
        }
    }

    /// P6-4: declare the vault kind at construction.
    pub fn with_kind(mut self, kind: VaultKind) -> Self {
        self.kind = kind;
        self
    }

    /// Server-side only: the real secret. Marker-named so call sites
    /// read as privileged (`real_for` mirrors it on the broker).
    pub fn real_secret(&self) -> &str {
        &self.real
    }
}

/// Sentinel for (id, real): HMAC-SHA256 under the per-process key, first
/// 32 hex chars. Stable for the life of the process (so sentinel↔real
/// mapping holds across brokers and turns); a fresh process draws a fresh
/// key, so a low-entropy real cannot be dictionary-recovered offline.
pub fn sentinel_for(id: &str, real: &str) -> String {
    sentinel_with_key(sentinel_key(), id, real)
}

fn sentinel_with_key(key: &[u8], id: &str, real: &str) -> String {
    let mut msg = Vec::with_capacity(id.len() + real.len() + 18);
    msg.extend_from_slice(b"overseer-cred-v2:");
    msg.extend_from_slice(id.as_bytes());
    msg.push(b':');
    msg.extend_from_slice(real.as_bytes());
    let hex: String = hmac_sha256(key, &msg)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{SENTINEL_PREFIX}{}", &hex[..SENTINEL_HEX_LEN])
}

/// The per-process sentinel key: 32 bytes from `/dev/urandom`, falling
/// back to two v7 UUIDs (74 random bits each) where that device is absent.
fn sentinel_key() -> &'static [u8; 32] {
    static KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        use std::io::Read;
        let mut k = [0u8; 32];
        let urandom = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut k));
        if urandom.is_err() {
            k[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
            k[16..].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
        }
        k
    })
}

/// HMAC-SHA256 (RFC 2104) over `sha2`.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| k.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// True when `s` is exactly a sentinel (`ovsent_` + 32 lowercase hex).
pub fn is_sentinel(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != SENTINEL_PREFIX.len() + SENTINEL_HEX_LEN {
        return false;
    }
    if !b.starts_with(SENTINEL_PREFIX.as_bytes()) {
        return false;
    }
    b[SENTINEL_PREFIX.len()..]
        .iter()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// The broker: id → credential, plus the P6-4 consent grants. Default-empty;
/// cloned into tool contexts that need injection/sanitization.
#[derive(Default, Clone)]
pub struct Broker {
    map: HashMap<String, Credential>,
    /// Issued consent grants — the OAuth shape's audit record.
    grants: GrantBook,
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Reals never render: ids + sentinels only.
        let ids: Vec<(&str, &str)> = self
            .map
            .values()
            .map(|c| (c.id.as_str(), c.sentinel.as_str()))
            .collect();
        f.debug_struct("Broker")
            .field("creds", &ids)
            .field("grants", &self.grants.len())
            .finish()
    }
}

impl Broker {
    pub fn new() -> Self {
        Broker {
            map: HashMap::new(),
            grants: GrantBook::new(),
        }
    }

    /// Register a credential; returns its sentinel (the only value the
    /// model ever sees).
    pub fn issue_capability(
        &mut self,
        id: impl Into<String>,
        selector: impl Into<String>,
        real: impl Into<String>,
        inject_hosts: Vec<String>,
        scopes: Vec<String>,
        expires_ms: Option<u64>,
    ) -> String {
        self.issue_vault(
            id,
            selector,
            real,
            VaultKind::Password,
            inject_hosts,
            scopes,
            expires_ms,
        )
    }

    /// Same as `issue_capability` with the vault kind declared (P6-4).
    #[allow(clippy::too_many_arguments)]
    pub fn issue_vault(
        &mut self,
        id: impl Into<String>,
        selector: impl Into<String>,
        real: impl Into<String>,
        kind: VaultKind,
        inject_hosts: Vec<String>,
        scopes: Vec<String>,
        expires_ms: Option<u64>,
    ) -> String {
        let cred =
            Credential::new(id, selector, real, inject_hosts, scopes, expires_ms).with_kind(kind);
        let s = cred.sentinel.clone();
        self.map.insert(cred.id.clone(), cred);
        s
    }

    /// Install a parsed payload (P6-4): secrets become sentinel-mapped
    /// capabilities (selector = key), grants join the grant book. Returns
    /// (secrets, grants) installed. Reals never leave the process.
    pub fn install_payload(&mut self, payload: &Payload) -> (usize, usize) {
        for (k, v) in &payload.secrets {
            // Payload grammar v1 declares no hosts/scopes per secret — a
            // `grant` line carries the scope authority instead.
            self.issue_capability(
                k.clone(),
                k.clone(),
                v.clone(),
                Vec::new(),
                Vec::new(),
                None,
            );
        }
        for g in &payload.grants {
            self.grants.add(g.clone());
        }
        (payload.secrets.len(), payload.grants.len())
    }

    /// The issued consent grants (manifest/audit — no secret material).
    pub fn grants(&self) -> &[Grant] {
        self.grants.grants()
    }

    /// The issued-grant book (audit record; no enforcement state).
    pub fn grant_book(&self) -> &GrantBook {
        &self.grants
    }

    pub fn grant_book_mut(&mut self) -> &mut GrantBook {
        &mut self.grants
    }

    /// (selector, real) env pairs for bash child injection.
    pub fn inject_env(&self) -> Vec<(String, String)> {
        self.map
            .values()
            .map(|c| (c.selector.clone(), c.real_secret().to_string()))
            .collect()
    }

    /// Server-side only: real secret for `id`. Call sites are the bash
    /// injector and the sanitizer — never the model path.
    pub fn real_for(&self, id: &str) -> Option<&str> {
        self.map.get(id).map(|c| c.real_secret())
    }

    /// True when `cmd` mentions any registered selector (`$TOKEN`,
    /// `${TOKEN}`, or bare) — the command could exfil the secret (RT-4).
    /// Selectors are operator-declared env names; substring match is the
    /// documented conservative rule (same rigor as EXTERNAL_MARKERS).
    pub fn mentions_selector(&self, cmd: &str) -> bool {
        self.map.values().any(|c| {
            let s = c.selector.as_str();
            !s.is_empty() && (cmd.contains(s) || cmd.contains(&format!("${s}")))
        })
    }

    pub fn get(&self, id: &str) -> Option<&Credential> {
        self.map.get(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// (real, sentinel) pairs for sanitize/inject loops.
    pub fn pairs(&self) -> Vec<(&str, &str)> {
        self.map
            .values()
            .map(|c| (c.real_secret(), c.sentinel.as_str()))
            .collect()
    }
}

/// One user consent grant (P6-4; playbook Ch.11 §5.5 OAuth shape without
/// a redirect server): `client` may exercise `scopes` until `expires_ms`.
/// `approved_by` names the human (or policy) that granted it — never the
/// model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub client: String,
    pub scopes: Vec<String>,
    /// Absolute expiry, ms since the unix epoch; 0 = no expiry.
    pub expires_ms: u64,
    /// Acting principal the grant was issued to.
    pub actor: String,
    /// Who approved it.
    pub approved_by: String,
}

/// Issued consent grants: recorded into manifest.json and emitted as
/// `ConsentGranted` at session start.
// DEFERRED(owner): grants are recorded for audit (manifest + ConsentGranted) but rate/window enforcement is unwired — restore from git history and wire into perm::Policy::gate when needed
#[derive(Debug, Clone, Default)]
pub struct GrantBook {
    grants: Vec<Grant>,
}

impl GrantBook {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    pub fn len(&self) -> usize {
        self.grants.len()
    }

    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    pub fn add(&mut self, grant: Grant) {
        self.grants.push(grant);
    }
}

/// Configured credential store (P6-4). `Auto` is the default: try the OS
/// keychain, fall back to the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CredentialStore {
    /// Environment only — never spawns a keychain backend (CI, containers).
    Env,
    /// OS keychain via subprocess, with the env as the fallback when no
    /// backend exists (so headless containers still work).
    Keychain,
    /// Keychain first, env fallback. The manifest records whichever store
    /// actually held the secret.
    #[default]
    Auto,
}

impl CredentialStore {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialStore::Env => "env",
            CredentialStore::Keychain => "keychain",
            CredentialStore::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "env" => Ok(CredentialStore::Env),
            "keychain" => Ok(CredentialStore::Keychain),
            "auto" => Ok(CredentialStore::Auto),
            other => Err(format!(
                "cred: bad credential store `{other}` — want env|keychain|auto"
            )),
        }
    }
}

/// Parsed credential payload: secrets + grants. One payload = one store
/// entry (the keychain holds exactly one; the env var mirrors it).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Payload {
    pub secrets: Vec<(String, String)>,
    pub grants: Vec<Grant>,
}

impl Payload {
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty() && self.grants.is_empty()
    }
}

/// Parse the payload grammar (line-based; `#` comments and blanks skipped):
///
/// ```text
/// API_TOKEN=tok-...                          one secret → selector API_TOKEN
/// grant gh read,repo 0 user user             client, scopes, expires_ms, actor, approved_by
/// ```
///
/// Errors name the line *number* only — a malformed line may itself be
/// secret material, so it is never echoed back.
pub fn parse_payload(text: &str) -> Result<Payload, String> {
    let mut out = Payload::default();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let n = i + 1;
        if let Some(rest) = line.strip_prefix("grant ") {
            let f: Vec<&str> = rest.split_whitespace().collect();
            if f.len() != 5 {
                return Err(format!(
                    "cred: payload line {n} — grant wants `grant <client> <scopes> \
                     <expires_ms> <actor> <approved_by>`"
                ));
            }
            let expires_ms = f[2]
                .parse::<u64>()
                .map_err(|_| format!("cred: payload line {n} — bad expires_ms"))?;
            out.grants.push(Grant {
                client: f[0].to_string(),
                scopes: f[1]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                expires_ms,
                actor: f[3].to_string(),
                approved_by: f[4].to_string(),
            });
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!(
                "cred: payload line {n} — want `NAME=value` or `grant …`"
            ));
        };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() || v.is_empty() {
            return Err(format!("cred: payload line {n} — empty name or value"));
        }
        out.secrets.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

/// The single env payload (`OVERSEER_CREDENTIALS`), read once per session
/// so it can be fetched while the keychain subprocess is still running.
pub fn env_payload() -> Option<String> {
    std::env::var(ENV_PAYLOAD_VAR)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// What the OS keychain said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeychainOutcome {
    /// Entry read (raw payload).
    Found(String),
    /// The backend ran and holds no entry for service/account.
    Missing,
    /// No usable backend (binary absent, spawn failed) — fall back.
    Unavailable,
}

/// The OS keychain, driven through a subprocess (zero new deps): macOS
/// `security`, Linux `secret-tool`. Exactly ONE entry per service/account,
/// so the keychain never becomes a shadow credential database.
#[derive(Debug, Clone)]
pub struct Keychain {
    /// Backend binary; None = no backend on this platform.
    program: Option<String>,
    /// argv templates for read/delete; `{service}`/`{account}` substitute.
    find_args: Vec<String>,
    delete_args: Vec<String>,
    service: String,
    account: String,
}

impl Keychain {
    /// Backend spec (tests inject a fake or a missing binary).
    pub fn new(program: Option<&str>, find_args: &[&str], delete_args: &[&str]) -> Self {
        Keychain {
            program: program.map(str::to_string),
            find_args: find_args.iter().map(|s| s.to_string()).collect(),
            delete_args: delete_args.iter().map(|s| s.to_string()).collect(),
            service: KEYCHAIN_SERVICE.to_string(),
            account: KEYCHAIN_ACCOUNT.to_string(),
        }
    }

    /// The platform backend; a `None` program on other platforms reads as
    /// `Unavailable`, which the resolver turns into the env fallback.
    pub fn detect() -> Self {
        #[cfg(target_os = "macos")]
        {
            Keychain::new(
                Some("security"),
                &[
                    "find-generic-password",
                    "-s",
                    "{service}",
                    "-a",
                    "{account}",
                    "-w",
                ],
                &[
                    "delete-generic-password",
                    "-s",
                    "{service}",
                    "-a",
                    "{account}",
                ],
            )
        }
        #[cfg(target_os = "linux")]
        {
            Keychain::new(
                Some("secret-tool"),
                &["lookup", "service", "{service}", "account", "{account}"],
                &["clear", "service", "{service}", "account", "{account}"],
            )
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Keychain::new(None, &[], &[])
        }
    }

    /// The backend binary name (audit notes; None when unsupported).
    pub fn program_name(&self) -> Option<&str> {
        self.program.as_deref()
    }

    /// The single-entry contract: the (service, account) this backend
    /// reads and clears.
    pub fn entry(&self) -> (&str, &str) {
        (&self.service, &self.account)
    }

    fn run(&self, template: &[String]) -> Option<std::process::Output> {
        let program = self.program.as_deref()?;
        let args: Vec<String> = template
            .iter()
            .map(|a| {
                a.replace("{service}", &self.service)
                    .replace("{account}", &self.account)
            })
            .collect();
        std::process::Command::new(program)
            .args(&args)
            .output()
            .ok()
    }

    /// Read the single entry.
    pub fn fetch(&self) -> KeychainOutcome {
        let Some(out) = self.run(&self.find_args) else {
            return KeychainOutcome::Unavailable;
        };
        if !out.status.success() {
            return KeychainOutcome::Missing;
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if text.is_empty() {
            KeychainOutcome::Missing
        } else {
            KeychainOutcome::Found(text)
        }
    }

    /// Best-effort delete of the single entry; true when the backend ran
    /// and succeeded. Used to clear a stale entry on the env fallback.
    pub fn delete(&self) -> bool {
        self.run(&self.delete_args)
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Start the read now, on its own thread (P6-4 prefetch): the
    /// subprocess spawn overlaps session bootstrap; `resolve()` joins.
    pub fn prefetch(&self) -> Prefetch {
        let kc = self.clone();
        Prefetch {
            handle: Some(std::thread::spawn(move || kc.fetch())),
        }
    }
}

/// A keychain read already in flight.
pub struct Prefetch {
    handle: Option<std::thread::JoinHandle<KeychainOutcome>>,
}

impl Prefetch {
    /// Join the read. A failed thread reads as `Unavailable` — fail to the
    /// env fallback, never to a stale claim.
    pub fn resolve(mut self) -> KeychainOutcome {
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or(KeychainOutcome::Unavailable)
    }
}

/// Where the secrets actually came from (P6-4) — what the manifest
/// records, so a fallback shows up in the provenance trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreResolution {
    /// Store actually used (`Env` whenever the keychain didn't deliver).
    pub store: CredentialStore,
    /// Raw payload, when the resolved store holds one.
    pub payload: Option<String>,
    /// One-line audit note; "ok" when no fallback happened.
    pub note: String,
}

/// Resolve the effective store. `outcome` is the (possibly prefetched)
/// keychain answer; the env payload is supplied by the caller (it is read
/// during the prefetch overlap, and tests pin it without touching process
/// env).
///
/// `Env` never touches the keychain. `Keychain`/`Auto` without a usable
/// entry fall back to the env — and a *stale* entry (corrupt, or holding
/// nothing parseable) is deleted first, so the next run doesn't inherit it.
pub fn resolve_store_with(
    store: CredentialStore,
    kc: &Keychain,
    outcome: KeychainOutcome,
    env: Option<String>,
) -> StoreResolution {
    if store == CredentialStore::Env {
        return StoreResolution {
            store: CredentialStore::Env,
            payload: env,
            note: "ok".to_string(),
        };
    }
    match outcome {
        KeychainOutcome::Found(text) => match parse_payload(&text) {
            Ok(p) if !p.is_empty() => StoreResolution {
                store: CredentialStore::Keychain,
                payload: Some(text),
                note: "ok".to_string(),
            },
            // Corrupt or emptied: stale. Clear it, then fall back.
            _ => {
                let cleared = kc.delete();
                StoreResolution {
                    store: CredentialStore::Env,
                    payload: env,
                    note: format!(
                        "stale keychain entry{} — using env",
                        if cleared {
                            " cleared"
                        } else {
                            " (delete failed)"
                        }
                    ),
                }
            }
        },
        KeychainOutcome::Missing => StoreResolution {
            store: CredentialStore::Env,
            payload: env,
            note: "no keychain entry — using env".to_string(),
        },
        KeychainOutcome::Unavailable => StoreResolution {
            store: CredentialStore::Env,
            payload: env,
            note: format!(
                "keychain backend '{}' unavailable — fell back to env",
                kc.program_name().unwrap_or("(none)")
            ),
        },
    }
}

/// Verbatim real→sentinel rewrite over tool-result text. Longest reals
/// first (a real that contains another still maps 1:1). Pure string
/// replace — no regex, no allocation beyond the result.
pub fn sanitize(broker: &Broker, text: &str) -> String {
    let mut pairs = broker.pairs();
    pairs.sort_by_key(|(real, _)| std::cmp::Reverse(real.len()));
    let mut out = text.to_string();
    for (real, sentinel) in pairs {
        if real.is_empty() {
            continue;
        }
        if out.contains(real) {
            out = out.replace(real, sentinel);
        }
    }
    out
}

/// One redaction span: byte range + family name. Span-only (no content
/// copied into the notice/metadata log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub start: usize,
    pub end: usize,
    pub family: &'static str,
}

/// Curated secret families (regex-lite hand-rolls, no new deps):
/// `AKIA…` keys, `gh[ps]…`/`github_pat_` tokens, `xox[bpas]-` Slack
/// tokens, `sk-live-`/`sk-test-` keys, PEM blocks, plus high-entropy
/// long tokens (≥20 chars, entropy ≥ 3.5). FP allowlist: `EXAMPLE`,
/// `TEST-ONLY`, `...`, `xxx` (case-insensitive) suppress a span.
pub fn scan(text: &str) -> Vec<Redaction> {
    let b = text.as_bytes();
    let mut spans: Vec<Redaction> = Vec::new();
    macro_rules! push {
        ($start:expr, $end:expr, $family:expr) => {{
            let (s, e): (usize, usize) = ($start, $end);
            if e > s
                && e <= text.len()
                && text.is_char_boundary(s)
                && text.is_char_boundary(e)
                && !is_fp(&text[s..e])
            {
                spans.push(Redaction {
                    start: s,
                    end: e,
                    family: $family,
                });
            }
        }};
    }
    // AKIA + 16 uppercase alnum. Byte-indexed (F1): ASCII patterns only
    // match at char boundaries, so pushed spans are always valid str slices.
    for i in 0..(b.len() + 1).saturating_sub(20) {
        if &b[i..i + 4] == b"AKIA"
            && b[i + 4..i + 20]
                .iter()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            push!(i, i + 20, "aws-key");
        }
    }
    // ghp_/gho_/ghs_/ghu_/github_pat_.
    for pat in ["ghp_", "gho_", "ghs_", "ghu_", "github_pat_"] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(pat) {
            let s = from + rel;
            let mut e = s + pat.len();
            while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'_') && e - s < 120 {
                e += 1;
            }
            if e - s >= pat.len() + 8 {
                push!(s, e, "github-token");
            }
            from = e.max(s + 1);
            if from >= b.len() {
                break;
            }
        }
    }
    // xox[bpas]- + token body.
    for pat in ["xoxb-", "xoxp-", "xoxa-", "xoxs-"] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(pat) {
            let s = from + rel;
            let mut e = s + pat.len();
            while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'-') && e - s < 120 {
                e += 1;
            }
            if e - s >= pat.len() + 8 {
                push!(s, e, "slack-token");
            }
            from = e.max(s + 1);
            if from >= b.len() {
                break;
            }
        }
    }
    // sk-live- / sk-test- + body.
    for pat in ["sk-live-", "sk-test-"] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(pat) {
            let s = from + rel;
            let mut e = s + pat.len();
            while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'-' || b[e] == b'_') {
                e += 1;
            }
            if e - s >= pat.len() + 8 {
                push!(s, e, "api-key");
            }
            from = e.max(s + 1);
            if from >= b.len() {
                break;
            }
        }
    }
    // PEM blocks: -----BEGIN ...----- … -----END ...-----.
    let mut from = 0;
    while let Some(rel) = text[from..].find("-----BEGIN") {
        let s = from + rel;
        if let Some(end_rel) = text[s..].find("-----END") {
            let tail = &text[s + end_rel..];
            let line_end = tail
                .find('\n')
                .map(|n| s + end_rel + n)
                .unwrap_or(text.len());
            let e = (s + end_rel + "-----END".len()).max(line_end.min(text.len()));
            let e = e.min(text.len());
            push!(s, e.max(s + 10), "pem-block");
            from = e.max(s + 1);
        } else {
            // F1: s+64 can land mid-char — floor to the boundary.
            let mut e = text.len().min(s + 64);
            while e > s && !text.is_char_boundary(e) {
                e -= 1;
            }
            push!(s, e.max(s + 10).min(text.len()), "pem-block");
            break;
        }
        if from >= b.len() {
            break;
        }
    }
    // High-entropy long tokens: ≥20 alnum/+/=/_/- chars with entropy ≥3.5.
    let mut i = 0;
    while i < b.len() {
        if is_token_char(b[i]) {
            let s = i;
            while i < b.len() && is_token_char(b[i]) {
                i += 1;
            }
            // Broker sentinels are already the redacted form: scan only the
            // pieces of the token around them.
            let mut pieces = Vec::new();
            let (mut seg, mut k) = (s, s);
            while let Some(rel) = text[k..i].find(SENTINEL_PREFIX) {
                let at = k + rel;
                let end = at + SENTINEL_PREFIX.len() + SENTINEL_HEX_LEN;
                if end <= i && is_sentinel(&text[at..end]) {
                    pieces.push((seg, at));
                    seg = end;
                    k = end;
                } else {
                    k = at + SENTINEL_PREFIX.len();
                }
            }
            pieces.push((seg, i));
            for (ps, pe) in pieces {
                if pe - ps >= 20
                    && !spans.iter().any(|r| r.start <= ps && ps < r.end)
                    && shannon(&text[ps..pe]) >= 3.5
                {
                    push!(ps, pe, "high-entropy");
                }
            }
        } else {
            i += 1;
        }
    }
    // Sort + drop spans fully contained in an earlier (longer) span.
    spans.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
    let mut out: Vec<Redaction> = Vec::new();
    for r in spans {
        if out
            .iter()
            .any(|o: &Redaction| o.start <= r.start && r.end <= o.end)
        {
            continue;
        }
        out.push(r);
    }
    out
}

fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'+' || c == b'/' || c == b'='
}

fn is_fp(frag: &str) -> bool {
    let u = frag.to_uppercase();
    u.contains("EXAMPLE")
        || u.contains("TEST-ONLY")
        || u.contains("TEST_ONLY")
        || frag.contains("...")
        || u.contains("XXX")
}

/// Shannon entropy (bits/char) over the fragment's bytes.
pub fn shannon(s: &str) -> f64 {
    let b = s.as_bytes();
    if b.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for c in b {
        counts[*c as usize] += 1;
    }
    let n = b.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = f64::from(*c) / n;
            -p * p.log2()
        })
        .sum()
}

/// Apply `scan` redactions: each span → `[redacted:<family>]`, plus a
/// one-line notice naming families only (never content). Returns
/// (redacted_text, notice_or_empty).
pub fn redact(text: &str) -> (String, String) {
    let spans = scan(text);
    if spans.is_empty() {
        return (text.to_string(), String::new());
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut fams: Vec<&str> = Vec::new();
    for r in &spans {
        if r.start < last {
            continue;
        }
        out.push_str(&text[last..r.start]);
        out.push_str(&format!("[redacted:{}]", r.family));
        if !fams.contains(&r.family) {
            fams.push(r.family);
        }
        last = r.end;
    }
    out.push_str(&text[last..]);
    let notice = format!(
        "[overseer] redacted {} secret span(s): {}",
        spans.len(),
        fams.join(", ")
    );
    (out, notice)
}

/// Metadata log for scan redactions: stderr line naming span count +
/// families only (never content). The spill path calls this so the
/// redaction itself stays auditable without copying secrets.
pub fn note_redaction(notice: &str) {
    eprintln!("{notice}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_format_is_frozen_contract() {
        // `ovsent_` + 32 lowercase hex (shared with P7's channel sentinel).
        let s = sentinel_for("db", "s3cr3t-real");
        assert!(s.starts_with(SENTINEL_PREFIX), "{s}");
        assert_eq!(s.len(), SENTINEL_PREFIX.len() + SENTINEL_HEX_LEN);
        assert!(is_sentinel(&s), "{s}");
        // Rejections: uppercase, short, wrong prefix, trailing junk.
        assert!(!is_sentinel("ovsent_ABCD1234abcd1234abcd1234abcd12"));
        assert!(!is_sentinel("ovsent_abc123"));
        assert!(!is_sentinel("bearer_abcdef1234567890abcdef1234567890"));
        assert!(!is_sentinel(&format!("{s}X")));
    }

    #[test]
    fn is_sentinel_rejects_multibyte_prefix_without_panicking() {
        // Right byte length, but byte 7 falls inside `€`.
        let s = format!("é€€{}", "a".repeat(31));
        assert_eq!(s.len(), SENTINEL_PREFIX.len() + SENTINEL_HEX_LEN);
        assert!(!s.is_char_boundary(SENTINEL_PREFIX.len()));
        assert!(!is_sentinel(&s));
    }

    #[test]
    fn scan_finds_aws_key_ending_at_eof() {
        for body in ["AKIAIOSFODNN7QWERTYZ", "key AKIAIOSFODNN7QWERTYZ"] {
            let spans = scan(body);
            assert!(
                spans
                    .iter()
                    .any(|r| r.family == "aws-key" && r.end == body.len()),
                "{body}: {spans:?}"
            );
        }
        assert!(scan("AKIAIOSFODNN7QWERTY")
            .iter()
            .all(|r| r.family != "aws-key"));
    }

    #[test]
    fn hmac_sha256_matches_rfc4231_case_1() {
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn sentinel_is_keyed_and_stable_within_the_process() {
        let mut br = Broker::new();
        let s1 = br.issue_capability("db", "DB_PASS", "1234", vec![], vec![], None);
        let s2 = br.issue_capability("db", "DB_PASS", "1234", vec![], vec![], None);
        assert_eq!(s1, s2, "stable within one broker");
        assert_eq!(s1, sentinel_for("db", "1234"));
        let a = sentinel_with_key(&[1; 32], "db", "1234");
        let b = sentinel_with_key(&[2; 32], "db", "1234");
        assert_ne!(a, b, "different keys, different sentinels");
        assert!(is_sentinel(&a) && is_sentinel(&b));
        // Not the old unkeyed sha256(id:real) — a PIN is not dictionary-recoverable.
        use sha2::{Digest, Sha256};
        let unkeyed: String = Sha256::digest(b"overseer-cred-v1:db:1234")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_ne!(
            s1,
            format!("{SENTINEL_PREFIX}{}", &unkeyed[..SENTINEL_HEX_LEN])
        );
    }

    #[test]
    fn sentinel_mapping_is_one_to_one() {
        // Same (id, real) → same sentinel; different real → different.
        let a1 = sentinel_for("db", "real-1");
        let a2 = sentinel_for("db", "real-1");
        let b = sentinel_for("db", "real-2");
        let c = sentinel_for("other", "real-1");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert_ne!(a1, c);
        let mut br = Broker::new();
        let s = br.issue_capability("db", "DB_PASS", "real-1", vec![], vec![], None);
        assert_eq!(s, a1);
        assert_eq!(br.real_for("db"), Some("real-1"));
    }

    #[test]
    fn reals_never_serialize_or_debug() {
        let mut br = Broker::new();
        br.issue_capability("db", "DB_PASS", "super-secret-real", vec![], vec![], None);
        let dbg = format!("{br:?}");
        assert!(!dbg.contains("super-secret-real"), "{dbg}");
        assert!(dbg.contains("ovsent_"), "{dbg}");
        let cred = br.get("db").unwrap();
        // Credential exposes no Serialize impl: only Debug (derived) —
        // assert the Debug form redacts by construction (field is private,
        // so serde can't see it either without a manual impl).
        let cdbg = format!("{cred:?}");
        assert!(!cdbg.contains("super-secret-real"), "{cdbg}");
    }

    #[test]
    fn scan_never_panics_on_multibyte() {
        // F1 (extreme): byte-index loops must not slice inside multibyte chars.
        for body in [
            "é".repeat(30),
            format!("café {} end", "é".repeat(100)),
            format!("AKIA{} café", "É".repeat(30)),
            "é".repeat(10_000),
            format!("-----BEGIN X-----\n{}\n", "é".repeat(200)),
        ] {
            let spans = scan(&body);
            let (red, _) = redact(&body);
            assert!(
                red.len() >= body.len() - body.len() / 2,
                "no wild truncation"
            );
            let _ = spans.len();
        }
    }

    #[test]
    fn scan_families_fp_and_entropy() {
        // Each curated family fires.
        assert!(scan("key AKIAIOSFODNN7QWERTY12 here")
            .iter()
            .any(|r| r.family == "aws-key"));
        // …except the FP allowlist suppresses EXAMPLE-bearing spans.
        assert!(scan("key AKIAIOSFODNN7EXAMPLE here").is_empty());
        assert!(scan("token ghp_abcdefgh12345678 here")
            .iter()
            .any(|r| r.family == "github-token"));
        assert!(scan("token xoxb-123456789012-abcdefgh here")
            .iter()
            .any(|r| r.family == "slack-token"));
        assert!(scan("key sk-live-abcdefgh12345678 here")
            .iter()
            .any(|r| r.family == "api-key"));
        assert!(
            scan("-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----")
                .iter()
                .any(|r| r.family == "pem-block")
        );
        // High entropy ≥3.5 fires; low-entropy runs don't.
        assert!(scan("tok aB3dE5gH7jK9mN2pQ4rS6tU8vW here")
            .iter()
            .any(|r| r.family == "high-entropy"));
        assert!(scan("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_empty());
        assert!(scan("hello world, nothing secret here").is_empty());
        // FPfamilies: TEST-ONLY / xxx / ... suppress.
        assert!(scan("token ghp_TEST-ONLY-abcdefgh12345678 here").is_empty());
        assert!(scan("key sk-live-xxxxxxxxxxxxxxxxxx here").is_empty());
    }

    #[test]
    fn injection_child_sees_real_log_sees_sentinel() {
        // Broker pairs drive both halves: injector maps selector→real,
        // sanitize maps real→sentinel. Pin the round trip here.
        let mut br = Broker::new();
        let s = br.issue_capability(
            "api",
            "API_TOKEN",
            "tok-real-123",
            vec!["api.example.com".into()],
            vec!["read".into()],
            None,
        );
        let child_env = format!("API_TOKEN={}", br.real_for("api").unwrap());
        assert!(child_env.contains("tok-real-123"));
        let log = sanitize(&br, &format!("called with {child_env}"));
        assert!(!log.contains("tok-real-123"), "{log}");
        assert!(log.contains(&s), "{log}");
    }

    #[test]
    fn no_plaintext_in_tool_result_path() {
        // Verbatim sanitize over a ToolResult-shaped string.
        let mut br = Broker::new();
        let s = br.issue_capability("db", "DB_PASS", "pw-real-9", vec![], vec![], None);
        let text = "query ok, password was pw-real-9 done";
        let clean = sanitize(&br, text);
        assert!(!clean.contains("pw-real-9"));
        assert!(clean.contains(&s));
        // Idempotent: sanitizing twice is stable.
        assert_eq!(sanitize(&br, &clean), clean);
    }

    // ---------- P6-4: grants, vault kinds, store ----------

    #[test]
    fn vault_kind_round_trips() {
        assert_eq!(VaultKind::parse("oauth_token"), Ok(VaultKind::OAuthToken));
        assert_eq!(VaultKind::parse("Session"), Ok(VaultKind::SessionHandle));
        assert_eq!(VaultKind::parse("password"), Ok(VaultKind::Password));
        assert!(VaultKind::parse("bogus").is_err());
        assert_eq!(VaultKind::OAuthToken.as_str(), "oauth_token");

        let mut br = Broker::new();
        br.issue_vault(
            "gh",
            "GH_TOKEN",
            "ghp_real_secret_value_123",
            VaultKind::OAuthToken,
            vec![],
            vec!["repo".into()],
            None,
        );
        let cred = br.get("gh").unwrap();
        assert_eq!(cred.kind, VaultKind::OAuthToken);
    }

    #[test]
    fn payload_parses_secrets_and_grants() {
        let text = "# comment\n\nAPI_TOKEN=tok-abc=def\n\
                    grant gh read,repo 0 user alice\n";
        let p = parse_payload(text).unwrap();
        assert_eq!(
            p.secrets,
            vec![("API_TOKEN".to_string(), "tok-abc=def".to_string())]
        );
        assert_eq!(p.grants.len(), 1);
        assert_eq!(p.grants[0].client, "gh");
        assert_eq!(p.grants[0].scopes, vec!["read", "repo"]);
        assert_eq!(p.grants[0].approved_by, "alice");
        assert!(!p.is_empty());

        // Malformed lines fail with the line number, never the content.
        let err = parse_payload("# ok\nnot a pair").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        assert!(!err.contains("not a pair"));
        let err = parse_payload("grant gh read").unwrap_err();
        assert!(err.contains("line 1"), "{err}");
        assert!(parse_payload("grant gh read zzz a b").is_err());
        assert!(parse_payload("# nothing\n\n").unwrap().is_empty());

        // Installing a payload brokers the secrets and books the grants.
        let mut br = Broker::new();
        let (n_secrets, n_grants) = br.install_payload(&parse_payload(text).unwrap());
        assert_eq!((n_secrets, n_grants), (1, 1));
        assert_eq!(br.real_for("API_TOKEN"), Some("tok-abc=def"));
        assert_eq!(br.grants().len(), 1);
        // The broker's Debug stays redacted with grants attached.
        assert!(!format!("{br:?}").contains("tok-abc=def"));
    }

    #[test]
    fn keychain_missing_binary_falls_back_to_env() {
        // No such backend on this machine → Unavailable → env fallback.
        let kc = Keychain::new(
            Some("overseer-definitely-not-a-keychain-binary"),
            &["find-generic-password"],
            &["delete-generic-password"],
        );
        assert_eq!(kc.fetch(), KeychainOutcome::Unavailable);
        let env = Some("API_TOKEN=from-env".to_string());
        let r = resolve_store_with(CredentialStore::Keychain, &kc, kc.fetch(), env.clone());
        assert_eq!(r.store, CredentialStore::Env, "{r:?}");
        assert_eq!(r.payload, env);
        assert!(r.note.contains("unavailable"), "{}", r.note);
        // Auto behaves the same; Env never probes the backend at all.
        let r = resolve_store_with(CredentialStore::Auto, &kc, kc.fetch(), env.clone());
        assert_eq!(r.store, CredentialStore::Env);
        let r = resolve_store_with(
            CredentialStore::Env,
            &kc,
            KeychainOutcome::Unavailable,
            env.clone(),
        );
        assert_eq!(r.store, CredentialStore::Env);
        assert_eq!(r.note, "ok");
        // A live keychain wins when it holds a parseable entry.
        let live = resolve_store_with(
            CredentialStore::Keychain,
            &kc,
            KeychainOutcome::Found("API_TOKEN=k".into()),
            env.clone(),
        );
        assert_eq!(live.store, CredentialStore::Keychain);
        assert_eq!(live.payload.as_deref(), Some("API_TOKEN=k"));
        // Missing entry (backend ran, nothing stored) → env too.
        let missing = resolve_store_with(
            CredentialStore::Keychain,
            &kc,
            KeychainOutcome::Missing,
            env,
        );
        assert_eq!(missing.store, CredentialStore::Env);
        assert!(
            missing.note.contains("no keychain entry"),
            "{}",
            missing.note
        );
        // Single-entry contract: one service/account per backend.
        assert_eq!(kc.entry(), (KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT));
    }

    /// A stale (unparseable) keychain entry is cleared before the env
    /// fallback, so the next run doesn't inherit it. Driven through a fake
    /// backend script — the same Command path production uses.
    #[cfg(unix)]
    #[test]
    fn stale_keychain_entry_is_cleared_on_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("overseer-kc-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("deleted");
        let bin = dir.join("fake-keychain");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n\
                 find-generic-password) echo 'total garbage, not a payload' ;;\n\
                 delete-generic-password) touch '{}' ;;\nesac\nexit 0\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let kc = Keychain::new(
            Some(&bin.to_string_lossy()),
            &["find-generic-password", "{service}", "{account}"],
            &["delete-generic-password", "{service}", "{account}"],
        );
        let outcome = kc.fetch();
        assert_eq!(
            outcome,
            KeychainOutcome::Found("total garbage, not a payload".into())
        );
        assert!(!marker.exists());
        let r = resolve_store_with(
            CredentialStore::Auto,
            &kc,
            outcome,
            Some("API_TOKEN=fresh".into()),
        );
        assert_eq!(r.store, CredentialStore::Env, "{r:?}");
        assert_eq!(r.payload.as_deref(), Some("API_TOKEN=fresh"));
        assert!(marker.exists(), "stale entry must be deleted");
        assert!(r.note.contains("stale"), "{}", r.note);
        assert!(r.note.contains("cleared"), "{}", r.note);
        // A commented-out entry is empty, not corrupt — same disposition.
        let marker2 = dir.join("deleted2");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\ncase \"$1\" in\nfind-generic-password) echo '# empty' ;;\n\
                 delete-generic-password) touch '{}' ;;\nesac\nexit 0\n",
                marker2.display()
            ),
        )
        .unwrap();
        let r = resolve_store_with(CredentialStore::Auto, &kc, kc.fetch(), None);
        assert_eq!(r.store, CredentialStore::Env);
        assert!(r.note.contains("stale"), "{}", r.note);
        assert!(marker2.exists(), "empty entry must be deleted too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefetch_returns_the_same_outcome_as_fetch() {
        let kc = Keychain::new(
            Some("overseer-definitely-not-a-keychain-binary"),
            &["find-generic-password"],
            &["delete-generic-password"],
        );
        // The prefetch runs on its own thread and joins to the same answer.
        assert_eq!(kc.prefetch().resolve(), kc.fetch());
        let detected = Keychain::detect();
        assert_eq!(detected.prefetch().resolve(), detected.fetch());
    }
}
