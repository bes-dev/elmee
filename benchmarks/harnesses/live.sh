#!/bin/bash
# live.sh HARNESS PROMPT — one headless turn against the local model through the recording proxy.
B="$(cd "$(dirname "$0")" && pwd)"
case "$1" in
  elmee)  HOME=$B/homes/elmee-live exec elmee --no-session --reasoning-effort none -m local/qwen3.8-27b "$2" ;;
  pi)     HOME=$B/homes/pi-live PI_OFFLINE=1 exec pi -p --no-session --thinking off --provider local --model qwen3.8-27b "$2" ;;
  codex)  CODEX_HOME=$B/homes/codex-live exec codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -c model_reasoning_effort=\"none\" "$2" ;;
  claude) CLAUDE_CONFIG_DIR=$B/homes/claude-live ANTHROPIC_BASE_URL=http://127.0.0.1:18802 ANTHROPIC_API_KEY=sk-bench \
            CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 MAX_THINKING_TOKENS=0 exec claude -p --dangerously-skip-permissions --model qwen3.8-27b "$2" ;;
esac
