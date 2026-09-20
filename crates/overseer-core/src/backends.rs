//! Backend-selection ports (P8-C) — sandbox backends, workspace providers,
//! sandbox runtimes, MCP server spawning, and MCP grants.
//!
//! Five ports, all pure: validated data shapes plus the deterministic
//! decision each real project makes at run time. The engine links no
//! container runtime, no hypervisor, and no MCP transport, so what lands
//! here is the *choice* and the honest refusal that must precede it:
//!
//! - **e2b (E2B sandboxes)** — a sandbox is requested by capability:
//!   `Capabilities` against a `SandboxSpec` via `requirements_met`, which
//!   names the first capability the backend cannot provide. A backend that
//!   cannot serve the request is refused by name — never handed a weaker
//!   sandbox and told it succeeded. `SandboxBackend` is the slot a real E2B
//!   client implements.
//! - **Daytona workspace providers** — the provider enum, its parse table,
//!   and the spec rules. `WorkspaceProvider::is_remote` marks the providers
//!   whose workspace *and its environment* leave this machine; those are
//!   opt-in, chosen by the operator, never a default.
//! - **gVisor / macOS seatbelt / bubblewrap runtime selection** — the
//!   `--runtime` decision (`SandboxRuntime`, `requirement`, `select_runtime`,
//!   `check_runtime`). A requested-but-unavailable runtime is an error naming
//!   the missing binary; there is no silent downgrade to unsandboxed native
//!   execution, which is the entire point of the port.
//! - **ToolHive MCP server spawning** — `grant_spawn` assembles the exact
//!   `thv run` argv shape and enforces the no-creds invariant: the plan
//!   carries environment *keys* only, because a spec has no way to express a
//!   secret value and a key containing `=` is rejected outright.
//! - **ContextForge grants** — `validate_grant`, `allows`, and
//!   `effective_grants`: exact subject and server matching (a server
//!   wildcard is a cross-server escalation, refused), a tool matched exactly
//!   or through the single `*` entry, and an expiry that is an exclusive
//!   upper bound.
//!
//! Every list this module emits is in a fixed order, every timestamp is
//! compared lexicographically as a stamp (never parsed), and every error
//! names the offending value so the caller can repair it.
//!
//! `// DEFERRED(owner): actually creating or destroying sandboxes, spawning
//! `thv`/`runsc`/`sandbox-exec` (process supervision belongs with the ops
//! surface), preparing the gVisor OCI bundle at `<root>/.overseer/runsc-bundle`
//! (no runsc argv is invented here), and any MCP transport — this batch lands
//! the validation, the selection, and the argv assembly only.`

// ── E2B: sandbox backends ────────────────────────────────────────────────

/// What one sandbox backend can actually provide.
///
/// Fail-closed default: every field is `false`, so an unconfigured backend
/// grants nothing rather than being assumed capable. The flags are read only
/// through [`requirements_met`], which is the single place the
/// capability-vs-request decision is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// A real filesystem to run the code in. Every sandbox needs this.
    pub filesystem: bool,
    /// Outbound network access from inside the sandbox.
    pub network: bool,
    /// A GPU attached to the sandbox.
    pub gpu: bool,
    /// Filesystem snapshots (create/restore), the basis of sandbox reuse.
    pub snapshot: bool,
}

/// What the caller wants from a sandbox.
///
/// The `needs_*` flags are declarative: they are what the caller *asks* for,
/// and only [`requirements_met`] decides whether a given backend can meet
/// them. Nothing here is inferred from the language — a caller that needs a
/// network says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSpec {
    /// The language the sandbox runs (`"python"`, `"node"`, …). Non-empty;
    /// it also keys any future image lookup, so an empty one is a caller bug.
    pub language: String,
    /// The sandbox must expose a GPU.
    pub needs_gpu: bool,
    /// The sandbox must reach the network.
    pub needs_network: bool,
    /// The sandbox must be snapshottable.
    pub needs_snapshot: bool,
    /// Wall-clock cap in seconds, `1..=3600`.
    pub timeout_s: u64,
}

/// The sandbox lifecycle a real backend implements.
///
/// Contract for implementors:
/// - `create` validates the spec ([`validate_spec`]) and the request
///   ([`requirements_met`]) *before* allocating anything, and returns `Err`
///   with the reason when it cannot serve the request. Returning an id for a
///   silently degraded sandbox is the failure this trait exists to prevent.
/// - `destroy` takes the id `create` returned; destroying an unknown id is an
///   `Err` naming it (a caller that leaked an id should learn that).
pub trait SandboxBackend {
    /// Stable identifier of the backend kind (`"e2b"`, `"local-docker"`, …).
    fn name(&self) -> &'static str;

    /// What this backend can provide. Read-only: capabilities describe the
    /// backend, they are not negotiated per request.
    fn capabilities(&self) -> Capabilities;

    /// Create a sandbox for `spec`, returning the backend-scoped id used to
    /// destroy it later.
    fn create(&mut self, spec: &SandboxSpec) -> Result<String, String>;

    /// Destroy the sandbox `id` previously returned by [`SandboxBackend::create`].
    fn destroy(&mut self, id: &str) -> Result<(), String>;
}

/// Validate the parts of a spec that no backend can fix.
///
/// Invariants: `language` is non-empty (whitespace-only counts as empty —
/// it names nothing), and `timeout_s` is in `1..=3600` (zero is not a
/// duration, and an hour is the cap this batch commits to). The `needs_*`
/// flags are declarative and are never validated here; they are checked
/// against [`Capabilities`] by [`requirements_met`].
pub fn validate_spec(spec: &SandboxSpec) -> Result<(), String> {
    if spec.language.trim().is_empty() {
        return Err(
            "sandbox spec language must be non-empty: it names the runtime the sandbox boots, \
             and no backend can serve a spec without one"
                .to_string(),
        );
    }
    if spec.timeout_s == 0 || spec.timeout_s > 3600 {
        return Err(format!(
            "sandbox spec timeout_s must be 1..=3600, got {}",
            spec.timeout_s
        ));
    }
    Ok(())
}

/// Whether `caps` can serve `spec`, refusing the request when it cannot.
///
/// The capability order is fixed — filesystem, network, gpu, snapshot — and
/// only the **first** unmet one is reported, so the message is reproducible
/// and the caller repairs one thing at a time. A missing capability is
/// always an error: there is no fallback path in which the sandbox quietly
/// gets less than the spec asked for.
pub fn requirements_met(caps: Capabilities, spec: &SandboxSpec) -> Result<(), String> {
    validate_spec(spec)?;
    if !caps.filesystem {
        return Err(format!(
            "sandbox backend is missing the `filesystem` capability required by every spec \
             (language `{}`); pick a backend that provides one — the request is never \
             silently downgraded",
            spec.language
        ));
    }
    if spec.needs_network && !caps.network {
        return Err(format!(
            "sandbox backend is missing the `network` capability required by language `{}` \
             (needs_network = true); pick a backend that provides it — the request is never \
             silently downgraded",
            spec.language
        ));
    }
    if spec.needs_gpu && !caps.gpu {
        return Err(format!(
            "sandbox backend is missing the `gpu` capability required by language `{}` \
             (needs_gpu = true); pick a backend that provides it — the request is never \
             silently downgraded",
            spec.language
        ));
    }
    if spec.needs_snapshot && !caps.snapshot {
        return Err(format!(
            "sandbox backend is missing the `snapshot` capability required by language `{}` \
             (needs_snapshot = true); pick a backend that provides it — the request is never \
             silently downgraded",
            spec.language
        ));
    }
    Ok(())
}

// ── Daytona: workspace providers ─────────────────────────────────────────

/// Where a workspace runs.
///
/// `Local` and `Docker` keep the code and its environment on this machine;
/// `Daytona` and `E2b` do not. There is deliberately no `Default`: a remote
/// workspace means the code, its dependencies, and anything it reads leave
/// the operator's machine, so the operator must select it explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceProvider {
    /// The host filesystem, no container, no isolation beyond the process.
    Local,
    /// A container on this machine.
    Docker,
    /// A Daytona workspace — remote.
    Daytona,
    /// An E2B sandbox — remote.
    E2b,
}

impl WorkspaceProvider {
    /// Every provider, in declaration order — the order [`WorkspaceProvider::parse`]'s
    /// error lists and the order a UI should offer them in.
    pub const ALL: [WorkspaceProvider; 4] = [
        WorkspaceProvider::Local,
        WorkspaceProvider::Docker,
        WorkspaceProvider::Daytona,
        WorkspaceProvider::E2b,
    ];

    /// The canonical, lower-case name. Round-trips through
    /// [`WorkspaceProvider::parse`].
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceProvider::Local => "local",
            WorkspaceProvider::Docker => "docker",
            WorkspaceProvider::Daytona => "daytona",
            WorkspaceProvider::E2b => "e2b",
        }
    }

    /// Parse a provider name. Surrounding whitespace, case, and `-`/`_`
    /// separators are tolerated (`"E-2-B"` is `E2b`) so a config file and a
    /// CLI flag agree. Anything else is an `Err` listing every accepted name.
    pub fn parse(s: &str) -> Result<Self, String> {
        let norm: String = s
            .trim()
            .chars()
            .filter(|c| *c != '-' && *c != '_')
            .flat_map(char::to_lowercase)
            .collect();
        WorkspaceProvider::ALL
            .into_iter()
            .find(|p| p.as_str() == norm)
            .ok_or_else(|| {
                format!(
                    "unknown workspace provider `{s}`; expected one of: {}",
                    WorkspaceProvider::ALL
                        .map(WorkspaceProvider::as_str)
                        .join(", ")
                )
            })
    }

    /// True when the workspace runs *off this machine*.
    ///
    /// A remote workspace is not a hosting detail: the code, its environment,
    /// and anything it touches are shipped to a third party. Only the
    /// operator may choose one — hence no `Default` on this enum, and hence
    /// [`validate_provider`] demanding an explicit image for these.
    pub fn is_remote(self) -> bool {
        matches!(self, WorkspaceProvider::Daytona | WorkspaceProvider::E2b)
    }
}

