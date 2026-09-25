# Contributing

## Development shell

```sh
nix develop          # cargo, clippy, rustfmt, rust-analyzer, tmux, git, just
just check           # fmt-check + lint + lint-sh + test, exactly what CI runs
just coverage        # the test suite plus a full-region coverage gate on src/
just link            # shadow the installed binary with this checkout's release build
```

`just check` is what CI runs. `just coverage` runs the same suite and then
fails on any region of `src/` nothing reached; a line that genuinely cannot
be reached carries a trailing `// coverage: off` saying why.

## PR titles

The PR title becomes the squash subject on merge and must be a conventional
commit; a required check (`pr-title`) fails PRs whose title isn't. The check
matches types case-sensitively, so Dependabot's default `Build(deps): ...`
fails; `commit-message` in `.github/dependabot.yml` makes it `build(deps): ...`.

| Title                    | Effect while 0.x |
| ------------------------ | ---------------- |
| `feat`                   | minor            |
| `fix`                    | patch            |
| `!` or `BREAKING CHANGE` | minor            |
| any other type           | no release alone |

## Public repository

This repository is public and open source: nothing in it may contain local
paths, machine names, personal configuration, secrets or anything else that is
not useful to a stranger who cloned it. `.tasks/` is a machine-local symlink
into a private task store when it exists on a developer's checkout; it is
gitignored, never committed and never packaged (the nix package's source
filter excludes it by name).

## Design rules that are easy to break

- **The write boundary is the product.** Writes are limited to three surfaces:
  versioned records under `$XDG_STATE_HOME/agent-sessions/`; agent hook config
  files edited by a human-invoked `register`, at exactly the paths
  `share/agents/` enumerates; and the confirmed cleanup argv `git worktree
  remove` / `git branch -d`/`-D`. Everything else is forbidden: no tmux
  `set-option`, no `git config` write, no window rename, no edit to an agent's
  own session files or transcripts. `tests/source_invariants.rs` scans for
  this; add your surface's assertion there when you add one.
- **`Unknown` is a value, not a default.** A dim `?` beats a confident lie:
  absent evidence renders `Unknown`, never a guessed state.
- **`(pid, pid_start)`, never a bare PID.** Providers leave stale locks behind;
  a pid is only evidence while its process instance is provably alive.
- **Disposable fixtures only.** Tests create throwaway tmux servers (`tmux -L
  <socket>`), scratch git repositories and temp `$HOME`s. Nothing tests
  against live state, a live agent, or the user's real tmux server.
- **Subcommands ship with their behaviour.** An argument the binary cannot act
  on is a usage error, not a stub.

## The read boundary

Every external read is read-only and lives behind one audited spawn site
per program - `git.rs`, `forge.rs`, `process.rs`, `tmux.rs` - which
`tests/source_invariants.rs` enforces alongside the write ban. A record
that does not parse fails closed: dropped to a warnings surface or
degraded to `Unknown`, never guessed. The platform contracts behind the
readers:

- tmux: one `list-panes -a -F` per discovered server per refresh, and
  nothing else. tmux does not promise control characters through `-F`
  output across builds, so `|` is the record separator and a field value
  containing `|` drops just that record to warnings. Sockets are
  `<base>/tmux-<uid>` (`base` = `$TMUX_TMPDIR`, default `/tmp`) plus the
  first component of `$TMUX`, deduplicated by canonical path - an aliased
  socket must not return every pane twice. `tmux-agent-status`'s
  `@agent_status`/`@agent_pane_status` options are never read: they are a
  lossy projection of the primary evidence collected here.
- `ps`: one `ps -A -o pid,ppid,etime,stat,tty,comm` snapshot per refresh.
  `etime` is elapsed time, so a start is `snapshot - elapsed`, compared
  within one second. macOS `ps` has no `etimes`, and `lstart` needs a
  timezone database - which is why provider start times are normalized to
  UTC epochs rather than the other way round. `comm` is not a
  full-fidelity basename: Linux caps it at 15 bytes and macOS can emit a
  16-byte argv0 prefix, so a cap-length prefix mismatch is unproven, not
  dead. A row whose `etime` cannot be parsed is kept with an unavailable
  start (pid-only evidence); a zombie is dead. Controlling ttys are
  normalized to `/dev/...` on both platforms.
