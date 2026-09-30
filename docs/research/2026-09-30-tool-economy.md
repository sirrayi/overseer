# Tool economy: deferred tools and code-mode (decision record, 2026-09-30)

This record supersedes §2–3 of `2026-09-29-pillars.md` wherever the two disagree. The tags are the ones defined in `2026-09-30-memory-v2.md`.

## Problem, measured

The resident tool specs total **9,886 chars (~2.5K tokens)** with every optional tool on and MCP configured. This comes from the per-spec printout of `resident_tool_specs_stay_within_the_startup_budget`:

| Tool | Chars |
|---|---|
| bash | 614 |
| read | 512 |
| write | 426 |
| edit | 773 |
| grep | 613 |
| glob | 549 |
| plan | 686 |
| task | 768 |
| skill | 353 |
| repo_map | 251 |
| symbol | 286 |
| computer | 1,828 |
| diagnostics | 570 |
| struct_search | 1,028 |
| mcp | 629 |

The target is ≤ 1.5K tokens resident. For comparison, pi carries ~1.1K tokens in total, Codex ~8.5K and Claude Code 27–33K [src, 09-29 note].

## Evidence

**Deferral**
- Anthropic's Tool Search Tool (`defer_loading`) cut token use by 85% on its example workload. On MCP evals, Opus 4 went 49% → 74% and Opus 4.5 went 79.5% → 88.1% [vendor].
- Deferred definitions are expanded into the conversation, never into the cached prefix. The system prompt and core tool definitions stay cacheable [doc: platform.claude.com tool-search-tool, tool-reference].
- RAG-MCP (top-k tool retrieval) halved prompt tokens (2,134 → 1,084) and raised selection accuracy from 13.6% to 43.1% [paper: Gan et al. 2025].
- "Less is More": an 8B model fails with 46 candidate tools and succeeds with 19 [paper: Paramanayakam et al. 2024].
- Practitioner consensus puts the accuracy knee at 30–40+ tools [src].
- At 15 tools, Overseer sits below the knee, so **deferral here saves tokens rather than fixing accuracy**. The accuracy gain applies once MCP servers add many tools, and those already defer [infer].
- No published A/B compares calling tools through a generic dispatcher against native calls [unverified].
- Mitigations:
  - keep the hot tools native;
  - use namespaced names, which have "non-trivial" eval effects [doc: Anthropic, "Writing effective tools for agents"];
  - rank exact names first;
  - validate args against each tool's own schema.

**Code-mode**
- CodeAct: executable Python actions beat JSON tool calls by up to 20% success, with ~30% fewer turns, across 17 LLMs. The gain is largest on models that are strong at code [paper: Wang et al., ICML 2024].
- Anthropic's programmatic tool calling: +11% on agentic search with 24% fewer input tokens [doc].
- "Code execution with MCP": ~150K → ~2K tokens on large tool catalogs [vendor]. An independent replication found 94% on a small synthetic task and noted that savings concentrate in high-volume cases [src].
- Cloudflare Code Mode and OpenAI's programmatic tool calling both chose JS in a no-network sandbox [doc/src].
- In smolagents, freeform code has a 2.4% first-call parse-failure rate [src].
- **Security precedent.** DeepSeek Harness `run_code` ran model code in a worker thread without file-effect confinement, so `node:fs` / `child_process` bypassed its sandbox [src: dsh discussion #3245]. The runtime must be capability-free by construction, with every effect going through the harness gate.
- **Savings are situational** [infer]. Coding agents usually need to see `read`/`bash` output to reason. Code-mode wins on fan-in aggregation, mechanical transforms and multi-call pipelines, so we offer it rather than force it.
- Observation masking roughly halves cost at equal solve rate [paper: Lindenbauer et al. 2025, arXiv 2508.21433]. Overseer's stale-result clearing already does this, which makes pinning loaded schemas mandatory.

## Runtime choice for code-mode

| Option | Binary cost | Verdict |
|---|---|---|
| QuickJS via `rquickjs` | ~0.5–1 MB (engine ~210 KiB) [doc: crates.io] | **Chosen.** ES2020 JS is a language models write fluently. Memory limits and an interrupt handler come built in. Without the libc `std`/`os` modules there is no fs, network or process access. Startup is ~300 µs and needs no process spawn. |
| Shell out to `python3` / `node` | 0 | Host-dependent. `/usr/bin/python3` on macOS is an install stub that can open a dialog. A spawn costs 10–100 ms. |
| Rhai / Starlark | small | Models rarely write these languages. |
| Boa, RustPython | multi-MB [unverified] | Too heavy. |
| Wasmtime | 5–10 MB | Breaks the 15 MB budget. |

**Gate:** if `rquickjs` adds more than 1.5 MB to the release binary, or fails `cargo deny`, the tool does not ship, and the python3 fallback comes back to the owner for a decision.

## Decisions

1. **One resident `tools` op tool.**
   - `op=search {query}` returns up to 5 `{name, description, input_schema}`, exact name first. An empty query lists every deferred tool as a one-liner.
   - `op=call {name, args}` re-enters `ToolRegistry::call` for the inner tool. Disabled, unavailable, hooks, `check_args`, the gate, taint, sanitize and budget all apply to the *inner* name.
   - Deferred: `computer`, `struct_search`, `diagnostics`, `repo_map`, `symbol`, `plan` (still native in plan mode), plus every MCP tool. The `mcp` spec folds into `tools`, and its dispatch path stays internal.
2. **Schemas are pinned in the view.** `tools op=search` results survive stale-result clearing (the last 3 are kept verbatim). This is a view-only rule; events are untouched.
3. **`skill` is advertised only when skills exist** (detected once per registry). The `computer` static prompt segment moves into the computer tool's own description, and its stale scaling wording is fixed.
4. **`run_code`** (QuickJS, cargo feature `code-mode`, on by default).
   - The script sees one global, `tools.<name>(args)`, plus `print`. Every call goes through the full gate pipeline.
   - Limits: 64 MB heap, a 30 s default deadline, 64 sub-calls, 8 MB total sub-call bytes, and 16K chars of output.
   - Each sub-call is audited as an event.
5. **Budget.**
   - Resident tool specs ≤ 5,400 chars with every optional tool, MCP and skills present, before memory v2's `memory` tool (≤ 600) lands. That keeps the total ≤ ~6,000 chars (~1.5K tokens).
   - Static prompt base ≤ 1,000 chars.
   - Both are pinned by tests.

## Deferred
- Anthropic-native `defer_loading` for Anthropic sessions, so deferred tools get native calls: owner engine; gate is measured accuracy parity via the eval rig once evals resume.
- `computer` calls from `run_code` (GUI batch scripts): owner engine; gate is live cua-driver verification on macOS.
- Memory writes from `run_code`: owner engine; gate is memory v2 in use.
- A persistent QuickJS context across calls: owner engine; gate is demonstrated need.
