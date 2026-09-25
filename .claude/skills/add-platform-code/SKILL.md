---
name: add-platform-code
description: Where macOS- or Linux-specific behaviour goes in Amalgum. Use when a change needs to open a URL or file, reveal in Finder/file manager, show an OS notification, find listening ports, install the CLI shim, style the window, read an OS setting (dark mode, accent, reduce motion), or otherwise behave differently per OS — even if the words "platform" or "cfg" never come up. Also use when check-portability fails with "OS-specific cfg outside src/platform/" or "OS tool invoked outside src/platform/". Not for POSIX things that are identical on both OSes (PTYs, unix sockets, signals, ssh, tmux).
---

# Adding OS-specific behaviour

SPEC §1: all platform code lives in `src/platform/{macos,linux}.rs`; everything else is portable.
`scripts/check-portability` (in the gate) rejects `target_os`, OS tools (`open`, `xdg-open`, `osascript`,
`lsof`, `ss`, `notify-send`, …), and absolute paths anywhere else.

## Decide first
1. POSIX and identical on both OSes (PTY, socket, signal, ssh, tmux, git)? → not platform code. Write it
   once in the feature module.
2. An existing `platform::` function already does it? (`src/platform/mod.rs` is the whole surface.) → call it.
3. Otherwise the surface must grow. SPEC §1 says "Nothing else may call OS APIs", so a new function is a
   spec-level change: propose it to the human before adding it.

## Adding a function (once agreed)
```rust
// src/platform/mod.rs — portable signature + doc; no cfg here beyond the module switch.
/// Whether the OS asks for reduced motion (§4).
pub fn reduce_motion() -> bool {
    os::reduce_motion()
}
// src/platform/macos.rs and src/platform/linux.rs — one implementation each, same name.
// Shell out through the private helpers `spawn_detached` / `capture`. Slow calls run on a worker,
// never on the UI thread.
```
- Both OS files must implement it or the other OS stops compiling. Locally you compile only your own OS;
  CI builds both on every push, so read the other file as carefully as the one you can compile.
- Parsing tool output is portable: put the parser outside `platform` (like `ports::parse_lsof`) and test it
  with captured output. The platform function only runs the tool and calls the parser.
- A tool missing on the user's machine (no `notify-send`, no D-Bus file manager) degrades or falls back;
  it never panics.

## Verify
`scripts/agent/verify` (portability leg), then the CI `Gate · macos-15` job for the other OS.
