# Agent harness

How this repo is set up so coding agents (and humans) can work on it reliably. The premise: an agent's
competence is bounded by its harness, so the harness is maintained like production code — it lands on `main`
with the product, and it has tests (`scripts/test-hooks`).

Every layer is a plain file. The two that matter most, the gate and the bootstrap, are shell scripts and work
with any agent or none.

| Layer | Files | Contract |
|---|---|---|
| Session start (web) | `.claude/hooks/session-start.sh` | Claude Code on the web only: pinned toolchain, crates, and the gate's builds warmed before the session starts (synchronous). |
| Bootstrap | `scripts/agent/bootstrap` | Clean checkout → green gate in one command. Idempotent; network used once to prime the cargo cache. |
| **Gate** | `scripts/agent/verify` | One command, one exit code, cheapest signal first, remedy on every `✗` line. Scoped to paths changed vs `origin/main`; prints what it skipped; `VERIFY_FULL=1` for branch-level callers; unknown scope runs everything. |
| Runbook | `CLAUDE.md` (`AGENTS.md` → symlink) | Under 200 lines; only facts that change a decision. Points at files instead of copying them. |
| Guardrails | `.claude/settings.json`, `.claude/hooks/guard-bash.sh` | Deny-list + allow-list, plus a PreToolUse script that blocks releases, pushes to main/master, force pushes, hook bypass, destructive deletes, repo-specific hazards, and credential reads. |
| Stop gate | `.claude/hooks/stop-gate.sh` | "Done" means the gate is green. Skips when no toolchain, caches the last green state, never loops forever. |
| Format on write | `.claude/hooks/format-on-write.sh` | rustfmt on the one `.rs` file just written. |
| Procedures | `.claude/skills/*/SKILL.md` | One skill per trap where the plausible action is wrong: platform code, git operations, CLI commands, agent adapters, CI/release. |
| Reviewers | `.claude/agents/*.md`, `/review`, `/review-branch` | Read-only, diff-only, enumerated checks with fixed severity, mandatory COVERAGE line. Portability (haiku), safety (opus), tests (sonnet). |
| Git hook | `scripts/git-hooks/pre-commit` | The gate at commit time, for tools without a Stop hook. Installed by bootstrap. |
| CI | `.github/workflows/ci.yml` | Runs the same `VERIFY_FULL=1` gate on Linux and macOS, then builds, linkage-checks, smoke-tests, and packages every target. |

## Repo-specific guardrails (deny-list category 5)
Amalgum's own features are the hazards an agent can trigger while developing it:
- The hooks installer rewrites the developer's real `~/.claude/settings.json`, `~/.codex/config.toml`,
  `~/.gemini/settings.json`, and OpenCode plugin → blocked unless the command sets `HOME=`.
- `tmux -L amalgum kill-server/kill-session` kills the user's live agent sessions → blocked.
- Writing into agent config directories from the shell → blocked.

## What the guardrails do NOT cover
They are pattern matches on the command text — a backstop, not a guarantee. Not covered: commands built at
runtime (`eval`, variables, aliases), a script written to disk and then executed, heredoc bodies fed to a
shell, anything a compiled program does, and edits made through non-shell tools. The enforcement that must
hold lives outside the agent: branch protection on `main`, required CI checks, and a human reviewing the PR.
Unattended runs belong in a container or VM without credentials.

## Portability across agents
- OpenCode reads `AGENTS.md` (symlink to `CLAUDE.md`) and `.claude/skills/` natively; `.opencode/commands`
  symlinks to `.claude/commands`; `.opencode/plugins/amalgum-guardrails.js` shells out to the same
  `guard-bash.sh`, so there is one rule source. OpenCode has no Stop-hook equivalent: the pre-commit hook and
  CI are the gate there.
- Codex and other tools read `AGENTS.md`; enforcement for them is the pre-commit hook and CI.

## Verifying the harness itself
- `scripts/test-hooks` exercises the guard's block and allow paths; `scripts/test-verify-scope` checks the
  gate's scoping regexes against paths that must and must not trigger the cargo legs;
  `scripts/test-check-secrets` exercises the secrets scanner's working-tree and history modes;
  `scripts/test-stop-gate` checks the Stop hook's cache key; `scripts/test-workflow-outputs` statically
  rejects patterns in the workflows that hide a failing command from `bash -e {0}` (GitHub's default
  `run:` shell, which also has no pipefail): `echo "x=$(cmd)" >> "$GITHUB_OUTPUT"` directly, and
  `x="$(cmd | head -1)"` unless that step sets `shell: bash`. All run by the gate whenever harness files
  change.
- To check the Stop hook live: break a test, ask the agent to finish, and watch it get blocked with the
  failing output; fix, and it stops cleanly.
- Measure: CI failures the local gate could have caught should be zero; gate wall-clock under 60 s warm.

## Deliberately not built yet
Backlog/progress-log state files, a work loop, and external ticket ingestion. They come after single-session
work is reliably good, and ticket text must then be treated as untrusted input (fenced where stored, never
followed as instructions, sensitive-area items routed to a human).
