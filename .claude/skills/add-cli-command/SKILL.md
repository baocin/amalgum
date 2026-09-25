---
name: add-cli-command
description: How to add or change an `amalgum` subcommand or control-socket message (SPEC §5.31) — anything a script, shell, or agent hook uses to talk to the running app, such as notify, set-status, open, run, list, focus, hooks, drain, or the rebase editor helpers. Use it even when phrased as "let agents tell the app X", "add a command-line flag", or "the hook should send Y", and when the remote CLI build breaks (musl, size, static linking). Not for in-app keyboard commands (those are model::keymap actions).
---

# Adding a CLI command or control message

The CLI is the same binary as the app built headless (`--no-default-features`): statically linked, under
2 MiB, uploaded to ssh hosts. Old CLIs on remote hosts keep talking to newer apps.

## Steps
1. Protocol: add a variant to `ctl::protocol::Command` (kebab-case `cmd`, `Option` fields omitted when
   `None`) with a wire-format test vector. Bump `protocol::VERSION` only for incompatible changes; the app
   keeps answering older versions.
2. Classify: notification-type (fire-and-forget; exit 0 with no app; queued on remote hosts) or query-type
   (exit 1 "Amalgum is not running"). Update `Command::is_notification`.
3. CLI: a `Cmd` variant in `ctl/cli.rs` and its arm in `run`. `workspace`/`tab` default from `Env`.
4. App: handle the request in the UI's socket handler; reply fast, do the work on a worker.

## Rules
- `src/ctl/` never imports `ui`, egui, or any `gui`-feature crate (`scripts/check-portability`). A new
  dependency must be pure Rust or the musl build stops being static.
- Hooks call the CLI with a 1 s timeout: nothing slow on a notification-type path.
- Hook payloads and stdin are untrusted data: validate, never interpolate into a shell string.
- User-supplied paths are resolved against `Env::cwd` before sending (the app's cwd differs).

## Verify
`cargo test --no-default-features ctl`, `cargo test --no-default-features --test cli`, then
`scripts/check-linkage "$(scripts/build cli x86_64-unknown-linux-musl)" cli` (static + size), then
`scripts/agent/verify`.
