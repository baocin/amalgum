#!/usr/bin/env bash
# PreToolUse guardrail for shell commands. Protocol (shared by every harness adapter, see
# .opencode/plugins/amalgum-guardrails.js): tool-call JSON on stdin; exit 2 with the reason on
# stderr to block; exit 0 to allow. The reason is an instruction to the agent.
#
# This is a pattern-matching BACKSTOP, not a guarantee. Known gaps: commands assembled at
# runtime (eval, variables, aliases, scripts written then executed), and anything a compiled
# program does. CI and branch protection are the enforcement that must hold.
set -uo pipefail
payload="$(cat)"
cmd=""
if command -v jq >/dev/null 2>&1; then
  cmd="$(printf '%s' "$payload" | jq -r '.tool_input.command // empty' 2>/dev/null || true)"
fi
# Fail closed: if the payload could not be parsed, scan the raw payload instead.
[ -n "$cmd" ] || cmd="$payload"
# Heredoc bodies are data (file contents, docs), not commands: drop them before matching.
# Gap: a body piped into a shell (`bash <<EOF … EOF`) is therefore not inspected.
cmd="$(printf '%s\n' "$cmd" | awk '
  skip { if ($0 ~ "^[ \t]*" delim "[ \t]*$") skip = 0; next }
  { print }
  match($0, /<<-?[ \t]*["\047]?[A-Za-z_][A-Za-z0-9_]*["\047]?/) {
    delim = substr($0, RSTART, RLENGTH); gsub(/<<-?[ \t]*|["\047]/, "", delim); skip = 1
  }')"

block() { printf '%s\n' "$1" >&2; exit 2; }
has() { printf '%s' "$cmd" | grep -Eq -- "$1"; }
# Flag checks ignore quoted text, so `git commit -m "about -n"` is not a bypass.
unquoted="$(printf '%s' "$cmd" | sed -E "s/'[^']*'//g; s/\"[^\"]*\"//g")"
flag() { printf '%s' "$unquoted" | grep -Eq -- "$1"; }

# 1. Release / deploy: releases are cut by pushing a v* tag from a human's machine.
has '(^|[;&|[:space:]])cargo[[:space:]]+publish' && block "cargo publish is not allowed; this crate is not published."
has 'gh[[:space:]]+release[[:space:]]+(create|upload|delete|edit)' && block "Creating or editing GitHub releases is a human step (push a v* tag; release.yml does the rest)."
has 'gh[[:space:]]+workflow[[:space:]]+run' && block "Triggering workflows manually is a human step."
if has 'git[[:space:]]+push'; then
  has '--tags|refs/tags/|[[:space:]:]v[0-9]+\.[0-9]+' && block "Pushing tags triggers a release; leave tagging to a human."
  # 2. Protected branches: explicit destinations, refspecs, and bare pushes while on them.
  has '([[:space:]:+/]|^)(main|master)([[:space:]]|$)' && block "Pushing to main/master is not allowed — push a feature branch (claude/*, agent/*) and open a PR."
  branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
  case "$branch" in main|master) block "You are on $branch; create a feature branch (git switch -c agent/<topic>) before pushing." ;; esac
  # 3. Force pushes (lease-protected pushes to your own branch are fine).
  stripped="$(printf '%s' "$unquoted" | sed -E 's/--force-with-lease(=[^[:space:]]*)?//g; s/--force-if-includes//g')"
  printf '%s' "$stripped" | grep -Eq -- '--force|(^|[[:space:]])-[a-zA-Z]*f([[:space:]]|$)|[[:space:]]\+[^[:space:]]' \
    && block "Force push is not allowed; use --force-with-lease on your own branch, or push a new branch."
fi

# 4. Gate bypass.
flag '--no-verify' && block "Do not bypass git hooks; fix what scripts/agent/verify reports instead."
flag 'git[[:space:]]+commit([[:space:]]+[^[:space:]]+)*[[:space:]]+-[a-zA-Z]*n([[:space:]]|$)' && block "git commit -n skips the pre-commit gate; fix what scripts/agent/verify reports instead."
has 'core\.hooksPath' && ! has 'core\.hooksPath[[:space:]]+scripts/git-hooks' && block "Do not change core.hooksPath; the pre-commit gate lives in scripts/git-hooks."

# 5. Destructive.
has 'rm[[:space:]]+-[a-zA-Z]*[rR][a-zA-Z]*[[:space:]]+(-[^[:space:]]+[[:space:]]+)*(/|~|\$HOME|\.git)([[:space:]/]|$)' \
  && block "Refusing to recursively delete /, ~, \$HOME, or .git."

# 6. Repo-specific: Amalgum edits real agent configs and kills real sessions.
if has 'hooks[[:space:]]+(setup|remove)' && has '(amalgum|cargo[[:space:]]+run)'; then
  has '(^|[[:space:]])HOME=' || block "amalgum hooks setup/remove edits the real ~/.claude, ~/.codex, ~/.gemini, and opencode configs. Run it with a throwaway home: HOME=\$(mktemp -d) cargo run -- hooks setup"
fi
has 'tmux[[:space:]]+(-[a-zA-Z]+[[:space:]]+)*-L[[:space:]]*amalgum[[:space:]].*kill-(server|session)' \
  && block "That kills the user's live Amalgum tmux sessions (running agents). Use a test socket: tmux -L amalgum-test …"
has '(>|tee|cp|mv|ln|sed[[:space:]]+-i)[^|;&]*(~|\$HOME)/\.(claude|codex|gemini|config/opencode)/' \
  && block "Do not edit the user's agent configs directly; tests must use a temp HOME."

# 7. Credential exfiltration.
has '(cat|less|head|tail|base64|cp|scp|curl)[^|;&]*(\.ssh/id_|\.p12|\.env([[:space:]]|$)|\.netrc)' \
  && block "Reading or copying credential files is not allowed."
has 'security[[:space:]]+find-(generic|internet)-password' && block "Reading the keychain is not allowed."

exit 0
