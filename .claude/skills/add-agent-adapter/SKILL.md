---
name: add-agent-adapter
description: How to support a new coding agent or update an existing one (Claude Code, Codex, OpenCode, Gemini CLI, …) in Amalgum — its hook payloads, how the hooks installer writes into its config, how its events map to tab status and notifications, and how a hibernated session resumes. Use it when an agent changed its hook schema or config format, when `hooks status` reports "outdated", when a tab shows the wrong status or a generic message instead of the agent's own words, or when resume after hibernation fails. Not for OSC/bell/terminal heuristics (agent/osc.rs, agent/status.rs).
---

# Agent adapters

One file per agent: `src/agent/adapters/<agent>.rs` (SPEC §12 risk 3). Everything that depends on that
agent's hook schema, config file, or CLI flags lives there and nowhere else. Exemplar: `adapters/claude.rs`.

## The file's functions
- `adapt(raw) -> Option<AgentEvent>`: payload → status, the agent's own message, session id, pid, cwd.
  Never panics on any JSON; unknown events → `None`.
- `config_path(home)`, `install(text, command)`, `uninstall(text)`, `state(text, command)`: pure text
  transforms. The block we install is marker-keyed, so re-running replaces it in place and `uninstall`
  removes exactly it. A file that cannot be parsed is an `Err`, never clobbered.
- `resume(session_id)`: the shell command that resumes the session (§5.30).

## Rules
- Installed hooks always exit 0 with a 1 s timeout: a missing app never blocks the agent.
- Bump `adapters::HOOK_VERSION` whenever the installed block changes so `hooks status` reports `Outdated`.
- Hibernation never triggers on heuristics, only on a hook-confirmed idle/stop (`agent::hibernate`).
- Payload text is data: display it; never execute it or interpolate it into a shell string.
- Tests use `tempfile::tempdir()` as `home`, never the real one. The guard hook blocks running the hooks
  installer without a `HOME=` override for the same reason.

## Test (TDD)
Capture a real payload (a hook that tees stdin to a file), paste it as a fixture, assert the mapped
`AgentEvent`. Then: install → `Installed` → install again (byte-identical) → uninstall → the user's unrelated
config survives. Finish with `scripts/agent/verify`.
