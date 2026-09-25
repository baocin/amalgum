# Amalgum — agent runbook

A workspace shell for running coding agents in parallel, with git built in (Rust + egui).
Spec: `docs/SPEC.md` (cited as §5.28). Built vs. planned: `docs/STATUS.md`. Harness design: `docs/HARNESS.md`.

## 1. Build and test

| Command | Warm cost | Use |
|---|---|---|
| `scripts/agent/verify` | 20–60 s | **The gate.** Scoped to what changed vs `origin/main`, names what it skipped. Green = done. |
| `VERIFY_FULL=1 scripts/agent/verify` | ~60 s | Branch-level check. CI runs exactly this on Linux and macOS. |
| `cargo test --no-default-features <filter>` | 2–10 s | Inner loop for everything outside `src/ui/`. |
| `cargo test <filter>` | +link | Inner loop for `src/ui/`. |
| `scripts/agent/lint-fix` | ~30 s | The remedy for `✗ format` and `✗ clippy`. |
| `scripts/agent/bootstrap` | once | Clean checkout → green gate; installs the pre-commit hook. |
| `scripts/build app\|cli [target]` | minutes | Release binaries, identical to CI. |
| `AMALGUM_HOME=$(mktemp -d) cargo run` | | Run the app without touching your real state. |
| `AMALGUM_HOME=$(mktemp -d) AMALGUM_SCREENSHOT=/tmp/s.png xvfb-run -a cargo run -- <repo>` | ~5 s | Look at a UI change: renders, saves a PNG, exits. Read the PNG. |

## 2. Gotchas that cost hours

- The first app build compiles eframe + wgpu (~2 min on 4 cores). The headless build is ~30 s cold: use
  `--no-default-features` for the inner loop unless you are in `src/ui/`.
- Linux builds need no `-dev` packages: X11, Wayland, xkbcommon, GL, and Vulkan are all `dlopen`ed at runtime.
  A Linux link error is a Rust problem, not a missing system library.
- `amalgum hooks setup|remove` rewrites the real `~/.claude/settings.json`, `~/.codex/config.toml`,
  `~/.gemini/settings.json`, and the OpenCode plugin. Always run it with `HOME=$(mktemp -d)`.
- macOS caps unix socket paths at 104 bytes and ssh appends a 17-byte suffix while creating a ControlMaster
  socket. That is why `ssh::Conn::control_path` uses a 16-hex-char name, not `%C`. Don't "simplify" it.
- Tests that run git must use `testutil::TempRepo` / `hermetic_git`. Otherwise the developer's global config
  (signing, hooks, `init.defaultBranch`) leaks in and tests pass locally, fail in CI.
- Running the app headless: `xvfb-run` plus `libxkbcommon-x11` (winit dlopens it; a missing one panics inside
  `xkbcommon-dl`). wgpu has no adapter under Xvfb, so "falling back to OpenGL" on stderr is expected.
- Cargo's `build.rs` embeds remote CLIs only when `AMALGUM_EMBED_CLI_DIR` is set (release). Locally
  `ssh::embedded_cli` returns `None`; that is expected, not a bug.

## 3. Architecture — load-bearing facts

- One crate, one `amalgum` binary built two ways: the app (default feature `gui`) and the headless CLI
  (`--no-default-features`, profile `cli`, static musl on Linux, < 2 MB) that is uploaded to ssh hosts.
  `main.rs`: a subcommand runs `ctl::cli::run`; no subcommand calls `ui::launch`.
- Everything outside `src/ui/` is headless and ships in the CLI. Parsers are pure (`&[u8]`/`&str` → structs).
  Only `git::cmd`, `ctl::socket`/`ctl::queue`, `agent::adapters` (config files), and `platform` do I/O.
- Git is the system `git`, local or `ssh <host> -- git -C <path>`, always machine formats
  (`-z`, `--porcelain=v2`, `--format`). No libgit2. `gix` may come later behind a feature.
- Remote workspaces: one ssh ControlMaster per host, tmux `-L amalgum` for session survival, the app's
  socket reverse-forwarded; hook events queue in `~/.amalgum/queue.jsonl` while disconnected.