/// A workspace request: which provider, what image, how much machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpec {
    /// The provider to use. Never defaulted — see
    /// [`WorkspaceProvider::is_remote`].
    pub provider: WorkspaceProvider,
    /// The container image. Required for every non-local provider; forbidden
    /// for `Local`, which runs on the host and could only ignore it.
    pub image: Option<String>,
    /// vCPUs, `1..=64`.
    pub cpu: u32,
    /// Memory in MiB, `128..=262144` (128 MiB .. 256 GiB).
    pub memory_mb: u32,
}

/// Validate a workspace request.
///
/// Invariants: every non-local provider names an image (a container without
/// one is not a configuration, it is a placeholder) and the error says which
/// provider and why; `Local` must *not* carry one (it would be ignored, and a
/// silently ignored field is a lie); `cpu` is `1..=64`; `memory_mb` is
/// `128..=262144`. Both bounds errors name the field and the accepted range.
pub fn validate_provider(p: &ProviderSpec) -> Result<(), String> {
    match (&p.image, p.provider) {
        (None, WorkspaceProvider::Local) => {}
        (None, other) => {
            return Err(format!(
                "provider `{}` requires an image: the workspace runs in a container that must \
                 be named (only `local` runs on the host without one)",
                other.as_str()
            ));
        }
        (Some(image), WorkspaceProvider::Local) => {
            if image.trim().is_empty() {
                return Err(
                    "provider `local` image must be omitted, not empty: a blank image names \
                     nothing"
                        .to_string(),
                );
            }
            return Err(format!(
                "provider `local` runs on the host and cannot use image `{image}`: the field \
                 would be ignored, so it is refused; drop `image` or pick `docker`"
            ));
        }
        (Some(image), _) => {
            if image.trim().is_empty() {
                return Err(
                    "workspace image must be non-empty: a blank image names no container"
                        .to_string(),
                );
            }
        }
    }
    if p.cpu == 0 || p.cpu > 64 {
        return Err(format!("workspace cpu must be 1..=64, got {}", p.cpu));
    }
    if p.memory_mb < 128 || p.memory_mb > 262144 {
        return Err(format!(
            "workspace memory_mb must be 128..=262144, got {}",
            p.memory_mb
        ));
    }
    Ok(())
}

// ── gVisor / seatbelt / bubblewrap: sandbox runtimes ─────────────────────

/// The sandboxing mechanism a command runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRuntime {
    /// No sandbox: the command runs as the user, on the host.
    Native,
    /// macOS `sandbox-exec` profiles (seatbelt).
    Seatbelt,
    /// Linux user namespaces via `bwrap`.
    Bubblewrap,
    /// gVisor's `runsc` user-space kernel.
    Gvisor,
}

impl SandboxRuntime {
    /// Every runtime, in declaration order — the order
    /// [`SandboxRuntime::parse`]'s error lists.
    pub const ALL: [SandboxRuntime; 4] = [
        SandboxRuntime::Native,
        SandboxRuntime::Seatbelt,
        SandboxRuntime::Bubblewrap,
        SandboxRuntime::Gvisor,
    ];

    /// The canonical name — the value a `--runtime` flag takes.
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxRuntime::Native => "native",
            SandboxRuntime::Seatbelt => "seatbelt",
            SandboxRuntime::Bubblewrap => "bubblewrap",
            SandboxRuntime::Gvisor => "gvisor",
        }
    }

    /// Parse a runtime name, tolerating case and `-`/`_` separators so the
    /// platform name and the CLI flag agree: `native`, `seatbelt` /
    /// `sandbox-exec`, `bwrap` / `bubblewrap`, `gvisor` / `runsc`. Anything
    /// else is an `Err` listing [`SandboxRuntime::ALL`] *and* the aliases —
    /// a typo must not read as "no sandbox requested".
    pub fn parse(s: &str) -> Result<Self, String> {
        let norm: String = s
            .trim()
            .chars()
            .filter(|c| *c != '-' && *c != '_')
            .flat_map(char::to_lowercase)
            .collect();
        match norm.as_str() {
            "native" => Ok(SandboxRuntime::Native),
            "seatbelt" | "sandboxexec" => Ok(SandboxRuntime::Seatbelt),
            "bwrap" | "bubblewrap" => Ok(SandboxRuntime::Bubblewrap),
            "gvisor" | "runsc" => Ok(SandboxRuntime::Gvisor),
            _ => Err(format!(
                "unknown sandbox runtime `{s}`; expected one of: {} (also accepted: \
                 sandbox-exec, bwrap, runsc)",
                SandboxRuntime::ALL.map(SandboxRuntime::as_str).join(", ")
            )),
        }
    }
}

/// The OCI bundle `runsc` needs, relative to the workspace root. Preparing it
/// is out of scope for this batch (see the module's DEFERRED note): this
/// constant exists so the error names the exact path an operator must create.
pub const GVISOR_BUNDLE_DIR: &str = ".overseer/runsc-bundle";

/// What selecting a runtime requires from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeRequirement {
    /// The runtime this requirement describes.
    pub runtime: SandboxRuntime,
    /// The executable that must be present, or `None` when the runtime needs
    /// no binary (only `Native`).
    pub binary: Option<&'static str>,
    /// True when the runtime additionally needs a prepared OCI bundle
    /// (`Gvisor` only); presence of the binary is not sufficient.
    pub needs_bundle: bool,
    /// One line the caller can show the operator: what this runtime does, and
    /// what it costs.
    pub note: &'static str,
}

/// The requirement for `r`. Total: every runtime has an entry, so a caller
/// never has to invent a default.
pub fn requirement(r: SandboxRuntime) -> RuntimeRequirement {
    match r {
        SandboxRuntime::Native => RuntimeRequirement {
            runtime: r,
            binary: None,
            needs_bundle: false,
            note: "commands run unsandboxed on the host — the operator chose this",
        },
        SandboxRuntime::Seatbelt => RuntimeRequirement {
            runtime: r,
            binary: Some("/usr/bin/sandbox-exec"),
            needs_bundle: false,
            note: "macOS seatbelt: commands run under a sandbox-exec profile",
        },
        SandboxRuntime::Bubblewrap => RuntimeRequirement {
            runtime: r,
            binary: Some("bwrap"),
            needs_bundle: false,
            note: "Linux user namespaces via bwrap; the user must be allowed to create one",
        },
        SandboxRuntime::Gvisor => RuntimeRequirement {
            runtime: r,
            binary: Some("runsc"),
            needs_bundle: true,
            note: "gVisor runsc with a prepared OCI bundle at .overseer/runsc-bundle",
        },
    }
}

/// Preferences when the caller expresses none, best isolation first.
const RUNTIME_PREFERENCE: [SandboxRuntime; 4] = [
    SandboxRuntime::Gvisor,
    SandboxRuntime::Bubblewrap,
    SandboxRuntime::Seatbelt,
    SandboxRuntime::Native,
];

/// How a skipped runtime is reported: the runtime and the binary it lacks.
fn missing_desc(r: SandboxRuntime) -> String {
    match requirement(r).binary {
        Some(binary) => format!("`{}` (`{binary}` missing)", r.as_str()),
        None => format!("`{}`", r.as_str()),
    }
}

