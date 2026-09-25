# Agent notes for agent-sessions

This repository is **public and open source**: nothing in it may contain local
paths, machine names, personal configuration, secrets or anything else that is
not useful to a stranger who cloned it.

## Development

Run development tools within the shell `nix develop` creates, or use
`. "$HOME/.cargo/env" && [cmd]`. `just check` is what CI runs; `just coverage`
adds a full-region coverage gate on `src/`.

Tests run against disposable fixtures only: throwaway tmux servers
(`tmux -L <socket>`), scratch git repositories and temp `$HOME`s. Never test
against live state, a live agent or the user's real tmux server.
