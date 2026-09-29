//! Sandbox-runtime selection port (P8-C) — the `--runtime` decision.
//!
//! Pure: validated data plus the deterministic decision made at run time.
//! The engine links no container runtime or hypervisor, so what lands here
//! is the *choice* and the honest refusal that must precede it:
//! `SandboxRuntime::parse` for the `--runtime` flag, `requirement`, and
//! `check_runtime`. A requested-but-unavailable runtime is an error naming
//! the missing binary; there is no silent downgrade to unsandboxed native
//! execution, which is the entire point of the port. Every error names the
//! offending value so the caller can repair it.
//!
//! `// DEFERRED(owner): spawning `runsc` and preparing the gVisor OCI bundle
//! at `<root>/.overseer/runsc-bundle` (no runsc argv is invented here).`
// DEFERRED(owner): removed unused ports — E2B sandbox backends (SandboxBackend, SandboxSpec, Capabilities, validate_spec, requirements_met), Daytona workspace providers (WorkspaceProvider, ProviderSpec, validate_provider), select_runtime, the ToolHive grant_spawn block, ContextForge grants and the egress-allowlist block (allowlist_match, is_public_ip_literal); restore from git history (base 29fa7ce).

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