/// Choose a runtime, refusing rather than quietly weakening the sandbox.
///
/// - `Some(name)` is a request: an unknown name is an `Err` listing
///   [`SandboxRuntime::ALL`], and a requested runtime that is not in
///   `available` is an `Err` naming the binary it needs. It is **never**
///   downgraded to native — that substitution is the exact failure this port
///   exists to prevent.
/// - `None` picks the first runtime in `RUNTIME_PREFERENCE` present in
///   `available` (gVisor, bubblewrap, seatbelt, native).
///
/// The returned note is `Some` only when the choice was a **fallback** (the
/// preferred runtime was unavailable) or **native** (unsandboxed execution,
/// which is always worth saying out loud); an exactly honored request to a
/// sandboxing runtime reports `None`.
pub fn select_runtime(
    requested: Option<&str>,
    available: &[SandboxRuntime],
) -> Result<(SandboxRuntime, Option<String>), String> {
    if let Some(raw) = requested {
        let r = SandboxRuntime::parse(raw)?;
        if !available.contains(&r) {
            let err = match requirement(r).binary {
                Some(binary) => format!(
                    "requested sandbox runtime `{}` is unavailable: `{binary}` was not found \
                     (or is not runnable) on this host; install it or request a different \
                     runtime explicitly — this never falls back to native execution",
                    r.as_str()
                ),
                None => format!(
                    "requested sandbox runtime `{}` is not in the available set; the caller \
                     must allow it — this never falls back to a different runtime",
                    r.as_str()
                ),
            };
            return Err(err);
        }
        let note = (r == SandboxRuntime::Native).then(|| requirement(r).note.to_string());
        return Ok((r, note));
    }

    let chosen = RUNTIME_PREFERENCE
        .into_iter()
        .find(|r| available.contains(r))
        .ok_or_else(|| {
            "no sandbox runtime is available: the caller's available set is empty; probe the \
             host for gvisor/bubblewrap/seatbelt first — native is never picked silently"
                .to_string()
        })?;

    let skipped: Vec<SandboxRuntime> = RUNTIME_PREFERENCE
        .into_iter()
        .take_while(|r| *r != chosen)
        .collect();
    let native_note = requirement(SandboxRuntime::Native).note;
    let note = if chosen == SandboxRuntime::Native {
        let note = if skipped.is_empty() {
            format!("sandbox runtime `native` selected: {native_note}")
        } else {
            format!(
                "fell back to `native` ({}): {native_note}",
                skipped
                    .iter()
                    .map(|r| missing_desc(*r))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        Some(note)
    } else if skipped.is_empty() {
        None
    } else {
        Some(format!(
            "fell back to `{}`: {} unavailable",
            chosen.as_str(),
            skipped
                .iter()
                .map(|r| missing_desc(*r))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    };
    Ok((chosen, note))
}

/// Check that a runtime can actually start on this host.
///
/// `binary_present` and `bundle_present` are the caller's probe results — no
/// probing happens here, so the decision is deterministic and testable.
/// `Gvisor` needs **both** its binary and the prepared bundle, and the `Err`
/// names every missing piece (both when both are missing). `Native` always
/// passes: it needs nothing, which is exactly why it is never selected
/// implicitly.
pub fn check_runtime(
    r: SandboxRuntime,
    binary_present: bool,
    bundle_present: bool,
) -> Result<(), String> {
    let req = requirement(r);
    let mut missing: Vec<String> = Vec::new();
    if let Some(binary) = req.binary {
        if !binary_present {
            missing.push(format!("`{binary}` is not on PATH"));
        }
    }
    if req.needs_bundle && !bundle_present {
        missing.push(format!(
            "no prepared OCI bundle at `{GVISOR_BUNDLE_DIR}` (relative to the workspace root)"
        ));
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "sandbox runtime `{}` cannot start: {}",
            r.as_str(),
            missing.join(" and ")
        ))
    }
}

// ── ToolHive: MCP server spawning ────────────────────────────────────────

/// What an MCP server is allowed to touch.
///
/// There is no field for a secret *value* anywhere in this shape, and that is
/// the point: [`grant_spawn`] emits environment **keys**, which the host
/// resolves from its own environment. A spec therefore cannot smuggle
/// credentials into an argv that lands in a process table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GrantSet {
    /// Allow the server egress. `false` is the default and maps to
    /// `--network=none`.
    pub allow_network: bool,
    /// Host paths to bind into the container. Each must be absolute and under
    /// the allowed root.
    pub mounts: Vec<String>,
    /// Environment variable *names* to forward. Selector-shaped, keys only.
    pub env_keys: Vec<String>,
}

/// An MCP server to spawn: a name, an image, and its grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    /// Container/server name; also the `SpawnPlan::server` the caller tracks.
    pub name: String,
    /// The image the server runs.
    pub image: String,
    /// What the server may reach.
    pub permissions: GrantSet,
}

/// The argv a spawn would use, plus the keys it forwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    /// The server name, echoed for correlation with the audit log.
    pub server: String,
    /// The complete, byte-stable argv (see [`grant_spawn`] for the shape).
    pub args: Vec<String>,
    /// The environment keys the plan forwards, sorted and deduped. Never
    /// values — see [`GrantSet`].
    pub env_keys: Vec<String>,
}

/// Lexically normalize an absolute path into components. `None` when the path
/// is not absolute or when a `..` escapes above the root (fail closed).
fn normalize_abs(path: &str) -> Option<Vec<&str>> {
    if !path.starts_with('/') {
        return None;
    }
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            p => out.push(p),
        }
    }
    Some(out)
}

/// True when `path` resolves at or under `root`. Both must already be
/// normalized absolute component lists.
fn under_root(root: &[&str], path: &[&str]) -> bool {
    path.len() >= root.len() && path[..root.len()] == root[..]
}

/// Environment-name selector: `^[A-Z][A-Z0-9_]*$`.
fn is_env_selector(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Build the spawn plan for an MCP server.
///
/// Validated invariants, each refusal naming the offending field:
/// - `name` and `image` are non-empty.
/// - `allowed_root` is an absolute path; every mount is absolute too, and
///   resolves at or under the root (`..` is resolved lexically and an escape
///   is refused). The error names the offending mount.
/// - every `env_keys` entry is selector-shaped (`^[A-Z][A-Z0-9_]*$`) and
///   contains no `=`. A key with `=` is a *value*: this API carries keys only
///   — the host resolves them — so a spec that tries to embed a credential is
///   rejected rather than written into a process table. That is the no-creds
///   invariant; there is no field in [`GrantSet`] that could hold a value.
///
/// The argv shape is exactly:
/// `[run, --name, <name>, --network=none|host, (-v <mount>:<mount>)…, (-e <KEY>)…, <image>]`
/// with `mounts` and `env_keys` sorted (and deduped) so two equal specs give
/// byte-identical argv regardless of the order in which the operator listed
/// them.
pub fn grant_spawn(spec: &McpServerSpec, allowed_root: &str) -> Result<SpawnPlan, String> {
    if spec.name.trim().is_empty() {
        return Err(
            "MCP server name must be non-empty: it names the spawned server in the audit log"
                .to_string(),
        );
    }
    if spec.image.trim().is_empty() {
        return Err(
            "MCP server image must be non-empty: there is nothing to spawn without one".to_string(),
        );
    }
    let root = normalize_abs(allowed_root).ok_or_else(|| {
        format!(
            "allowed root `{allowed_root}` must be an absolute path without `..` escapes: it is \
             the only boundary the mounts are checked against"
        )
    })?;

    let mut mounts: Vec<String> = Vec::with_capacity(spec.permissions.mounts.len());
    for mount in &spec.permissions.mounts {
        let normalized = normalize_abs(mount).ok_or_else(|| {
            format!(
                "mount `{mount}` must be an absolute path without `..` escapes (root-relative \
                 paths are not accepted)"
            )
        })?;
        if !under_root(&root, &normalized) {
            return Err(format!(
                "mount `{mount}` is outside the allowed root `{allowed_root}`: an MCP server \
                 may only be handed paths under the root the operator granted"
            ));
        }
        if !mounts.contains(mount) {
            mounts.push(mount.clone());
        }
    }
    mounts.sort();

    let mut env_keys: Vec<String> = Vec::with_capacity(spec.permissions.env_keys.len());
    for key in &spec.permissions.env_keys {
        if key.contains('=') {
            return Err(format!(
                "environment key `{key}` carries a value: the spawn plan forwards keys only \
                 (the host resolves them), never `KEY=VALUE`"
            ));
        }
        if !is_env_selector(key) {
            return Err(format!(
                "environment key `{key}` is not a selector: expected `^[A-Z][A-Z0-9_]*$` \
                 (uppercase letters, digits, and underscores)"
            ));
        }
        if !env_keys.contains(key) {
            env_keys.push(key.clone());
        }
    }
    env_keys.sort();

    let mut args: Vec<String> = vec![
        "run".to_string(),
        "--name".to_string(),
        spec.name.clone(),
        format!(
            "--network={}",
            if spec.permissions.allow_network {
                "host"
            } else {
                "none"
            }
        ),
    ];
    for mount in &mounts {
        args.push("-v".to_string());
        args.push(format!("{mount}:{mount}"));
    }
    for key in &env_keys {
        args.push("-e".to_string());
        args.push(key.clone());
    }
    args.push(spec.image.clone());

    Ok(SpawnPlan {
        server: spec.name.clone(),
        args,
        env_keys,
    })
}

// ── ContextForge: grants ─────────────────────────────────────────────────

/// One ContextForge grant: a subject may call `tools` on `server` until
/// `expires_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The principal. Matched exactly; `*` is refused (see [`validate_grant`]).
    pub subject: String,
    /// The MCP server. Matched exactly; a wildcard here would be a
    /// cross-server escalation.
    pub server: String,
    /// Tool names, exact; the single entry `"*"` means every tool on this one
    /// server.
    pub tools: Vec<String>,
    /// `YYYY-MM-DDTHH:MM:SSZ`, or `None` for a grant that never expires.
    /// An **exclusive** upper bound when compared with `now`.
    pub expires_at: Option<String>,
}

/// `YYYY-MM-DDTHH:MM:SSZ` — 20 bytes, digits where digits belong, `T` at 10,
/// `:` at 13 and 16, `Z` at 19.
///
/// Shape only: the stamp is compared lexicographically, never parsed, so
/// calendar validity (e.g. `2025-02-30`) is not checked here and is not
/// observable through [`allows`].
fn is_stamp(s: &str) -> bool {
    // `-` sits at 4 and 7, `T` at 10, `:` at 13 and 16, `Z` at 19.
    const DIGITS: [usize; 14] = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let b = s.as_bytes();
    b.len() == 20
        && DIGITS.iter().all(|&i| b[i].is_ascii_digit())
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b[19] == b'Z'
}

