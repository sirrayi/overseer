"""B2 CLI tests: run smoke, chat smoke, stubs, sessions, export, budget."""

from __future__ import annotations

from pathlib import Path

from typer.testing import CliRunner

from overseer.cli import app
from overseer.providers.base import ChatMessage, ChatResult, Provider
from overseer.providers.registry import ProviderRegistry
from overseer.session import SessionStore
from overseer.tools import ToolContext, ToolRegistry, get_tool_class, registered_tools

runner = CliRunner()


class _ScriptedProvider(Provider):
    name = "scripted"

    def __init__(self, responses: list[ChatResult]) -> None:
        self.responses = responses
        self.calls = 0

    def complete(self, messages, tools=None, stream_callback=None, **kwargs):
        if self.calls >= len(self.responses):
            return ChatResult(content="(no more responses)")
        r = self.responses[self.calls]
        self.calls += 1
        return r


def _fake_runtime(tmp_path: Path, responses: list[ChatResult]):
    """Build a Runtime with a scripted provider, monkeypatched into the CLI."""
    from overseer.approval import ApprovalPolicy
    from overseer.cli import Runtime

    reg = ProviderRegistry()
    reg.add("scripted", _ScriptedProvider(responses))
    tools = ToolRegistry()
    for name in registered_tools():
        tools.add(get_tool_class(name)())
    vault = tmp_path / "vault"
    vault.mkdir(exist_ok=True)
    policy = ApprovalPolicy(allowed_roots=[vault])
    ctx = ToolContext(allowed_roots=[vault], artifacts_dir=vault / ".overseer" / "artifacts")
    store = SessionStore(vault)
    return Runtime(
        cfg=type(
            "Cfg",
            (),
            {
                "provider": type("P", (), {"name": "scripted"})(),
                "max_tokens_per_turn": 10000,
                "vault_path": str(vault),
            },
        )(),
        providers=reg,
        tools=tools,
        policy=policy,
        context=ctx,
        session_store=store,
        approvals_log=vault / ".overseer" / "logs" / "approvals.log",
    )


def test_version_flag():
    result = runner.invoke(app, ["--version"])
    assert result.exit_code == 0
    assert "overseer 0.1.0" in result.output


def test_version_subcommand():
    result = runner.invoke(app, ["version"])
    assert result.exit_code == 0
    assert "overseer" in result.output


def test_run_smoke(tmp_path, monkeypatch):
    """overseer run executes a task and prints the answer."""

    monkeypatch.setattr(
        "overseer.cli._build_runtime",
        lambda config, provider_registry=None: _fake_runtime(
            tmp_path, [ChatResult(content="the answer is 42")]
        ),
    )
    result = runner.invoke(app, ["run", "what is the answer", "--config", "x.yaml"])
    assert result.exit_code == 0, result.output
    assert "the answer is 42" in result.output
    assert "tokens" in result.output


def test_run_creates_session(tmp_path, monkeypatch):

    monkeypatch.setattr(
        "overseer.cli._build_runtime",
        lambda config, provider_registry=None: _fake_runtime(
            tmp_path, [ChatResult(content="done")]
        ),
    )
    runner.invoke(app, ["run", "task", "--config", "x.yaml"])
    store = SessionStore(tmp_path / "vault")
    metas = store.list()
    assert len(metas) == 1
    assert metas[0].task == "task"
    assert metas[0].status == "done"


def test_run_redacts_output(tmp_path, monkeypatch):

    monkeypatch.setattr(
        "overseer.cli._build_runtime",
        lambda config, provider_registry=None: _fake_runtime(
            tmp_path, [ChatResult(content="key=sk-1234567890abcdef1234567890abcdef")]
        ),
    )
    result = runner.invoke(app, ["run", "task", "--config", "x.yaml"])
    assert "sk-1234567890abcdef1234567890abcdef" not in result.output
    assert "sk-***REDACTED***" in result.output


def test_run_budget_warning(tmp_path, monkeypatch):

    rt = _fake_runtime(tmp_path, [ChatResult(content="done", usage={"total_tokens": 9000})])
    rt.cfg.max_tokens_per_turn = 10000  # 90% of budget -> warning
    monkeypatch.setattr("overseer.cli._build_runtime", lambda config, provider_registry=None: rt)
    result = runner.invoke(app, ["run", "task", "--config", "x.yaml"])
    assert "budget warning" in result.output


def test_sessions_list_empty(tmp_path, monkeypatch):

    monkeypatch.setattr(
        "overseer.cli._build_runtime",
        lambda config, provider_registry=None: _fake_runtime(tmp_path, []),
    )
    result = runner.invoke(app, ["sessions", "--config", "x.yaml"])
    assert result.exit_code == 0
    assert "no sessions yet" in result.output


def test_export_redacted(tmp_path, monkeypatch):

    rt = _fake_runtime(tmp_path, [])
    s = rt.session_store.create(task="t")
    rt.session_store.append(
        s, ChatMessage(role="user", content="key=sk-1234567890abcdef1234567890abcdef")
    )
    monkeypatch.setattr("overseer.cli._build_runtime", lambda config, provider_registry=None: rt)
    result = runner.invoke(app, ["export", s.id, "--config", "x.yaml"])
    assert result.exit_code == 0
    assert "sk-1234567890abcdef1234567890abcdef" not in result.output
    assert "sk-***REDACTED***" in result.output


def test_trace_missing_session(tmp_path, monkeypatch):

    monkeypatch.setattr(
        "overseer.cli._build_runtime",
        lambda config, provider_registry=None: _fake_runtime(tmp_path, []),
    )
    result = runner.invoke(app, ["trace", "nonexistent", "--config", "x.yaml"])
    assert result.exit_code == 1
    assert "not found" in result.output


def test_memory_stub():
    result = runner.invoke(app, ["memory"])
    assert result.exit_code == 0
    assert "B5" in result.output


def test_skills_stub():
    result = runner.invoke(app, ["skills"])
    assert result.exit_code == 0
    assert "B7" in result.output


def test_cron_refuses():
    result = runner.invoke(app, ["cron"])
    assert result.exit_code == 0
    assert "disabled" in result.output
    assert "B10" in result.output


def test_tools_lists_registered():
    result = runner.invoke(app, ["tools"])
    assert result.exit_code == 0
    for name in ("terminal", "file_read", "file_write", "file_patch", "list_dir", "grep"):
        assert name in result.output


def test_model_shows_no_secrets(tmp_path, monkeypatch):
    """model display must show env-var names, never values."""

    monkeypatch.setattr(
        "overseer.cli._build_runtime",
        lambda config, provider_registry=None: _fake_runtime(tmp_path, []),
    )
    # model command reads config directly; use a real config file.
    cfg_path = tmp_path / "config.yaml"
    cfg_path.write_text(
        "vault_path: " + str(tmp_path / "vault") + "\n"
        "provider:\n  name: openai-compat\n  base_url: https://api.example.com/v1\n"
        "  model: test-model\n  api_key_env: OVERSEER_TEST_KEY\n",
        encoding="utf-8",
    )
    result = runner.invoke(app, ["model", "--config", str(cfg_path)])
    assert result.exit_code == 0
    assert "OVERSEER_TEST_KEY" in result.output  # env var NAME shown
    assert "sk-" not in result.output  # never a value