- git: the optional `wt.*` worktree metadata is read via
  `git config --worktree --get`; absence covers both an unset key and the
  `extensions.worktreeConfig`-disabled refusal, and it is never written.

## The `register` write contract

`register` is the only code that edits user files, and only when a human types
it. Its `SafeWrite` primitive, planner and fault-suite are a port of the proven
implementation in `tmux-agent-status` - normative source: that repository's
`src/register/write.rs` and the register contract in its `CONTRIBUTING.md`, at
revision `d661a146bfe2ed5c3e5066360facb04f2e47aee0`. The port keeps behaviour
and fault coverage; only target discovery, semantic merge logic and the
installed hook commands differ.

The contract it ports:

- Resolve the symlink chain and edit the target; the symlink must still be a
  symlink afterwards.
- Lock with an adjacent `O_CREAT|O_EXCL` lock file held through verification.
  Its record carries PID, process start time and hostname; break it only when
  that exact process is provably gone. Age or PID alone is unsafe, and a lock
  from another host or of unknown liveness is never broken.
- Back up to an adjacent, mode-preserving, fsynced, uniquely timestamped file;
  never reused or overwritten.
- Never truncate. Write a sibling temp file, fsync it, `rename(2)` over the
  target, fsync the parent directory.
- Permission checks look at the resolved target's parent directory:
  `rename(2)` can replace a read-only file when its directory is writable. A
  read-only target warns; an unwritable parent or a non-regular target is
  refused.
- Immediately before the rename, re-check the file's `(length, mtime, hash)`
  fingerprint against what the plan read. After the rename, verify; restore
  the backup automatically only if the result is empty, truncated or
  unparseable. If a complete parseable third-party write won the race, do not
  restore over it - report the current file, the backup and the temp file for
  manual reconciliation.
- "Already installed" is semantic, not byte equality: a semantically complete
  config produces no rewrite and no backup. Existing hook entries other tools
  own - `tmux-agent-status`'s included - are independent and stay
  byte-for-byte unchanged.
- Delivery order is provider plugin > own drop-in file > merging into a file
  the user maintains, so the riskiest route is the last resort. A safer
  available route never silently falls back to a riskier one.

## Working on it against your real install

To shadow an installed binary with this checkout, symlink the built artifact
into a writable directory that appears on `PATH` first. The recipes default to
`~/.local/bin`:

```sh
just build && just link   # ~/.local/bin/agent-sessions -> <checkout>/target/release/agent-sessions
just unlink               # back to the installed binary
```

`~/.local/bin` is only a default; set `AGENT_SESSIONS_BIN_DIR` on both commands
for another destination. `agent-sessions --version` prints the executable that
actually ran, so a shadow is never invisible.

## Packaging

To verify packaging works, build the local flake; it installs nothing.

```sh
nix run . -- --version
just nix-build
```

## Releases

[release-please](https://github.com/googleapis/release-please) turns
conventional commits on `main` (see [PR titles](#pr-titles)) into a standing PR
titled `chore(main): release X.Y.Z`. That PR is the only place `Cargo.toml`,
`Cargo.lock` and the pin examples in `install.sh` change version - never bump
them by hand, and never tag or publish a release by hand. Merging it tags
`vX.Y.Z`, builds the release binaries, and publishes the GitHub release once
every platform archive is attached.

release-please pushes its release PR and its tag through a GitHub App
installed on this repo (not the default `GITHUB_TOKEN`), so the PR gets real
CI runs and the push triggers the release workflow. That requires the
`APP_CLIENT_ID` and `APP_PRIVATE_KEY` secrets on the repository.

Before merging a release PR, verify the release build end to end on a
supported system without relying on the development symlink:

1. Run `just check` and `just nix-build` on the release PR's branch.
2. Install the resulting package or release binary using one of the documented
   installation routes.
3. Confirm `agent-sessions --version` resolves to that installed binary.

For Nix, the checkout itself can be tested without changing another
configuration:

```sh
nix build path:.#agent-sessions
./result/bin/agent-sessions --version
```

If testing through a separate system or home-manager flake, temporarily
override its `spindle` input with `path:/absolute/path/to/this/checkout`. The
exact rebuild command is specific to that configuration. Do not commit the
`path:` input: it is machine-local, and its lock entry changes with the
checkout contents.