/// Validate a grant before it is stored or handed to [`allows`].
///
/// Invariants: `subject` and `server` are non-empty and contain no `*` — a
/// wildcard subject authorizes every principal, and a wildcard server is a
/// cross-server escalation, so both are refused rather than stored and left
/// to silently match nothing; every `tools` entry is non-empty; and
/// `expires_at`, when present, is `YYYY-MM-DDTHH:MM:SSZ`-shaped (the error
/// echoes the offending value).
pub fn validate_grant(g: &Grant) -> Result<(), String> {
    if g.subject.trim().is_empty() {
        return Err("grant subject must be non-empty: it names the principal".to_string());
    }
    if g.subject.contains('*') {
        return Err(format!(
            "grant subject `{}` contains a wildcard: subjects are matched exactly, and a `*` \
             would authorize every principal",
            g.subject
        ));
    }
    if g.server.trim().is_empty() {
        return Err("grant server must be non-empty: it names the MCP server".to_string());
    }
    if g.server.contains('*') {
        return Err(format!(
            "grant server `{}` contains a wildcard: it would authorize this subject on every \
             server (a cross-server escalation), so it is refused",
            g.server
        ));
    }
    if g.tools.is_empty() {
        return Err(
            "grant must name at least one tool: a grant that authorizes nothing is a \
             misconfiguration, not a rule"
                .to_string(),
        );
    }
    for tool in &g.tools {
        if tool.trim().is_empty() {
            return Err(
                "grant tool names must be non-empty: an empty entry authorizes nothing (use \
                 `*` for every tool on this server)"
                    .to_string(),
            );
        }
    }
    if let Some(expiry) = &g.expires_at {
        if !is_stamp(expiry) {
            return Err(format!(
                "grant expires_at `{expiry}` is not a `YYYY-MM-DDTHH:MM:SSZ` stamp"
            ));
        }
    }
    Ok(())
}

/// Whether `g` authorizes `subject` to call `tool` on `server` at `now`.
///
/// Subject and server match **exactly** — no wildcards, no prefixes, no
/// suffix matching; a `*` in either field can never authorize (defensive:
/// [`validate_grant`] already refuses such a row). A tool matches its exact
/// name or the single `"*"` entry, and the wildcard is scoped to this one
/// server because the server had to match first. `expires_at` is an
/// **exclusive** upper bound compared lexicographically with `now`, so a
/// grant is dead at its stamp, not after it; `None` never expires.
pub fn allows(g: &Grant, subject: &str, server: &str, tool: &str, now: &str) -> bool {
    if g.subject != subject || g.server != server {
        return false;
    }
    if g.subject.contains('*') || g.server.contains('*') {
        return false;
    }
    if let Some(expiry) = &g.expires_at {
        if now >= expiry.as_str() {
            return false;
        }
    }
    g.tools.iter().any(|t| t == tool || t == "*")
}

/// The grants that apply to `subject` at `now`, in input order.
///
/// Same matching rules as [`allows`] for subject and expiry. Duplicates — the
/// same `(server, tools)` pair appearing twice — are collapsed to their first
/// occurrence, so a caller that stacks an old grant file on a new one gets
/// each rule once and the order it was written in.
pub fn effective_grants<'a>(grants: &'a [Grant], subject: &str, now: &str) -> Vec<&'a Grant> {
    let mut seen: Vec<(&str, &Vec<String>)> = Vec::new();
    let mut out: Vec<&'a Grant> = Vec::new();
    for g in grants {
        if g.subject != subject || g.subject.contains('*') {
            continue;
        }
        if let Some(expiry) = &g.expires_at {
            if now >= expiry.as_str() {
                continue;
            }
        }
        let key = (g.server.as_str(), &g.tools);
        if seen.iter().any(|k| k.0 == key.0 && k.1 == key.1) {
            continue;
        }
        seen.push(key);
        out.push(g);
    }
    out
}

// ── Egress allowlist (Phase-C gate) ──────────────────────────────────────────
//
// Pure domain/IP decision for the future loopback egress proxy. No DNS, no
// sockets, no I/O: the proxy calls `allowlist_match` on the (already
// resolved-and-rechecked) hostname and `is_public_ip_literal` to refuse
// direct IP literals and DNS-rebinding targets. Deny-wins across lists is
// handled by the caller (the proxy), not here.
//
// DNS-rebinding note: matching the hostname is not enough — the proxy MUST
// re-resolve after matching and refuse private/loopback results (see
// `is_public_ip_literal`), or `trusted.com` can rebind to `127.0.0.1`.

/// Normalize a hostname for matching: trim, lowercase, strip one trailing dot.
fn normalize_host(s: &str) -> String {
    let t = s.trim().to_lowercase();
    t.strip_suffix('.').unwrap_or(&t).to_string()
}

/// Whether `domain` is allowed by `patterns`.
///
/// Rules, in order:
/// - empty domain or empty pattern list matches nothing (fail closed);
/// - a domain containing `*` is never a real hostname: refused;
/// - exact entries match exactly (after normalization);
/// - `*.example.com` matches subdomains only (`a.example.com`,
///   `a.b.example.com`) and never the bare `example.com`;
/// - `*` alone, empty entries, and any other entry containing `*` are ignored.
///
/// Matching is case-insensitive with trailing-dot tolerance; deny-wins across
/// separate allow/deny lists is the caller's job.
pub fn allowlist_match(domain: &str, patterns: &[&str]) -> bool {
    let d = normalize_host(domain);
    if d.is_empty() || d.contains('*') || patterns.is_empty() {
        return false;
    }
    for p in patterns {
        let raw = p.trim().to_lowercase();
        let pat = raw.strip_suffix('.').unwrap_or(&raw);
        if pat.is_empty() || pat == "*" {
            continue;
        }
        if let Some(suffix) = pat.strip_prefix("*.") {
            if suffix.is_empty() || suffix.contains('*') {
                continue;
            }
            if d.len() > suffix.len()
                && d.ends_with(suffix)
                && d.as_bytes()[d.len() - suffix.len() - 1] == b'.'
            {
                return true;
            }
        } else {
            if pat.contains('*') {
                continue;
            }
            if d == pat {
                return true;
            }
        }
    }
    false
}

/// True when `s` is exactly four decimal octets (`1-3` digits each, `0-255`).
fn is_ipv4_literal(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.len() <= 3
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u32>().map(|v| v <= 255).unwrap_or(false)
    })
}

/// Octets of a string already known to satisfy `is_ipv4_literal`.
fn ipv4_octets(s: &str) -> [u32; 4] {
    let mut out = [0u32; 4];
    for (i, p) in s.split('.').enumerate().take(4) {
        out[i] = p.parse::<u32>().unwrap_or(256);
    }
    out
}

/// Shape check for an IPv6 literal (brackets/zone already stripped): hex
/// groups, at most one `::`, optional embedded dotted-quad tail.
fn is_ipv6_shape(s: &str) -> bool {
    if s.is_empty() || !s.contains(':') {
        return false;
    }
    if s.chars()
        .any(|c| !(c.is_ascii_hexdigit() || c == ':' || c == '.'))
    {
        return false;
    }
    let (v6part, v4part) = match s.rfind(':') {
        Some(idx) if s[idx + 1..].contains('.') => (&s[..idx], Some(&s[idx + 1..])),
        _ => (s, None),
    };
    if let Some(v4) = v4part {
        if !is_ipv4_literal(v4) {
            return false;
        }
    }
    let mut pieces = v6part.split("::");
    let first = pieces.next().unwrap_or("");
    let second = pieces.next();
    if pieces.next().is_some() {
        return false;
    }
    for part in [first, second.unwrap_or("")] {
        if part.is_empty() {
            continue;
        }
        for g in part.split(':') {
            if g.is_empty() || g.len() > 4 || !g.chars().all(|c| c.is_ascii_hexdigit()) {
                return false;
            }
        }
    }
    let count = |p: &str| {
        if p.is_empty() {
            0
        } else {
            p.split(':').count()
        }
    };
    let groups = count(first) + count(second.unwrap_or("")) + usize::from(v4part.is_some()) * 2;
    if second.is_some() {
        groups < 8
    } else {
        groups == 8
    }
}

/// First non-empty colon group of an IPv6 literal as a `u16`, if any.
fn ipv6_first_group(host: &str) -> Option<u16> {
    for g in host.split(':') {
        if g.is_empty() || g.contains('.') {
            continue;
        }
        if g.len() <= 4 && g.chars().all(|c| c.is_ascii_hexdigit()) {
            return u16::from_str_radix(g, 16).ok();
        }
        return None;
    }
    None
}

