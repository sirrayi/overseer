## 19. Extensibility and Ecosystem

### 19.1 Where we stand

Skills (metadata resident, bodies on demand), MCP servers with trust lanes, `hooks.json` data rules, microagents, six built-in modes **[have]**. No plugin model, no versioning or trust tiers for extensions, no project MCP configuration, no SDK.

### 19.2 Workstreams

#### Y1 — A plugin model

**What.** One packaging format that bundles skills, modes, contextual rules, hooks, MCP server declarations, workflow templates and persistent agent definitions, so a capability ships as one unit.

**Design.** A directory with a manifest (`plugin.toml`: name, version, author, declared capabilities and the permissions they need), installed under `~/.overseer/plugins/`. Plugins contribute through the policy compiler (P2), so precedence, provenance and explanations apply; a plugin cannot loosen owner policy. No plugin executes code in the harness process: code runs only as sandboxed hook scripts or MCP servers.

#### Y2 — Versioning and trust tiers

**What.** Every extension (skill, plugin, MCP server, hook) has a version and a trust tier: `builtin`, `owner` (authored or explicitly trusted by the owner), `project` (hash-pinned trust per repository), `untrusted` (content only, never instructions). Learned skills (M2) have their own lifecycle and ledger.

#### Y3 — Project-level MCP

**What.** T5's project `mcp.json` with hash-pinned approval.

#### Y4 — An embedding SDK and peer agents

**What.** A stable library surface for running Overseer's engine inside other programs (Overseer Life is the first consumer), with the wire protocol for out-of-process clients; peer-runtime subagents over ACP so Overseer can delegate to, or be delegated by, other harnesses **[src: DeepSeek harness]**.

#### Y5 — The hooks protocol

**What.** P10's typed events and JSON protocol, documented and versioned so third-party hooks keep working across releases.

#### Y6 — Sharing

**What.** Publishing and installing plugins from git URLs with hash pinning and a review screen showing every capability and permission requested. No central marketplace until the owner decides the product phase has begun.
