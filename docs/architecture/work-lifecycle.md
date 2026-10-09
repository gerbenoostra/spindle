# Work lifecycle architecture

How branches, worktrees, project folders and conversations become durable
work history. User-visible behavior and its rationale are in
[work](../work.md); this page holds the invariants and the decisions that
live between modules.

## Invariants

- Work identity is scoped by the repository's canonical git common dir, not
  its checkout path. Branch names are reusable, so a name alone is not an
  identity: an incarnation is one observed lifetime of a name, numbered `#N`
  per `(repo, ref)`.
- Continuity is proven, not assumed: an exact rename or a force-push keeps
  the incarnation's id, while a detected recreation or a boundary that
  cannot be proven continuous starts a new one.
- A detached checkout and a non-git project space are keyed by canonical
  path. Runtime pane identity never defines a work row.
- A conversation's placement is exact only: the live process's cwd, or a
  Claude record's dated `gitBranch` resolving into the project repo - never
  an arbitrary recorded cwd, an inferred intent, a same-name guess, or
  process ancestry (a daemonized tmux process does not prove who spawned
  it). A dead transcript keeps the project branch it was placed on - valid
  history - while the live branch moving finished work to another row does
  not rewrite it.
- Closed same-name incarnations are excluded from the current row's
  attention, counts and recency; their history still shows under `h`, and a
  gone anchor keeps its row while current live references name it (then
  under CleanupReview) - transcript or resumable history alone never
  retains it.
- Closed branch records are retained 90 days, or while a conversation touch
  references them:
  unreferenced history is bounded so the store cannot grow forever, while
  history that is still in use is never aged out.

## Activities versus observations

The store keeps two bounded histories per record: `activities` are
source-dated occurrences of real work; `observations` are detection times of
what a pass learned. Only source timestamps date activity - a forge item's
merge or close date counts, its `updatedAt` or pipeline bot churn does not; a
conversation's transcript message or hook producer time counts, a live
record's publication time does not; the newest mtime among changed paths
counts, a clean or deleted tree gets no proxy. No missing source time ever
falls back to scan time. First presence is a silent baseline; a first real
source event backfills into the history at its own time. Keeping
unproven-time history separate is what makes "worked 30 days ago" provable
rather than "noticed 30 days ago".

## Decisions

- Reflog activity is dated by the work entries' own timestamps - because
  maintenance rewrites the logs without adding work; not the file's mtime
  (a last-write time cannot prove an event).
- The reflog activity reason describes the one selected work entry -
  because qualifying entries compete as whole `(time, old, new, message)`
  tuples across the HEAD and branch logs, so the displayed sha, message
  and timestamp provably come from the same line; not a separately chosen
  newest entry per field (a same-second commit and amend could attribute
  one entry's message to another's time). Same-time ties resolve
  lexicographically on `(old, new, message)` and identical tuples
  collapse, so selection is deterministic. The message is stored raw and
  escaped once at render; a timestamp without matching in-memory
  metadata - fabricated or inconsistent evidence, never a real
  selection - keeps the generic `reflog work` text rather than borrowing
  another line's, and generic reasons persisted earlier stay untouched
  (history is never backfilled from current metadata).
- A conversation's first recorded turn backfills source-dated activity -
  because the source's own timestamp is real work history however old; not
  seating the cursor alone (recency and history then disagree).
- A dated working-tree transition links its observation to the activity
  the same transition emits - because both events describe one fact and
  the provenance is known only at emission; the optional `covered_by`
  field records the counterpart's `(source, occurred_at_ms)`, never a
  retroactive match (a later pass cannot prove which activity the
  detection duplicated). The detail hides such an observation only while
  its linked WorkingTree activity stays retained and carries every
  observation reason - a pruned counterpart, a partial reason match, an
  unlinked legacy event and every clean or deleted transition whose
  remaining changed paths prove no mtime all stay visible. Suppression
  is display-only: the raw history, scan times, recency and retention
  bounds are untouched, the
  filter runs before the section check and the seven-per-source cap, and
  the field serializes only when set so legacy records read unchanged
  under the same `WORK_SCHEMA`.
