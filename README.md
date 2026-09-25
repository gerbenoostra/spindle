# agent-sessions

One dashboard for every agentic session and worktree: **what is running where,
what is blocked on you, and which work has been forgotten.**

With several agents and worktrees in flight it is easy to lose track of which
are finished and which are mid-flight. `agent-sessions` reads the evidence
already on disk - agent session stores, transcripts, tmux, git and live
processes - and presents the derived facts in one terminal UI, so deciding what
needs you takes a glance instead of a window tour.

> Status: scaffolding. The binary builds, packages and releases; the dashboard
> itself lands piecemeal. Today `agent-sessions` answers `--version` and
> `--help`, and any subcommand that has not shipped is a usage error.

`agent-sessions` is the binary and the state directory; `spindle` is this
repository.

## Install

Simplest:

```sh
curl -fsSL https://raw.githubusercontent.com/gerbenoostra/spindle/main/install.sh | sh
```

The installer detects your platform, downloads the matching release tarball,
verifies its checksum and installs the binary to `~/.local/bin` (override with
`AGENT_SESSIONS_INSTALL_DIR`). Pin a release with `AGENT_SESSIONS_VERSION=vX.Y.Z`;
see `install.sh` for the full set of environment variables.

### Nix

```sh
nix profile install github:gerbenoostra/spindle
```

Or as a flake input:

```nix
inputs.spindle.url = "github:gerbenoostra/spindle";
inputs.spindle.inputs.nixpkgs.follows = "nixpkgs";
```

```nix
home.packages = [ inputs.spindle.packages.${pkgs.system}.agent-sessions ];
```

### Cargo

```sh
cargo install --git https://github.com/gerbenoostra/spindle
```

### From source

```sh
git clone https://github.com/gerbenoostra/spindle
cd spindle
cargo install --path .
```

## Configuration

Nothing is required to run. An optional config file lives at
`$XDG_CONFIG_HOME/agent-sessions/config.toml` (default
`~/.config/agent-sessions/config.toml`); every key can also be set through an
`AGENT_SESSIONS_<KEY>` environment variable, which wins over the file. A
malformed file is reported once and the defaults stay in effect.

| Key | Default | Meaning |
| --- | --- | --- |
| `forgotten_after` | `14d` | unfinished work with no live process enters `Forgotten` |

Durations are `<n><unit>` with `s`, `m`, `h` or `d`.

## Interop

`agent-sessions` is read-only against everything it observes: agent session
stores, transcripts, tmux servers, git repositories and process tables are
inputs, never outputs.

What it writes, all of it bounded:

- Its own records under `$XDG_STATE_HOME/agent-sessions/` (default
  `~/.local/state/agent-sessions/`): the hook-event journal, seen-state,
  parked flags, incarnation history, not-busy marks and reference dismissals.
- User-confirmed cleanup commands only: `git worktree remove` and
  `git branch -d`/`-D`, each printed and re-checked immediately before it runs.
- Hook entries invoking `agent-sessions hook` inside agent config files,
  written by `agent-sessions register` only when a human types it, through the
  safe-write contract described in [CONTRIBUTING.md](CONTRIBUTING.md). Entries
  other tools installed - `tmux-agent-status`'s included - are left
  byte-for-byte alone.

It never writes a tmux option, a git config value or a window name, and it
never edits an agent's own session files or transcripts.

Where `wt` worktree markers (`@wt_*` window options, `wt.*` worktree config)
exist they are read as additional
evidence; where they do not, the same edges are derived from pane working
directories. `agent-sessions` does not read `tmux-agent-status`'s pane or
window options at all: the two agree by applying the same event vocabulary and
attention projection to independent observations, not by reading each other.

### The shared worktree state vector

Cleanup verdicts and section placement read one derived vector per work
anchor. Runtime fields come from the tmux/process inventory; everything else
is read from Git with `GIT_OPTIONAL_LOCKS=0`. Facts that cannot be proven are
`Unknown` and fail closed - the tool never guesses a remote or branch name.

| Field | Source | Notes |
| --- | --- | --- |
| `worktree` | `git worktree list --porcelain` | a path, or `none` for a branch with no workspace |
| `windows` | tmux window options + derived cwd | total, and how many are orphaned |
| `live_pids` | pid-cwd scan, worktree root or below | catches agents, servers, watchers alike |
| `live_agent_sessions` | agent session stores, filtered by `(pid, pid_start)` liveness | |
| `past_agent_sessions` | per-agent history for that path | "no live session" never means "nothing ever ran here" |
| `dirty` | `git status --porcelain --untracked-files=all` | the primary removal blocker |
| `commits_ahead_of_base` | `git rev-list --count <base>..HEAD` | zero plus no sessions means the worktree was never used |
| `upstream_state` | `never_pushed` / `tracked` / `remote_gone` | proven by `ls-remote`; no fetch, no local mutation |
| `unpushed_commits` | `git rev-list --count @{u}..HEAD`; vs the proven base when never pushed; commits no `refs/*` reaches when detached | the real data-loss risk on removal |
| `landed` | `no` / `ancestor-merged` / `content-merged` | ancestry first, then path-scoped tree comparison |
| `last_git_activity` | worktree HEAD reflog; branch reflog when there is no worktree | the reflog is per worktree, a real recency signal |

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development shell, the checks CI
runs, the write boundary and the rules around disposable test fixtures.

## Licence

MIT.
