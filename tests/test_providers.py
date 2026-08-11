"""Provider tests: fallback chains, registry, OpenAI-compatible wire format."""

from __future__ import annotations

import pytest

from overseer.errors import ProviderError
from overseer.providers.base import ChatMessage, ChatResult, Provider, ToolCall
from overseer.providers.registry import (
    ProviderRegistry,
    get_provider_class,
    register_provider,
    registered_providers,
)


@register_provider
class _FakeProvider(Provider):
    name = "fake"

    def __init__(self, fail: bool = False, result: ChatResult | None = None) -> None:
        self.fail = fail
        self.result = result or ChatResult(content="ok")
        self.calls = 0

    def complete(self, messages, tools=None, stream_callback=None, **kwargs):
        self.calls += 1
        if self.fail:
            raise ProviderError("boom")
        return self.result


def test_registry_self_registration():
    assert get_provider_class("fake") is _FakeProvider
    assert "fake" in registered_providers()


def test_registry_unknown_provider():
    with pytest.raises(ProviderError, match="unknown provider"):
        get_provider_class("nope")


def test_fallback_chain_uses_first_healthy():
    reg = ProviderRegistry()
    good = _FakeProvider()
    reg.add("good", good)
    reg.add("backup", _FakeProvider())
    result, used = reg.complete_with_fallback(["good", "backup"], [])
    assert used == "good"
    assert result.content == "ok"


def test_fallback_chain_fails_over():
    reg = ProviderRegistry()
    bad = _FakeProvider(fail=True)
    good = _FakeProvider(result=ChatResult(content="recovered"))
    reg.add("bad", bad)
    reg.add("good", good)
    result, used = reg.complete_with_fallback(["bad", "good"], [])
    assert used == "good"
    assert result.content == "recovered"
    assert bad.calls == 1


def test_fallback_chain_all_fail():
    reg = ProviderRegistry()
    reg.add("a", _FakeProvider(fail=True))
    reg.add("b", _FakeProvider(fail=True))
    with pytest.raises(ProviderError, match="all providers failed"):
        reg.complete_with_fallback(["a", "b"], [])


def test_fallback_chain_empty():
    reg = ProviderRegistry()
    with pytest.raises(ProviderError, match="empty"):
        reg.complete_with_fallback([], [])


def test_fallback_chain_unknown_name():
    reg = ProviderRegistry()
    with pytest.raises(ProviderError, match="not configured"):
        reg.complete_with_fallback(["ghost"], [])


def test_openai_compat_wire_format(monkeypatch):
    """Verify the request payload shape against a captured request."""
    from overseer.providers.openai_compat import OpenAICompatProvider

    captured: dict = {}

    class _FakeResp:
        status_code = 200

        def json(self):
            return {
                "choices": [
                    {
                        "message": {
                            "content": "hello",
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {
                                        "name": "list_dir",
                                        "arguments": '{"path": "/workspace"}',
                                    },
                                }
                            ],
                        }
                    }
                ],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            }

    class _FakeClient:
        def __init__(self, *a, **k):
            pass

        def post(self, url, json=None):
            captured["url"] = url
            captured["payload"] = json
            return _FakeResp()

        def close(self):
            pass

    monkeypatch.setattr("overseer.providers.openai_compat.httpx.Client", _FakeClient)
    monkeypatch.setenv("OVERSEER_TEST_KEY", "sk-test-1234567890abcdef")

    p = OpenAICompatProvider(
        base_url="https://api.example.com/v1", model="test-model", api_key_env="OVERSEER_TEST_KEY"
    )
    result = p.complete(
        [ChatMessage(role="user", content="hi")],
        tools=[{"type": "function", "function": {"name": "list_dir"}}],
    )

    assert captured["url"] == "/chat/completions"
    assert captured["payload"]["model"] == "test-model"
    assert captured["payload"]["messages"][0] == {"role": "user", "content": "hi"}
    assert captured["payload"]["tools"] == [{"type": "function", "function": {"name": "list_dir"}}]
    assert result.content == "hello"
    assert len(result.tool_calls) == 1
    assert result.tool_calls[0].name == "list_dir"
    assert result.tool_calls[0].arguments == {"path": "/workspace"}
    assert result.usage["total_tokens"] == 15


