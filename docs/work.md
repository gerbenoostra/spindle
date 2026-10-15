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

## Branch lives and history

Branch names are reusable. When a name has several retained lives the current
one carries an English ordinal: `feat-x (2nd)` in compact labels, `2nd feat-x`
leading the detail's `branch:` line, `feat-x (1st of 2)` on an earlier life's
row. The ordinal numbers the retained lives of the branch, not a count of
your work on it. A sole life shows no number, and the count drops when a
closed life is pruned. `h` shows the earlier lives - retained for audit and
never counted on the current row's counts or recency.

The `branch:` line states what the record proves about this life, in plain
words: `created <age> ago from <sha>` when the reflog proved the creation
(`first seen <age> ago` when it did not), `renamed from <old> <age> ago` for a
proven rename (`renamed (earlier name not recorded)` on a legacy record), and
`history rewritten <age> ago` for a proven non-fast-forward tip move - `seen
rewritten <age> ago` when only the detecting pass could date it, and a bare
`history rewritten` when a legacy record carries no time. Earlier
lives fold into one `N earlier <name> not counted here (<verb> <age> ago) - h`
line, where `<verb>` says what the closing observation proves: `deleted` when
a pass observed the ref absent, `replaced` when a boundary was detected while
the ref stayed present, `ended` when that observation is no longer retained.
When continuity with the earlier same-name branch is unproven, a `!`
warning takes that line's place and the earlier life stays separate; once the
earlier life is no longer kept the warning drops `- h` and says so.

A conversation can touch several branches over its life; it is shown once
with its placements in the detail view.

## Detail and evidence

The `[4]` detail pane follows the focused row. `e` shows the evidence behind
a claim - which source won and what was outranked, `?` where nothing proved
it. On a branch row it renders the raw `branch identity record:`: the record
id and its ordinal among retained lives, the seen range and how it ended,
proven creation and last tip, rename and rewrite evidence, and the continuity
decision in its stored spelling with a one-line gloss. The full vocabulary is
in [attention](attention.md).

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

Directly under the activity groups, a `git state (observed)` subgroup lists
the delivery-count transitions a pass observed: `ahead:`, `behind:` and
`unpushed:` readings, each dated `observed <age>` at the time the collector
saw the change. That timestamp is a detection, not work - a remote push or a
base move shifts the counts without any local reflog entry, so the subgroup
is never paired with one and `activity:` still names only source-backed
occurrences (`?` when none exist). Only lifecycle reasons with the exact
`ahead: `, `behind: ` or `unpushed: ` prefix move there; every other
observation reason stays under `observations:`, an event left with no other
reasons leaves no empty group, and the subgroup itself is omitted when no
counts were observed. At most seven count events show, newest scan first
with ties ordered by their reason list.

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
- Each conversation gets one summary row keyed by its full
  provider-qualified key - because labels and short ids collide and repeated
  labels obscure which conversation a row names; not label or short-id
  deduplication (distinct conversations would merge) and not per-turn
  display (the noise the summary exists to remove).
- Rejected transcript paths are neither retained nor exposed - because they
  provide no actionable evidence and add noise; not hiding them only in the
  evidence pane (that leaves unused data in the JSON contract).
- Rewrites say "history rewritten", never "force-push" - because detection
  proves a local non-fast-forward move; not claiming a push the collector
  never sees.
- Ordinals count retained lives and appear only above one - because a `1st`
  on every branch is the noise `#N` was; not stable lifetime counters (they
  need persisted counters for little gain).
- The ordinal leads the `branch:` line but trails compact labels, and an
  earlier life's label counts the retained lives (`1st of 2`) - because a
  suffix keeps names aligned in lists while the summary line reads the number
  first, and the of-N form states what the number ranges over.
- An end verb follows the closing observation - `deleted` for an observed
  absence, `replaced` for a boundary detected while the ref stayed present,
  `ended` when the closing observation is not retained; not `deleted` for
  every close, which a boundary never proved.
- Earlier lives fold into the `branch:` block - because a separate same-name
  history section repeats the same fact; not keeping both.
- The TUI says "branch", "earlier" and "same branch" - "incarnation" stays a
  store and architecture term; not displaying the wire spellings.
- An unproven boundary warns instead of listing earlier lives - because both
  lines would repeat the kept-separate fact, and the warning carries the one
  `- h` pointer, or drops it and says the earlier life is no longer kept.
- A boundary close is recorded as `branch replaced by a new branch of the
  same name`, and the older stored spelling `branch incarnation changed` maps
  to it at render; `p` on a branch record that has since ended fails with
  `no active branch <id>` - because stored history is never rewritten; not
  leaving "incarnation" on display until old events age out.

The architecture and failure semantics live in
[work-lifecycle](architecture/work-lifecycle.md) and
[attention](architecture/attention.md).
