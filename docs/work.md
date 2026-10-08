# Work

The `[2]` pane lists every piece of work the collector knows about: checked-out
worktrees, local branches without a workspace, detached checkouts and non-git
project folders a conversation ran in. `1-4` move focus between panes, `tab`
cycles them, `j`/`k` move the cursor and `/` filters the focused list.

## Sections

Rows are grouped by what they need, most urgent first:

- **Needs you** - a bound conversation is waiting, errored or finished unseen.
  Attention outranks every other reading, so a row that is also safe to clean
  still lands here.
- **Active** - a live agent is working.
- **Follow up** - unfinished work that asks for a look: failed or pending
  pipeline checks, a dirty tree, unpushed commits, a recent resumable idle
  conversation (in a project folder, always), landed work whose cleanup is
  blocked, or any other open row that fits nowhere else.
- **Forgotten** - unfinished work whose newest source-backed activity is
  older than `forgotten_after` (14 days, configurable) with no live process.
  Old work stays visible; it does not silently drop out.
- **Ready to clean** and **Cleanup review** - cleanup verdicts: what the
  recorded evidence makes provably safe, or safe-looking enough to need a
  read. Neither says the work is done - the tool never infers your intent.
  In the `all` rollup each collapses to a count; expansion and the cleanup
  execution itself have not shipped.

A section names attention and delivery evidence, never a task verdict: the
tool can only show what the recorded sources prove.

`p` parks a row. Parking suppresses only `Forgotten` placement - a parked row
is your local note to leave it alone, not a claim that the work completed.

## Incarnations and history

Branch names are reusable. A `feat-x #2` label means the collector has
observed that name before: `#N` numbers the current incarnation's lifetime of
the branch, not a count of your work on it. `h` opens the same-name history -
earlier incarnations, retained for audit and excluded from the current row's
counts and recency.

A conversation can touch several branches over its life; it is shown once
with its placements in the detail view.

## Detail and evidence

The `[4]` detail pane follows the focused row. `e` shows the evidence behind
a claim - which source won and what was outranked, `?` where nothing proved
it. The full vocabulary is in [attention](attention.md).

Under **Activity**, each conversation the row's record has observed gets one
row - `last activity` at its newest source-backed occurrence - rather than one
line per turn. The row shows a short id plus the latest-known submitted prompt,
kept as a bounded excerpt; the excerpt is context, not a claim that the
activity dates that prompt. Where no prompt was captured the provider title
stands in, then the bare id. At most seven conversations show, newest first,
after the per-turn events compact; a record without conversation cursors keeps
its raw per-turn trail, and the other sources' raw histories are unchanged.
Context is captured per work row, so a later prompt after moving to other work
does not overwrite the earlier row's captured context.

## Navigation

- `enter` on a live conversation selects the bound socket-qualified pane the
  evidence resolved - by published target, process ancestry or TTY. On a
  work row it selects the window the row is bound to. On a stopped
  conversation it resumes it in place: the dashboard's own terminal runs the
  provider's resume argv with the proven root as working directory - the
  recorded checkout, or a non-git project's canonical folder. If no root,
  resume capability or executable can be proven, it reports and launches
  nothing, and it never creates tmux windows or panes.
- `o` opens the work row's recorded pull/merge request URL in the platform
  opener, verbatim. A row without a recorded URL only reports.
- `enter` and `o` re-resolve their target against a fresh local collect, so
  nothing about them waits on the network; the forge verdict and URL stay
  what the displayed row captured.

## Decisions

- Attention outranks delivery evidence for section placement - because a
  mergeable branch with an unanswered prompt is still asking for you; not
  verdict-first ordering (the inbox buries under "done" rows).
- Parking is a local identity flag, not a verdict - because the tool cannot
  prove your intent; not inferring completion from absence of activity.
- A stopped conversation resumes by `exec` in the dashboard's own terminal -
  because the tool is not a tmux topology manager; not a new window or pane
  (unrequested structure is worse than replacing the dashboard).
- Navigation re-resolves locally, not through a full remote pass - because a
  keypress must not wait on the network; not stale-target jumps (a vanished
  pane reports instead).
- `o` trusts only the URL the collected row recorded - because a local
  refresh cannot replace missing forge evidence; not re-deriving or guessing
  a URL at keypress time.

The architecture and failure semantics live in
[work-lifecycle](architecture/work-lifecycle.md) and
[attention](architecture/attention.md).
