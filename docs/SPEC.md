# Amalgum Specification

A workspace shell for running coding agents in parallel, with git built in. Vertical sidebar of repos, remotes, and workspaces. Each workspace is a set of terminals bound to one location (local clone, worktree, or `ssh host:path`) plus a git pane for that location. Agent-aware: the sidebar tells you which tab needs you and why. Rust-native, single static binary. Ships first on macOS. Every design decision must keep a single-binary Linux build possible without a rewrite.

Not a GitKraken clone. Not a terminal that happens to show a branch name. The git integration is built around the workspace shell: every ref-changing action is one right-click away from the commit it affects, and every terminal knows which branch and box it lives on.

---

## 1. Tech Stack

| Layer | Choice | Why |
|---|---|---|
| UI | `eframe` + `egui` (wgpu backend, glow fallback) | One static executable per OS. No webview, no system runtime dependency. Same pixels on macOS and Linux. |
| Accessibility | `accesskit` via egui integration | VoiceOver on macOS, AT-SPI on Linux, one code path. |
| Terminal | `alacritty_terminal` | VT state machine, grid, scrollback, damage tracking, and its own unix PTY module. Maintained, on crates.io, used by Zed. Renderer is ours (Section 5.27). |
| Process inspection | `sysinfo` | Foreground process and child tree lookup on both OSes for agent detection. |
| Git backend | System `git` via `std::process::Command`, output parsed with `--porcelain` / `-z` / `--format` | One backend for local and `ssh host:path` workspaces. SSH agent, `~/.ssh/config`, credential helpers, hooks, and proxies all work with zero code. |
| Git accelerator | `gix` (gitoxide), optional, local workspaces only | Fast graph walk, blame, and diff when measured to beat the CLI. Feature-gated; the app works without it. |
| SSH | System `ssh` subprocess with `ControlMaster` | Config, agents, ProxyJump, hardware keys, host-key prompts for free. `russh` would reimplement all of it. |
| Remote session survival | Remote `tmux -L amalgum` on a private server socket | Sessions survive disconnects and app restarts with no daemon to write. Per-host off switch. |
| Control socket | Unix socket, newline-delimited JSON | `amalgum` CLI and agent hooks talk to the running app. Filesystem permissions are the auth. |
| Syntax highlighting | `tree-sitter` + `tree-sitter-highlight`, grammars compiled in | Same grammars on every OS. |
| File watching | `notify` | FSEvents on macOS, inotify on Linux. |
| Config dirs | `directories` crate | `~/Library/Application Support/amalgum` on macOS, `$XDG_CONFIG_HOME/amalgum` on Linux. |
| Clipboard | `arboard` | Cross-platform. |
| System theme | `eframe` `follow_system_theme` | Cross-platform dark/light detection. |
| Fonts | Bundled Inter (UI) and JetBrains Mono (terminal, code) | Identical rendering on every OS. System font optional via settings. |
| Async | `tokio` worker pool for git, ssh, and socket I/O; one reader thread per PTY; UI thread never blocks | |

### Why not Tauri
Tauri needs a system webview. On Linux that is `webkit2gtk`, a large dynamic dependency that is not part of the binary. That fails the single-binary requirement. Tauri also renders differently per OS webview. eframe has neither problem.

### Why no embedded browser pane
Same reason. The only Rust webview is `wry`, which needs `webkit2gtk` on Linux. Instead: detected listening ports appear in the sidebar; clicking one opens the URL in the default browser, through an `ssh -L` forward for remote workspaces (Section 5.28). If a browser split is ever added it is feature-gated so Linux still builds a single binary without it.

### Portability rules (enforced by CI)
- All platform-specific code lives in `src/platform/{macos,linux}.rs` behind `#[cfg(target_os)]`. Everything else is portable.
- The `platform` module exposes exactly: `open_in_default_app(path_or_url)`, `reveal_in_file_manager(path)`, `open_terminal_at(path)`, `install_cli_shim()`, `listening_ports(pids) -> Vec<(pid, port)>` (`lsof` on macOS, `ss` on Linux), `primary_modifier_name()`, `window_decorations()`. Nothing else may call OS APIs.
- PTYs, unix sockets, signals, `ssh`, and `tmux` are POSIX and identical on both OSes. They do not enter the platform module.
- No Objective-C, Swift, Xcode, or `.app` bundle assumptions in the core crate. Bundling is a packaging step in `scripts/`.
- No absolute paths. Every path comes from `directories` or git config.
- The crate builds two binaries from one source: the app (default features) and a headless CLI (`--no-default-features`, no egui, static musl on Linux) that is uploaded to remote hosts.
- `cargo build --release --target x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` (CLI only), and `aarch64-apple-darwin` all run in CI from day one. Linux CI builds but does not ship until Linux is a target.
- Runtime dynamic links allowed on Linux app binary: libc, libm, libxkbcommon, libwayland/libX11, libvulkan or libGL. Nothing else. The CLI binary is fully static.
- Keyboard shortcuts are written with `Mod`. macOS: `Mod` = ⌘, `Alt` = ⌥. Linux: `Mod` = Ctrl+Shift, `Mod+Shift` = Ctrl+Alt, `Mod+Alt` = Ctrl+Alt+Shift, because bare Ctrl belongs to the terminal. A **Super as Mod** preset exists in settings for desktops that leave Super free.
- Menu bar is drawn by egui in-window on both platforms. On macOS an additional native menu bar mirrors it (in `platform/macos.rs`) so ⌘Q, ⌘H, ⌘, behave natively. Linux shows only the in-window bar.
- Vibrancy, traffic-light window buttons, and Touch Bar are cosmetic and macOS-only. They live in `platform/macos.rs` and no feature depends on them.
- Required on the remote end for SSH workspaces: OpenSSH ≥ 6.7 (unix socket forwarding), `git`, and optionally `tmux ≥ 3.2` (session survival). Nothing else is installed except the Amalgum CLI binary under `~/.amalgum/bin/`.

---

## 2. UI / UX Principles

Every item here is a point where GitKraken or a plain terminal is weak. These are requirements.

### Agents first
- One glance at the sidebar answers: which workspace needs me, on which branch, on which box, and what it said last.
- A tab that needs input gets a colored ring on its pane, a badge on its sidebar row, and an entry in the notification panel with the agent's own message, never a generic "waiting for input".
- Nothing about agent awareness requires configuration beyond `amalgum hooks setup`. Without hooks the app degrades to terminal-level heuristics and still works.

### Speed
- Cold start to interactive sidebar and restored terminals: under 500 ms.
- Git pane first paint under 100 ms after workspace open on a 50k-commit repo; further history streams in.
- 60 fps at all times, including while a terminal receives 100 MB/s of output. Idle terminals cost zero frames.
- Memory under 300 MB with three workspaces and six live terminals open.

### Never block
- No modal spinners. Every git, ssh, and socket operation runs in the background with a progress pill in the status bar.
- Errors appear as a toast with the exact stderr one click away. Never a modal alert.
- The only modal dialogs are destructive confirmations (Section 5.20). Settings is a separate non-modal window.

### Keyboard first
- `Mod+K` command palette lists every action, workspace, branch, stash, tag, and recent location. Fuzzy matched.
- No action requires a mouse. Full map in Section 7.

### Discoverable
- Every action is reachable three ways: menu bar, context menu or button, and palette. Tooltips show shortcuts. `?` shows the shortcut overlay when a terminal is not focused, `Mod+/` always.
- Every empty list states why it is empty and offers the one action that fills it (W20).
- Every in-progress or unusual state (detached HEAD, mid-merge, mid-rebase, no upstream, reconnecting, hibernated) is named in the status bar or the sidebar row with the next action as a button.

### Universal undo
- `Mod+Z` in the git pane undoes the last ref-changing operation. `Mod+Shift+Z` redoes. Section 5.18.

### Honest and quiet
- No account, no login, no telemetry, no update nag, no upsell, no feature gates. Works offline for local workspaces.
- Nothing is stored on remote hosts except the CLI binary, a tmux socket, and an offline event queue under `~/.amalgum/`.

---

## 3. Layout

```
┌──────────────────────────────────────────────────────────────────────────┐
│ Menu bar (in-window)                                                     │
├────────────────┬─────────────────────────────────────────────────────────┤
│ Sidebar        │ Workspace: conduit / origin / feat-oauth      [⚑ git ⌘J]│
│                ├───────────────────────────┬─────────────────────────────┤
│ ▾ conduit      │ claude ▸ zsh  +           │ Git pane                    │
│  ▾ origin      │ ┌───────────┬───────────┐ │ [Graph] [Changes] [Refs]    │
│   ● feat-oauth │ │ $ claude  │ $ cargo   │ │ ● feat-oauth  Add OAuth     │
│     ~/w/c-oauth│ │ …         │   test    │ │ │ ● Refresh tokens          │
│   ◐ fix-crash  │ │           │           │ │ ●─┘ main                    │
│     ~/w/conduit│ │           │           │ │                             │
│  ▸ upstream    │ │           │           │ ├─────────────────────────────┤
│ ▾ amalgum      │ │           │           │ │ ab12cd3  Add OAuth          │
│  ▾ origin      │ │           │           │ │ M src/auth/mod.rs  +12 −3   │
│   ○ main       │ │           │           │ │ …                           │
│     gpu-box:~/g│ └───────────┴───────────┘ │                             │
│ + New workspace│                           │                             │
├────────────────┴───────────────────────────┴─────────────────────────────┤
│ ⑂ feat-oauth ↑2 ↓0 · 3 changed   ● claude running   :3000   Last fetch 4m 🔔│
└──────────────────────────────────────────────────────────────────────────┘
```

