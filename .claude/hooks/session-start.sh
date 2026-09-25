#!/bin/bash
# SessionStart (Claude Code on the web only): make a fresh container ready for the gate.
# Installs the toolchain pinned in rust-toolchain.toml, fetches crates, and warms exactly the
# builds scripts/agent/verify runs, so the first gate in a session is warm. The container
# state is cached after this hook, so later sessions start fast. Idempotent.
set -euo pipefail
[ "${CLAUDE_CODE_REMOTE:-}" = "true" ] || exit 0
cd "${CLAUDE_PROJECT_DIR:-$(git rev-parse --show-toplevel)}"

rustup toolchain install >/dev/null            # pinned toolchain + rustfmt + clippy
rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true   # static CLI builds
cargo fetch --locked -q

# Warm both feature sets for tests and clippy. A red tree must not fail session start.
for flags in "--no-default-features" ""; do
  # shellcheck disable=SC2086
  cargo test --locked --no-run -q $flags >/dev/null 2>&1 || true
  # shellcheck disable=SC2086
  cargo clippy --locked --all-targets -q $flags >/dev/null 2>&1 || true
done
echo "session-start: $(rustc --version); dependencies fetched; gate builds warmed"