- Lifecycle `ahead`/`behind`/`unpushed` transitions render under a separate
  `git state (observed)` subgroup in the work detail - because they are
  scan-time readings of delivery counts, not work: a remote-only push or a
  base move changes them with no local reflog entry, so each is labeled
  `observed` at its detection time and never paired with a reflog row. The
  split is a render-time projection on exact prefixes only - a `Lifecycle`
  reason starting `ahead: `, `behind: ` or `unpushed: ` moves, every other
  reason and source stays under `observations:`, and a cloned event carries
  the projected subset so the stored history is untouched. The subgroup
  follows the source-backed groups (whose order it never joins), omits
  itself when empty, and caps at seven events after partitioning, newest
  scan first with same-scan ties on the sorted reason list; `activity:`,
  recency, age filtering and `Forgotten` classification keep reading only
  source timestamps.
- `work.json` versions separately under `WORK_SCHEMA` and older files reset
  rather than migrate while pre-release - because backwards compatibility is
  not promised before release and the collector rebuilds the state; not
  migration code (reinterpreting ambiguously dated history is machinery for
  state nothing depends on). Incarnation and parked history reset; journal,
  seen and marks are unaffected. A future-versioned or malformed file still
  reports and refuses the read-modify-write, keeping its bytes.
- A resume's acknowledgement is written in the instant between preflight
  and `exec` - because an earlier write could acknowledge a conversation
  whose resume never launched, and a later one can never run. The honest
  limit: if `exec` fails after the write, the acknowledgement stays
  persisted - there is no safe atomic rollback, since overwriting the record
  could clobber a concurrent acknowledgement - and the launch failure is
  reported rather than hidden. A live `enter` jump instead acknowledges
  only after both selects succeed.
- Work identity is the incarnation under the repo's canonical common dir -
  because branch names are reused and checkout paths move; not name or path
  identity (both silently merge distinct lifetimes).
- Touches and placements come only from proven cwd and dated `gitBranch`
  evidence - because a dead transcript's project branch is valid history
  while a live branch moving finished work must not rewrite it; not
  inferring the current branch (it misattributes history to wherever the
  checkout happens to sit).
- Relations between conversations are provider lineage, live ancestry and
  same-incarnation touches only - because tmux's daemonized process
  ancestry cannot prove which conversation created a pane; not name, intent
  or creation-time inference.
- Closed records are retained 90 days, or while a touch references them -
  because unreferenced history must stay bounded while history that is
  still in use must survive; not unbounded growth (the store becomes the
  problem) nor aggressive expiry (audit value is the point of keeping it).
- Conversation context is captured per work record - because
  conversations move between work records; not resolving historical rows
  from current provider state (a later placement would overwrite earlier
  context).
- An equal-time session update only fills missing metadata - because
  polling proves no newer source occurrence; not unconditional
  replacement (it rewrites captured context without newer activity).
- Prompt excerpts are stored bounded and raw, escaped once at render -
  because excerpts avoid retaining transcripts and escaped-cell bounds
  preserve safe terminal display; not byte truncation (it splits Unicode
  or undercounts control escapes) or pre-escaped storage (it risks double
  escaping).
- Additive optional context and coverage metadata leave the work schema
  unchanged - because resetting it discards incarnation and parking
  history; not versioning an additive change (legacy records can default
  absent fields).
- Linux pane matching recognizes the kernel's trailing ` (deleted)`
  annotation only when both the reported literal path and recorded root
  are proven absent - because deleted-cwd panes still retain gone work;
  not unconditional suffix stripping (real suffix-named directories and
  recreated roots must not be rebound). tmux reads the cwd verbatim from
  `/proc/<pid>/cwd` on Linux, where the kernel appends the suffix:
  [osdep-linux.c](https://github.com/tmux/tmux/blob/master/osdep-linux.c).
