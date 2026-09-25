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
# Join `\`-newline line continuations into one line: has()/flag()/the segment regexes below all
# match line by line, so `git push \` then `  origin main` on the next line must be scanned as
# `git push   origin main`, not as two lines neither of which contains the whole statement.
# (awk, not a sed label loop: BSD sed on macOS rejects `:a;…;ba` one-liners.)
cmd="$(printf '%s\n' "$cmd" | awk '{ if (sub(/\\$/, "")) printf "%s ", $0; else print }')"

block() { printf '%s\n' "$1" >&2; exit 2; }
has() { printf '%s' "$cmd" | grep -Eq -- "$1"; }
# Quote-stripper for flag checks, so `git commit -m "about -n"` is not a bypass. This is one
# alternation, not two separate passes: two independent passes (strip '…' then strip "…") pair
# an apostrophe from inside a double-quoted string with a later, unrelated single quote and
# delete everything in between — `"it's done" … 'x'` loses everything from the apostrophe to
# the first `'`, including a `git push` in between.
strip_quotes() { printf '%s' "$1" | sed -E "s/\"[^\"]*\"|'[^']*'//g"; }
unquoted="$(strip_quotes "$cmd")"
flag() { printf '%s' "$unquoted" | grep -Eq -- "$1"; }

# 1. Release / deploy: releases are cut by pushing a v* tag from a human's machine.
has '(^|[;&|[:space:]])cargo[[:space:]]+publish' && block "cargo publish is not allowed; this crate is not published."
has 'gh[[:space:]]+release[[:space:]]+(create|upload|delete|edit)' && block "Creating or editing GitHub releases is a human step (push a v* tag; release.yml does the rest)."
has 'gh[[:space:]]+workflow[[:space:]]+run' && block "Triggering workflows manually is a human step."

# Global options between `git` and the subcommand (`-C <dir>`, `-c k=v`, `-P`/`-p`, `--no-pager`,
# …) must not let a push or commit slip past the checks below — agents are told to use absolute
# paths since cwd resets between Bash calls, and `git -C <path> …` is the natural way to do that.
GIT_OPTS='([[:space:]]+(-C|-c)[[:space:]]*[^[:space:]]+|[[:space:]]+-[pP]|[[:space:]]+--[a-zA-Z-]+(=[^[:space:]]+)?)*'

# Each `git … push` invocation, from `git` through the end of that statement (up to the next
# `;`, `&&`, `||`, or `|`), taken from the RAW command — not quote-stripped. A push target can
# legitimately be quoted (`git push origin "refs/tags/v1.2.0"`, `git -C "/path" push origin
# main`), and a push can arrive wrapped in an outer shell's own quoting (`bash -c 'git push
# origin main'`, `sh -c "git push origin main --tags"`); stripping quotes first would blank
# exactly the text the tag/branch checks below look for. Scoping to the push segment (rather
# than the whole command) is still what keeps a rebase target, an earlier diff argument, or a
# quoted commit message that mentions "main" from blocking a push to a feature branch.
push_segments="$(printf '%s\n' "$cmd" | grep -Eo -- "git${GIT_OPTS}[[:space:]]+push([[:space:]][^;&|]*)?" || true)"
if [ -n "$push_segments" ]; then
  printf '%s\n' "$push_segments" | grep -Eq -- '--tags|refs/tags/|--follow-tags|[[:space:]:]v[0-9]+\.[0-9]+' \
    && block "Pushing tags triggers a release; leave tagging to a human."
  # 2. Protected branches: explicit destinations, refspecs, and bare pushes while on them.
  printf '%s\n' "$push_segments" | grep -Eq -- '([[:space:]:+/]|^)(main|master)([[:space:]]|$)' \
    && block "Pushing to main/master is not allowed — push a feature branch (claude/*, agent/*) and open a PR."
  # GUARD_BRANCH (tests only) overrides which branch counts as "checked out"; otherwise honor
  # an explicit `-C <dir>` in the push segment, and fall back to this process's cwd.
  gitdir="$(printf '%s\n' "$push_segments" | grep -Eo -- '(^|[[:space:]])-C[[:space:]]*[^[:space:]]+' | head -1 | sed -E 's/^[[:space:]]*-C[[:space:]]*//')"
  # The captured token may be a quoted path (`-C "/home/user/amalgum"`, `-C '/home/user/amalgum'`);
  # the quote characters are not part of the directory name.
  gitdir="$(printf '%s' "$gitdir" | sed -E "s/^\"(.*)\"\$/\\1/; s/^'(.*)'\$/\\1/")"
  if [ -n "${GUARD_BRANCH:-}" ]; then
    branch="$GUARD_BRANCH"
  elif [ -n "$gitdir" ]; then
    branch="$(git -C "$gitdir" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
    # -C names a directory this process cannot resolve (an unexpanded `$VAR`, a path relative
    # to some other cwd, …): fall back to this process's own cwd instead of failing open.
    [ -n "$branch" ] || branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
  else
    branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
  fi
  case "$branch" in main|master) block "You are on $branch; create a feature branch (git switch -c agent/<topic>) before pushing." ;; esac
  # 3. Force pushes (lease-protected pushes to your own branch are fine).
  stripped="$(printf '%s\n' "$push_segments" | sed -E 's/--force-with-lease(=[^[:space:]]*)?//g; s/--force-if-includes//g')"
  printf '%s' "$stripped" | grep -Eq -- '--force|(^|[[:space:]])-[a-zA-Z]*f[a-zA-Z]*([[:space:]]|$)|[[:space:]]\+[^[:space:]]' \
    && block "Force push is not allowed; use --force-with-lease on your own branch, or push a new branch."
fi

# 4. Gate bypass.
flag '--no-verify' && block "Do not bypass git hooks; fix what scripts/agent/verify reports instead."
# git commit -n (skip hooks) can be combined into any short-flag cluster, in any order and any
# position (-n, -an, -nm, -amn, -m wip -n, …); a single regex anchored on one flag shape at the
# end of the command cannot see all of those, so scope to each `git … commit` segment (raw, so
# `-C`'s value doesn't get in the way — see push_segments above) and scan its flag tokens, with
# quoted values (`-m "note: -n flag docs"`) blanked out first so a message isn't mistaken for a
# flag. `&` stays a segment boundary (not just `;` and `|`) so a chained `sort -rn` or a piped
# `tail -n 5` is never pulled into the commit segment — the cost is that `2>&1` inside a commit
# segment also ends it early, so a `-n` placed after a `2>&1` redirect is not caught here.
commit_segments="$(printf '%s\n' "$cmd" | grep -Eo -- "git${GIT_OPTS}[[:space:]]+commit([[:space:]][^;&|]*)?" || true)"
if [ -n "$commit_segments" ]; then
  set -f # word-splitting a commit segment below must not glob-expand a token like `-n *.txt`
  while IFS= read -r seg; do
    for tok in $(strip_quotes "$seg"); do
      case "$tok" in
        --*) : ;; # long flags (--no-verify is handled above, --dry-run etc. are not -n)
        -*n*) set +f; block "git commit -n skips the pre-commit gate; fix what scripts/agent/verify reports instead." ;;
      esac
    done
  done <<EOF
$commit_segments
EOF
  set +f
fi
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
