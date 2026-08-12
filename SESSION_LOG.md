# Overseer shared session log

Both agents append after each turn. Lines are newest at the bottom.

## 2026-08-12T19:00:58Z (telegram)
HERMES (telegram): set up scripts/session_log.sh shared log. Both agents append after each turn (AGENT_LABEL=<name>). Use: scripts/session_log.sh add/tail/sync. Latest repo state: B8 committed (c79bd90, 273 tests). This log solves stale-context reviews: each agent catches up from the file instead of its own memory.
