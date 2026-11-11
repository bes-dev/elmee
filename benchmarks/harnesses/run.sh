#!/bin/bash
# run.sh HARNESS PROMPT  — one headless turn of HARNESS against the stub, in $PWD.
B="$(cd "$(dirname "$0")" && pwd)"
URL=${BENCH_URL:-http://127.0.0.1:18801}
case "$1" in
  elmee)  HOME=$B/homes/elmee exec elmee --no-session -m stub/bench "$2" ;;
  pi)     HOME=$B/homes/pi PI_OFFLINE=1 exec pi -p --no-session --provider stub --model bench "$2" ;;
  codex)  CODEX_HOME=$B/homes/codex exec codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
            -c model_provider=stub -c "model_providers.stub={name=\"stub\",base_url=\"$URL/v1\",wire_api=\"responses\"}" -m bench "$2" ;;
  claude) CLAUDE_CONFIG_DIR=$B/homes/claude ANTHROPIC_BASE_URL=$URL ANTHROPIC_API_KEY=sk-bench \
            CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 exec claude -p --dangerously-skip-permissions --model bench "$2" ;;
esac
