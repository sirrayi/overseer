"""Provider-key mapping — the rig reads only provider-specific key names."""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from rig import keys

KEY_NAMES = ("OPENCODE_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GOOGLE_API_KEY")


@pytest.fixture
def clean_env(monkeypatch):
    for name in (*KEY_NAMES, "OVERSEER_PROVIDER"):
        monkeypatch.delenv(name, raising=False)
    return monkeypatch


@pytest.mark.parametrize(
    ("provider", "name"),
    [
        ("opencode", "OPENCODE_API_KEY"),
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
        ("gemini", "GOOGLE_API_KEY"),
    ],
)
def test_provider_maps_to_its_own_key(clean_env, provider, name):
    clean_env.setenv("OVERSEER_PROVIDER", provider)
    assert keys.key_env() == name
    assert keys.api_key() is None
    clean_env.setenv(name, "k")
    assert keys.api_key() == "k"


def test_default_provider_is_opencode(clean_env):
    assert keys.key_env() == "OPENCODE_API_KEY"
    clean_env.setenv("OPENCODE_API_KEY", "k")
    assert keys.api_key() == "k"


def test_other_providers_keys_and_empty_values_do_not_count(clean_env):
    clean_env.setenv("OVERSEER_PROVIDER", "anthropic")
    clean_env.setenv("OPENCODE_API_KEY", "wrong-provider")
    assert keys.api_key() is None
    clean_env.setenv("ANTHROPIC_API_KEY", "")
    assert keys.api_key() is None


def test_unknown_provider_is_refused(clean_env):
    clean_env.setenv("OVERSEER_PROVIDER", "bogus")
    with pytest.raises(ValueError, match="bogus"):
        keys.key_env()


def test_require_names_the_missing_variable(clean_env):
    with pytest.raises(RuntimeError, match="OPENCODE_API_KEY"):
        keys.require()
