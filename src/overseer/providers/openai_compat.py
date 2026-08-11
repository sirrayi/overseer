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
from overseer.providers.base import ChatMessage, ChatResult, Provider, ToolCall
from overseer.providers.registry import register_provider

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
            raise ProviderError(
                f"provider {self.name} returned HTTP {resp.status_code}: {resp.text[:300]}"
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

    def close(self) -> None:
        if self._client is not None:
            self._client.close()
            self._client = None
