# Amalgum

A workspace shell for running coding agents in parallel, with git built in. A vertical sidebar of
repos, remotes, and workspaces; each workspace is a set of terminals bound to one location (local
clone, worktree, or `ssh host:path`) plus a git pane for that location. Agent-aware: the sidebar
tells you which tab needs you and why. Rust-native, single binary, macOS first, Linux from the
same source.

- Product spec: [docs/SPEC.md](docs/SPEC.md)
- What works today: [docs/STATUS.md](docs/STATUS.md)
- Working on the code (humans and agents): [CLAUDE.md](CLAUDE.md), [docs/HARNESS.md](docs/HARNESS.md)

## Build

```sh
scripts/agent/bootstrap          # toolchain, git hooks, dependencies, then the gate
cargo run                        # the app
cargo run --no-default-features -- --help   # the headless CLI
scripts/build app                # release app binary for this machine
scripts/build cli x86_64-unknown-linux-musl # static remote CLI
```

Requires only `rustup` and `git`. Linux builds need no system development packages.
