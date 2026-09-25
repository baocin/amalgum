---
name: ci-and-release
description: How Amalgum's GitHub Actions pipeline maps to local commands, how to reproduce and fix a red CI job (gate, per-target build, linkage, packaging), and how a release is cut. Use when CI failed, when a macOS-only or Linux-only build breaks, when the static CLI grows past 2 MB or links a dynamic library, when the .dmg/.app or tarball is wrong, or when asked to "ship", "release", "tag", or "package". Pushing the release tag is a human step; never do it.
---

# CI and releases

The workflow YAML is thin: every step calls a script, so every CI failure reproduces locally.

| CI job | Local command |
|---|---|
| Gate · ubuntu-24.04 / macos-15 | `VERIFY_FULL=1 scripts/agent/verify` |
| Build · app · <target> | `scripts/check-linkage "$(scripts/build app <target>)" app` |
| Build · cli · <target> | `scripts/check-linkage "$(scripts/build cli <target>)" cli` |
| Package · macOS | `scripts/package-macos <ver> <arm64 bin> <x86_64 bin>` (macOS only) |
| Package · Linux | `scripts/package-linux <ver> <bin> x86_64` |

## Diagnosing
1. Read the failing step's log; the gate's failure line names its remedy.
2. Fails only on the other OS: read the matching `src/platform/<os>.rs`; keep the fix inside `src/platform/`.
3. "CLI must be fully static" or over 2 MiB: a dependency brought in C code or a `gui` crate leaked into the
   headless build. `cargo tree --no-default-features -e normal` shows what came in.
4. "app links libraries outside the allow-list": SPEC §1 fixes that list. Prefer a crate feature that
   `dlopen`s; ask before widening the list.
5. Red on something the diff cannot have caused: check the same job on `main`. Re-run at most once;
   "flaky" is not a root cause.

## Releasing (humans)
Bump `version` in Cargo.toml, merge to main, push tag `v<version>`. `release.yml` builds static CLIs for four
targets, embeds them into the apps (`AMALGUM_EMBED_CLI_DIR`, see build.rs), signs and notarizes when the Apple
secrets exist, and publishes the .dmg, the Linux tarball, the CLIs, and SHA256SUMS.
