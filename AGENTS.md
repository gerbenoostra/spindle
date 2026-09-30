# Agent notes for agent-sessions

This repository is **public and open source**: nothing in it may contain local
paths, machine names, personal configuration, secrets or anything else that is
not useful to a stranger who cloned it.

## Development

Run development tools within the shell `nix develop` creates, or use
`. "$HOME/.cargo/env" && [cmd]`. `just check` is the fast subset of CI;
`just coverage` adds a full-region coverage gate on `src/`; `just ci` runs
every CI job against the committed `HEAD`, natively and in a Linux container.

Tests run against disposable fixtures only: throwaway tmux servers
(`tmux -L <socket>`), scratch git repositories and temp `$HOME`s. Never test
against live state, a live agent or the user's real tmux server. The harness
lives in `tests/support/` and `tests/claude_store.rs`; a fake agent process
is a symlink to a system binary (a copied signed binary is killed on macOS,
while `comm` still reports the invoked name).