def test_openai_compat_echoes_tool_calls(monkeypatch):
    """Assistant tool-call turns must round-trip on the wire."""
    from overseer.providers.openai_compat import OpenAICompatProvider

    captured: dict = {}

    class _FakeResp:
        status_code = 200

        def json(self):
            return {"choices": [{"message": {"content": "done"}}], "usage": {}}

    class _FakeClient:
        def __init__(self, *a, **k):
            pass

        def post(self, url, json=None):
            captured["payload"] = json
            return _FakeResp()

        def close(self):
            pass

    monkeypatch.setattr("overseer.providers.openai_compat.httpx.Client", _FakeClient)

    p = OpenAICompatProvider(base_url="https://api.example.com/v1", model="m")
    p.complete(
        [
            ChatMessage(role="user", content="run it"),
            ChatMessage(
                role="assistant",
                content="",
                tool_calls=[ToolCall(id="call_9", name="list_dir", arguments={"path": "/"})],
            ),
            ChatMessage(role="tool", content="[ok]", tool_call_id="call_9"),
        ]
    )
    msgs = captured["payload"]["messages"]
    assert msgs[1]["role"] == "assistant"
    assert msgs[1]["tool_calls"][0]["function"]["name"] == "list_dir"
    assert msgs[1]["tool_calls"][0]["function"]["arguments"] == '{"path": "/"}'
    assert msgs[2] == {"role": "tool", "content": "[ok]", "tool_call_id": "call_9"}


def test_openai_compat_http_error(monkeypatch):
    from overseer.providers.openai_compat import OpenAICompatProvider

    class _FakeResp:
        status_code = 401
        text = "unauthorized"

    class _FakeClient:
        def __init__(self, *a, **k):
            pass

        def post(self, url, json=None):
            return _FakeResp()

        def close(self):
            pass

    monkeypatch.setattr("overseer.providers.openai_compat.httpx.Client", _FakeClient)
    p = OpenAICompatProvider(base_url="https://api.example.com/v1", model="m")
    with pytest.raises(ProviderError, match="401"):
        p.complete([ChatMessage(role="user", content="hi")])


def test_openai_compat_malformed_response(monkeypatch):
    from overseer.providers.openai_compat import OpenAICompatProvider

    class _FakeResp:
        status_code = 200

        def json(self):
            return {"unexpected": True}

    class _FakeClient:
        def __init__(self, *a, **k):
            pass

        def post(self, url, json=None):
            return _FakeResp()

        def close(self):
            pass

    monkeypatch.setattr("overseer.providers.openai_compat.httpx.Client", _FakeClient)
    p = OpenAICompatProvider(base_url="https://api.example.com/v1", model="m")
    with pytest.raises(ProviderError, match="malformed"):
        p.complete([ChatMessage(role="user", content="hi")])


def test_openai_compat_malformed_tool_arguments(monkeypatch):
    """Broken JSON in tool arguments must not crash the parse."""
    from overseer.providers.openai_compat import OpenAICompatProvider

    class _FakeResp:
        status_code = 200

        def json(self):
            return {
                "choices": [
                    {
                        "message": {
                            "content": "",
                            "tool_calls": [
                                {
                                    "id": "c1",
                                    "type": "function",
                                    "function": {"name": "grep", "arguments": "{not json"},
                                }
                            ],
                        }
                    }
                ],
                "usage": {},
            }

    class _FakeClient:
        def __init__(self, *a, **k):
            pass

        def post(self, url, json=None):
            return _FakeResp()

        def close(self):
            pass

    monkeypatch.setattr("overseer.providers.openai_compat.httpx.Client", _FakeClient)
    p = OpenAICompatProvider(base_url="https://api.example.com/v1", model="m")
    result = p.complete([ChatMessage(role="user", content="hi")])
    assert result.tool_calls[0].arguments == {}
