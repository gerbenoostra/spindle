# Attention architecture

How hook events, provider state and acknowledgement become a row's glyph.
User-visible behavior is in [attention](../attention.md).

## Data flow

1. `hook` (`src/hook.rs`) maps `(provider, native event)` to a normalized
   event and appends one record to the journal (`src/store.rs`).
2. Every refresh loads the store in stage 1, beside the provider inventory.
   Loading folds the journal tail onto the checkpoint.
3. For each conversation, `attention::derive` combines the fold, seen-state,
   the not-busy mark, the provider's published state and process liveness
   into an execution state and an attention value. It is a pure function:
   `now` is an argument.
4. The collector writes the focus acknowledgement. It is the only write a
   refresh makes, and it re-derives the row from what was stored.
5. Work rows roll up their bound conversations' attention and pick their
   section on every publish. The TUI's `space` writes seen-state or a mark,
   and the next refresh reflects it.

## Invariants

- The journal is the only shared mutable file, and `journal.lock` serializes
  every mutation: append, compaction and the authored-file rewrites.
- Reduction is a pure function of commit order. Replaying the journal,
  before or after compaction, yields the same folds.
- A record with a future schema, an empty session id, or bytes that do not
  parse is excluded from derivation but keeps its bytes and its sequence
  number. While the journal holds one, compaction defers.
- Acknowledgement never rewrites the journal. Seen-state is applied at
  derive time, and compaction may drop only latches it already covers.
- An event carries the `(pid, pid_start)` the hook resolved. An event from a
  replaced process instance never drives the live replacement's execution,
  but its unseen `end` or `error` still shows.
- Dead processes are reaped on read. `SessionEnd` and other teardown hints
  never reap.

## Operational constraints

- The lock is an OS advisory lock (`File::try_lock`, Rust 1.89), released by
  the kernel when its holder exits. A writer waits up to 2 s, then the hook
  drops its event and exits zero.
- An append that finds an incomplete frame at the journal's end cuts it off
  first, so later records frame correctly. The cut bytes go to
  `journal.cut-<epoch ms>` beside the journal, and every load reports the
  file as a journal error.
- `$TMUX` holds tmux's resolved socket path, while discovery may list a
  symlinked spelling. The dashboard's own pane is matched by canonical
  socket path for that reason.

## Decisions

- One append-only journal under an OS advisory lock - because the kernel
  releases the lock when its holder dies; not a lock file judged stale and
  stolen (the check-then-remove race let two writers hold it).
- A wait's acknowledgement is compared by time, not by event identity -
  because the hook event and the provider's record date one wait a few
  milliseconds apart, and whichever wins arbitration must read the same
  acknowledgement; not by sequence (a provider-published wait has none).
- A provider record without timestamps is dated when first read and keeps
  that date while its content and process instance stay unchanged; a
  replacement process starts a new episode, and the date is forgotten once
  the conversation is no longer live - because re-dating it every poll
  made each poll a new wait that no acknowledgement could cover. The
  instance is dated by the provider's start, or the OS process table's
  when the provider reports none.
- Compaction drops latches seen-state has acknowledged - because Vibe
  publishes no `start`, so its turn-end latches would otherwise accumulate
  forever.
- Compaction defers while the journal holds a frame the reduction cannot
  carry - because rewriting would lose those bytes; the cost is a journal
  that grows until the frame is repaired. Rewriting only the retained tail
  is not built.
- An append sets aside the bytes it cuts instead of deleting them - because
  a reader cannot tell a fresh tear from a corrupt length header mid-file,
  and the latter has committed records behind it; not truncating outright
  (those records would be lost), nor scanning forward for the next frame
  (heuristic framing).
- A Claude hook takes a lone matching `sessions/<pid>.json` as is, and
  checks liveness only when several match - because a resumed session can
  leave a dead process's file beside the live one; checking every time
  would spawn `ps` on every hook.
- `list --json` keeps `own_pane` as the bare `%N` id, while the dashboard
  compares socket plus id internally - because changing the field's meaning
  needs a schema version bump.
- `seen.json` written by builds before the episode format is not read -
  because nothing has been released yet.