/// Whether `s` is an IP literal with a globally routable address.
///
/// - IPv4 dotted quads: `10/8`, `172.16/12`, `192.168/16`, `127/8`,
///   CGNAT `100.64/10`, `0/8` (this-host), `169.254/16` (link-local), and
///   `224.0.0.0/4`+ (multicast/reserved) are NOT public; the rest is.
/// - IPv6 literals (optional `[brackets]`, optional `%zone`): `::`,
///   `::1`, `fe80::/10`, `fc00::/7` (`fc00`/`fd00`), `ff00::/8`, and
///   `::ffff:`-mapped private IPv4 are NOT public; the rest is.
/// - hostnames, empty strings, and malformed numerics are NOT IP literals:
///   returns `false` (fail closed — a hostname is never a "public IP").
pub fn is_public_ip_literal(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    if is_ipv4_literal(t) {
        let o = ipv4_octets(t);
        if o[0] == 10 {
            return false;
        }
        if o[0] == 172 && (16..=31).contains(&o[1]) {
            return false;
        }
        if o[0] == 192 && o[1] == 168 {
            return false;
        }
        if o[0] == 127 {
            return false;
        }
        if o[0] == 100 && (64..=127).contains(&o[1]) {
            return false;
        }
        if o[0] == 0 {
            return false;
        }
        if o[0] == 169 && o[1] == 254 {
            return false;
        }
        if o[0] >= 224 {
            return false;
        }
        return true;
    }
    let bracketed = if t.starts_with('[') && t.ends_with(']') && t.len() >= 2 {
        &t[1..t.len() - 1]
    } else {
        t
    };
    let host = bracketed.split('%').next().unwrap_or("");
    if !host.contains(':') {
        // Hostname, or dotted digits that failed IPv4 validation: never public.
        return false;
    }
    let lower = host.to_lowercase();
    if lower == "::" || lower == "::1" {
        return false;
    }
    // v4-mapped (`::ffff:1.2.3.4`): the embedded quad decides.
    if let Some(idx) = lower.rfind(':') {
        if lower[idx + 1..].contains('.') {
            let raw_tail = host.rsplit(':').next().unwrap_or("");
            if !is_ipv4_literal(raw_tail) {
                return false;
            }
            let o = ipv4_octets(raw_tail);
            return !(o[0] == 10
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || o[0] == 127
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                || o[0] == 0
                || (o[0] == 169 && o[1] == 254)
                || o[0] >= 224);
        }
    }
    if !is_ipv6_shape(host) {
        return false;
    }
    match ipv6_first_group(&lower) {
        Some(g) => {
            if g & 0xffc0 == 0xfe80 {
                return false; // fe80::/10 link-local
            }
            if g & 0xfe00 == 0xfc00 {
                return false; // fc00::/7 unique-local
            }
            if g & 0xff00 == 0xff00 {
                return false; // ff00::/8 multicast
            }
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Capabilities {
        Capabilities {
            filesystem: true,
            network: true,
            gpu: true,
            snapshot: true,
        }
    }

    fn ok_spec() -> SandboxSpec {
        SandboxSpec {
            language: "python".to_string(),
            needs_gpu: false,
            needs_network: false,
            needs_snapshot: false,
            timeout_s: 300,
        }
    }

    struct MockSandbox {
        caps: Capabilities,
        next: u32,
        live: Vec<String>,
    }

    impl MockSandbox {
        fn new(caps: Capabilities) -> Self {
            MockSandbox {
                caps,
                next: 0,
                live: Vec::new(),
            }
        }
    }

    impl SandboxBackend for MockSandbox {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn capabilities(&self) -> Capabilities {
            self.caps
        }

        fn create(&mut self, spec: &SandboxSpec) -> Result<String, String> {
            validate_spec(spec)?;
            requirements_met(self.caps, spec)?;
            self.next += 1;
            let id = format!("mock-{}", self.next);
            self.live.push(id.clone());
            Ok(id)
        }

        fn destroy(&mut self, id: &str) -> Result<(), String> {
            match self.live.iter().position(|x| x == id) {
                Some(i) => {
                    self.live.remove(i);
                    Ok(())
                }
                None => Err(format!("no live sandbox `{id}`")),
            }
        }
    }

    // ── E2B ──────────────────────────────────────────────────────────────

    #[test]
    fn validate_spec_rejects_an_empty_language_naming_the_field() {
        for language in ["", " ", "\t"] {
            let spec = SandboxSpec {
                language: language.to_string(),
                ..ok_spec()
            };
            let err = validate_spec(&spec).unwrap_err();
            assert!(err.contains("language"), "must name the field: {err}");
        }
        assert!(validate_spec(&ok_spec()).is_ok());
    }

    #[test]
    fn validate_spec_bounds_the_timeout_to_one_through_one_hour() {
        for (timeout_s, accepted) in [(0u64, false), (1, true), (3600, true), (3601, false)] {
            let spec = SandboxSpec {
                timeout_s,
                ..ok_spec()
            };
            let got = validate_spec(&spec);
            assert_eq!(got.is_ok(), accepted, "timeout_s={timeout_s}: {got:?}");
            if let Err(err) = got {
                assert!(err.contains("timeout_s"), "must name the field: {err}");
                assert!(err.contains("1..=3600"), "must name the range: {err}");
                assert!(
                    err.contains(&timeout_s.to_string()),
                    "must name the value: {err}"
                );
            }
        }
    }

    #[test]
    fn requirements_met_names_the_first_missing_capability_in_fixed_order() {
        let spec = SandboxSpec {
            needs_network: true,
            needs_gpu: true,
            needs_snapshot: true,
            ..ok_spec()
        };
        let none = Capabilities::default();
        let err = requirements_met(none, &spec).unwrap_err();
        assert!(err.contains("filesystem"), "{err}");

        let filesystem = Capabilities {
            filesystem: true,
            ..none
        };
        let err = requirements_met(filesystem, &spec).unwrap_err();
        assert!(err.contains("network"), "{err}");
        assert!(!err.contains("gpu") && !err.contains("snapshot"), "{err}");

        let network = Capabilities {
            network: true,
            ..filesystem
        };
        let err = requirements_met(network, &spec).unwrap_err();
        assert!(err.contains("gpu"), "{err}");
        assert!(!err.contains("snapshot"), "{err}");

        let gpu = Capabilities {
            gpu: true,
            ..network
        };
        let err = requirements_met(gpu, &spec).unwrap_err();
        assert!(err.contains("snapshot"), "{err}");

        assert!(requirements_met(full(), &spec).is_ok());
    }

    #[test]
    fn requirements_met_only_checks_the_declared_needs() {
        let bare = Capabilities {
            filesystem: true,
            ..Capabilities::default()
        };
        assert!(
            requirements_met(bare, &ok_spec()).is_ok(),
            "needs_* are declarative: a spec that asks for nothing needs only a filesystem"
        );
        let snapshot_spec = SandboxSpec {
            needs_snapshot: true,
            ..ok_spec()
        };
        assert!(requirements_met(bare, &snapshot_spec).is_err());
        assert!(requirements_met(full(), &snapshot_spec).is_ok());
    }

    #[test]
    fn requirements_met_validates_the_spec_before_the_backend() {
        let bad = SandboxSpec {
            language: " ".to_string(),
            ..ok_spec()
        };
        let err = requirements_met(full(), &bad).unwrap_err();
        assert!(err.contains("language"), "{err}");
    }

    #[test]
    fn a_backend_refuses_a_spec_its_capabilities_cannot_serve() {
        let mut backend = MockSandbox::new(Capabilities {
            filesystem: true,
            ..Capabilities::default()
        });
        assert_eq!(backend.name(), "mock");
        assert!(!backend.capabilities().gpu);

        let gpu_spec = SandboxSpec {
            needs_gpu: true,
            ..ok_spec()
        };
        let err = backend.create(&gpu_spec).unwrap_err();
        assert!(err.contains("gpu"), "{err}");
        assert!(
            backend.live.is_empty(),
            "a refused create must not leave a sandbox behind"
        );

        let first = backend.create(&ok_spec()).unwrap();
        let second = backend.create(&ok_spec()).unwrap();
        assert_ne!(first, second, "ids are distinct per sandbox");
        assert_eq!(backend.live.len(), 2);
        backend.destroy(&first).unwrap();
        assert_eq!(backend.live, vec![second.clone()]);
        let err = backend.destroy(&first).unwrap_err();
        assert!(err.contains(&first), "an unknown id is named: {err}");
    }

    #[test]
    fn a_full_capability_backend_serves_every_spec() {
        let mut backend = MockSandbox::new(full());
        let demanding = SandboxSpec {
            language: "node".to_string(),
            needs_network: true,
            needs_gpu: true,
            needs_snapshot: true,
            timeout_s: 3600,
        };
        let id = backend.create(&demanding).unwrap();
        backend.destroy(&id).unwrap();
        assert!(backend.live.is_empty());
    }

    // ── Daytona / workspace providers ────────────────────────────────────

    #[test]
    fn is_remote_is_true_for_daytona_and_e2b_only() {
        let table = [
            (WorkspaceProvider::Local, false),
            (WorkspaceProvider::Docker, false),
            (WorkspaceProvider::Daytona, true),
            (WorkspaceProvider::E2b, true),
        ];
        for (provider, expected) in table {
            assert_eq!(provider.is_remote(), expected, "{}", provider.as_str());
        }
        assert_eq!(table.len(), WorkspaceProvider::ALL.len());
    }

    #[test]
    fn provider_parse_round_trips_all_and_lists_all_on_unknown() {
        for p in WorkspaceProvider::ALL {
            assert_eq!(WorkspaceProvider::parse(p.as_str()).unwrap(), p);
        }
        for spelling in ["Daytona", "  DAYTONA ", "e2b", "E2B", "e-2-b", "e_2_b"] {
            assert!(
                WorkspaceProvider::parse(spelling).is_ok(),
                "`{spelling}` should parse"
            );
        }
        let err = WorkspaceProvider::parse("sandbox").unwrap_err();
        for p in WorkspaceProvider::ALL {
            assert!(err.contains(p.as_str()), "error must list {p:?}: {err}");
        }
        assert!(err.contains("sandbox"), "error must name the value: {err}");
    }

    #[test]
    fn validate_provider_requires_an_image_for_every_non_local_provider() {
        let without_image = |provider| ProviderSpec {
            provider,
            image: None,
            cpu: 2,
            memory_mb: 2048,
        };
        for provider in [
            WorkspaceProvider::Docker,
            WorkspaceProvider::Daytona,
            WorkspaceProvider::E2b,
        ] {
            let err = validate_provider(&without_image(provider)).unwrap_err();
            assert!(err.contains("image"), "must name the field: {err}");
            assert!(
                err.contains(provider.as_str()),
                "must name the provider: {err}"
            );
        }
        assert!(
            validate_provider(&without_image(WorkspaceProvider::Local)).is_ok(),
            "local runs on the host and needs no image"
        );
    }

    #[test]
    fn validate_provider_refuses_an_image_for_local_as_meaningless() {
        let spec = ProviderSpec {
            provider: WorkspaceProvider::Local,
            image: Some("alpine:3".to_string()),
            cpu: 2,
            memory_mb: 2048,
        };
        let err = validate_provider(&spec).unwrap_err();
        assert!(err.contains("local"), "{err}");
        assert!(err.contains("alpine:3"), "must name the value: {err}");
    }

    #[test]
    fn validate_provider_bounds_cpu_and_memory_naming_field_and_range() {
        let base = ProviderSpec {
            provider: WorkspaceProvider::Docker,
            image: Some("alpine:3".to_string()),
            cpu: 2,
            memory_mb: 2048,
        };
        for (cpu, accepted) in [(0u32, false), (1, true), (64, true), (65, false)] {
            let got = validate_provider(&ProviderSpec {
                cpu,
                ..base.clone()
            });
            assert_eq!(got.is_ok(), accepted, "cpu={cpu}: {got:?}");
            if let Err(err) = got {
                assert!(err.contains("cpu") && err.contains("1..=64"), "{err}");
                assert!(err.contains(&cpu.to_string()), "{err}");
            }
        }
        for (memory_mb, accepted) in [
            (127u32, false),
            (128, true),
            (262144, true),
            (262145, false),
        ] {
            let got = validate_provider(&ProviderSpec {
                memory_mb,
                ..base.clone()
            });
            assert_eq!(got.is_ok(), accepted, "memory_mb={memory_mb}: {got:?}");
            if let Err(err) = got {
                assert!(
                    err.contains("memory_mb") && err.contains("128..=262144"),
                    "{err}"
                );
                assert!(err.contains(&memory_mb.to_string()), "{err}");
            }
        }
        assert!(validate_provider(&base).is_ok());
    }

    #[test]
    fn validate_provider_rejects_a_blank_image_even_where_one_is_required() {
        let spec = ProviderSpec {
            provider: WorkspaceProvider::Daytona,
            image: Some("   ".to_string()),
            cpu: 2,
            memory_mb: 2048,
        };
        let err = validate_provider(&spec).unwrap_err();
        assert!(err.contains("image"), "{err}");
    }

    // ── sandbox runtimes ─────────────────────────────────────────────────

    #[test]
    fn runtime_parse_accepts_every_platform_alias() {
        let table = [
            ("native", SandboxRuntime::Native),
            ("Native", SandboxRuntime::Native),
            ("seatbelt", SandboxRuntime::Seatbelt),
            ("sandbox-exec", SandboxRuntime::Seatbelt),
            ("SANDBOX_EXEC", SandboxRuntime::Seatbelt),
            ("bwrap", SandboxRuntime::Bubblewrap),
            ("Bubblewrap", SandboxRuntime::Bubblewrap),
            (" gvisor ", SandboxRuntime::Gvisor),
            ("runsc", SandboxRuntime::Gvisor),
            ("Run-Sc", SandboxRuntime::Gvisor),
        ];
        for (spelling, expected) in table {
            assert_eq!(
                SandboxRuntime::parse(spelling).unwrap(),
                expected,
                "`{spelling}`"
            );
        }
        for r in SandboxRuntime::ALL {
            assert_eq!(SandboxRuntime::parse(r.as_str()).unwrap(), r);
        }
    }

    #[test]
    fn runtime_parse_unknown_error_lists_all() {
        let err = SandboxRuntime::parse("firejail").unwrap_err();
        for r in SandboxRuntime::ALL {
            assert!(err.contains(r.as_str()), "error must list {r:?}: {err}");
        }
        assert!(err.contains("firejail"), "error must name the value: {err}");
    }

    #[test]
    fn runtime_requirements_name_the_binary_and_only_gvisor_needs_a_bundle() {
        assert_eq!(
            requirement(SandboxRuntime::Seatbelt).binary,
            Some("/usr/bin/sandbox-exec")
        );
        assert_eq!(
            requirement(SandboxRuntime::Bubblewrap).binary,
            Some("bwrap")
        );
        assert_eq!(requirement(SandboxRuntime::Gvisor).binary, Some("runsc"));
        assert_eq!(requirement(SandboxRuntime::Native).binary, None);
        for r in [
            SandboxRuntime::Native,
            SandboxRuntime::Seatbelt,
            SandboxRuntime::Bubblewrap,
        ] {
            assert!(!requirement(r).needs_bundle, "{r:?} needs no bundle");
        }
        assert!(requirement(SandboxRuntime::Gvisor).needs_bundle);
        assert!(requirement(SandboxRuntime::Native)
            .note
            .contains("unsandboxed"));
        for r in SandboxRuntime::ALL {
            assert_eq!(requirement(r).runtime, r, "the table is total");
        }
    }

    #[test]
    fn select_runtime_none_follows_the_preference_order() {
        let all = SandboxRuntime::ALL;
        let (chosen, note) = select_runtime(None, &all).unwrap();
        assert_eq!(chosen, SandboxRuntime::Gvisor);
        assert_eq!(note, None, "the top preference is not a fallback");

        let (chosen, _) =
            select_runtime(None, &[SandboxRuntime::Native, SandboxRuntime::Seatbelt]).unwrap();
        assert_eq!(
            chosen,
            SandboxRuntime::Seatbelt,
            "preference order beats the caller's listing order"
        );

        let (chosen, _) =
            select_runtime(None, &[SandboxRuntime::Native, SandboxRuntime::Bubblewrap]).unwrap();
        assert_eq!(chosen, SandboxRuntime::Bubblewrap);

        let (chosen, note) = select_runtime(None, &[SandboxRuntime::Gvisor]).unwrap();
        assert_eq!(chosen, SandboxRuntime::Gvisor);
        assert_eq!(note, None);
    }

    #[test]
    fn select_runtime_note_is_some_only_for_a_fallback_or_native() {
        // Honored request to a sandboxing runtime: no note.
        let (chosen, note) = select_runtime(Some("bwrap"), &[SandboxRuntime::Bubblewrap]).unwrap();
        assert_eq!(chosen, SandboxRuntime::Bubblewrap);
        assert_eq!(note, None);

        // Fallback: the note names what was missing.
        let (chosen, note) =
            select_runtime(None, &[SandboxRuntime::Bubblewrap, SandboxRuntime::Native]).unwrap();
        assert_eq!(chosen, SandboxRuntime::Bubblewrap);
        let note = note.expect("a fallback is reported");
        assert!(note.contains("gvisor") && note.contains("runsc"), "{note}");

        // Native is always reported, requested or fallen back to.
        let (chosen, note) = select_runtime(Some("native"), &[SandboxRuntime::Native]).unwrap();
        assert_eq!(chosen, SandboxRuntime::Native);
        let note = note.expect("native execution is reported");
        assert!(note.contains("unsandboxed"), "{note}");

        let (chosen, note) = select_runtime(None, &[SandboxRuntime::Native]).unwrap();
        assert_eq!(chosen, SandboxRuntime::Native);
        let note = note.expect("native execution is reported");
        assert!(note.contains("unsandboxed"), "{note}");
    }

    #[test]
    fn select_runtime_requested_but_unavailable_errors_instead_of_downgrading() {
        let got = select_runtime(Some("gvisor"), &[SandboxRuntime::Native]);
        let err = got.unwrap_err();
        assert!(err.contains("gvisor"), "{err}");
        assert!(err.contains("runsc"), "must name the missing binary: {err}");
        assert!(err.contains("never falls back"), "{err}");

        let err = select_runtime(Some("bwrap"), &[]).unwrap_err();
        assert!(err.contains("bwrap"), "{err}");

        let err = select_runtime(Some("firejail"), &SandboxRuntime::ALL).unwrap_err();
        for r in SandboxRuntime::ALL {
            assert!(err.contains(r.as_str()), "{err}");
        }
        let err = select_runtime(Some("native"), &[SandboxRuntime::Gvisor]).unwrap_err();
        assert!(err.contains("native"), "{err}");
    }

    #[test]
    fn select_runtime_with_nothing_available_refuses_rather_than_picking_native() {
        let err = select_runtime(None, &[]).unwrap_err();
        assert!(err.contains("empty"), "{err}");
        assert!(err.contains("never picked silently"), "{err}");
    }

    #[test]
    fn check_runtime_gvisor_names_the_binary_and_the_bundle_separately() {
        let err = check_runtime(SandboxRuntime::Gvisor, false, false).unwrap_err();
        assert!(err.contains("runsc"), "{err}");
        assert!(err.contains(GVISOR_BUNDLE_DIR), "{err}");

        let err = check_runtime(SandboxRuntime::Gvisor, false, true).unwrap_err();
        assert!(err.contains("runsc"), "{err}");
        assert!(
            !err.contains(GVISOR_BUNDLE_DIR),
            "a present bundle is not reported missing: {err}"
        );

        let err = check_runtime(SandboxRuntime::Gvisor, true, false).unwrap_err();
        assert!(err.contains(GVISOR_BUNDLE_DIR), "{err}");
        assert!(!err.contains("not on PATH"), "{err}");

        assert!(check_runtime(SandboxRuntime::Gvisor, true, true).is_ok());
    }

    #[test]
    fn check_runtime_others_need_only_their_binary_and_native_never_fails() {
        assert!(check_runtime(SandboxRuntime::Bubblewrap, true, false).is_ok());
        let err = check_runtime(SandboxRuntime::Bubblewrap, false, true).unwrap_err();
        assert!(err.contains("bwrap"), "{err}");
        assert!(check_runtime(SandboxRuntime::Seatbelt, true, false).is_ok());
        let err = check_runtime(SandboxRuntime::Seatbelt, false, false).unwrap_err();
        assert!(err.contains("/usr/bin/sandbox-exec"), "{err}");
        for bundle in [false, true] {
            assert!(check_runtime(SandboxRuntime::Native, false, bundle).is_ok());
        }
    }

    // ── ToolHive ─────────────────────────────────────────────────────────

    fn server_spec(allow_network: bool, mounts: &[&str], env_keys: &[&str]) -> McpServerSpec {
        McpServerSpec {
            name: "fs-server".to_string(),
            image: "ghcr.io/example/fs:1".to_string(),
            permissions: GrantSet {
                allow_network,
                mounts: mounts.iter().map(|m| m.to_string()).collect(),
                env_keys: env_keys.iter().map(|k| k.to_string()).collect(),
            },
        }
    }

    #[test]
    fn grant_spawn_rejects_an_empty_name_or_image_naming_the_field() {
        let mut spec = server_spec(false, &[], &[]);
        spec.name = "  ".to_string();
        let err = grant_spawn(&spec, "/work").unwrap_err();
        assert!(err.contains("name"), "{err}");

        let mut spec = server_spec(false, &[], &[]);
        spec.image = String::new();
        let err = grant_spawn(&spec, "/work").unwrap_err();
        assert!(err.contains("image"), "{err}");
    }

    #[test]
    fn grant_spawn_rejects_a_relative_mount_naming_the_path() {
        let spec = server_spec(false, &["work/data"], &[]);
        let err = grant_spawn(&spec, "/work").unwrap_err();
        assert!(err.contains("work/data"), "must name the mount: {err}");
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn grant_spawn_rejects_a_mount_outside_the_allowed_root_naming_the_path() {
        for outside in ["/etc/passwd", "/work/../etc", "/workshop"] {
            let spec = server_spec(false, &[outside], &[]);
            let err = grant_spawn(&spec, "/work").unwrap_err();
            assert!(err.contains(outside), "must name the mount: {err}");
            assert!(err.contains("/work"), "must name the root: {err}");
        }
        let spec = server_spec(false, &["/work/../etc"], &[]);
        let err = grant_spawn(&spec, "/work").unwrap_err();
        assert!(err.contains("outside"), "{err}");
    }

    #[test]
    fn grant_spawn_accepts_the_root_itself_and_paths_beneath_it() {
        let spec = server_spec(false, &["/work", "/work/data", "/work/a/b"], &[]);
        let plan = grant_spawn(&spec, "/work/").unwrap();
        assert!(plan.args.contains(&"/work:/work".to_string()));
        assert!(plan.args.contains(&"/work/data:/work/data".to_string()));

        let err = grant_spawn(&server_spec(false, &["/work"], &[]), "work").unwrap_err();
        assert!(err.contains("allowed root"), "{err}");
    }

    #[test]
    fn grant_spawn_rejects_env_keys_that_are_not_selectors() {
        for bad in ["api_key", "1BAD", "API-KEY", "API KEY", "_KEY"] {
            let spec = server_spec(false, &[], &[bad]);
            let err = grant_spawn(&spec, "/work").unwrap_err();
            assert!(err.contains(bad), "must name the key: {err}");
            assert!(
                err.contains("[A-Z][A-Z0-9_]*"),
                "must teach the shape: {err}"
            );
        }
        assert!(grant_spawn(
            &server_spec(false, &[], &["API_KEY", "HOME", "K1"]),
            "/work"
        )
        .is_ok());
    }

    #[test]
    fn grant_spawn_rejects_a_key_carrying_a_value() {
        let spec = server_spec(false, &[], &["API_KEY=secret"]);
        let err = grant_spawn(&spec, "/work").unwrap_err();
        assert!(err.contains("API_KEY=secret"), "must name the key: {err}");
        assert!(err.contains("keys only"), "must state the invariant: {err}");
    }

    #[test]
    fn grant_spawn_args_are_deterministic_with_mounts_and_keys_sorted() {
        let one = grant_spawn(
            &server_spec(false, &["/work/z", "/work/a"], &["HOME", "API_KEY"]),
            "/work",
        )
        .unwrap();
        let two = grant_spawn(
            &server_spec(false, &["/work/a", "/work/z"], &["API_KEY", "HOME"]),
            "/work",
        )
        .unwrap();
        assert_eq!(one, two, "equal specs give byte-identical plans");
        assert_eq!(
            one.args,
            vec![
                "run",
                "--name",
                "fs-server",
                "--network=none",
                "-v",
                "/work/a:/work/a",
                "-v",
                "/work/z:/work/z",
                "-e",
                "API_KEY",
                "-e",
                "HOME",
                "ghcr.io/example/fs:1",
            ]
        );
        assert_eq!(one.server, "fs-server");
        assert_eq!(one.env_keys, vec!["API_KEY", "HOME"]);
    }

    #[test]
    fn grant_spawn_plan_carries_keys_only_never_values() {
        let plan = grant_spawn(
            &server_spec(true, &["/work/data"], &["API_KEY", "TOKEN"]),
            "/work",
        )
        .unwrap();
        assert_eq!(plan.env_keys, vec!["API_KEY", "TOKEN"]);
        for (i, arg) in plan.args.iter().enumerate() {
            if arg == "-e" {
                let key = &plan.args[i + 1];
                assert!(
                    !key.contains('='),
                    "an `-e` value must be a bare key, got `{key}`"
                );
            }
        }
        assert!(
            !plan
                .args
                .iter()
                .any(|a| a.contains('=') && !a.starts_with("--network=")),
            "only the network flag may carry a value: {:?}",
            plan.args
        );
    }

    #[test]
    fn grant_spawn_network_flag_follows_allow_network() {
        let off = grant_spawn(&server_spec(false, &[], &[]), "/work").unwrap();
        assert!(off.args.contains(&"--network=none".to_string()));
        assert_eq!(off.env_keys, Vec::<String>::new());
        let on = grant_spawn(&server_spec(true, &[], &[]), "/work").unwrap();
        assert!(on.args.contains(&"--network=host".to_string()));
        assert!(!on.args.contains(&"--network=none".to_string()));
    }

    #[test]
    fn grant_spawn_collapses_duplicate_mounts_and_keys() {
        let plan = grant_spawn(
            &server_spec(false, &["/work/a", "/work/a"], &["HOME", "HOME"]),
            "/work",
        )
        .unwrap();
        assert_eq!(plan.env_keys, vec!["HOME"]);
        assert_eq!(
            plan.args.iter().filter(|a| *a == "-v").count(),
            1,
            "a duplicated mount is listed once: {:?}",
            plan.args
        );
    }

    // ── ContextForge ─────────────────────────────────────────────────────

    fn grant(subject: &str, server: &str, tools: &[&str], expires_at: Option<&str>) -> Grant {
        Grant {
            subject: subject.to_string(),
            server: server.to_string(),
            tools: tools.iter().map(|t| t.to_string()).collect(),
            expires_at: expires_at.map(str::to_string),
        }
    }

    #[test]
    fn validate_grant_rejects_empty_subject_server_and_tool_entries() {
        let err = validate_grant(&grant("  ", "fs", &["read"], None)).unwrap_err();
        assert!(err.contains("subject"), "{err}");
        let err = validate_grant(&grant("alice", "", &["read"], None)).unwrap_err();
        assert!(err.contains("server"), "{err}");
        let err = validate_grant(&grant("alice", "fs", &[], None)).unwrap_err();
        assert!(err.contains("tool"), "{err}");
        let err = validate_grant(&grant("alice", "fs", &["read", " "], None)).unwrap_err();
        assert!(err.contains("tool"), "{err}");
        assert!(validate_grant(&grant("alice", "fs", &["read"], None)).is_ok());
    }

    #[test]
    fn validate_grant_requires_the_stamp_shape_when_an_expiry_is_present() {
        for bad in [
            "2025-01-01",
            "2025-01-01T00:00:00",
            "2025-01-01 00:00:00Z",
            "2025-01-01T00:00:00z",
            "2025-1-01T00:00:00Z",
            "2025x01x01T00:00:00Z",
            "2025-01-01T00:00:00ZC",
            "01/01/2025T00:00:00Z",
            "",
        ] {
            let err = validate_grant(&grant("alice", "fs", &["read"], Some(bad))).unwrap_err();
            assert!(
                err.contains(bad) || bad.is_empty(),
                "must name the value: {err}"
            );
            assert!(
                err.contains("YYYY-MM-DDTHH:MM:SSZ"),
                "must teach the shape: {err}"
            );
        }
        assert!(validate_grant(&grant(
            "alice",
            "fs",
            &["read"],
            Some("2025-01-01T00:00:00Z")
        ))
        .is_ok());
    }

    #[test]
    fn validate_grant_refuses_a_wildcard_subject_or_server() {
        let err = validate_grant(&grant("*", "fs", &["read"], None)).unwrap_err();
        assert!(err.contains('*') && err.contains("subject"), "{err}");
        let err = validate_grant(&grant("alice", "*", &["read"], None)).unwrap_err();
        assert!(err.contains('*') && err.contains("server"), "{err}");
        assert!(
            validate_grant(&grant("alice", "fs", &["*"], None)).is_ok(),
            "the wildcard is a tool entry, not a subject or server"
        );
    }

    #[test]
    fn allows_matches_subject_and_server_exactly() {
        let g = grant("alice", "fs", &["read"], None);
        assert!(allows(&g, "alice", "fs", "read", "2025-01-01T00:00:00Z"));
        assert!(!allows(&g, "alice2", "fs", "read", "2025-01-01T00:00:00Z"));
        assert!(!allows(&g, "alice", "fsx", "read", "2025-01-01T00:00:00Z"));
        assert!(!allows(&g, "alice", "fs", "write", "2025-01-01T00:00:00Z"));
        assert!(
            !allows(
                &grant("alice", "*", &["read"], None),
                "alice",
                "*",
                "read",
                "x"
            ),
            "an unvalidated wildcard row can never authorize"
        );
        assert!(!allows(
            &grant("*", "fs", &["read"], None),
            "*",
            "fs",
            "read",
            "x"
        ));
    }

    #[test]
    fn allows_honors_only_the_single_wildcard_tool_entry() {
        let wildcard = grant("alice", "fs", &["*"], None);
        assert!(allows(
            &wildcard,
            "alice",
            "fs",
            "read",
            "2025-01-01T00:00:00Z"
        ));
        assert!(allows(
            &wildcard,
            "alice",
            "fs",
            "any.thing-else",
            "2025-01-01T00:00:00Z"
        ));
        let two = grant("alice", "fs", &["read", "edit"], None);
        assert!(allows(&two, "alice", "fs", "edit", "2025-01-01T00:00:00Z"));
        assert!(!allows(
            &two,
            "alice",
            "fs",
            "delete",
            "2025-01-01T00:00:00Z"
        ));
        assert!(!allows(
            &grant("alice", "fs", &["read*"], None),
            "alice",
            "fs",
            "read-everything",
            "2025-01-01T00:00:00Z"
        ));
    }

    #[test]
    fn allows_treats_expiry_as_an_exclusive_upper_bound() {
        let expiring = grant("alice", "fs", &["read"], Some("2025-06-01T00:00:00Z"));
        assert!(allows(
            &expiring,
            "alice",
            "fs",
            "read",
            "2025-05-31T23:59:59Z"
        ));
        assert!(
            !allows(&expiring, "alice", "fs", "read", "2025-06-01T00:00:00Z"),
            "the grant is dead at its stamp"
        );
        assert!(!allows(
            &expiring,
            "alice",
            "fs",
            "read",
            "2025-06-02T00:00:00Z"
        ));
        let forever = grant("alice", "fs", &["read"], None);
        assert!(allows(
            &forever,
            "alice",
            "fs",
            "read",
            "2099-01-01T00:00:00Z"
        ));
    }

    #[test]
    fn effective_grants_filters_other_subjects_and_expired_rows_keeping_order() {
        let grants = vec![
            grant("alice", "fs", &["read"], None),
            grant("bob", "fs", &["read"], None),
            grant("alice", "git", &["push"], Some("2025-01-01T00:00:00Z")),
            grant("alice", "web", &["fetch"], Some("2030-01-01T00:00:00Z")),
        ];
        let live = effective_grants(&grants, "alice", "2025-06-01T00:00:00Z");
        let servers: Vec<&str> = live.iter().map(|g| g.server.as_str()).collect();
        assert_eq!(servers, vec!["fs", "web"]);
        assert_eq!(live.len(), 2);
        assert!(effective_grants(&grants, "carol", "2025-06-01T00:00:00Z").is_empty());
        assert!(
            effective_grants(&grants, "*", "2025-06-01T00:00:00Z").is_empty(),
            "a wildcard requester matches nothing"
        );
    }

    #[test]
    fn effective_grants_dedupes_by_server_and_tools_keeping_the_first() {
        let grants = vec![
            grant("alice", "fs", &["read"], None),
            grant("alice", "fs", &["read"], Some("2030-01-01T00:00:00Z")),
            grant("alice", "fs", &["read", "edit"], None),
            grant("alice", "git", &["read"], None),
        ];
        let live = effective_grants(&grants, "alice", "2025-06-01T00:00:00Z");
        assert_eq!(
            live.len(),
            3,
            "the duplicate (server, tools) pair is dropped"
        );
        assert_eq!(live[0].tools, vec!["read"]);
        assert_eq!(
            live[0].expires_at, None,
            "the first occurrence survives, not the later one"
        );
        assert_eq!(live[1].tools, vec!["read", "edit"]);
        assert_eq!(live[2].server, "git");
    }

    // ── Egress allowlist ─────────────────────────────────────────────────

    #[test]
    fn allowlist_matches_exact_and_subdomains_but_not_the_bare_domain() {
        assert!(allowlist_match("api.example.com", &["api.example.com"]));
        assert!(allowlist_match("a.example.com", &["*.example.com"]));
        assert!(allowlist_match("a.b.example.com", &["*.example.com"]));
        assert!(
            !allowlist_match("example.com", &["*.example.com"]),
            "a wildcard never authorizes the bare domain"
        );
        assert!(!allowlist_match("evil.com", &["*.example.com"]));
        assert!(!allowlist_match("example.com.evil.com", &["*.example.com"]));
        assert!(!allowlist_match("example.com", &["api.example.com"]));
    }

    #[test]
    fn allowlist_star_is_always_refused_and_lists_are_deny_open() {
        assert!(!allowlist_match("anything.com", &["*"]));
        assert!(!allowlist_match("api.example.com", &["*"]));
        assert!(
            !allowlist_match("api.example.com", &[]),
            "empty list denies"
        );
        assert!(
            !allowlist_match("", &["api.example.com"]),
            "empty domain denies"
        );
        assert!(
            !allowlist_match("api.example.com", &[""]),
            "empty entry denies"
        );
        assert!(
            !allowlist_match("*", &["*"]),
            "a wildcard domain is never real"
        );
        assert!(
            !allowlist_match("api.example.com", &["api.*", "pre-*.example.com"]),
            "partial wildcards are refused, not matched"
        );
        assert!(
            allowlist_match(" api.EXAMPLE.com. ", &["API.example.COM"]),
            "matching is case-insensitive with trim/trailing-dot tolerance"
        );
        // Deny-wins is the caller's composition, not the matcher's: the
        // matcher answers "is it listed", the proxy intersects allow minus deny.
        let allowed = allowlist_match("a.example.com", &["*.example.com"]);
        let denied = allowlist_match("a.example.com", &["a.example.com"]);
        assert!(allowed && denied, "both lists hit; the proxy must deny");
    }

    #[test]
    fn public_ip_refuses_private_loopback_and_hostnames() {
        assert!(is_public_ip_literal("8.8.8.8"));
        assert!(is_public_ip_literal("1.1.1.1"));
        assert!(!is_public_ip_literal("10.0.0.5"), "RFC1918 10/8");
        assert!(
            !is_public_ip_literal("172.16.0.1"),
            "RFC1918 172.16/12 low edge"
        );
        assert!(
            !is_public_ip_literal("172.31.255.255"),
            "RFC1918 172.16/12 high edge"
        );
        assert!(
            is_public_ip_literal("172.32.0.1"),
            "just above 172.16/12 is public"
        );
        assert!(!is_public_ip_literal("192.168.1.1"), "RFC1918 192.168/16");
        assert!(!is_public_ip_literal("127.0.0.1"), "loopback");
        assert!(
            !is_public_ip_literal("100.64.0.1"),
            "CGNAT 100.64/10 low edge"
        );
        assert!(
            !is_public_ip_literal("100.127.255.255"),
            "CGNAT 100.64/10 high edge"
        );
        assert!(
            is_public_ip_literal("100.128.0.1"),
            "just above CGNAT is public"
        );
        assert!(!is_public_ip_literal("0.0.0.0"), "this-host");
        assert!(
            !is_public_ip_literal("169.254.169.254"),
            "link-local metadata"
        );
        assert!(!is_public_ip_literal("224.0.0.1"), "multicast");
        assert!(
            !is_public_ip_literal("example.com"),
            "a hostname is never a public IP"
        );
        assert!(!is_public_ip_literal(""), "empty is never public");
        assert!(
            !is_public_ip_literal("999.1.1.1"),
            "malformed numerics are never public"
        );
        assert!(
            is_public_ip_literal("2606:4700:4700::1111"),
            "global unicast v6"
        );
        assert!(!is_public_ip_literal("::1"), "v6 loopback");
        assert!(!is_public_ip_literal("fe80::1"), "v6 link-local");
        assert!(!is_public_ip_literal("fc00::1"), "v6 unique-local low");
        assert!(!is_public_ip_literal("fd00::1"), "v6 unique-local high");
        assert!(!is_public_ip_literal("ff02::1"), "v6 multicast");
        assert!(
            !is_public_ip_literal("::ffff:127.0.0.1"),
            "a v4-mapped private quad is still private: the rebinding target"
        );
        assert!(
            is_public_ip_literal("::ffff:8.8.8.8"),
            "a v4-mapped public quad stays public"
        );
    }
}
