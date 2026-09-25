#!/usr/bin/env bash
# Stop hook: "done" means the gate is green. Three short-circuits keep it cheap and satisfiable:
#   1. no toolchain here   → allow (a gate that cannot pass in this environment must not block)
#   2. same code as the last green run (HEAD + working-tree content hash) → allow
#   3. already blocked once on exactly this state and nothing changed → allow, to avoid a
#      loop the agent cannot break; the red gate is still reported to the human
# Otherwise run scripts/agent/verify (itself scoped: docs-only changes cost nothing) and block
# with the last 20 lines of its output, so the agent gets the error, not just the fact.
set -uo pipefail
input="$(cat)"
root="$(git rev-parse --show-toplevel 2>/dev/null)" || exit 0
cd "$root" || exit 0
command -v cargo >/dev/null 2>&1 || exit 0

state="$( { git rev-parse HEAD 2>/dev/null; git diff HEAD --binary 2>/dev/null
            git ls-files -o --exclude-standard -z | xargs -0 cat 2>/dev/null; } | cksum | cut -d' ' -f1)"
stamp=".claude/.gate-stamp"
[ -f "$stamp" ] && [ "$(sed -n 1p "$stamp")" = "green $state" ] && exit 0
if printf '%s' "$input" | grep -Eq '"stop_hook_active"[[:space:]]*:[[:space:]]*true' \
   && [ -f "$stamp" ] && [ "$(sed -n 1p "$stamp")" = "red $state" ]; then
  echo "stop-gate: gate is still red and nothing changed since the last block; allowing stop." >&2
  exit 0
fi

if out="$(scripts/agent/verify 2>&1)"; then
  printf 'green %s\n' "$state" > "$stamp"
  exit 0
fi
printf 'red %s\n' "$state" > "$stamp"
{ echo "scripts/agent/verify is red, so the work is not done. Fix this, then stop again:"
  printf '%s\n' "$out" | tail -20; } >&2
exit 2
