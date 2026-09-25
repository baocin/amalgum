# Status

What exists today, mapped to docs/SPEC.md. Update this file in the same PR as the change.
✅ implemented and tested · 🟡 partial (see note) · ⬜ not started.

## Build, CI, packaging (§1, §9)

| Item | Status | Where |
|---|---|---|
| One crate, two builds: app (`gui`) and headless CLI (`--no-default-features`) | ✅ | `Cargo.toml`, `src/main.rs` |
| Portability rules enforced (platform isolation, no absolute paths, headless boundary, theme tokens) | ✅ | `scripts/check-portability` (in the gate) |
| CI: gate on Linux + macOS; builds for x86_64-linux-gnu app, x86_64/aarch64-linux-musl CLI, aarch64/x86_64-apple-darwin app + CLI | ✅ | `.github/workflows/ci.yml` |
| Link allow-list for the Linux app, fully static CLI < 2 MiB | ✅ | `scripts/check-linkage` |
| macOS universal `.app` + `.dmg` (ad-hoc signed; Developer ID + notarization when secrets exist) | ✅ | `scripts/package-macos`, `release.yml` |
| Linux tarball + `.desktop`; AppImage when `APPIMAGETOOL` is set | 🟡 | `scripts/package-linux` — no icon asset yet, so AppImage is not built in CI |
| Remote CLIs embedded into release apps | ✅ | `build.rs`, `release.yml`, `ssh::embedded_cli` |
| Bundled fonts (Inter, JetBrains Mono) and icons | ⬜ | egui's default fonts are used |

## Core (headless, unit-tested)

| Spec | Module | Status |
|---|---|---|
| §5.31 control socket, protocol, CLI, offline queue | `ctl::*` | ✅ |
| §5.28 ssh/tmux argv builders, quoting, backoff, host-key prompt parsing | `ssh` | ✅ (the connection manager that runs them is ⬜) |
| §5.7, §5.17 status porcelain v2, in-progress operation detection | `git::status` | ✅ |
| §5.9–5.12, §5.21 refs, stashes, worktrees, remotes | `git::refs` | ✅ |
| §5.4 log parsing, incremental lane layout | `git::log`, `git::graph` | ✅ |
| §5.5–5.7 diff parsing, hunk/line patch synthesis, word diff | `git::diff` | ✅ |
| §5.2, §5.10 URL normalisation, repo identity, forge links, branch-name validation | `git::remote` | ✅ |
| §5.8 search query language | `git::search` | ✅ (tantivy index ⬜; `Query::matches` is the in-memory path) |
| §5.18 undo/redo journal | `git::journal` | ✅ |
| §5.16 interactive rebase plans + editor helpers | `git::rebase`, `amalgum --sequence-editor/--editor` | ✅ |
| §5.7 commit message rules | `git::message` | ✅ |
| §5.29 status model, per-agent adapters, hook installers | `agent::status`, `agent::adapters::*` | ✅ |
| §5.27 OSC 7/0/2/9/99/777/133 + BEL scanner | `agent::osc` | ✅ |
| §5.30 hibernation rules | `agent::hibernate` | ✅ (the UI that acts on them is ⬜) |
| §5.29 notification store | `agent::notifications` | ✅ |
| §5.27 port parsing (lsof/ss) | `ports`, `platform::listening_ports` | ✅ (polling from the UI ⬜) |
| §5.23 settings with spec defaults | `model::settings` | ✅ |
| §5.22 persistent state, §5.27 split layout | `model::state`, `model::layout` | ✅ |
| §5.19, §7 action table, presets, palette matching | `model::keymap`, `model::fuzzy` | ✅ |
| §4 theme tokens, ANSI + lane palettes, WCAG checks | `model::theme` | ✅ |

## App (`src/ui/`)

| Spec | Status | Note |
|---|---|---|
| §3 layout: menu bar, sidebar, git pane, surface, status bar | 🟡 | composed in `ui/app.rs`; components below |
| §5.1 welcome screen | see below | |
| §5.2 open folder / drop folder / `amalgum <path>` | 🟡 | path sheet + drag-and-drop; native folder picker ⬜ |
| §5.3 clone, §5.28 SSH workspaces | ⬜ | next milestone; argv builders and parsers exist |
| §5.27 terminal | see below | |
| §5.4–5.21 git pane | see below | |
| §5.29 agent status, notifications, OS notifications | 🟡 | hooks, OSC, and shell integration feed the status; the process heuristic (source 4) ⬜ |
| §5.30 hibernation | ⬜ | |
| §5.23 settings window | ⬜ | `settings.toml` is read; edit it by hand for now |

## Decisions that need a human

1. **Spec colors that fail the spec's own WCAG AA rule** (§4). Values were kept exactly as specified;
   `model::theme` tests pin these gaps so any change is noticed:
   - `diff.gutter.fg` on `bg.base`: 3.26:1 (both modes; text needs 4.5:1)
   - `border` on `bg.base`: 1.45:1 light, 1.57:1 dark (UI borders need 3:1)
   - `fg.secondary` on `bg.selected`, dark: 3.77:1
   - `fg.on-accent` (white) on `accent`, dark: 2.77:1
2. **Shortcut conflict in §7.1**: `Mod+Alt+↑/↓` is both "previous/next workspace" and "move focus between
   panes". The default table keeps the workspace meaning; pane focus up/down ships unbound (rebindable).
3. **Linux "Super as Mod" preset** (§1, §7): egui does not report the Super key on Linux, so the preset
   cannot be matched until egui exposes it.
4. **Background work uses std threads, not tokio** (§1 lists tokio): there is no async I/O yet, so threads +
   channels are simpler. Revisit when SSH connection management lands.
5. **License**: none chosen; `Cargo.toml` has `publish = false` and no license field.
6. **Apple signing**: release builds are ad-hoc signed until `MACOS_*` / `APPLE_*` secrets are added.
