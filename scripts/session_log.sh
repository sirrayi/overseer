#!/usr/bin/env bash
# Shared session log for multi-agent overseer work (B7 helper).
#
# Both Hermes sessions (this Telegram one and the terminal one) read and write
# this log so each can catch up on the other's progress without trusting its
# own (possibly stale) context window. The log is git-tracked and pushed so
# both sides see it.
#
# Usage:
#   scripts/session_log.sh add "message"   # append a line with agent + timestamp
#   scripts/session_log.sh tail [N]        # show last N lines (default 25)
#   scripts/session_log.sh sync            # pull latest, then push local log
#   scripts/session_log.sh reset           # (dev) empty the log
#
# Each agent should call `add` after a meaningful turn and `sync` to share it.
set -euo pipefail

LOG_FILE="SESSION_LOG.md"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_DIR"

AGENT_LABEL="${AGENT_LABEL:-unknown}"   # set per session, e.g. AGENT_LABEL=telegram

_ensure() {
    if [[ ! -f "$LOG_FILE" ]]; then
        printf '# Overseer shared session log\n\nBoth agents append after each turn. Lines are newest at the bottom.\n' > "$LOG_FILE"
    fi
}

cmd_add() {
    _ensure
    local msg="$1"
    local stamp
    stamp="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    printf '\n## %s (%s)\n%s\n' "$stamp" "$AGENT_LABEL" "$msg" >> "$LOG_FILE"
    echo "logged: $AGENT_LABEL @ $stamp"
}

cmd_tail() {
    _ensure
    local n="${1:-25}"
    tail -n "$n" "$LOG_FILE"
}

cmd_sync() {
    # Pull any remote changes first (fast-forward only), then push our log.
    git pull --ff-only --quiet origin main 2>/dev/null || true
    git add "$LOG_FILE"
    if git diff --cached --quiet; then
        echo "no changes to sync"
    else
        git commit -q -m "session log: $AGENT_LABEL $(date -u +%s)"
        git push --quiet origin main
        echo "synced to origin/main"
    fi
}

cmd_reset() {
    printf '# Overseer shared session log\n\nBoth agents append after each turn. Lines are newest at the bottom.\n' > "$LOG_FILE"
    echo "log reset"
}

case "${1:-}" in
    add)   cmd_add "${2:?usage: session_log.sh add \"message\"}" ;;
    tail)  cmd_tail "${2:-25}" ;;
    sync)  cmd_sync ;;
    reset) cmd_reset ;;
    *)     grep -E '^#' "$0" | head -20 ;;
esac