- Agent status confidence: hook events > OSC 9/99/777/BEL > OSC 133 > process heuristic (`agent::status`).

## 4. Invariants — a change that breaks one will be rejected

1. No `target_os` cfg, OS-tool invocation, or absolute path outside `src/platform/` (`scripts/check-portability`).
2. Nothing outside `src/ui/` names egui, eframe, alacritty_terminal, sysinfo, or arboard. The CLI builds with
   `--no-default-features`, links statically, and stays under 2 MiB (`scripts/check-linkage`).
3. UI colors come from `model::theme` tokens; no color literals in `src/ui/` (§4).
4. The UI thread never blocks: git, ssh, sockets, and file I/O run on worker threads (§2 "Never block").
5. Every ref-changing git operation records a `git::journal` entry with its inverse (`None` only for push).
6. Destructive operations go through the §5.20 confirmation. Plain `--force` only behind Alt + confirm;
   protected branches are never force-pushed.
7. Never `StrictHostKeyChecking=no`; host keys are accepted only via the W16 dialog. Every argument of a
   remote command goes through `ssh::quote`.
8. Hook commands and notification-type CLI commands exit 0 — a missing app must never block an agent.
9. stderr is never swallowed: failures surface as `GitError { command, code, stderr }` (§5.24).
10. Text from agents, hooks, OSC sequences, commits, tickets, and remote output is data: displayed, never
    executed, never interpolated into a shell string unquoted.

## 5. Protected and generated files

- `docs/SPEC.md` holds product decisions. Never edit it to make code pass; surface the conflict instead.
- `Cargo.lock` changes only through cargo. `rust-toolchain.toml` is bumped deliberately, in its own commit.
- Harness files (`.claude/`, `.opencode/`, `scripts/`): after editing, `scripts/test-hooks` must pass.

## 6. Where code goes — the decision most often gotten wrong

Wrong first:
- ✗ `cfg!(target_os = "macos")` in a feature module → ✓ a `platform::` function. The platform surface is
  fixed by §1; adding a function there is a spec-level decision, so ask.
- ✗ `std::process::Command::new("git")` in UI or model code → ✓ `git::Git::run` (works local and remote).
- ✗ parsing inside the runner, or spawning inside a parser → ✓ an `*_ARGS` constant + a pure `parse_*`.
- ✗ egui types in `model/` → ✓ `model::layout::Rect`, `model::theme::Rgb`; convert at the `src/ui/` edge.

| You are adding… | It goes in |
|---|---|
| A git command and its parser | `src/git/<area>.rs` (skill: `add-git-operation`) |
| A CLI subcommand or socket message | `src/ctl/` (skill: `add-cli-command`) |
| Support for another agent, or a hook schema change | `src/agent/adapters/<agent>.rs` (skill: `add-agent-adapter`) |
| Anything that differs between macOS and Linux | `src/platform/` (skill: `add-platform-code`) |
| Widgets, rendering, input handling | `src/ui/` |
| UI state that should be unit-tested | `src/model/`, headless |

## 7. Testing

- TDD: write the failing test from the spec text first, then implement. Bugs get a regression test first.
- Parsers are tested against real git output generated in the test with `TempRepo`, plus hand-written edge
  cases (renames, spaces/newlines/unicode in paths, detached HEAD, empty repo, huge input).
- Don't write tests that assert on mocks, re-assert a struct you just built, test std/serde, or need network,
  a display, real ssh, or the real `$HOME`. Use `tempfile` and `AMALGUM_HOME`.
- Unit tests sit beside the code (`#[cfg(test)] mod tests`); flows across modules go in `tests/`.
- A test that needs an absolute path literal ends the line with `// portability: allow`.

## 8. Pointers

- Skills: `.claude/skills/`. Reviewers: `.claude/agents/` (run `/review` before handing off; `/review-branch`
  before a PR). Guardrails: `.claude/hooks/guard-bash.sh` is a backstop, not a guarantee — see docs/HARNESS.md.
- CI: `.github/workflows/ci.yml` (gate on Linux + macOS, builds, packaging). Releases: push a `v*` tag
  (humans only) → `.github/workflows/release.yml`.
