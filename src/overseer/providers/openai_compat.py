"""OpenAI-compatible provider adapter (Ollama Cloud, DeepSeek, local Ollama).

Implements the /chat/completions wire format with tool-call support.
Secrets come from env vars (config.api_key_env), never from config.yaml.
"""

from __future__ import annotations

import json
import os
from typing import Any

import httpx

from overseer.errors import ProviderError, Timeout
from overseer.providers.base import ChatMessage, ChatResult, Provider, StreamEvent, ToolCall
from overseer.providers.registry import register_provider
from overseer.redact import redact

DEFAULT_TIMEOUT = 60.0


@register_provider
class OpenAICompatProvider(Provider):
    """OpenAI-compatible chat completions provider."""

    name = "openai-compat"

    def __init__(
        self,
        base_url: str,
        model: str,
        api_key_env: str | None = None,
        timeout: float = DEFAULT_TIMEOUT,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.model = model
        self.api_key_env = api_key_env
        self.timeout = timeout
        self._client: httpx.Client | None = None

    @property
    def _api_key(self) -> str | None:
        if not self.api_key_env:
            return None
        return os.environ.get(self.api_key_env)

    def _client_sync(self) -> httpx.Client:
        if self._client is None:
            headers = {"Content-Type": "application/json"}
            key = self._api_key
            if key:
                headers["Authorization"] = f"Bearer {key}"
            self._client = httpx.Client(
                base_url=self.base_url, headers=headers, timeout=self.timeout
            )
        return self._client

    def _to_wire(self, messages: list[ChatMessage]) -> list[dict[str, Any]]:
        wire: list[dict[str, Any]] = []
        for m in messages:
            if m.role == "tool":
                wire.append(
                    {
                        "role": "tool",
                        "content": m.content,
                        "tool_call_id": m.tool_call_id or "",
                    }
                )
            elif m.tool_calls:
                wire.append(
                    {
                        "role": "assistant",
                        "content": m.content or None,
                        "tool_calls": [
                            {
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": json.dumps(tc.arguments),
                                },
                            }
                            for tc in m.tool_calls
                        ],
                    }
                )
            else:
                wire.append({"role": m.role, "content": m.content})
        return wire

    def complete(
        self,
        messages: list[ChatMessage],
        tools: list[dict[str, Any]] | None = None,
        stream_callback: Any | None = None,
        **kwargs: Any,
    ) -> ChatResult:
        payload: dict[str, Any] = {
            "model": self.model,
            "messages": self._to_wire(messages),
        }
        if tools:
            payload["tools"] = tools
        if kwargs:
            payload.update(kwargs)

        try:
            resp = self._client_sync().post("/chat/completions", json=payload)
        except httpx.TimeoutException as exc:
            raise Timeout(f"provider {self.name} timed out after {self.timeout}s") from exc
        except httpx.HTTPError as exc:
            raise ProviderError(f"provider {self.name} request failed: {exc}") from exc

        if resp.status_code >= 400:
            # Redact the body: provider errors can echo request headers/keys.
            raise ProviderError(
                f"provider {self.name} returned HTTP {resp.status_code}: {redact(resp.text[:300])}"
            )

        try:
            data = resp.json()
        except ValueError as exc:
            raise ProviderError(f"provider {self.name} returned non-JSON response") from exc

        try:
            choice = data["choices"][0]
            message = choice["message"]
            content = message.get("content") or ""
            tool_calls_raw = message.get("tool_calls") or []
            tool_calls: list[ToolCall] = []
            for tc in tool_calls_raw:
                fn = tc.get("function", {})
                try:
                    arguments = json.loads(fn.get("arguments") or "{}")
                except ValueError:
                    arguments = {}
                tool_calls.append(
                    ToolCall(
                        id=tc.get("id", ""),
                        name=fn.get("name", ""),
                        arguments=arguments,
                    )
                )
            usage = data.get("usage") or {}
            return ChatResult(content=content, tool_calls=tool_calls, usage=usage)
        except (KeyError, IndexError, TypeError) as exc:
            raise ProviderError(f"provider {self.name} returned malformed response: {exc}") from exc

    def stream(
        self,
        messages: list[ChatMessage],
        tools: list[dict[str, Any]] | None = None,
        **kwargs: Any,
    ) -> Any:
        """Stream a response as an iterator of StreamEvent.

        Handles: partial tool-call argument deltas (accumulated by index),
        malformed SSE/JSON (skipped, never crashes), timeouts, and
        cancellation (generator close releases the connection). Error
        messages are redacted before raising.
        """
        payload: dict[str, Any] = {
            "model": self.model,
            "messages": self._to_wire(messages),
            "stream": True,
        }
        if tools:
            payload["tools"] = tools
        if kwargs:
            payload.update(kwargs)

        def _iter() -> Any:
            # Accumulators for tool-call deltas, keyed by index.
            acc: dict[int, dict[str, Any]] = {}
            try:
                with self._client_sync().stream("POST", "/chat/completions", json=payload) as resp:
                    if resp.status_code >= 400:
                        body = redact(resp.text[:300])
                        raise ProviderError(
                            f"provider {self.name} returned HTTP {resp.status_code}: {body}"
                        )
                    for line in resp.iter_lines():
                        if not line or not line.startswith("data:"):
                            continue
                        data = line[5:].strip()
                        if data == "[DONE]":
                            break
                        try:
                            chunk = json.loads(data)
                        except ValueError:
                            continue  # malformed chunk: skip, don't crash
                        try:
                            delta = chunk["choices"][0]["delta"]
                        except (KeyError, IndexError, TypeError):
                            continue
                        if delta.get("content"):
                            yield StreamEvent(type="delta", content=delta["content"])
                        for tc in delta.get("tool_calls") or []:
                            idx = tc.get("index", 0)
                            slot = acc.setdefault(idx, {"id": "", "name": "", "args": ""})
                            if tc.get("id"):
                                slot["id"] = tc["id"]
                            fn = tc.get("function") or {}
                            if fn.get("name"):
                                slot["name"] = fn["name"]
                            if fn.get("arguments"):
                                slot["args"] += fn["arguments"]
                            # Emit the accumulated call (lenient parse).
                            try:
                                parsed = json.loads(slot["args"]) if slot["args"] else {}
                            except ValueError:
                                parsed = {}
                            yield StreamEvent(
                                type="tool_call_delta",
                                tool_call=ToolCall(
                                    id=slot["id"], name=slot["name"], arguments=parsed
                                ),
                            )
                    # Final complete tool calls + usage.
                    for idx in sorted(acc):
                        slot = acc[idx]
                        try:
                            parsed = json.loads(slot["args"]) if slot["args"] else {}
                        except ValueError:
                            parsed = {}
                        yield StreamEvent(
                            type="tool_call_delta",
                            tool_call=ToolCall(id=slot["id"], name=slot["name"], arguments=parsed),
                        )
                    yield StreamEvent(type="done")
            except httpx.TimeoutException as exc:
                raise Timeout(
                    f"provider {self.name} stream timed out after {self.timeout}s"
                ) from exc
            except httpx.HTTPError as exc:
                raise ProviderError(
                    f"provider {self.name} stream failed: {redact(str(exc))}"
                ) from exc

        return _iter()

    def close(self) -> None:
        if self._client is not None:
            self._client.close()
            self._client = None