- **Sidebar** (left, default 240 px, `Mod+Shift+1` toggles). Tree: Repo › Remote › Workspace. Workspace rows show a status dot, name, and a subtitle line with location (`~/w/c-oauth` or `gpu-box:~/g`), branch when it differs from the name, `↑↓` counts, dirty count, detected ports, and the last notification text truncated to one line. Drag to reorder. Section 5.22.
- **Workspace surface** (center). A tab strip of terminals for the active workspace, each tab a split tree. `Mod+T` new terminal, `Mod+D` split right, `Mod+Shift+D` split down, `Mod+W` close. Section 5.27.
- **Git pane** (right by default, bottom via settings, default 420 px, `Mod+J` toggles, `Mod+\` maximizes). Bound to the workspace location. Three views on a segmented control: **Graph** (graph with details stacked below), **Changes** (staging and commit), **Refs** (branches, remotes, tags, stashes, worktrees). Maximized, the git pane becomes the classic three-column layout: refs | graph | details. Every git flow in Section 5 runs inside this pane. Section 5.4 onward.
- **Status bar**. Left: current branch, `↑N ↓M`, dirty count (click opens Changes). Center: agent status for the focused tab, ports for the workspace, progress pills. Right: last fetch, connection state for remote workspaces, notification bell with unread count.
- Every pane is resizable by drag; layout persists per workspace.
- Minimum window size 900×600. Below 1100 px the git pane overlays the workspace surface as a slide-in sheet.
- Toolbar `⋯` on the workspace header: **Open in external terminal**, **Open in editor**, **Reveal in file manager**, **Copy path**, **Copy ssh command** (remote).
- Every toolbar button and icon has a tooltip with its name and shortcut after 400 ms hover.
- Every row that has a context menu shows a `⋯` button on hover. Right-click, `Ctrl`-click (macOS), `Mod+Enter`, and `⋯` all open the same menu.
- Relative times ("2h") show the absolute time on hover. Setting **Absolute timestamps** replaces them.
- Toasts stack bottom-left over the sidebar so they never cover a terminal cursor, the diff, or the commit button. The bell opens the notification panel (Section 5.29).

### 3.1 Wireframes

All wireframes at 1280×800, light and dark identical in structure. `▸` collapsed, `▾` expanded, `●` node or running, `◐` needs input, `○` idle, `◌` hibernated, `⊘` disconnected, `◆` selected, `✓` current branch, `⋯` row action button (appears on hover, also opens the context menu for trackpad users).

#### W1 Welcome, no workspaces (5.1)
```
┌────────────────────────────────────────────────────────────────────────────┐
│ File  Edit  View  Workspace  Repository  Window  Help                      │
├────────────────────────────────────────────────────────────────────────────┤
│                                                                            │
│                              Amalgum                                       │
│                                                                            │
│      ┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐        │
│      │  Open Folder     │ │ Clone Repository │ │  Connect to Host │        │
│      │      Mod+O       │ │   Mod+Shift+O    │ │   Mod+Shift+K    │        │
│      └──────────────────┘ └──────────────────┘ └──────────────────┘        │
│                                                                            │
│   Recent locations                                                Clear    │
│   ┌────────────────────────────────────────────────────────────────────┐   │
│   │ conduit / origin     ~/work/conduit                 2 hours ago    │   │
│   │ amalgum / origin     gpu-box:~/amalgum              yesterday      │   │
│   │ linux / torvalds     ~/src/linux                    3 days ago     │   │
│   │ old-thing            ~/tmp/old-thing    Not found   [Remove]       │   │
│   └────────────────────────────────────────────────────────────────────┘   │
│                                                                            │
│   Drop a folder anywhere in this window to open it as a workspace.         │
│                                                                            │
│   ⓘ Install the `amalgum` command line tool and agent hooks [Set up]  [Skip]│
└────────────────────────────────────────────────────────────────────────────┘
```

#### W2 Main shell (3, 5.22, 5.27)
```
┌────────────────────────────────────────────────────────────────────────────┐
│ File  Edit  View  Workspace  Repository  Window  Help                      │
├────────────────┬───────────────────────────────────────────────────────────┤
│ 🔍             │ conduit / origin / feat-oauth   ~/w/c-oauth   [⚑ Git ⌘J] ⋯│
│ ▾ conduit      ├──────────────────────────────┬────────────────────────────┤
│  ▾ origin   ⟳  │ [claude ●] [zsh] [cargo] +   │ [Graph] [Changes] [Refs]   │
│   ● feat-oauth │ ┌──────────────┬───────────┐ │ ⚠ Working tree · 3 changed │
│     ~/w/c-oauth│ │ ╭─ claude ─╮ │ $ cargo   │ │ ● ✓feat-oauth Add OAuth 2h │
│     ↑2 ·3 :3000│ │ │ Ran tests│ │   watch   │ │ │ ● Refresh tokens     5h │
│     "Ran tests"│ │ │ 12 passed│ │ …         │ │ ●─┘ main   Fix login   1d │
│   ◐ fix-crash  │ │ │ Continue?│ │           │ │ │ ● v2.1.0 Bump        2d │
│     ~/w/conduit│ │ ╰──────────╯ │           │ │ ◆ Handle 401          2d │
│     "Approve?" │ │              │           │ ├────────────────────────────┤
│  ▸ upstream    │ │              │           │ │ 5e6f  Handle 401 [Copy]    │
│ ▾ amalgum      │ │              │           │ │ M src/auth/mod.rs  +12 −3  │
│  ▾ origin      │ │              │           │ │ M src/auth/token.rs +4 −1  │
│   ◌ main       │ │              │           │ ├────────────────────────────┤
│     gpu-box:~/g│ │              │           │ │ @@ -40,7 +40,16 @@         │
│     hibernated │ │              │           │ │ 41 - ttl_ms: u64           │
│ + New workspace│ └──────────────┴───────────┘ │ 41 + ttl: Duration         │
├────────────────┴──────────────────────────────┴────────────────────────────┤
│ ⑂ feat-oauth ↑2 ↓0 · 3 changed   ● claude running 4m   :3000   4m ago  🔔 2│
└────────────────────────────────────────────────────────────────────────────┘
```

#### W3 Sidebar row anatomy (5.22, 5.29)
```
│ ◐ fix-crash                        ⋯ │  ← status dot, name, hover actions
│   ~/w/conduit · main · ↑0 ↓3 · 2      │  ← location, branch, ↑↓, dirty count
│   :8080 :5173                          │  ← listening ports, clickable
│   "Approve edit to src/db.rs?"      2m │  ← last notification, one line, age
```
Remote workspace variant:
```
│ ⊘ main                              ⋯ │
│   gpu-box:~/amalgum · reconnecting 12s │
```
Remote row:
```
│  ▾ origin  github.com/acme/conduit  ⟳ │  ← remote, host/path, fetch
```

#### W4 Workspace surface, splits and tab strip (5.27)
```
├──────────────────────────────────────────────────────────────────────────┤
│ [claude ●] [zsh] [cargo ○] [+]                      ⇆ split  ⊟ zoom  ⋯   │
├────────────────────────────────┬─────────────────────────────────────────┤
│ ╭──────────────────────────────╮│ $ cargo watch -x test                   │
│ │ $ claude                     ││ Compiling conduit v0.3.1                │
│ │ ● Read src/auth/mod.rs       ││ Running 12 tests                        │
│ │ ● Edited src/auth/token.rs   ││ test auth::refresh ... ok               │
│ │                              ││                                         │
│ │ I changed the TTL to a       ││                                         │
│ │ Duration. Run the tests?     ││                                         │
│ │ › Yes  › No                  ││                                         │
│ ╰──────────────────────────────╯│                                         │
│   needs input · 2m              │                                         │
└────────────────────────────────┴─────────────────────────────────────────┘
```
The left pane has the needs-input ring in `accent`. Focus a pane to clear its ring. `Mod+Alt+←/→/↑/↓` moves between panes.

#### W5 Git pane, Changes view (5.7)
```
├──────────────────────────────┤
│ [Graph] [Changes] [Refs]     │
│ Unstaged (2)     [Stage all] │
│ ☐ M src/auth/mod.rs       ⋯  │
│ ☐ ? notes.txt             ⋯  │
│ Staged (1)     [Unstage all] │
│ ☑ M src/lib.rs            ⋯  │
├──────────────────────────────┤
│ Fix token TTL units          │ ← subject, ruler at 50/72
│ ──────────────────────────── │
│ The refresh path passed ms   │
│ into a Duration…             │
│ ☐ Amend  ☐ Sign-off          │
│ [Commit ▾]  →origin Mod+Enter│
├──────────────────────────────┤
│ src/auth/mod.rs              │
│ @@ -40,7 @@ [Stage hunk][Discard]│
│ 40   fn refresh(             │
│ 41 - ttl_ms: u64             │
│ 41 + ttl: Duration           │
```
Checkboxes stage/unstage with the mouse; `s`/`u`/`Space` do it from the keyboard. The commit editor sits between the file lists and the diff so it is always visible without scrolling. The commit button names the bound remote.

#### W6 Split diff, git pane maximized with `Mod+\` (5.6)
```
├────────────────────────────────────────────────────────────────────────────┤
│ ← Back   src/auth/mod.rs   M  +12 −3    [Unified|Split]  [Whitespace] [⋯] │
├──────────────────────────────────────┬─────────────────────────────────────┤
│ 38  pub struct Session {             │ 38  pub struct Session {            │
│ 39      user: UserId,                │ 39      user: UserId,               │
│ 40 -    ttl_ms: u64,                 │ 40 +    ttl: Duration,              │
│ 41  }                                │ 41  }                               │
│     ┈┈ 12 unchanged lines ┈┈ [expand]│     ┈┈ 12 unchanged lines ┈┈        │
│ 54 -    let t = ttl_ms / 1000;       │ 54 +    let t = ttl.as_secs();      │
│                                      │ 55 +    debug_assert!(t > 0);       │
```

#### W7 Search results in the graph (5.8)
```
├──────────────────────────────┤
│ 🔍 author:mp path:src/auth  ×│
│ 142 matches                  │
│ ──────────────────────────── │
│   Fix login timeout  2h ab12 │
│   Refresh tokens     5h ef56 │
│   Handle 401         2d 5e6f │
```
Graph lanes hidden, rows flat. `Enter` jumps to the commit in the full graph. `Esc` restores.

#### W8 Command palette (5.19)
```
                ┌──────────────────────────────────────────────┐
                │ > push                                       │
                ├──────────────────────────────────────────────┤
                │ Actions                                      │
                │  ⇡ Push to origin                    Mod+P   │
                │  ⇡ Push with lease                           │
                │  ⇡ Push all tags                             │
                │ Workspaces                                   │
                │  ◐ conduit / fix-crash      needs input  2m  │
                │ Branches                                     │
                │  ⑂ feat/push-retry          Enter to checkout│
                └──────────────────────────────────────────────┘
```

#### W9 Interactive rebase, git pane maximized (5.16)
```
├──────────────┬────────────────────────────────────┬────────────────────────┤
│ (refs)       │ Rebase feat-oauth onto main · 4 commits │ Preview            │
│              │ ┌────────────────────────────────┐ │ ● feat-oauth           │
│              │ │ ⋮ [pick   ▾] ef56 Refresh tokens│ │ │ ● Add OAuth (2 sq.)   │
│              │ │ ⋮ [squash ▾] cd34 Add OAuth     │ │ ●─┘ main               │
│              │ │ ⋮ [reword ▾] 5e6f Handle 401 ▸  │ │                        │
│              │ │   Handle 401 on token refresh  │ │                        │
│              │ │ ⋮ [drop   ▾] 7a8b Add retry     │ │                        │
│              │ └────────────────────────────────┘ │                        │
│              │ p pick r reword e edit s squash    │                        │
│              │ f fixup x drop · drag ⋮ to reorder │                        │
│              │             [Cancel]  [Start rebase]│                        │
```

#### W10 Conflict state (5.17)
```
├──────────────────────────────┤
│ Conflicts (2)                │
│ ! src/auth/mod.rs         ⋯  │
│ ! Cargo.lock              ⋯  │
│ Unstaged (0)                 │
│ Staged (5)                   │
├──────────────────────────────┤
│ src/auth/mod.rs              │
│ [Take ours: main]            │
│ [Take theirs: feat-oauth]    │
│ [Open in editor] [Merge tool]│
│ [Mark resolved] 2 markers    │
│ <<<<<<< main (ours)          │
│   ttl: Duration,             │
│ =======                      │
│   ttl_ms: u64,               │
│ >>>>>>> feat-oauth           │
├──────────────────────────────┴───────────────────────────────────────────┤
│ ⚠ Merging feat-oauth into main · 2 conflicts        [Abort]  [Continue ✗]│
```

#### W11 Compare refs (5.13)
```
├──────────────────────────────┤
│ Compare [main ▾] ⇄ [feat ▾]  │
│ 12 commits · 34 files · ...  │
│ [Files] [Commits]            │
│ ▾ src/auth                   │
│    M mod.rs      +12 −3      │
│    A oauth.rs    +210        │
│ ▾ tests                      │
│    A oauth.rs    +88         │
```

#### W12 Blame, git pane maximized (5.15)
```
├────────────────────────────────────────────────────────────────────────────┤
│ ← Back   Blame src/auth/mod.rs @ ab12cd3   main › 3c4d › ab12   [History] │
├────────────┬───────────────────────────────────────────────────────────────┤
│ ab12 mp 2h │ 38  pub struct Session {                                      │
│ ab12 mp 2h │ 39      user: UserId,                                         │
│ 5e6f mp 2d │ 40      ttl: Duration,                                        │
│ 9c0d jr 8d │ 41  }                                                         │
```
Gutter background runs `blame.hot` to `blame.cold` by age. `Mod+←` on a line re-blames at that commit's parent.

#### W13 Destructive confirmation (5.20)
```
              ┌────────────────────────────────────────────────┐
              │ Delete branch `feat/oauth`?                    │
              │                                                │
              │ 4 commits are not on `main` and will be lost   │
              │ unless another ref points to them:             │
              │   ef56  Refresh tokens                          │
              │   cd34  Add OAuth                               │
              │   5e6f  Handle 401 on refresh                   │
              │   7a8b  Add retry helper                        │
              │                                                │
              │ ☐ Also delete `origin/feat/oauth`              │
              │                                                │
              │              [Cancel  Esc]  [Delete  Mod+Enter]│
              └────────────────────────────────────────────────┘
```

#### W14 Toast and notification panel (5.24, 5.29)
```
│ ┌─────────────────────────────────────────┐                               │
│ │ ✗ Push rejected                          ×│                              │
│ │ Remote has commits you don't have.       │                              │
│ │ [Pull then push] [Force with lease] [Details ▾]                          │
│ └─────────────────────────────────────────┘                               │
├──────────────┴─────────────────────────────────────────────────────────────┤
│ ⑂ main ↑2 ↓1                                        Last fetch 0s   🔔 3  │
```
Notification panel (`Mod+Shift+I`), slides in from the right:
```
                            ┌──────────────────────────────────────────┐
                            │ Notifications              [Clear all]   │
                            │ ◐ conduit / fix-crash              2m    │
                            │   claude: Approve edit to src/db.rs?     │
                            │ ● amalgum / main (gpu-box)         9m    │
                            │   codex: Finished: added retry helper    │
                            │ ✗ conduit / feat-oauth             14m   │
                            │   Push rejected                          │
                            │ ✓ conduit / feat-oauth             15m   │
                            │   Committed ab12cd3 · Fix login timeout  │
                            └──────────────────────────────────────────┘
```
Click a row to focus that workspace and tab. Unread rows bold. Panel keeps the last 500.

#### W15 New workspace sheet (5.22)
```
              ┌────────────────────────────────────────────────┐
              │ New workspace in conduit / origin              │
              │                                                │
              │ Location  (●) Existing folder  ( ) New worktree│
              │           ( ) Remote host                      │
              │                                                │
              │ Folder    [~/work/conduit               ][Browse]│
              │                                                │
              │ ── if New worktree ──                          │
              │ Branch    [feat/oauth-retry     ] from [main ▾]│
              │ Path      [~/work/conduit-oauth-retry        ] │
              │                                                │
              │ ── if Remote host ──                           │
              │ Host      [gpu-box            ▾] from ssh config│
              │ Path      [~/amalgum                         ] │
              │ ☑ Keep sessions alive with tmux on the host    │
              │                                                │
              │ Name      [oauth-retry                       ] │
              │ ☑ Open a terminal   ☐ Run: [claude           ] │
              │                          [Cancel]  [Create ↵] │
              └────────────────────────────────────────────────┘
```

#### W16 Remote workspace connecting and reconnecting (5.28)
```
├──────────────────────────────────────────────────────────────────────────┤
│ amalgum / origin / main  gpu-box:~/amalgum                   [⚑ Git ⌘J] ⋯│
├──────────────────────────────────────────────────────────────────────────┤
│ ⟳ Connecting to gpu-box…  verifying amalgum CLI 0.4.1 ▮▮▮▯       [Cancel]│
│                                                                          │
│ (terminal area greyed, last screen contents visible)                     │
│ $ claude                                                                 │
│ ● Working on tests…                                                      │
│                                                                          │
│ ⚠ Connection lost 12 s ago · retrying in 6 s          [Retry now] [Details]│
```
On first connect to a host with an unknown key:
```
              ┌────────────────────────────────────────────────┐
              │ Unknown host key for gpu-box                   │
              │ ED25519 SHA256:k3s9…Qz                          │
              │ Verify this fingerprint with the host owner.   │
              │ ssh said:                                      │
              │   The authenticity of host 'gpu-box' can't be… │
              │         [Copy fingerprint] [Cancel] [Trust]    │
              └────────────────────────────────────────────────┘
```

#### W17 Hibernated tab (5.30)
```
├──────────────────────────────────────────────────────────────────────────┤
│ [claude ◌] [zsh]                                                         │
├──────────────────────────────────────────────────────────────────────────┤
│ (last screen, greyed 50 %)                                               │
│ $ claude                                                                 │
│ ● Finished: added retry helper. 12 tests pass.                           │
│                                                                          │
│         ┌──────────────────────────────────────────────┐                 │
│         │ ◌ Hibernated 40 min ago to free 1.2 GB       │                 │
│         │   claude session 7f3a…  cwd ~/w/conduit      │                 │
│         │   [Resume  Enter]  [Close tab]               │                 │
│         └──────────────────────────────────────────────┘                 │
```

#### W18 Ports popover (5.27, 5.28)
```
   :3000 :5173
   ┌───────────────────────────────────────┐
   │ Listening in this workspace           │
   │ :3000  node (pid 4412)   [Open ↗]     │
   │ :5173  vite (pid 4420)   [Open ↗]     │
   │ Remote: opens via ssh -L on gpu-box   │
   └───────────────────────────────────────┘
```

#### W19 Branch popover from a commit (5.9)
```
   ◆───┘ Handle 401 on refresh  2d  5e6f
   ┌──────────────────────────────────┐
   │ New branch at 5e6f               │
   │ [fix/refresh-401               ] │
   │ ☑ Checkout after create          │
   │ ☐ Open in new worktree workspace │
   │            [Cancel]  [Create ↵]  │
   └──────────────────────────────────┘
```

#### W20 Empty states
Every list has one. Format: one line of grey text and one action.
- Sidebar, no workspaces: W1.
- Repo with no workspaces: "No workspaces for conduit." [New workspace]
- Graph, empty repo: "No commits yet. Stage files and make the first commit." [Go to Changes]
- Stashes: "No stashes." [Stash changes]
- Remotes: "No remotes." [Add remote]
- Tags: "No tags." [Tag this commit]
- Worktrees: "One worktree (this one)." [Add worktree]
- Search: "No commits match." [Clear filters]
- Working tree clean: "Nothing to commit. Working tree clean." with a small ✓.
- Notifications: "Nothing yet. Run `amalgum hooks setup` so agents can notify you." [Set up hooks]
- Terminal tab strip with no tabs: "No terminals." [New terminal Mod+T]

---

## 4. Theme and Dark Mode

### Behaviour
- Three settings: **System** (default), **Light**, **Dark**.
- System mode follows the OS and switches live, no restart, within one frame of the OS notification.
- Theme switch does not lose scroll position, selection, terminal contents, or an in-progress commit message.
- Syntax highlighting, graph lanes, diff colors, and terminal ANSI colors each have a light and a dark variant. There is no shared color between modes except pure black and white text on accent.
- All foreground/background pairs meet WCAG AA (4.5:1 for text, 3:1 for UI borders and graph lines).
- Reduce Motion (macOS) / `gtk-enable-animations=false` (Linux) disables all animation. Animations are otherwise 120 ms ease-out and only used for pane collapse, toast enter/exit, the needs-input ring pulse, and hover.
- Graph lanes must be distinguishable under deuteranopia and protanopia. A **Shape-coded lanes** setting adds a distinct node glyph per lane (circle, square, diamond, triangle, hexagon, star, cross, ring). Status dots use shape as well as color (● ◐ ○ ◌ ⊘).

### Semantic tokens
Every color in the app references one of these. No literal hex in UI code.

| Token | Light | Dark | Use |
|---|---|---|---|
| `bg.base` | `#FFFFFF` | `#1E1F22` | Terminal, graph, diff, details background |
| `bg.raised` | `#F5F5F7` | `#2B2D31` | Sidebar, toolbar, status bar, tab strip |
| `bg.sunken` | `#EBEBEF` | `#17181A` | Input fields, code blocks |
| `bg.hover` | `#EDEDF2` | `#33353A` | Row hover |
| `bg.selected` | `#D9E7FF` | `#2C4370` | Selected row, active tab |
| `bg.selected.unfocused` | `#E8E8EC` | `#3A3C42` | Selected row, pane unfocused |
| `fg.primary` | `#1D1D1F` | `#E6E6E8` | Body text |
| `fg.secondary` | `#5C5C61` | `#A0A0A6` | Timestamps, hashes, subtitles, hints |
| `fg.disabled` | `#A5A5AA` | `#5E5E64` | |
| `fg.on-accent` | `#FFFFFF` | `#FFFFFF` | Text on accent buttons |
| `border` | `#D6D6DB` | `#3D3F45` | Pane dividers, inputs, split handles |
| `border.focus` | `#0A66FF` | `#5B9BFF` | Focus ring, 2 px; focused terminal pane, 1 px |
| `accent` | `#0A66FF` | `#5B9BFF` | Primary buttons, links, needs-input ring |
| `success` | `#1A7F37` | `#3FB950` | Ahead count, pushed, staged, running dot |
| `warning` | `#9A6700` | `#D29922` | Behind count, detached HEAD, uncommitted, reconnecting |
| `danger` | `#CF222E` | `#F85149` | Force push, delete, conflicts, disconnected |
| `status.idle` | `#8E8E93` | `#6E6E76` | Idle dot |
| `status.hibernated` | `#A5A5AA` | `#5E5E64` | Hibernated dot, greyed pane overlay at 50 % |
| `diff.add.bg` | `#DDF4E4` | `#1B3A26` | |
| `diff.add.fg` | `#116329` | `#7EE787` | |
| `diff.add.word` | `#ABE9B9` | `#2B6B3E` | Word-level highlight inside added line |
| `diff.del.bg` | `#FFE1E1` | `#3E1C1F` | |
| `diff.del.fg` | `#A40E26` | `#FF7B72` | |
| `diff.del.word` | `#FFB3B3` | `#7A2D33` | |
| `diff.hunk.bg` | `#EEF3FF` | `#1F2A44` | Hunk header row |
| `diff.gutter.fg` | `#8E8E93` | `#6E6E76` | Line numbers |
| `conflict.ours` | `#DDF4E4` | `#1B3A26` | |
| `conflict.theirs` | `#E5E1FF` | `#2A2545` | |
| `blame.hot` | `#FFB3B3` | `#7A2D33` | Newest lines |
| `blame.cold` | `#F5F5F7` | `#2B2D31` | Oldest lines |

Users can override `accent` with the OS accent color (**Use system accent** setting, on by default on macOS, off on Linux where no standard exists).

### Terminal ANSI palette
Terminal foreground is `fg.primary`, background `bg.base`, cursor `accent`, selection `bg.selected`. Setting **Terminal theme** accepts any Ghostty or iTerm2 color file for each of light and dark; defaults:

| Index | Name | Light | Dark |
|---|---|---|---|
| 0 | black | `#1D1D1F` | `#1E1F22` |
| 1 | red | `#CF222E` | `#FF7B72` |
| 2 | green | `#1A7F37` | `#3FB950` |
| 3 | yellow | `#9A6700` | `#D29922` |
| 4 | blue | `#0A66FF` | `#5B9BFF` |
| 5 | magenta | `#8250DF` | `#A371F7` |
| 6 | cyan | `#1B7C83` | `#39C5CF` |
| 7 | white | `#D6D6DB` | `#E6E6E8` |
| 8 | bright black | `#5C5C61` | `#6E6E76` |
| 9 | bright red | `#A40E26` | `#FFA198` |
| 10 | bright green | `#116329` | `#56D364` |
| 11 | bright yellow | `#7A5200` | `#E3B341` |
| 12 | bright blue | `#0969DA` | `#79C0FF` |
| 13 | bright magenta | `#6639BA` | `#D2A8FF` |
| 14 | bright cyan | `#15646B` | `#56D4DD` |
| 15 | bright white | `#FFFFFF` | `#FFFFFF` |

### Graph lane palette
Eight lanes, cycling. Lane index is assigned per branch name hash so a branch keeps its color across sessions and repos.

| Lane | Light | Dark |
|---|---|---|
| 0 | `#0A66FF` | `#5B9BFF` |
| 1 | `#1A7F37` | `#3FB950` |
| 2 | `#BF3989` | `#F778BA` |
| 3 | `#9A6700` | `#D29922` |
| 4 | `#8250DF` | `#A371F7` |
| 5 | `#0969DA` | `#79C0FF` |
| 6 | `#CF222E` | `#F85149` |
| 7 | `#1B7C83` | `#39C5CF` |

### Syntax theme
Light: GitHub Light token colors. Dark: GitHub Dark token colors. Token names follow tree-sitter highlight captures (`keyword`, `string`, `comment`, `function`, `type`, `variable`, `number`, `operator`, `punctuation`). Users can point at any `.tmTheme` in settings; both light and dark slots accept one.

### Typography
- UI: Inter, 13 px body, 11 px secondary, 15 px section headers, 20 px welcome screen title.
- Terminal, code, and hashes: JetBrains Mono 12 px. Line height 1.5 in diffs, 1.2 in terminals. Terminal font size independent (`Mod+=`/`-`/`0` with a terminal focused).
- Setting: **UI scale** 80–200 % in 10 % steps, independent of OS scaling.

---

## 5. Flows

Conventions used below:
- **Trigger** lists every way to start the flow. Every trigger also exists in the command palette.
- **Git** lists the underlying command so behaviour is unambiguous. For remote workspaces every git command is prefixed with `ssh <host> -- git -C <path>` over the shared ControlMaster (Section 5.28).
- **Undo** says what `Mod+Z` does afterwards.
- **Errors** lists failure modes and the toast text.
- Status-bar progress pill: `<verb> <target>… ▮▮▯▯` with a cancel `×` where the operation supports cancel (fetch, clone, push, pull, connect).
- Git flows (5.4 through 5.21) operate inside the git pane of the active workspace. Single-key shortcuts in those flows apply only when the git pane has focus; a focused terminal receives every key except `Mod` combinations.

### 5.1 First launch and welcome screen
1. No workspaces shows the welcome screen (W1): **Open Folder**, **Clone Repository**, **Connect to Host**, **Recent locations** (last 20, with repo/remote, location, last-opened time), and a footer line offering to install the CLI and agent hooks.
2. On first launch only, the footer banner offers **Set up**: installs the CLI shim (5.31) and runs `amalgum hooks setup` for every detected agent (5.29), listing what it will write before writing. **Skip** persists. No other onboarding.
3. Recent list rows: `Enter` or double-click opens as a workspace. `Backspace` removes from recents. Missing local paths show greyed with "Not found" and a **Remove** button. Remote entries show the host and connect on open.

### 5.2 Open folder as workspace
- **Trigger**: `Mod+O`, File → Open Folder, drag a folder onto the window, `amalgum .` or `amalgum <path>` from the terminal, welcome Recent list, palette "Open recent: <name>".
1. If path is inside a work tree, resolve to the repo root. If a bare repo, open in read-only graph mode (no Changes view).
2. If path is not a git repo: toast "Not a git repository. Initialize one here?" with **Initialize** button (`git init`) and **Choose another**.
3. Identify the repo by its normalized primary remote URL (fallback: path). Add a Repo group to the sidebar if new, with one Remote row per `git remote`. Ask which remote to bind only if there is more than one (default `origin`).
4. Create a workspace bound to that remote and location (5.22), named after the branch. Open one terminal at the path. Focus it.
5. Git pane renders within 100 ms with whatever is loaded; the rest streams. Start the file watcher on `.git/` and the work tree. Start the search indexer if stale (Section 6).
- **CLI shim**: 5.31.

### 5.3 Clone
- **Trigger**: `Mod+Shift+O`, File → Clone, welcome **Clone**, palette "Clone".
1. Sheet with: URL (auto-filled from clipboard if it looks like a git URL), destination folder (picker, default from settings **Default clone directory**), folder name (auto-derived, editable), **Shallow clone** checkbox with depth field, **Open as workspace** checkbox (default on), **Clone on remote host** dropdown (off / a host from ssh config; when set, destination is a remote path and the clone runs there).
2. **Clone** runs `git clone --progress [--depth N] <url> <dest>`. Progress pill parses stderr percentages. Cancel kills the process and deletes the partial folder.
3. On success, open as workspace (5.2) if checked. Add to recents.
- **Errors**: auth failure ("Authentication failed. Check your SSH agent or credential helper."), host key unknown (W16 dialog; never auto-accept), destination exists ("Folder already exists"), network.

### 5.4 Graph browsing
- Rows: lane graphics | refs (chips) | subject | author avatar (optional, gravatar, cached) | author name | relative time | short hash. Column widths draggable; hash and author hideable via right-click on header. In the default (narrow) git pane, avatar and author are hidden and appear on hover.
- Row height default 24 px, settable 20–32.
- Uncommitted changes appear as a top row "Working tree · N changed" when there are changes, styled with `warning`. Selecting it opens the Changes view (5.7). The status bar also shows "N changed" and clicking it does the same, so Changes is one click away regardless of scroll position.
- Navigation: `j`/`k` or `↓`/`↑` move selection. `Mod+↑`/`Mod+↓` jump to top/bottom. `PageUp`/`PageDown`. `Home`/`End`. Selection scrolls into view with 3 rows of margin.
- `Enter` focuses the details area. `Esc` returns focus to the graph.
- Hover a row: highlight parent and child rows and the connecting lane segments at full opacity, dim others to 60 %.
- Click a ref chip: jumps the selection to that ref. Double-click a branch chip: checkout (5.9). Drag a branch chip onto another branch chip: menu **Merge into**, **Rebase onto**. Drag a commit row onto a branch chip: **Cherry-pick onto**. Drag a branch chip onto the sidebar: **New workspace in worktree** (5.22).
- Right-click a row opens the commit context menu. It is the primary git surface and must contain, in this order:
  1. **Checkout** (branch if the row has one, else detached commit)
  2. **Create branch here…** · **Create tag…**
  3. **Push `feat` to origin** · **Push to…** ▸ remotes · **Force push with lease** · **Set upstream…** ▸ remote branches (present when the row carries a local branch)
  4. **Merge `feat` into current** · **Rebase current onto `feat`** (present when the row carries a branch that is not current)
  5. **Cherry-pick onto current** · **Revert**
  6. **Reset `main` to here** ▸ **Soft** / **Mixed** / **Hard**
  7. **Undo last operation** (names it: "Undo: commit ab12cd3") · **Redo**
  8. **Interactively rebase from here** · **Edit message** · **Squash into parent** · **Fixup into parent**
  9. **Compare with current** · **Compare with…**
  10. **Rename branch** · **Delete branch** · **Delete tag** (present per ref on the row)
  11. **Copy hash** · **Copy subject** · **Copy permalink** · **Open in browser**
  Items that do not apply are hidden, not disabled, except Undo/Redo which are disabled with their description when the stack is empty.
- **Focus mode**: click the eye icon on a branch chip or press `f` with a branch selected. Only that branch's ancestry (and merges into it) render; everything else fades to 25 % and collapses to single dashed lines. Press `f` again or `Esc` to exit. A banner at the top of the graph says "Showing only `feat/x` · Esc to exit".
- **Collapse merged branch**: a merge commit shows a `⊟` toggle. Clicking collapses the merged branch's commits into one node "N commits from `feat/x`". State persists per repo. Setting **Auto-collapse merged branches** (default off).
- Single-key actions `m`, `r`, `p` (merge, rebase, cherry-pick) open an anchored popover stating the exact operation ("Merge `feat/oauth` into `main`") with `Enter` to run and `Esc` to cancel. `c`, `b`, `t` act directly since they are cheap and undoable.
- **Zoom**: `Mod+=` / `Mod+-` / `Mod+0` with the graph focused adjusts row height and font 10 % per step. Trackpad pinch does the same. Both platforms.
- Streaming: the graph renders the first 500 commits immediately and appends in 2000-commit batches on a background thread. Scrolling near the end triggers the next batch. A thin progress line under the segmented control shows load progress until complete. Layout of already-rendered rows never shifts when new rows arrive.
- Setting **Show remote branches** (default on; the bound remote's branches are always shown, other remotes' are toggled by this), **Show tags** (default on), **Show stashes in graph** (default on, stashes appear as a dashed node off their parent), **Author avatars** (default on, needs network; failure silently shows initials).

### 5.5 Commit details
- Selecting a commit fills the details area below the graph: header (full hash with **Copy** button, author, committer if different, date absolute + relative, parents as clickable hashes, refs), full message (subject bold, body monospaced, links clickable), then the changed-file list.
- File list: tree view or flat list, toggle `t`. Each row: status glyph (A/M/D/R/C with colors), path, +N −M counts. Renames show `old → new`.
- `↓`/`↑` move through files; `Enter` or click opens the diff below (or full-pane when maximized). `o` opens the file at that commit in the default app (remote: after copying it to a temp dir via scp).
- Diff view (5.6).
- Details for a commit load lazily: message and header from cache instantly, file list within 50 ms, diff on file selection.
- For merge commits: a selector **Diff vs first parent / second parent / combined**.

### 5.6 Diff view
- Default unified. `d` toggles split. Split view scrolls in sync.
- Syntax highlighted via tree-sitter using the file extension; unknown extensions plain.
- Word-level intra-line diff on by default; setting to disable.
- Gutter: old and new line numbers. Hunk header rows collapsible.
- **Whitespace** toggle `w` (`--ignore-all-space`). **Context lines** setting 3/5/10/all.
- Images (png, jpg, gif, webp, svg, bmp): side-by-side old/new with a slider swipe and an onion-skin toggle. Binary otherwise: "Binary file · 12.4 KB → 12.9 KB".
- Files over 1 MB or 10k lines: render the first 2000 lines with a **Load more** row. Never freezes.
- Long lines soft-wrap; setting to disable.
- `Mod+F` in the diff searches within the diff. `Mod+C` with a selection copies plain text without gutter.
- Right-click a line: **Copy line**, **Copy permalink** (path:line at commit hash), **Blame this line** (5.15), **Open file at this commit**, **Send path:line to terminal** (types `path:line` into the focused terminal without Enter, for pasting to an agent).

### 5.7 Working tree, staging, and commit (Changes view)
- **Trigger**: select the "Working tree" row, `Mod+Shift+C`, Repository → Commit, palette "Commit", the segmented control.
- Layout top to bottom: Unstaged list, Staged list, commit editor, diff. The editor is always visible without scrolling (W5).
- File rows: checkbox, status glyph, path, +N −M. The checkbox stages/unstages with one click. Click elsewhere on the row opens the diff in the lower half. `s` stages the focused unstaged file, `u` unstages the focused staged file, `Mod+A` stages all, `Mod+Shift+A` unstages all. Buttons **Stage all** / **Unstage all** on section headers.
- Diff in this view is interactive: each hunk has **Stage hunk** / **Unstage hunk** / **Discard hunk**. Click-drag on line numbers selects a line range; `Space` stages/unstages the selection, `Backspace` discards it (confirm, Section 5.20). Git: `git apply --cached` with a synthesised patch, `git apply -R` for discard.
- Untracked files appear in Unstaged with status `?`. Ignored files hidden; toggle **Show ignored** in the section menu. The Unstaged header menu also has **Discard all changes** and **Delete untracked files** (`git clean -fd`), both confirmed (5.20).
- Right-click file: **Stage**, **Unstage**, **Discard changes** (confirm), **Add to .gitignore** (offers `path`, `*.ext`, `dir/`), **Open**, **Reveal in file manager**, **Copy path**, **Send path to terminal**, **File history**, **Blame**.
- Commit message editor: subject field (single line, 72-char ruler, turns `warning` past 50 and `danger` past 72) and body field (multi-line, wraps at 72 visually, hard-wrap setting off by default). Pre-filled from `commit.template` if set. Standard editing shortcuts. Pasting multi-line text into the subject puts the first line in the subject and the rest in the body. `Tab` moves from subject to body. `Mod+Enter` commits from either field. Draft persists per workspace across restarts.
- **Amend** checkbox (`Mod+Shift+M`): loads the last commit's message and stages nothing extra. Disabled if HEAD is pushed to its upstream unless setting **Allow amending pushed commits** (default off; when on, the button turns `warning` and reads **Amend (pushed)**).
- **Sign-off** checkbox persists per repo. **GPG/SSH sign** follows `commit.gpgsign`; status shown as a lock icon, not configurable in-app in v1.
- **Commit** button: disabled with tooltip when staged is empty or subject blank. Runs `git commit [-S] [--amend] [--signoff] -F <tempfile>` (remote: message piped over stdin). On success: toast "Committed ab12cd3 · Fix login bug" with **Undo** button, clear editor, select the new commit in the graph.
- **Commit and push** split button: same, then 5.10 push to the bound remote. The button reads "Commit and push → origin".
- Hooks: pre-commit and commit-msg run with output streamed to the toast's stderr view. Hook failure blocks the commit and shows the output.
- **Undo**: `git reset --soft HEAD~1` (message restored into the editor, files remain staged). Amend undo resets to the pre-amend commit from the journal.
- Live updates: file watcher (local) or a 2 s `git status --porcelain=v2 -z` poll while the Changes view is visible (remote) refreshes the lists within 200 ms of a change, debounced. Selection and scroll are preserved by path.

### 5.8 Search
- **Trigger**: `/` or `Mod+F` with the graph focused, palette "Search commits".
- Field appears above the graph. Typing filters live, under 50 ms on 100k commits using the on-disk index (local) or `git log --grep/--author/-- path` with a 300 ms debounce (remote).
- Plain text matches subject, body, author name, author email, full and short hash, branch and tag names, and changed file paths.
- Prefix filters: `author:`, `path:`, `before:`, `after:`, `hash:`, `branch:`, `tag:`, `msg:`. Multiple filters AND together. Quotes for phrases.
- Results replace the graph rows (graph lines hidden, rows flat) with a count "142 matches". `Esc` clears and returns to the graph with the previously selected commit re-selected.
- `Enter` on a result selects it and exits search, positioning the graph on that commit.
- Search history: `↑` in an empty field recalls the last 20 queries.

### 5.9 Branches
- Refs view **Local** section lists branches with ahead/behind chips (`↑2 ↓1`) vs upstream, current branch bold with a check glyph. **Remote** section groups by remote, the bound remote first.
- **Create**: `Mod+B`, right-click a commit → **Create branch here**, palette. Popover (W19): name, base (default: selected commit, else HEAD), **Checkout after create** (default on), **Open in new worktree workspace** (default off; when on, creates a worktree and a workspace in one step, 5.22). Validates against `git check-ref-format` live. Git: `git branch <name> <base>` then optional checkout. Undo: delete the branch (and checkout back).
- **Checkout**: double-click in Refs or graph chip, `Enter` on a Refs row, right-click → **Checkout**, palette "Checkout <name>". Git: `git checkout <name>`. If the working tree has changes that conflict, toast "Checkout would overwrite 3 files" with **Stash and checkout**, **Force**, **Cancel**. Checking out a remote branch creates a tracking local branch of the same name (`git checkout -b <name> --track <remote>/<name>`), asking only if the local name exists. If another workspace of the same repo has this branch checked out in a worktree, offer **Switch to that workspace** instead. Undo: checkout the previous branch.
- **Checkout commit** (detached): right-click commit → **Checkout commit**. Status bar shows "Detached at ab12cd3" in `warning` with a **Create branch** button.
- **Rename**: right-click → **Rename**, or `F2` on a selected Refs row. Inline edit. Git: `git branch -m`. Undo: rename back. Renaming the branch a workspace is named after offers to rename the workspace.
- **Delete**: right-click → **Delete**, `Backspace` on a Refs row. If unmerged into HEAD, confirm (5.20) with "Branch has 4 commits not on `main`". Git: `git branch -d` or `-D`. If a remote tracking branch exists, the confirm offers **Also delete on <remote>**. Undo: recreate at the journaled hash.
- **Set upstream**: right-click → **Set upstream…**, list of remote branches with a fuzzy filter, bound remote first. Git: `git branch --set-upstream-to`. Undo: restore previous upstream or unset.
- **Merge into current**: right-click → **Merge into `main`**, drag chip. Git: `git merge --no-edit <name>`; setting **Fast-forward** (default `--ff`, options `--no-ff`, `--ff-only`). Conflicts → 5.17. Undo: `git reset --hard ORIG_HEAD` (confirm if working tree dirty).
- **Rebase current onto**: right-click → **Rebase `feat` onto `main`**, drag chip. Git: `git rebase <target>`. Conflicts → 5.17. Undo: `git reset --hard` to the journaled pre-rebase hash.
- **Push branch**, **Pull**, **Fetch** entries route to 5.10.
- **Compare with current** → 5.13.
- **Copy branch name**, **Send branch name to terminal**.

### 5.10 Remotes, fetch, pull, push
- Every workspace is bound to one remote (5.22). Push, pull, fetch, and new-branch upstream default to it. Toolbar and menu labels name it: "Push → origin". Other remotes are reachable through **Push to…** and **Fetch…** submenus.
- **Remotes** appear as sidebar rows under the repo and as groups under Refs → Remote. Right-click a remote: **Fetch**, **Prune**, **Edit URL**, **Rename**, **Remove**, **Open in browser** (derives the web URL from GitHub/GitLab/Bitbucket/Gitea-style remotes, falls back to nothing), **Bind active workspace to this remote**.
- **Add remote**: `+` on the repo row or Refs Remote header, palette. Name and URL. Git: `git remote add`. A new remote row appears in the sidebar with no workspaces.
- **Fetch**: `Mod+Shift+F`, sidebar `⟳` on the remote row, palette. Git: `git fetch <remote> --prune --progress`; **Fetch all** in the dropdown runs `--all`. Progress pill with cancel. On completion the graph updates in place; new remote commits animate in at their positions (respecting Reduce Motion). Status bar "Last fetch 0s". Never modifies local branches.
- **Background fetch**: setting **Fetch every** 1/2/5/10/30 min/off, default 5. Per workspace, only for the bound remote. Skipped when offline, when a fetch is in progress, when the workspace is disconnected, or when on battery below 20 %. Failures after the first show only in the status bar as "Fetch failed · 3m" with a click-to-see-stderr, not a toast, to avoid nagging.
- **Pull**: `Mod+Shift+L`, palette, commit context menu on the current branch row. Split button: default behaviour from `pull.rebase` config (**Pull (merge)** or **Pull (rebase)**), alternatives in the dropdown plus **Pull (fast-forward only)**. Git: `git pull [--rebase|--no-rebase|--ff-only] --progress`. No upstream: toast "No upstream for `feat`" with **Set upstream…**. Dirty working tree with a rebase pull: toast with **Stash, pull, pop** (autostash) or **Cancel**. Conflicts → 5.17. Undo: reset to journaled pre-pull HEAD (confirm if the pull created merge commits that were since pushed).
- **Push**: `Mod+P`, palette, right-click commit or branch → **Push `feat` to origin**. Git: `git push <remote> --progress`. No upstream: pushes to `<bound remote>/<branch>` with `-u` after a toast "Push `feat` to origin and set upstream?" with **Push**, **Choose remote…**, once per branch; setting **Always push new branches to bound remote without asking**. Rejected non-fast-forward: toast "Push rejected: remote has commits you don't have" with **Pull then push**, **Force push with lease**, **Cancel**. Undo: not offered for push (irreversible from the client side); the toast says so.
- **Force push**: dropdown on the Push button and in the commit context menu. Default **Force push (with lease)** = `--force-with-lease`. Plain `--force` is only in the dropdown while `Alt` is held, is styled `danger`, and always confirms (5.20). Setting **Protected branches** (default `main`, `master`, `develop`, `release/*`) blocks force push entirely with a toast.
- **Push tag / all tags**: 5.11.
- Status bar left shows current branch, `↑N ↓M` vs its upstream, a spinning arrow while syncing, and "Last fetch Xm".

### 5.11 Tags
- Refs view **Tags** section, newest first, filterable. Graph chips styled with an outline instead of a fill.
- **Create**: right-click commit → **Create tag**, `Mod+Shift+T`, palette. Popover: name, **Annotated** toggle (default on, message field appears), **Sign** if `tag.gpgsign`, **Push to <bound remote>** checkbox (default off). Git: `git tag [-a -m] <name> <commit>`. Undo: delete tag (and push a delete if pushed, with a confirm).
- **Delete**: right-click → **Delete tag**, `Backspace`. Confirm offers **Also delete on remotes**. Undo: recreate.
- **Push tag**: right-click → **Push to…** remote list. **Push all tags**: remote row menu. Git: `git push <remote> <tag>` / `--tags`.
- **Checkout tag** (detached) and **Create branch from tag**.

### 5.12 Stashes
- Refs view **Stashes** section: `stash@{n}` shown as message, branch it was made on, relative time, +N −M. Graph shows each as a dashed node hanging off its parent (setting).
- **Stash**: `Mod+Shift+S`, palette. Popover: message (default from git), **Include untracked** (default on), **Keep staged** (`--keep-index`). Git: `git stash push [-u] [-k] -m <msg>`. Nothing to stash: toast. Undo: pop it.
- Select a stash: details shows its diff exactly like a commit, with **Apply**, **Pop**, **Drop**, **Branch from stash** buttons.
- **Apply**: `git stash apply stash@{n}`. Conflicts → 5.17. Undo: `git checkout -- .` and `git clean` on the affected paths only, from the journal.
- **Pop**: `git stash pop`. Undo: re-stash the same paths (journal keeps the stash commit hash, so `git stash store` restores it exactly).
- **Drop**: confirm (5.20). Git: `git stash drop`. Undo: `git stash store <hash>`.
- **Branch from stash**: name prompt. Git: `git stash branch <name> stash@{n}`.

### 5.13 Compare refs
- **Trigger**: right-click branch/tag/commit → **Compare with current** or **Compare with…**, select two commits in the graph with `Mod`-click then right-click → **Compare**, palette "Compare".
- Details area switches to compare mode (W11): header "Comparing `main`…`feat` · 12 commits · 34 files" with a swap button and two ref pickers (fuzzy dropdowns accepting any ref or hash).
- Tabs: **Files** (tree, same as 5.5 with diff on click; Git: `git diff A...B` three-dot by default, toggle to two-dot) and **Commits** (list of commits in B not in A, click to view).
- `Esc` exits compare mode and restores the previous selection.

### 5.14 Multi-select commits
- `Shift+click` or `Shift+↓/↑` selects a contiguous range. `Mod+click` toggles individual commits.
- Details shows "5 commits selected · 18 files" and the combined diff of the range: Git `git diff <oldest>~1..<newest>` for contiguous ranges; for non-contiguous selections the Files tab lists each commit's files grouped by commit.
- Right-click with a selection: **Cherry-pick 5 commits onto…**, **Revert 5 commits**, **Squash into one** (only if contiguous and on the current branch, runs an interactive rebase plan with all but the first marked `squash`), **Copy hashes**, **Compare oldest…newest**.

### 5.15 File history and blame
- **File history**: right-click a file anywhere → **File history**, or `Mod+Shift+H` with a file focused. Details becomes a per-file commit list (`git log --follow -- <path>`), each row clickable to show the file's diff at that commit. Renames noted inline. Header has **Open at this commit** and **Blame**.
- **Blame**: right-click → **Blame**, `b` with a file focused, from the diff line menu. Full-pane view (W12): file content with a gutter column per line showing short hash, author, relative date, with a background heat from `blame.hot` (newest) to `blame.cold` (oldest). Hover a gutter cell shows the full commit message. Click selects that commit in the graph. `Mod+←` on a line re-blames at the parent of that line's commit ("blame back"), `Mod+→` steps forward. Breadcrumb at the top shows the blame stack. Git: `git blame -p`, `--ignore-revs-file` honoured.

### 5.16 History rewriting
- **Cherry-pick**: right-click commit(s) → **Cherry-pick onto current**, drag onto a branch chip. Git: `git cherry-pick [-x] <hashes>` (setting **Add -x line**, default on). Conflicts → 5.17. Undo: `git reset --hard` to the journaled hash.
- **Revert**: right-click → **Revert**. Creates a revert commit with the default message; setting **Open editor before commit** (default off) fills the commit editor instead. Merge commits prompt for `-m 1` or `-m 2` with the parent subjects shown. Undo: reset to pre-revert hash.
- **Reset**: right-click commit → **Reset `main` to here** → submenu **Soft** (keep changes staged), **Mixed** (keep changes unstaged), **Hard** (discard). Confirm dialog for all three lists the commits that will leave the branch by subject; Hard additionally lists the working-tree files that will be lost. Git: `git reset --soft|--mixed|--hard <hash>`. Undo: reset back to the journaled hash; for Hard, the pre-reset working tree is saved as a hidden stash (`amalgum-undo-<timestamp>`) and restored.
- **Interactive rebase**: right-click a commit → **Interactively rebase from here**, or `Mod+Shift+R` with a commit selected. Precondition: commit is an ancestor of HEAD on the current branch, and no commit in the range is on a protected branch's remote (warn otherwise).
  1. The git pane maximizes and shows the rebase plan on the left and a live preview graph on the right (W9): one row per commit from the selected one to HEAD, oldest at top. Each row: drag handle, action dropdown (**pick**, **reword**, **edit**, **squash**, **fixup**, **drop**), short hash, subject (editable inline when reword/squash).
  2. Drag rows to reorder. `Mod+↑/↓` also moves. `p/r/e/s/f/x` set the action on the focused row.
  3. The preview graph re-renders on every plan change.
  4. **Start rebase** runs `git rebase -i` with `GIT_SEQUENCE_EDITOR` set to a helper that writes the plan (the CLI binary in `--sequence-editor` mode, present on remote hosts too). `reword` and `squash` messages are collected in the plan UI beforehand and supplied via a `GIT_EDITOR` helper so git never opens a terminal editor. `edit` pauses; the status bar shows "Rebase paused at ab12cd3" with **Continue**, **Abort**, and the Changes view is active for amending.
  5. Conflicts → 5.17. Abort: `git rebase --abort`. Undo after success: reset to the journaled pre-rebase hash.
- **Squash into one** and **Fixup into parent** shortcuts on the commit menu build the plan for you and run it immediately.
- **Edit commit message** (any reachable commit): builds a `reword` plan for that commit only.

### 5.17 Conflicts
Full three-way merge editing is out of scope for v1. Conflicts must still never strand the user.
1. Any operation that stops on conflict (merge, rebase, pull, cherry-pick, revert, stash apply) switches the status bar to a persistent `danger` state: "Merge in progress · 3 conflicts" with **Abort** and **Continue** buttons. The git pane opens the Changes view (W10). The sidebar row shows a `!` badge.
2. Conflicted files sit in a **Conflicts** section above Unstaged, each with a `!` glyph. Clicking shows a read-only conflict diff with ours in `conflict.ours` and theirs in `conflict.theirs`, labelled with branch names.
3. Per-file actions: **Take ours** (`git checkout --ours`), **Take theirs** (`--theirs`), **Open in editor** (`$VISUAL`/`$EDITOR` in a new terminal tab of this workspace, so it works identically for remote), **Open in merge tool** (`git mergetool --no-prompt -- <path>` in a new terminal tab, enabled when `merge.tool` is set), **Send path to terminal** (for handing to an agent), **Mark resolved** (`git add`; disabled while the file still contains `<<<<<<<` markers, with the count shown).
4. When Conflicts is empty, **Continue** enables: `git merge --continue` / `git rebase --continue` / `git cherry-pick --continue` etc. The commit message editor is prefilled from `.git/MERGE_MSG`.
5. **Abort** confirms and runs the matching `--abort`. The working tree returns to pre-operation state.
6. Opening a workspace whose repo is already mid-operation shows the same state.

### 5.18 Undo and redo
- Every ref-changing operation listed in this document pushes a journal entry: `{id, time, description, before: {HEAD, branch refs touched, stash list, index tree}, after: {…}, inverse: <command plan>}`. Journal is stored at `<config dir>/journal/<repo-id>.json`, capped at 200 entries, surviving restarts. Remote workspaces journal locally, keyed by host and path.
- `Mod+Z` with the git pane focused runs the inverse of the newest entry not already undone, if `before` still matches the repo (guarding against changes from terminals or agents). Mismatch: toast "Can't undo: `main` has moved since. Open reflog?" with a button that opens the Undo panel in reflog mode. With a terminal focused `Mod+Z` is passed to the terminal; the commit context menu and toolbar buttons always work.
- `Mod+Shift+Z` redoes.
- Toolbar **Undo** / **Redo** buttons in the git pane header show the description on hover ("Undo: commit ab12cd3").
- **Undo panel** (`Mod+Alt+Z`, View → Undo History): list of entries with description, time, and a **Revert to here** action. A second tab shows the raw reflog (`git reflog`) with **Reset to this entry** per row.
- Push is journaled as an entry with no inverse; it appears greyed with "Cannot be undone from the client".
- Undo never touches the working tree except when the entry explicitly recorded one (Hard reset, stash pop, checkout that stashed). Those restore via the hidden `amalgum-undo-*` stash.

### 5.19 Command palette
- `Mod+K` or `Mod+Shift+P` opens. Centered overlay, fuzzy input, results grouped: **Actions**, **Workspaces** (with status dot and last notification), **Branches**, **Tags**, **Stashes**, **Worktrees**, **Recent locations**, **Commits** (only when the query is 7+ hex chars or prefixed `#`).
- Actions show their shortcut on the right. `↑/↓/Enter`, `Esc`. `Mod+Enter` on a branch opens its context menu instead of checking out.
- Prefixes jump straight to a group: `>` actions, `@` branches, `#` commits, `$` stashes, `~` locations, `!` workspaces.
- Palette entries are the single source of truth for shortcuts; the menu bar is generated from the same table.

### 5.20 Destructive confirmations
The only modal dialog type. Used for: delete unmerged branch, delete tag on remote, drop stash, discard changes/hunks/lines, hard/mixed/soft reset, plain force push, abort merge/rebase, remove worktree with changes, clean, remove remote, close a workspace or tab with a running agent, kill a remote tmux session, trust an unknown host key.
- Title names the exact target ("Delete branch `feat/x`?"). Body lists what is lost by name (commit subjects, file paths, agent session ids, up to 10 then "+N more"). Primary button is `danger` and states the verb ("Delete"). `Esc`/**Cancel**. `Enter` does **not** trigger the destructive button; `Mod+Enter` does.
- Checkbox **Don't ask again for this action** appears for: discard hunk, drop stash, soft reset, close tab with running non-agent process. Never for hard reset, force push, branch delete, host key trust, or closing a running agent.

### 5.21 Worktrees
- Refs view **Worktrees** section: path (abbreviated), branch, dirty indicator, current marked, and the workspace name if one is bound to it.
- **Add**: `+` on the header, right-click branch → **Open in new worktree workspace**, palette. This is the same sheet as W15 with **New worktree** preselected; it always creates a workspace (5.22). Git: `git worktree add [-b <new>] <path> <branch>`. Undo: remove the worktree and its workspace if clean.
- **Open as workspace**: `Enter` on a worktree that has no workspace yet creates one bound to the same remote.
- **Remove**: right-click → **Remove**, `Backspace`. Dirty worktree confirms (5.20) with the file list; a bound workspace with running agents is listed and closed too. Git: `git worktree remove [--force]`. **Prune** on the header runs `git worktree prune`.
- **Lock/Unlock** and **Reveal in file manager**.

### 5.22 Workspaces
The unit of work. A workspace is: one repo, one bound remote, one location, a set of terminal tabs with split layouts, and a git pane state.

- **Create**: `Mod+N`, sidebar **+ New workspace**, repo or remote row `+`, drag a folder onto a repo row, `amalgum open <path|host:path>`, W15 sheet.
  - **Existing folder**: any path; if it belongs to a repo already in the sidebar it nests there, otherwise a new repo group is created (5.2).
  - **New worktree**: branch name (new or existing), base, path (default sibling `<repo>-<branch>`). Creates the worktree then the workspace.
  - **Remote host**: host from `~/.ssh/config` aliases plus free text, path, **Keep sessions alive with tmux** (default on). Connects (5.28); the repo group is identified by the remote URL read over ssh so a remote clone of `conduit` nests under the same repo as the local one.
  - Common: name (default branch name, or `host:branch` for remote), **Open a terminal** (default on), **Run** command in that terminal (default empty; remembers last per repo, so `claude` is one checkbox away).
- **Sidebar row** (W3): status dot (5.29), name, location subtitle, branch if it differs, `↑↓` counts vs upstream, dirty count, ports, last notification line. Row menu: **New terminal**, **Run…**, **Rename**, **Bind to remote…**, **Move up/down**, **Open in external terminal**, **Reveal**, **Copy path**, **Hibernate now** / **Resume**, **Disconnect** / **Reconnect** (remote), **Close**.
- **Switch**: click, `Mod+1..9` by position, `Mod+Alt+↑/↓`, palette `!name`. Switching restores that workspace's terminals, git pane view, selection, and scroll. Under 16 ms since terminals stay alive in the background.
- **Reorder**: drag rows within a remote, drag between remotes to rebind. Repo groups reorder by drag too.
- **Rename**: `F2` or double-click the name. Names need not be unique.
- **Close**: `Mod+Shift+W` or row menu. Confirms (5.20) if any tab has a running agent or a foreground process other than the shell, listing them. Remote: offers **Keep tmux sessions on host** (default) or **Kill sessions**. Closing the last workspace of a repo keeps the repo group until **Forget repo** is chosen from the repo row.
- **Restore on launch**: setting (default on). Local terminals are recreated at their cwd with scrollback from `<data dir>/scrollback/<tab>.txt` (best-effort, last 10k lines) prepended in `fg.secondary`; remote tabs reattach to their tmux sessions with live scrollback. Agents are resumed per 5.30 if they were hibernated, otherwise their tabs show the last screen with a **Resume** hint.
- **Per-workspace state** in `<data dir>/state.json`: id, repo id, remote name, location, layout tree, tabs (title, cwd, agent session), git pane view and selection, pane sizes.
- **Window**: one window. Multiple windows are a non-goal for v1; `Mod+Shift+N` is reserved.

### 5.23 Settings
- `Mod+,`. A separate non-modal window with a left list: **General**, **Appearance**, **Terminal**, **Git**, **SSH**, **Agents**, **Shortcuts**, **Advanced**. Search box at the top filters settings across sections.
- Stored as TOML at `<config dir>/settings.toml`, hand-editable, watched for external changes.
- **General**: default clone directory, restore workspaces on launch, fetch interval, battery skip, avatars, CLI shim install/uninstall, check for updates (manual button only, no automatic check), absolute timestamps.
- **Appearance**: theme, use system accent, UI scale, font choices (bundled/system), row height, shape-coded lanes, reduce motion override, syntax themes, git pane position (right/bottom).
- **Terminal**: font size, terminal theme files (light/dark), scrollback lines (default 10 000), shell (default from `$SHELL`), cursor style and blink, bell (visual/sound/none), copy on select, paste bracketing, shell integration install, link detection.
- **Git**: path to git binary (auto-detected, overridable), fast-forward policy, pull policy override, cherry-pick -x, allow amending pushed, protected branches list, sign-off default, editor override.
- **SSH**: path to ssh binary, keepalive interval and count, ControlPersist duration, default tmux on/off per new host, per-host table (alias, tmux, CLI version installed, **Forget**), reconnect backoff cap.
- **Agents**: hook status per agent (installed / outdated / not found) with **Install** / **Update** / **Remove**, notification sound and OS notification toggle, hibernation on/off, idle threshold, live-terminal limit, auto-resume on focus, per-agent resume command override.
- **Shortcuts**: table of every palette action with a rebind button; conflicts shown in `danger`. Presets: **macOS**, **Linux Ctrl+Shift**, **Linux Super**. Export/import JSON. Reset to defaults.
- **Advanced**: journal size, search index location and rebuild button, notification history size, log level and **Open log folder**, control socket path (read-only display), reset all settings.
- Per-repo overrides (fast-forward policy, sign-off, layout) live in `<config dir>/repos/<repo-id>.toml`.

### 5.24 Errors and logging
- Every failed git, ssh, or socket command produces a toast: one-line human summary, **Details** expanding to the full command line and stderr in monospace with **Copy**. Toasts stack bottom-left, auto-dismiss after 8 s unless hovered or expanded; `danger` toasts never auto-dismiss. Every toast is also a row in the notification panel (5.29) so a missed toast is recoverable.
- The app never swallows stderr. Unknown errors show stderr verbatim.
- Rolling log at `<data dir>/logs/amalgum.log`, 5 files × 5 MB, info level by default. Every git and ssh invocation logged with duration and host. The log never contains terminal contents, file contents, or commit bodies.
- A crash writes a report to the log folder and offers **Open log folder** on next launch. No automatic upload.

### 5.25 Offline and repo changes from outside
- Local: file watcher on `.git/HEAD`, `.git/refs/`, `.git/packed-refs`, `.git/index`, and the work tree. Any change triggers an incremental refresh within 200 ms: only rows whose refs or status changed re-render. Selection is preserved by hash.
- Remote: no watcher. The git pane refreshes on focus, after any command the app itself ran, after any hook event from that workspace, and every 10 s while visible (`git status --porcelain=v2 -z` plus `git for-each-ref`, both cheap). Setting **Remote refresh interval**.
- Operations by other tools or agents (terminal `git commit`) show up as graph updates with no toast. If an agent commits, the notification panel row for its hook event links to the commit.
- If a local repo directory disappears, the workspace shows "Repository not found at <path>" with **Close** and **Locate…**.
- Network unavailable: push/pull/fetch/clone show "Offline" in the toast; background fetch pauses silently until a fetch succeeds. Remote workspaces enter the reconnecting state (5.28).

### 5.26 Menu bar
Generated from the palette table. Top-level: **Amalgum** (macOS only: About, Settings, Quit) / **File** (Open Folder, Clone, Connect to Host, Open Recent ▸, Close Workspace, Settings on Linux, Quit on Linux), **Edit** (Undo, Redo, Cut, Copy, Paste, Select All, Find, Clear terminal), **View** (sidebar, git pane, maximize pane, notifications, zoom, theme, focus mode, show/hide remotes/tags/stashes), **Workspace** (New, New terminal, Split right, Split down, Close tab, Next/Previous tab, Next/Previous workspace, Run…, Hibernate, Resume, Reconnect), **Repository** (Fetch, Pull, Push, Force push ▸, Commit, Stash, Branch ▸, Tag, Worktree ▸, Interactive rebase, Undo history, Set upstream), **Window** (Minimize, Zoom, workspaces list), **Help** (Keyboard shortcuts, Set up agent hooks, Open log folder, About on Linux).

### 5.27 Terminal
- **Model**: a workspace has a tab strip; each tab is a split tree whose leaves are terminals. One PTY per terminal, spawned with the workspace's shell at the workspace root (or the tab's last cwd on restore), env `AMALGUM_SOCK`, `AMALGUM_WORKSPACE`, `AMALGUM_TAB`, `TERM=xterm-256color`, `COLORTERM=truecolor`. Remote terminals are `ssh` processes (5.28).
- **Tabs**: `Mod+T` new, `Mod+W` close (confirm if a foreground process other than the shell is running, 5.20), `Mod+Shift+[`/`]` previous/next, drag to reorder, double-click to rename, `Mod+Alt+T` reopen last closed (same cwd, no contents). Tab label is the OSC 0/2 title, else the foreground process name, else the shell. Tab shows the status dot of its agent (5.29).
- **Splits**: `Mod+D` right, `Mod+Shift+D` down, drag the divider, `Mod+Alt+←/→/↑/↓` move focus, `Mod+Shift+Enter` zoom the focused pane to fill the tab and back, `Mod+Alt+W` close pane. Splits nest without limit. Minimum pane 20 columns × 5 rows.
- **Rendering**: one reader thread per PTY reads, runs the OSC pre-scanner, advances the `alacritty_terminal` parser, and requests a repaint. Rows are laid out as one text job per row keyed by content hash so unchanged rows cost nothing. Backgrounds are batched rectangles; cursor, selection, and links draw on top. Fixed cell metrics from JetBrains Mono; fallback fonts for CJK, emoji, and Nerd Font glyphs bundled. Ligatures off. Target: 60 fps with 100 MB/s input on a 200×60 grid, zero repaints while idle.
- **Scrollback**: 10 000 lines default (setting up to 100 000). `Shift+PageUp/Down` scroll, `Mod+Home/End` top/bottom, `Mod+Alt+K` clear. Trackpad and wheel scroll. Scrolled-back state shows a "↓ N new lines" pill; any keypress jumps to the bottom.
- **Selection and clipboard**: click-drag, double-click word, triple-click line, `Shift+click` extend, block select with `Alt`. `Mod+C` copies (no trailing whitespace), `Mod+V` pastes with bracketed paste when the app enables it; multi-line paste into a shell prompt confirms once per session. **Copy on select** setting.
- **Search**: `Mod+F` with a terminal focused opens an in-terminal search bar, regex toggle, `Enter`/`Shift+Enter` next/previous, matches highlighted in `bg.selected`.
- **Links**: URLs and `path:line` patterns underline on `Mod`-hover. `Mod+click` opens URLs in the default browser and paths in the editor (`$VISUAL`/`$EDITOR`, remote via a temp scp copy).
- **Escape sequences honoured beyond alacritty's set**, via the pre-scanner:
  - OSC 7: cwd. Updates the tab cwd, the git pane if the cwd leaves or enters a different repo (a warning bar says "This terminal is in `~/other`, not the workspace repo"), and the subtitle.
  - OSC 0/2: title.
  - OSC 9, OSC 99, OSC 777 `notify`: notification with the given title and body, marks the tab **needs input** (5.29).
  - OSC 133 A/B/C/D: semantic prompt marks. Prompt = idle, command running = running, command ended with non-zero = one-shot `warning` dot until next prompt. Also enables **jump to previous/next prompt** `Mod+↑/↓` with a terminal focused.
  - BEL: visual flash of the pane border plus optional sound, and a notification if the tab is not focused.
- **Shell integration**: **Install shell integration** in Settings appends a sourced snippet for zsh, bash, and fish that emits OSC 7 and OSC 133, and wraps `amalgum` discovery. Idempotent, marker-delimited, removable. Also uploaded to remote hosts on connect and sourced by the tmux session's default command.
- **Port detection**: every 5 s while a workspace is active, `platform::listening_ports(pids)` runs against the process tree under each PTY (remote: `ss -ltnp` or `lsof -iTCP -sTCP:LISTEN -P -n` over ssh). New ports appear in the sidebar subtitle and status bar with a toast "Listening on :3000" once. Click opens (W18); remote ports open through `ssh -L` (5.28).
- **Process exit**: when the shell exits, the pane shows "[Process exited with code N] · Enter to restart · Mod+W to close" in `fg.secondary`.
- **Input mapping**: with a terminal focused, every key goes to the PTY except bindings that contain `Mod` (Section 7). On macOS ⌘ never reaches the terminal, so nothing is lost. On Linux the Ctrl+Shift default keeps bare Ctrl for the terminal. `Mod+/` always opens the shortcut overlay.

### 5.28 SSH workspaces
A remote workspace behaves exactly like a local one. Terminals run on the host, git runs on the host, agents run on the host, and their hooks reach the app.

- **Connect**: from W15, `amalgum open host:path`, or reconnect. Steps, each with a progress line (W16):
  1. Open the ControlMaster: `ssh -o ControlMaster=auto -o ControlPath=<runtime dir>/ssh/%C -o ControlPersist=10m -o ServerAliveInterval=20 -o ServerAliveCountMax=2 -N -f host`. User `~/.ssh/config` values override the keepalive options when set. Unknown host key: W16 dialog with the fingerprint; **Trust** re-runs with `StrictHostKeyChecking=accept-new` for that one connection. Never `StrictHostKeyChecking=no`.
  2. Verify or install the CLI: `ssh host ~/.amalgum/bin/<version>/amalgum --version`; on mismatch upload the static CLI binary for the host's `uname -sm` with `scp`, then verify `sha256sum`. Under 2 MB, once per version per host.
  3. Check `git --version` and `tmux -V`. Missing git: error, cannot continue. Missing tmux with keepalive requested: warning toast "tmux not found on gpu-box; sessions will not survive disconnects", continue without.
  4. Open the reverse socket: `ssh -O forward -R ~/.amalgum/run/<app-id>.sock:<local app.sock> -o StreamLocalBindUnlink=yes host` on the ControlMaster. If `sshd` refuses (`AllowStreamLocalForwarding no`), fall back to queue mode (below) with a one-time warning.
  5. Read the repo identity: `git -C <path> remote -v`. Nest the workspace under the matching repo group or create one.
  6. Spawn terminals.
- **Terminal spawn**: with keepalive, `ssh -t host tmux -L amalgum -f ~/.amalgum/tmux.conf new-session -A -s <tab-id> -c <path> -e AMALGUM_SOCK=~/.amalgum/run/<app-id>.sock -e AMALGUM_WORKSPACE=… -e AMALGUM_TAB=…`. The bundled `tmux.conf` sets `status off`, `allow-passthrough on`, `set-titles on`, `set-titles-string '#T'`, `mouse on`, `history-limit 50000`, `escape-time 0`, and unbinds the prefix (`prefix None`) so tmux is invisible; all splitting is done by Amalgum. Without keepalive, `ssh -t host 'cd <path> && exec $SHELL -l'` with env passed via `SendEnv` when permitted, else prefixed in the command.
- **Keepalive and reconnect**: the ControlMaster carries keepalive. When it drops, every terminal in the workspace greys out with its last screen and the banner "Connection lost Ns ago · retrying in Ms" (W16). Retry backoff 3, 6, 12, 24, 48, 60, 60… seconds, **Retry now** button, cancel via **Disconnect**. On reconnect: ControlMaster, reverse socket, then each terminal re-runs its `new-session -A` command and tmux reattaches with full scrollback; without tmux the terminal restarts fresh at its cwd. Queued hook events are drained (below). Sidebar dot shows `⊘` while disconnected.
- **Remote git**: every git command in Section 5 runs as `ssh host -- git -C <path> <args>` on the ControlMaster, 20–50 ms on LAN. Output parsed identically to local. Commit messages and patches are piped over stdin. No syncing, no sshfs. Diffs of images and binaries fetch the blobs with `git show` over ssh, capped at 10 MB.
- **Hooks from remote agents**: the remote CLI (`~/.amalgum/bin/<version>/amalgum`) connects to `$AMALGUM_SOCK`, which is the reverse-forwarded socket. While disconnected the connect fails and the CLI appends the event to `~/.amalgum/queue.jsonl` instead; on reconnect the app runs `amalgum drain` remotely, which prints and truncates the queue. Events carry timestamps so late notifications are labelled with their real time.
- **Ports**: remote ports open with `ssh -O forward -L <free local port>:localhost:<port> host` on the ControlMaster, then `open_in_default_app("http://localhost:<local port>")`. Forwards are listed in the ports popover with a **Stop** button and closed on workspace close.
- **File transfer**: dragging a file onto a remote terminal uploads it with `scp` to the tab's cwd and types the remote path into the terminal. `o` on a file in the git pane copies it to a local temp dir and opens it.
- **Multiple hosts**: each host has one ControlMaster; several workspaces on the same host share it. Host settings (keepalive, tmux, CLI version) in Settings → SSH.
- **Disconnect**: row menu. Closes terminals locally, keeps tmux sessions on the host, closes forwards, keeps the ControlMaster until `ControlPersist` expires. **Kill sessions** in the same menu runs `tmux -L amalgum kill-server` after a confirm (5.20).
- **Security**: no keys, passwords, or tokens are stored or seen by the app; `ssh` owns auth. The reverse socket on the host is mode 0600 in the user's home. Host keys are only ever accepted through the W16 dialog.

### 5.29 Agent awareness
- **Status model** per tab, shown as a dot on the tab and rolled up to the sidebar row as the most urgent tab: **needs input** `◐` (accent, ring on the pane, pulses once unless Reduce Motion) > **running** `●` (success) > **idle** `○` (status.idle) > **hibernated** `◌` (status.hibernated) > **disconnected** `⊘` (danger, remote only). A tab with no agent and no foreground command shows no dot.
- **Detection sources**, highest confidence wins, each with a 10 s staleness after which the next source may override:
  1. Hook events over the control socket: `SessionStart`/`UserPromptSubmit` = running, `Notification`/`PermissionRequest`/`Stop` with a question = needs input, `Stop` = idle, `SessionEnd` = no agent. Carries the agent's own text.
  2. OSC 9/99/777 or BEL from the terminal = needs input with the OSC body as the message.
  3. OSC 133 from shell integration: at prompt = idle, command running = running.
  4. Heuristic: the foreground process group of the PTY (`tcgetpgrp`) resolves via `sysinfo` to a known agent binary (`claude`, `codex`, `opencode`, `gemini`, `aider`, `kiro`) and no output for 15 s = needs input (low confidence, drawn with a hollow dot).
- **Notification**: every needs-input transition and every hook `Notification` event creates a row in the notification panel (W14) with workspace, tab, agent, message, and time. It also raises an OS notification (macOS `UNUserNotification` via `platform`, Linux `notify-send` if present) when the app is not frontmost, unless muted. Clicking either focuses the workspace and tab. Focusing the tab marks it read and clears the ring.
- **Notification panel**: `Mod+Shift+I` or the bell. Unread count on the bell and the app icon badge. Rows grouped by time, filter by workspace, **Clear all**, **Mute workspace**. Persisted to `<data dir>/notifications.jsonl`, last 500.
- **Hook installer**: `amalgum hooks setup [claude|codex|opencode|gemini|all]`, also Settings → Agents and the welcome banner. For each agent it merges a marker-keyed block into the agent's config (`~/.claude/settings.json` hooks for SessionStart, UserPromptSubmit, Notification, PermissionRequest, Stop, SessionEnd; `~/.codex/config.toml` `notify` and hooks; OpenCode plugin file; `~/.gemini/settings.json`). Each hook runs `amalgum agent-event --agent <name>` reading the event JSON on stdin, with a 1 s timeout and always exit 0 so a missing app never blocks the agent. `amalgum hooks status` reports installed/outdated/missing; `amalgum hooks remove` reverts. Idempotent: rerunning updates the block in place. The same command runs on remote hosts during connect if **Install agent hooks on remote hosts** is on (default on).
- **Session capture**: `SessionStart` events carry the agent's session id, pid, and cwd. Stored on the tab for resume (5.30) and shown in the tab tooltip.
- **Explicit status**: `amalgum set-status <running|needs-input|idle> [--message …]` and `amalgum clear-status` let scripts drive the dot directly. `amalgum notify --title … --body …` posts a notification from anything, including Claude Code's PushNotification hook bridge.
- **Agent commit linking**: when a hook `Stop` event arrives and `HEAD` moved since the last event, the notification row links to the new commit and the graph highlights it for 3 s.

### 5.30 Hibernation
Opt-in, off by default. Frees memory and CPU from idle agents without losing their session.

- **Eligibility**: a tab is hibernatable when all hold: the last hook event was `Stop` or idle (never on heuristics alone), no PTY I/O since that event, the agent process is still the foreground process, the tab is not focused, and the idle time exceeds **Idle threshold** (default 30 min, per-workspace override in the row menu). Manual **Hibernate now** in the row and tab menus skips the threshold but not the other checks.
- **Live-terminal limit**: setting (default off, or N). When more than N tabs have a running agent, the oldest eligible ones hibernate first.
- **Hibernate**: `SIGTERM` to the agent pid captured by hooks (`libc::kill` locally, `ssh host kill` remotely, both via one function), wait 5 s, then `SIGKILL`. The shell under it stays alive. The pane greys to 50 % with its last screen and an overlay (W17): time, memory freed (from `sysinfo` before kill), session id, cwd, **Resume**, **Close tab**. Dot becomes `◌`.
- **Resume**: `Enter` or **Resume** on the overlay, focusing the tab if **Auto-resume on focus** is on (default on), or `amalgum resume <tab>`. Types the resume command into the shell: `claude --resume <id>`, `codex resume <id>`, `opencode --session <id>`, `gemini --resume <id>`, overridable per agent in Settings → Agents. The original argv (minus env) is appended when it contained flags such as `--dangerously-skip-permissions`.
- **Persist per tab**: agent kind, session id, pid, cwd, sanitized argv, last status, timestamps, hibernated flag, in `state.json`. Survives app restart: hibernated tabs restore as hibernated; running agents whose tabs restore without a live process show the same overlay with "App was closed" instead of "Hibernated".
- **Never**: hibernate a tab in needs-input state, a tab whose agent has no captured session id, or a tab with unsaved terminal input on the prompt line.

### 5.31 Control socket and CLI
The app binary doubles as the CLI. `amalgum <subcommand>` talks to the running app over a unix socket.

- **Socket**: `<runtime dir>/amalgum/app.sock` (`$XDG_RUNTIME_DIR` on Linux, `<data dir>/run` on macOS), mode 0600, created on launch, removed on quit. One app instance per user; a second launch forwards its args to the socket and exits.
- **Discovery**: shells spawned by the app get `AMALGUM_SOCK`, `AMALGUM_WORKSPACE`, `AMALGUM_TAB`. On remote hosts `AMALGUM_SOCK` points at the reverse-forwarded socket. Without `AMALGUM_SOCK` the CLI tries the default local path; if nothing answers, notification-type commands exit 0 silently and query-type commands exit 1 with "Amalgum is not running".
- **Protocol**: one connection per request, one line of JSON in, one line of JSON out. `{"cmd":"notify","title":"Done","body":"12 tests pass","workspace":"…","tab":"…"}` → `{"ok":true}`. Errors: `{"ok":false,"error":"…"}`. No framing beyond newline, no ids, no auth beyond socket permissions. Version field in every request; the app answers older minor versions.
- **Commands v1**:
  - `notify --title T [--body B] [--tab ID]`
  - `set-status <running|needs-input|idle> [--message M]`, `clear-status`
  - `agent-event --agent <claude|codex|opencode|gemini>` (event JSON on stdin; the app's per-agent adapter maps it to status, session id, and message)
  - `open <path|host:path> [--remote origin] [--name N] [--run CMD]` (creates or focuses a workspace)
  - `run <cmd> [--split right|down] [--tab ID] [--workspace ID]` (new terminal running the command)
  - `list [--json]` (workspaces, tabs, statuses, cwds, ports)
  - `focus <workspace|tab>`
  - `resume <tab>`, `hibernate <tab>`
  - `hooks setup|status|remove [agent]`
  - `drain` (remote only: print and truncate `~/.amalgum/queue.jsonl`)
  - `--sequence-editor` and `--editor` (internal, used for interactive rebase, 5.16)
  - `--version`, `--help`
  - Reserved for later: `send-keys`, `read-screen`, `wait`.
- **Remote build**: the CLI is the same crate with `--no-default-features`, statically linked (musl on Linux, plain static on macOS), no egui, no wgpu, under 2 MB. It is embedded in the app binary for each supported remote target (`x86_64-linux-musl`, `aarch64-linux-musl`, `aarch64-apple-darwin`, `x86_64-apple-darwin`) and uploaded on connect (5.28).
- **CLI shim install**: `install_cli_shim()` symlinks the app executable to `/usr/local/bin/amalgum` (macOS, prompts for admin) or `~/.local/bin/amalgum` (Linux, no prompt). `amalgum` with no args opens the current directory as a workspace.

---

## 6. Performance

Targets are measured in CI against three fixtures: a 1k-commit repo, a 50k-commit repo, and the linux kernel (1M+ commits); regressions over 20 % fail the build.

- **Startup**: window within 150 ms; sidebar and restored local terminals interactive within 500 ms; git panes render cached graphs before any git process runs.
- **Terminal**: 60 fps with 100 MB/s input on a 200×60 grid; zero repaints and zero CPU for idle terminals; keypress-to-glyph under 10 ms; 20 live local terminals under 150 MB total.
- **Workspace switch**: under 16 ms; nothing is torn down.
- **Graph**: commits read via `git log --format` (or `gix` when enabled) in topo order on a worker and streamed in batches; lane assignment incremental; already-laid-out rows never move. Virtual scrolling lays out only visible rows plus 20 above and below.
- **Commit metadata cache**: `<data dir>/cache/<repo-id>/commits.bin` (hash, parents, author, time, subject) so the second open of the kernel repo shows the full graph in under 2 s. Invalidated per-ref by fetch, watcher, and hook events. Remote repos cache the same way, keyed by host and path.
- **Search index**: `tantivy` at `<data dir>/index/<repo-id>/`, built on a worker after first open, updated incrementally. Local only; remote search uses `git log` filters.
- **Diffs**: `git diff` on demand; results cached per (commit, path) in an LRU of 200 entries. Blame via `git blame -p`, cached per (commit, path).
- **Status**: local via watcher events only, never polled. Remote polled at 2 s while Changes is visible, 10 s otherwise.
- **SSH**: connect to first prompt under 3 s on LAN including CLI verification; reconnect under 5 s; git command round-trip 20–50 ms on the ControlMaster.
- **Hibernation**: a hibernated tab holds under 2 MB (last screen plus metadata).
- **Memory**: under 300 MB with three workspaces and six live terminals; under 600 MB with the kernel repo open.

---

## 7. Keyboard reference

`Mod` = ⌘ on macOS. Linux: `Mod` = Ctrl+Shift, `Mod+Shift` = Ctrl+Alt, `Mod+Alt` = Ctrl+Alt+Shift (preset **Linux Super** swaps in Super). All rebindable. With a terminal focused only bindings containing `Mod` are intercepted; everything else reaches the PTY.

### 7.1 Global
| Key | Action |
|---|---|
| `Mod+K`, `Mod+Shift+P` | Command palette |
| `Mod+/` | Keyboard shortcut overlay (`?` also, outside terminals) |
| `Mod+O` / `Mod+Shift+O` / `Mod+Shift+K` | Open folder / Clone / Connect to host |
| `Mod+N` | New workspace |
| `Mod+Shift+W` | Close workspace |
| `Mod+1..9` | Switch to workspace by position |
| `Mod+Alt+↑` / `↓` | Previous / next workspace |
| `Mod+T` / `Mod+W` | New terminal tab / close tab or pane |
| `Mod+Shift+[` / `]` | Previous / next terminal tab |
| `Mod+Alt+T` | Reopen closed tab |
| `Mod+D` / `Mod+Shift+D` | Split right / split down |
| `Mod+Alt+←/→/↑/↓` | Move focus between panes |
| `Mod+Shift+Enter` | Zoom pane / unzoom |
| `Mod+Alt+W` | Close pane |
| `Mod+J` | Toggle git pane |
| `Mod+Shift+1` | Toggle sidebar |
| `Mod+\` | Maximize / restore focused pane |
| `Mod+Shift+I` | Notification panel |
| `Mod+Shift+G` | Focus git pane graph (from anywhere) |
| `Mod+Shift+C` | Git pane Changes view |
| `Mod+Shift+E` | Git pane Refs view |
| `Mod+Alt+Enter` | Focus terminal (from git pane) |
| `Mod+,` | Settings |
| `Mod+Z` / `Mod+Shift+Z` | Undo / Redo (git pane; passed through when a terminal is focused) |
| `Mod+Alt+Z` | Undo history panel |
| `Mod+Shift+F` | Fetch bound remote |
| `Mod+Shift+L` | Pull |
| `Mod+P` | Push to bound remote |
| `Mod+B` | New branch |
| `Mod+Shift+S` | Stash |
| `Mod+Shift+T` | New tag |
| `Mod+Shift+R` | Interactive rebase from selected commit |
| `Mod+Shift+H` | File history of focused file |
| `Mod+=` / `-` / `0` | Zoom focused pane (terminal font or graph rows) |
| `Mod+Alt+D` | Toggle dark/light (overrides System until relaunch) |

### 7.2 Terminal focused
| Key | Action |
|---|---|
| `Mod+C` / `Mod+V` | Copy / paste |
| `Mod+F` | Search in terminal |
| `Mod+Alt+K` | Clear scrollback |
| `Mod+Home` / `Mod+End` | Scroll to top / bottom |
| `Shift+PageUp` / `PageDown` | Scroll a page |
| `Mod+↑` / `Mod+↓` | Previous / next prompt (needs shell integration) |
| `Mod+click` | Open link or path |
| `Mod+Shift+R` | Run… (command in a new split) — overrides rebase while a terminal is focused |
| Everything else | Sent to the PTY |

### 7.3 Git pane, Graph view
| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | Move selection |
| `Shift+↓/↑`, `Shift+click` | Extend range selection |
| `Mod+click` | Toggle commit in selection |
| `Mod+↑` / `Mod+↓` | First / last commit |
| `Enter` | Focus details |
| `/`, `Mod+F` | Search |
| `f` | Focus mode on selected branch |
| `c` | Checkout selected branch or commit |
| `t` | Create tag at selected commit |
| `b` | Create branch at selected commit |
| `m` | Merge selected branch into current (popover, Enter confirms) |
| `r` | Rebase current onto selected (popover) |
| `p` | Cherry-pick selected onto current (popover) |
| `Space` | Toggle collapse of merged branch at selected merge commit |
| `Mod+C` | Copy hash |
| `Mod+Alt+C` | Copy subject |
| `Mod+Enter` | Commit context menu |
| `Esc` | Clear selection / exit focus mode / exit search / back to terminal |

### 7.4 Commit context menu
See 5.4 for the full ordered list. Summary: Checkout · Create branch · Create tag · Push to remote · Force push with lease · Set upstream · Merge into current · Rebase onto · Cherry-pick · Revert · Reset ▸ Soft/Mixed/Hard · Undo last operation · Redo · Interactive rebase · Edit message · Squash/Fixup into parent · Compare · Rename/Delete branch · Delete tag · Copy hash/subject/permalink · Open in browser.

### 7.5 Git pane, details / diff / Changes
| Key | Action |
|---|---|
| `↓` / `↑` | Next / previous file |
| `n` / `N` | Next / previous hunk |
| `t` | Toggle tree / flat file list |
| `d` | Toggle unified / split |
| `w` | Toggle whitespace |
| `s` / `u` | Stage / unstage focused file |
| `Mod+A` / `Mod+Shift+A` | Stage all / unstage all |
| `Space` | Stage/unstage selected lines or hunk |
| `Backspace` | Discard selected lines or hunk (confirm) |
| `Mod+Enter` | Commit (from message editor) |
| `Mod+Shift+M` | Toggle amend |
| `b` | Blame focused file |
| `o` | Open file in default app |
| `Esc` | Back to graph |

### 7.6 Git pane, Refs view and sidebar
| Key | Action |
|---|---|
| `↓` / `↑`, `j` / `k` | Move |
| `←` / `→` | Collapse / expand section or repo |
| `Enter` | Checkout / apply / open workspace |
| `F2` | Rename |
| `Backspace` | Delete or close (confirm when unsafe) |
| `/`, `Mod+F` | Filter |
| `Esc` | Clear filter / back |

---

## 8. Accessibility

- Every control has an AccessKit label and role. Every list row reports its position and content. Terminal panes expose their visible grid as text with the cursor position; screen readers announce new output in the focused terminal at a configurable rate (off / new lines / all).
- Full keyboard operation (Section 7). Focus ring 2 px `border.focus` on every focusable element, visible in both themes; focused terminal pane has a 1 px border.
- Contrast per Section 4. Color is never the only carrier of meaning: status dots use distinct shapes, status glyphs accompany colors, lane shapes are available, ahead/behind use arrows plus numbers.
- Reduce Motion honoured, including the needs-input ring pulse. No flashing content; the bell flash is a single 120 ms border change.
- UI scale to 200 %; layout reflows, nothing clips. Terminal font scales independently.
- Screen-reader announcements for: operation start/finish, toast text, notification arrival with workspace and message, conflict state, connection changes, undo/redo description.

---

## 9. Packaging

- One crate, two binaries: the app (default features) and the headless CLI (`--no-default-features`). The app embeds CLI builds for every supported remote target so a remote host never needs a download.
- macOS: universal app binary (`aarch64` + `x86_64` via `lipo`) inside a `.app`, signed and notarized, distributed as a `.dmg`. The executable inside the `.app` is the same single static binary.
- Linux (future): one statically linked `x86_64` and one `aarch64` app executable, plus an AppImage wrapper and a `.desktop` file. No runtime beyond the libs in Section 1. Wayland preferred, X11 fallback, both via `winit`.
- No installer steps beyond copying the binary. Settings, cache, index, logs, journal, scrollback, and state all live under the `directories`-provided paths and are safe to delete.
- Remote footprint: `~/.amalgum/bin/<version>/amalgum`, `~/.amalgum/tmux.conf`, `~/.amalgum/shell-integration.{zsh,bash,fish}`, `~/.amalgum/run/` (sockets), `~/.amalgum/queue.jsonl`. `amalgum hooks remove` and deleting `~/.amalgum` removes everything.
- Build command on both: `cargo build --release`. Fonts, grammars, icons, tmux.conf, shell snippets, and remote CLI binaries are `include_bytes!` into the app binary.

---

## 10. Stretch Goals

- Browser split via `wry`, feature-gated, macOS only until Linux has a single-binary path.
- CI/build status badges on commits (GitHub Checks, GitLab pipelines) and PR number on the workspace row.
- GitHub/GitLab PR and MR list and creation from a workspace.
- GPG and SSH signing configuration in-app.
- Three-way merge conflict editor.
- `send-keys`, `read-screen`, and `wait` control commands for scripting agents from outside.
- Own remote daemon replacing tmux for session survival.
- Submodule listing and update. Git LFS status and lock UI.
- Multiple windows.
- Linux as a shipped target with the packaging in Section 9.

## 11. Non-Goals (v1)

- Windows.
- Shipping Linux builds (the build must work; it is not released or tested for UX).
- Embedded browser (see Section 1 for the port-forward alternative).
- Submodule management beyond showing them as files.
- Git LFS UI.
- Three-way merge editor (see 5.17 for the minimum).
- Multiple windows.
- Any network call other than git, ssh, optional gravatar, and optional OS notifications. No accounts, no cloud sync.

---

## 12. Risks

1. **egui as a terminal renderer.** Unknown until measured: 200×60 grids at full throughput, glyph atlas growth with CJK, emoji, and Nerd Font fallback, no ligatures. Prototype in week one with `cat` of a large log and `vim` scrolling. Fallback: an owned per-cell glyph texture cache drawn as one mesh per pane.
2. **tmux as the survival layer.** Version constraints (≥ 3.2 for `-e`), OSC passthrough needing `allow-passthrough`, mouse and clipboard quirks. Mitigations: private `-L amalgum` server with our own minimal conf and no prefix, per-host off switch, own daemon as the later path.
3. **Agent hook stability.** Hook schemas, session-id fields, and resume flags for Claude Code, Codex, Gemini, and OpenCode change often; status and hibernation depend on them. Mitigation: one adapter file per agent, versioned, `amalgum hooks status` detects drift, OSC/bell/idle heuristics as degraded mode, hibernation never fires without a hook-confirmed idle.
4. **`sshd` forbidding unix-socket forwarding.** Fallback: queue file drained on a 10 s poll while connected; notifications arrive late but never lost.
5. **CLI-parsed git.** Output formats are stable with `--porcelain` and `-z`, but performance on the kernel repo may need `gix` for graph and blame. The accelerator is feature-gated and measured against the same fixtures.
