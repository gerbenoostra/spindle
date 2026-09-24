# Agent notes for agent-sessions

This repository is **public and open source**: nothing in it may contain local
paths, machine names, personal configuration, secrets or anything else that is
not useful to a stranger who cloned it.

`.tasks/` is an ignored symlink into a private task store. When it is present
on a checkout it may be read and written as task instructions direct, but it
is never committed and never packaged - the nix source filter excludes it by
name.

## What this tool is

`agent-sessions` answers, in one glance: what is running where, what is
blocked on the human, and which work has been forgotten. It reads agent
session stores, transcripts, tmux, git and live processes; it renders a
terminal dashboard; it is not a daemon.

The write boundary is the product and it is small: versioned records under
`$XDG_STATE_HOME/agent-sessions/`; agent hook configs edited only by a
human-invoked `register` at the paths `share/agents/` enumerates; and the
confirmed cleanup argv `git worktree remove` / `git branch -d`/`-D`. Never a
tmux `set-option`, a `git config` write, a window rename, or an edit to an
agent's own files. The design rules behind that boundary are in
`CONTRIBUTING.md`; read them before changing behaviour.

## Development

Run development tools within the shell `nix develop` creates, or use
`. "$HOME/.cargo/env" && [cmd]`. `just check` is what CI runs; `just coverage`
adds a full-region coverage gate on `src/`.

Tests run against disposable fixtures only: throwaway tmux servers
(`tmux -L <socket>`), scratch git repositories and temp `$HOME`s. Never test
against live state, a live agent or the user's real tmux server.
