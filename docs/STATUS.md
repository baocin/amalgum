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

Verified by screenshot under Xvfb on Linux (`AMALGUM_SCREENSHOT`); macOS is built and linkage-checked
in CI but has not been looked at on a real display.

| Spec | Status | Note |
|---|---|---|
| §3 layout: menu bar, sidebar, git pane, workspace surface, status bar | ✅ | `ui/app.rs`; below-1100 px overlay sheet ⬜ |
| §4 theme: tokens, System/Light/Dark, `Mod+Alt+D`; bundled Inter + JetBrains Mono | ✅ | UI scale and system accent ⬜ |
| §5.1 welcome screen, recents, missing-path handling | ✅ | first-launch hooks banner shows a hint, not the installer |
| §5.2 open folder: `amalgum <path>`, drag-and-drop, path sheet, single-instance forwarding | ✅ | native folder picker ⬜; non-repo "Initialize?" ⬜ |
| §5.3 clone, §5.28 SSH workspaces | ⬜ | next milestone; `ssh` argv builders, quoting, parsers exist |
| §5.4 graph: streamed log, lanes, chips, working-tree row, selection | ✅ | context menu, focus mode, collapse, drag-and-drop ⬜ |
| §5.5–5.6 details and diff (word-level) | 🟡 | unified only; split view, images, blame ⬜ |
| §5.7 Changes: stage/unstage, per-hunk stage/unstage/discard, commit, amend | ✅ | line-range selection, sign-off, commit-and-push ⬜ |
| §5.8 search, §5.10 fetch/pull/push, §5.11–5.16 tags/stashes/compare/rebase UI | ⬜ | parsers and journal exist |
| §5.9 checkout (Refs double-click, palette) with undo | ✅ | create/rename/delete branch ⬜ |
| §5.18 undo/redo (`Mod+Z` in the git pane) | ✅ | undo panel ⬜ |
| §5.19 command palette (actions, workspaces, branches, recents) | ✅ | commits/stashes/worktrees groups ⬜ |
| §5.22 workspaces: create, switch (`Mod+1..9`), close, reorder, rename, restore on launch | ✅ | new-worktree sheet (W15) ⬜ |
| §5.24 toasts + details, notification panel | ✅ | log file ⬜ |
| §5.27 terminal: PTY, VT, tabs, splits, focus movement, zoom, copy/paste, OSC events, ports | ✅ | search, links, scrollback persistence ⬜ |
| §5.29 agent status from hooks, OSC 9/99/777/BEL, OSC 133; OS notifications | ✅ | process heuristic (source 4) ⬜ |
| §5.30 hibernation | ⬜ | rules exist in `agent::hibernate` |
| §5.23 settings window | ⬜ | Settings opens `settings.toml` in the default editor |
| §5.31 control socket in the app (notify, set-status, agent-event, open, run, focus, list) | ✅ | resume/hibernate answer "not available" |
| §8 accessibility beyond egui's AccessKit defaults | ⬜ | |

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
