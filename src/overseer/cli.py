"""Overseer CLI (plan B2): chat, run, model, tools, config, sessions, trace,
doctor, version, and stubs (memory/skills/cron).

Design rules:
- Lazy imports: version/doctor never import the agent loop or providers.
- All display output passes through redact().
- Config display shows env-var NAMES, never values.
- Approval prompts show the exact command/path + risk reason; decisions log.
- Sessions persist under <vault>/.overseer/sessions/.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any

import typer
from rich.console import Console
from rich.prompt import Confirm, Prompt
from rich.table import Table

from overseer import __version__
from overseer.redact import redact

app = typer.Typer(
    name="overseer",
    help="Vault-native, self-improving, verification-driven agent harness.",
    no_args_is_help=True,
)
console = Console()

BUDGET_WARNING_FRACTION = 0.8


def _version_callback(value: bool) -> None:
    if value:
        console.print(f"overseer {__version__}")
        raise typer.Exit()


@app.callback()
def main(
    version: bool = typer.Option(
        False,
        "--version",
        "-V",
        help="Print version and exit.",
        is_eager=True,
        callback=_version_callback,
    ),
) -> None:
    """Overseer — the engine. Everything."""


# ---------------------------------------------------------------------------
# runtime builder (lazy; shared by chat/run)
# ---------------------------------------------------------------------------


@dataclass
class Runtime:
    cfg: Any
    providers: Any
    tools: Any
    policy: Any
    context: Any
    session_store: Any
    approvals_log: Path
    _current_session: Any = None  # set by chat/run; observation stream target
    live_learning: Any = None  # LiveLearningEngine (plan B4.5)


def _load_cfg(config: str) -> Any:
    from overseer.config import load_config

    return load_config(config)


def _build_runtime(config: str, provider_registry: Any | None = None) -> Runtime:
    """Build the agent runtime from config. Lazy: only called by chat/run."""
    from overseer.approval import ApprovalPolicy
    from overseer.providers.factory import build_provider
    from overseer.providers.registry import ProviderRegistry
    from overseer.session import SessionStore
    from overseer.tools import ToolContext, ToolRegistry, get_tool_class, registered_tools

    cfg = _load_cfg(config)

    if provider_registry is None:
        provider_registry = ProviderRegistry()
        provider_registry.add(cfg.provider.name, build_provider(cfg.provider))

    tools = ToolRegistry()
    for name in registered_tools():
        tools.add(get_tool_class(name)())

    vault_root = Path(cfg.vault_path).expanduser()
    allowed_roots = [vault_root]
    artifacts_dir = vault_root / ".overseer" / "artifacts"
    approvals_log = vault_root / ".overseer" / "logs" / "approvals.log"
    approvals_log.parent.mkdir(parents=True, exist_ok=True)

    policy = ApprovalPolicy(allowed_roots=allowed_roots)
    context = ToolContext(
        allowed_roots=allowed_roots,
        artifacts_dir=artifacts_dir,
    )
    store = SessionStore(vault_root)

    # Live learning engine (plan B4.5): per-turn micro-reflection.
    from overseer.live_learning import LiveLearningEngine

    ll_engine = LiveLearningEngine(vault_root, enabled=cfg.live_learning)

    runtime = Runtime(
        cfg=cfg,
        providers=provider_registry,
        tools=tools,
        policy=policy,
        context=context,
        session_store=store,
        approvals_log=approvals_log,
        live_learning=ll_engine,
    )

    # Approval UX: prompt the user, log the decision.
    def approver(tool_name: str, args: dict[str, Any]) -> bool:
        reason = policy.describe(tool_name, args)
        detail = args.get("command", "") if tool_name == "terminal" else args.get("path", "")
        console.print()
        console.print(f"[yellow]approval required[/yellow] — {reason}")
        console.print(f"  tool: [bold]{tool_name}[/bold]")
        console.print(f"  action: {redact(detail)[:300]}")
        approved = Confirm.ask("  approve?", default=False)
        with approvals_log.open("a", encoding="utf-8") as fh:
            fh.write(f"{tool_name}\t{approved}\t{redact(detail)[:300]}\n")
        return approved

    policy.approver = approver
    context.approver = approver
    return runtime


def _build_loop(runtime: Runtime, stream_callback: Any | None = None) -> Any:
    from overseer.agent import AgentLoop

    store = runtime.session_store

    def observer(event_type: str, payload: dict[str, Any]) -> None:
        # Observation stream (plan B3): mirror loop events into the
        # episodic store. The session is the current one (set by chat/run).
        session = getattr(runtime, "_current_session", None)
        if session is None:
            return
        if event_type == "tool_call":
            store.observe_tool_call(session, payload["name"], payload["arguments"])
        elif event_type == "approval":
            store.observe_approval(session, payload["tool_name"], payload["allowed"])
        elif event_type == "error":
            store.observe_error(session, payload["message"])

    return AgentLoop(
        providers=runtime.providers,
        tools=runtime.tools,
        policy=runtime.policy,
        context=runtime.context,
        max_tokens=runtime.cfg.max_tokens_per_turn,
        stream_callback=stream_callback,
        observer=observer,
        live_learning=(runtime.live_learning.detect_and_apply if runtime.live_learning else None),
    )


def _chain(runtime: Runtime) -> list[str]:
    return [runtime.cfg.provider.name]


def _session_cost(runtime: Runtime, tokens: int) -> float:
    """Provider-aware cost estimate (NOTE-02): uses _cost_for, not a constant."""
    from overseer.session import _cost_for

    return _cost_for(runtime.cfg.provider.name, tokens)


def _write_session_note(runtime: Runtime, session: Any) -> None:
    """Vault bridge (plan B3): write a summary note to 10-Sessions/.

    The note is a summary, not a transcript dump. Raw logs stay in
    .overseer/sessions/. Frontmatter is validated by Vault.write_note.
    """
    from overseer.vault import Vault

    vault = Vault(runtime.cfg.vault_path)
    vault.init()  # idempotent
    body = _session_note_body(session)
    # Map session status to the vault's session status vocabulary.
    vault_status = {"done": "accepted", "error": "rejected"}.get(session.status, "active")
    vault.write_note(
        "session",
        f"Session {session.id}",
        body,
        session_id=session.id,
        status=vault_status,
        tokens=session.tokens,
        cost=session.cost,
    )


def _session_note_body(session: Any) -> str:
    """Human-readable summary of a session (redacted, not a dump)."""
    lines = [
        f"Task: {redact(session.task)[:200]}",
        f"Status: {session.status}",
        f"Tokens: {session.tokens}",
        f"Cost: ${session.cost:.4f}",
        "",
        "## Summary",
        "",
    ]
    # First user message + final assistant answer give the arc.
    user_msgs = [m for m in session.messages if m.role == "user"]
    final = [m for m in session.messages if m.role == "assistant" and m.content]
    if user_msgs:
        lines.append(f"**Request:** {redact(user_msgs[0].content)[:300]}")
    if final:
        lines.append(f"**Result:** {redact(final[-1].content)[:500]}")
    tool_names = sorted({m.tool_call_id or "" for m in session.messages if m.role == "tool"})
    if tool_names:
        lines.append(f"**Tools used:** {', '.join(t for t in tool_names if t)}")
    return "\n".join(lines)


def _show_budget_warning(runtime: Runtime, tokens: int) -> None:
    limit = runtime.cfg.max_tokens_per_turn
    if limit and tokens >= limit * BUDGET_WARNING_FRACTION:
        console.print(
            f"[yellow]budget warning:[/yellow] {tokens}/{limit} tokens used "
            f"({tokens / limit:.0%} of budget)"
        )


# ---------------------------------------------------------------------------
# init
# ---------------------------------------------------------------------------


@app.command()
def init(
    vault_path: str = typer.Option(
        "~/overseer-vault", "--vault", "-v", help="Path to the canonical vault."
    ),
    config: str = typer.Option(
        "config.yaml", "--config", "-c", help="Path to config file (created if missing)."
    ),
) -> None:
    """Create a compliant vault and sample config (plan Part 3: overseer init)."""
    from overseer.config import write_sample_config
    from overseer.errors import ConfigError
    from overseer.vault import Vault

    vault = Vault(vault_path)
    created = vault.init()
    try:
        write_sample_config(config, vault_path=vault_path)
    except ConfigError as exc:
        console.print(f"[yellow]{exc}[/yellow]")
        console.print("[yellow]config left untouched; vault is ready[/yellow]")
    console.print(f"[green]vault ready:[/green] {vault.root}")
    console.print(f"[green]created {len(created)} system/template notes[/green]")
    console.print(
        f"[green]sample config:[/green] {config} (placeholders only — set secrets via env)"
    )
    console.print("[yellow]next:[/yellow] overseer doctor")


# ---------------------------------------------------------------------------
# chat
# ---------------------------------------------------------------------------


@app.command()
def chat(
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
    resume: str = typer.Option(None, "--resume", "-r", help="Session id to resume."),
    no_stream: bool = typer.Option(False, "--no-stream", help="Disable streaming output."),
) -> None:
    """Interactive session with the agent loop."""
    from overseer.providers.base import ChatMessage

    runtime = _build_runtime(config)
    session = runtime.session_store.load(resume) if resume else runtime.session_store.create()
    runtime._current_session = session  # observation stream target
    console.print(f"[dim]session {session.id}[/dim] (type 'exit' to quit, Ctrl+C to stop)")

    history = [ChatMessage(role=m.role, content=m.content) for m in session.messages]
    loop = _build_loop(runtime, stream_callback=lambda t: console.print(t, end=""))

    while True:
        try:
            user_input = Prompt.ask("[bold]you[/bold]")
        except (EOFError, KeyboardInterrupt):
            console.print("\n[dim]bye[/dim]")
            break
        if user_input.strip().lower() in ("exit", "quit"):
            break
        if not user_input.strip():
            continue

        msg = ChatMessage(role="user", content=user_input)
        history.append(msg)
        runtime.session_store.append(session, msg)

        console.print("[bold]overseer[/bold] ", end="")
        try:
            # Inject active live-learning constraints/preferences (B4.5).
            ll_block = runtime.live_learning.context_block() if runtime.live_learning else ""
            turn_history = history
            if ll_block:
                turn_history = [
                    ChatMessage(role="system", content=ll_block),
                    *history,
                ]
            result = loop.run(turn_history, chain=_chain(runtime), stream=not no_stream)
        except Exception as exc:  # BudgetExceeded etc.
            console.print(f"[red]{redact(str(exc))}[/red]")
            continue
        except KeyboardInterrupt:
            console.print("\n[yellow]interrupted[/yellow]")
            continue

        if result.content:
            history.append(ChatMessage(role="assistant", content=result.content))
            runtime.session_store.append(
                session, ChatMessage(role="assistant", content=result.content)
            )
        session.tokens += result.total_tokens
        session.cost = _session_cost(runtime, session.tokens)  # NOTE-02
        runtime.session_store.save_meta(session)
        _show_budget_warning(runtime, session.tokens)
        if result.stopped_reason == "max_iterations":
            console.print("[yellow]stopped: max iterations reached[/yellow]")

    session.status = "done"
    runtime.session_store.save_meta(session)
    _write_session_note(runtime, session)  # vault bridge (plan B3)
    console.print(
        f"[dim]session {session.id} saved — {session.tokens} tokens, ${session.cost:.4f}[/dim]"
    )


# ---------------------------------------------------------------------------
# run
# ---------------------------------------------------------------------------


@app.command()
def run(
    task: str = typer.Argument(..., help="The task to execute."),
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
    no_stream: bool = typer.Option(False, "--no-stream", help="Disable streaming output."),
) -> None:
    """Non-interactive single task execution."""
    from overseer.providers.base import ChatMessage

    runtime = _build_runtime(config)
    session = runtime.session_store.create(task=task)
    runtime._current_session = session  # observation stream target
    loop = _build_loop(runtime, stream_callback=lambda t: console.print(t, end=""))

    console.print(f"[dim]session {session.id} — {redact(task)[:80]}[/dim]")
    try:
        result = loop.run(
            [ChatMessage(role="user", content=task)],
            chain=_chain(runtime),
            stream=not no_stream,
        )
    except Exception as exc:
        console.print(f"[red]{redact(str(exc))}[/red]")
        session.status = "error"
        runtime.session_store.save_meta(session)
        raise typer.Exit(code=1) from exc

    if result.content:
        console.print()
        console.print(redact(result.content))
    session.tokens = result.total_tokens
    session.cost = _session_cost(runtime, session.tokens)  # NOTE-02
    session.status = "done" if result.stopped_reason == "final_answer" else "error"
    runtime.session_store.save_meta(session)
    _write_session_note(runtime, session)  # vault bridge (plan B3)
    _show_budget_warning(runtime, session.tokens)
    console.print(
        f"[dim]done — {result.iterations} iterations, {result.tool_calls_made} tool calls, "
        f"{result.total_tokens} tokens, ${session.cost:.4f}[/dim]"
    )
    if result.stopped_reason != "final_answer":
        raise typer.Exit(code=1)


# ---------------------------------------------------------------------------
# model / tools / config
# ---------------------------------------------------------------------------


@app.command()
def model(
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
    set_model: str | None = typer.Option(None, "--set", help="Set the model name."),
) -> None:
    """Inspect or switch the provider model."""
    from overseer.config import load_config

    cfg = load_config(config)
    if set_model:
        # Rewrite config.yaml with the new model, preserving other fields.
        import yaml

        p = Path(config)
        data = yaml.safe_load(p.read_text(encoding="utf-8")) or {}
        data.setdefault("provider", {})["model"] = set_model
        p.write_text(yaml.safe_dump(data, sort_keys=False), encoding="utf-8")
        console.print(f"[green]model set:[/green] {set_model}")
        return
    console.print(f"provider: {cfg.provider.name}")
    console.print(f"model:    {cfg.provider.model}")
    console.print(f"base_url: {cfg.provider.base_url or '(default)'}")
    console.print(f"api_key:  env var {cfg.provider.api_key_env or '(none)'} (value never shown)")


@app.command()
def tools() -> None:
    """List registered tools and their schemas."""
    from overseer.tools import get_tool_class, registered_tools

    table = Table(title="overseer tools")
    table.add_column("name")
    table.add_column("description")
    table.add_column("approval")
    for name in registered_tools():
        cls = get_tool_class(name)
        table.add_row(
            name,
            cls.description[:60],
            "required" if cls.requires_approval else "no",
        )
    console.print(table)


@app.command()
def config(
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
    validate: bool = typer.Option(False, "--validate", help="Validate the config."),
) -> None:
    """View config safely (no secrets) or validate it."""
    from overseer.config import load_config

    try:
        cfg = load_config(config)
    except Exception as exc:
        console.print(f"[red]{redact(str(exc))}[/red]")
        raise typer.Exit(code=1) from exc
    if validate:
        from overseer.doctor import run_doctor

        report = run_doctor(cfg)
        console.print(report.render())
        if not report.ok:
            raise typer.Exit(code=1)
        return
    console.print(f"vault_path: {cfg.vault_path}")
    console.print(f"log_dir:    {cfg.log_dir}")
    console.print(f"power_mode: {cfg.power_mode}")
    console.print(f"live_learning: {cfg.live_learning}")
    console.print(f"max_tokens_per_turn: {cfg.max_tokens_per_turn}")
    console.print(f"provider:   {cfg.provider.name} / {cfg.provider.model}")
    console.print(f"api_key:    env var {cfg.provider.api_key_env or '(none)'} (value never shown)")


# ---------------------------------------------------------------------------
# sessions / trace
# ---------------------------------------------------------------------------


@app.command()
def sessions(
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
) -> None:
    """List sessions (meta only)."""
    runtime = _build_runtime(config)
    metas = runtime.session_store.list()
    if not metas:
        console.print("[dim]no sessions yet[/dim]")
        return
    table = Table(title="sessions")
    table.add_column("id")
    table.add_column("created")
    table.add_column("task")
    table.add_column("status")
    table.add_column("tokens")
    for m in metas:
        table.add_row(m.id, m.created[5:16], redact(m.task)[:40], m.status, str(m.tokens))
    console.print(table)


@app.command()
def search(
    query: str = typer.Argument(..., help="Full-text query over session events."),
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
    limit: int = typer.Option(20, "--limit", "-l", help="Max results."),
) -> None:
    """Search the episodic store (FTS5) for session events."""
    runtime = _build_runtime(config)
    hits = runtime.session_store.episodic.search(query, limit=limit)
    if not hits:
        console.print("[dim]no matches[/dim]")
        return
    table = Table(title=f"search: {query}")
    table.add_column("session")
    table.add_column("type")
    table.add_column("ts")
    table.add_column("snippet")
    for h in hits:
        table.add_row(h["session_id"], h["type"], h["ts"][5:16], h["snippet"][:80])
    console.print(table)


@app.command()
def rebuild(
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
) -> None:
    """Rebuild the episodic index from raw transcript logs (derived cache)."""
    runtime = _build_runtime(config)
    transcripts: list[tuple[str, list[dict[str, str]]]] = []
    for meta in runtime.session_store.list():
        try:
            session = runtime.session_store.load(meta.id)
        except Exception as exc:
            # A corrupt session must not kill the rebuild; skip and continue.
            console.print(f"[dim]skipping corrupt session {meta.id}: {redact(str(exc))}[/dim]")
            continue
        lines = [
            {"role": m.role, "content": m.content, "tool_call_id": m.tool_call_id or ""}
            for m in session.messages
        ]
        transcripts.append((meta.id, lines))
    n = runtime.session_store.episodic.rebuild(transcripts)
    console.print(f"[green]rebuilt episodic index:[/green] {n} events")


@app.command()
def trace(
    session_id: str = typer.Argument(..., help="Session id to inspect."),
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
) -> None:
    """Inspect a session transcript (redacted)."""
    runtime = _build_runtime(config)
    try:
        session = runtime.session_store.load(session_id)
    except Exception as exc:
        console.print(f"[red]{redact(str(exc))}[/red]")
        raise typer.Exit(code=1) from exc
    console.print(f"session {session.id} — {session.status} — {session.tokens} tokens")
    for m in session.messages:
        role = m.role
        content = redact(m.content)[:500]
        console.print(f"[dim]{role}[/dim] {content}")


@app.command()
def export(
    session_id: str = typer.Argument(..., help="Session id to export."),
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
    out: str = typer.Option(None, "--out", "-o", help="Output file (default: stdout)."),
) -> None:
    """Export a session as redacted markdown."""
    runtime = _build_runtime(config)
    try:
        session = runtime.session_store.load(session_id)
    except Exception as exc:
        console.print(f"[red]{redact(str(exc))}[/red]")
        raise typer.Exit(code=1) from exc
    md = runtime.session_store.export_markdown(session)
    if out:
        Path(out).write_text(md, encoding="utf-8")
        console.print(f"[green]exported:[/green] {out}")
    else:
        console.print(md)


# ---------------------------------------------------------------------------
# live-learn (plan B4.5)
# ---------------------------------------------------------------------------


@app.command()
def live_learn(
    action: str = typer.Argument(
        "inspect", help="inspect | undo (revert the last live-learning update)."
    ),
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
) -> None:
    """Inspect or undo live-learning state for the current session."""
    runtime = _build_runtime(config)
    engine = runtime.live_learning
    if engine is None:
        console.print("[yellow]live learning is disabled in config[/yellow]")
        return
    if action == "inspect":
        console.print(engine.to_json())
    elif action == "undo":
        if engine.undo():
            console.print("[green]reverted the last live-learning update[/green]")
        else:
            console.print("[dim]nothing to undo[/dim]")
    else:
        console.print(f"[red]unknown action: {action} (use inspect or undo)[/red]")
        raise typer.Exit(code=1)


# ---------------------------------------------------------------------------
# doctor / version
# ---------------------------------------------------------------------------


@app.command()
def doctor(
    config: str = typer.Option("config.yaml", "--config", "-c", help="Path to config file."),
) -> None:
    """Validate config, vault, provider, and permissions."""
    from overseer.config import load_config
    from overseer.doctor import run_doctor
    from overseer.errors import ConfigError

    try:
        cfg = load_config(config)
    except ConfigError as exc:
        console.print(f"[red]config failed to load:[/red] {redact(str(exc))}")
        raise typer.Exit(code=1) from exc

    report = run_doctor(cfg)
    console.print(report.render())
    if not report.ok:
        raise typer.Exit(code=1)


@app.command()
def version() -> None:
    """Print the overseer version."""
    console.print(f"overseer {__version__}")


# ---------------------------------------------------------------------------
# stubs (not built yet — clear messages)
# ---------------------------------------------------------------------------


@app.command()
def memory() -> None:
    """Stub: knowledge layer arrives in B5."""
    console.print(
        "[yellow]memory:[/yellow] the knowledge layer arrives in B5. "
        "Facts, preferences, and corrections are not available yet."
    )


@app.command()
def skills() -> None:
    """Stub: recursive learning arrives in B7."""
    console.print(
        "[yellow]skills:[/yellow] recursive learning arrives in B7. "
        "Skill proposals and the curator are not available yet."
    )


@app.command()
def cron() -> None:
    """Refuse: scheduled autonomous execution is not safe before hardening."""
    console.print(
        "[red]cron:[/red] scheduled autonomous execution is disabled until "
        "security hardening (B10). This is intentional."
    )


if __name__ == "__main__":
    app()
