//! The versioned store under `$XDG_STATE_HOME/agent-sessions/`: the only
//! place this tool writes outside a confirmed cleanup or `register`.
//!
//! Three file shapes live here:
//!
//! - `journal.log` - hook events as length-delimited records (`u32` LE byte
//!   count + JSON), appended under `journal.lock` with the next monotonic
//!   commit sequence and fsynced. Concurrent writers can interleave on the
//!   lock but never overwrite one another: reduction is a pure function of
//!   committed sequence.
//! - `checkpoint.json` - the journal's reduction compacted through a commit
//!   sequence, written sibling-temp + fsync + rename; the journal then
//!   rewrites to only the tail past that sequence.
//! - `seen.json`, `marks.json`, `work.json` - authored records
//!   (acknowledgement, the not-busy mark and the work rows' parked flags,
//!   branch-incarnation identity and lifecycle fingerprints) as whole-file
//!   atomic renames.
//!
//! `journal.lock` is the one mutation lock: appends, compaction and the
//! authored files' read-modify-writes all hold it, so a rewrite can never
//! interleave with a concurrent read or commit.
//!
//! Every record carries a schema version. Journal, checkpoint, seen and
//! marks records share [`SCHEMA`]: readers accept the current version and
//! an absent `v` reads as the pre-versioned schema; a future-versioned or
//! malformed record is excluded from derivation, retained on disk and
//! reported for the evidence view, and a corrupt journal tail never hides
//! the valid prefix. `work.json` versions separately under `WORK_SCHEMA`:
//! while the tool is pre-release an outdated work file is reset rather
//! than migrated - compatibility is not promised before release, and the
//! collector rebuilds the state.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::provider::SourceError;
use crate::text;

/// The record schema this build reads and writes for the journal,
/// checkpoint, seen and marks files. `0` - an unversioned record from
/// before the field existed - reads as the previous schema.
/// v1 -> v2: records gained `updates`, session cursors and per-anchor
/// probes (`git_dir`, `worktree_state`, `head`, `commit`, `working_tree`,
/// `behind`), and `worktree` turned into a proven-or-`None` tri-state - a
/// v1 build rewrites the file without all of that, so its writes must
/// refuse rather than clobber what it cannot read.
/// v2 -> v3: the mixed `updates`/`activity_at` contract split into
/// source-backed `activities` and scan-time `observations`.
pub const SCHEMA: u32 = 3;

/// The `work.json` envelope's own schema. Work state versions separately
/// because its contract is the derived-history one: while the tool is
/// pre-release, a file written by an older contract is dropped and
/// rebuilt by the next sync rather than migrated - backwards
/// compatibility is not promised yet and regenerating is cheap. A future
/// version still reports and refuses like every authored file.
/// v3 -> v4: reflog maintenance no longer counts as work and first
/// session updates backfill source-dated activity.
const WORK_SCHEMA: u32 = 4;

/// Compact once the journal's un-checkpointed tail passes this many
/// records: enough that a busy day never rewrites, small enough that a
/// scan stays trivial.
const COMPACT_AFTER: usize = 128;

/// How many rejected records per conversation a checkpoint carries:
/// enough to diagnose a misbehaving producer, bounded so a stuck one
/// cannot grow the checkpoint.
const REJECTED_KEPT: usize = 16;

/// How long `acquire` waits for the holder before giving up - long enough
/// for a real append (milliseconds) many times over, short enough that a
/// hook never stalls its agent.
const LOCK_WAIT: Duration = Duration::from_secs(2);

const JOURNAL: &str = "journal.log";
const LOCK: &str = "journal.lock";
const CHECKPOINT: &str = "checkpoint.json";
const SEEN: &str = "seen.json";
const MARKS: &str = "marks.json";
const WORK: &str = "work.json";
/// Bytes an append cut off the journal's end, set aside as
/// `journal.cut-<epoch ms>` beside it.
const CUT_PREFIX: &str = "journal.cut-";

/// The normalized event a native hook event maps to - the event vocabulary
/// the shared projection uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormEvent {
    /// A new turn or prompt.
    Start,
    /// Work continuing inside a turn.
    Activity,
    /// Blocked on the human.
    Awaiting,
    /// A clean turn end.
    End,
    /// The turn aborted.
    Error,
    /// Session teardown is coming; never reaps on its own.
    TeardownHint,
}

impl NormEvent {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            NormEvent::Start => "start",
            NormEvent::Activity => "activity",
            NormEvent::Awaiting => "awaiting",
            NormEvent::End => "end",
            NormEvent::Error => "error",
            NormEvent::TeardownHint => "teardown_hint",
        }
    }

    /// The execution class the event claims. `TeardownHint` is none at
    /// all: it is diagnostic, not a state.
    pub fn execution(self) -> Option<Exec> {
        match self {
            NormEvent::Start | NormEvent::Activity => Some(Exec::Busy),
            NormEvent::Awaiting => Some(Exec::Waiting),
            NormEvent::End => Some(Exec::Idle),
            NormEvent::Error => Some(Exec::Unknown),
            NormEvent::TeardownHint => None,
        }
    }

    /// Whether the event is attention a human must acknowledge - the latch
    /// kind. `Start` and `Activity` acknowledge rather than latch.
    pub fn latches(self) -> bool {
        matches!(
            self,
            NormEvent::Awaiting | NormEvent::End | NormEvent::Error
        )
    }
}

/// Effective execution state, derived - not a provider's word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Exec {
    Busy,
    Idle,
    Waiting,
    /// No applicable evidence, or none that proves a live state.
    Unknown,
}

impl Exec {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Exec::Busy => "busy",
            Exec::Idle => "idle",
            Exec::Waiting => "waiting",
            Exec::Unknown => "unknown",
        }
    }
}

/// One committed journal record: one hook ping. Field names stay short -
/// every record carries them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    /// Schema version; absent is the pre-versioned schema and still reads.
    #[serde(default)]
    pub v: u32,
    /// The journal commit sequence: the local, monotonic order.
    #[serde(default)]
    pub seq: u64,
    /// When this process committed it, epoch milliseconds.
    #[serde(default)]
    pub at: u64,
    /// Who wrote it (`agent-sessions/<version>`), for the evidence view.
    #[serde(default)]
    pub writer: String,
    /// The provider's wire name (`claude`, `vibe`, `devin`).
    #[serde(default)]
    pub provider: String,
    /// The provider's own session id. May be empty when the payload carried
    /// none: the record is kept but reduces onto no conversation.
    #[serde(default)]
    pub session: String,
    /// The provider's native event name, verbatim.
    #[serde(default)]
    pub native: String,
    /// The normalized event; `None` on an unmapped ping, which is retained
    /// as diagnostic activity with a five-second weak `Busy` lease.
    #[serde(default)]
    pub event: Option<NormEvent>,
    /// The producer's own timestamp, epoch milliseconds.
    #[serde(default)]
    pub pts: Option<u64>,
    /// The producer's own sequence; a lower one than already seen rejects
    /// the record as reordered.
    #[serde(default)]
    pub pseq: Option<u64>,
    /// The process instance the hook resolved, when it could.
    #[serde(default)]
    pub pid: Option<u32>,
    /// The instance's start, epoch seconds.
    #[serde(default)]
    pub pid_start: Option<u64>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The wait reason an `awaiting` carries, or the native name an
    /// `error` came in as - evidence-view material.
    #[serde(default)]
    pub reason: Option<String>,
}

impl Record {
    /// A record as `hook` builds it: sequence and commit time are the
    /// store's to assign.
    pub fn new(provider: &str, session: &str, native: &str) -> Record {
        Record {
            v: SCHEMA,
            seq: 0,
            at: 0,
            writer: writer(),
            provider: provider.to_owned(),
            session: session.to_owned(),
            native: native.to_owned(),
            event: None,
            pts: None,
            pseq: None,
            pid: None,
            pid_start: None,
            cwd: None,
            reason: None,
        }
    }
}

/// The newest mapped event on a conversation: the lifecycle claim hooks
/// make. Process identity rides along so a delayed event from a demoted
/// attachment cannot mutate the replacement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastEvent {
    pub kind: NormEvent,
    /// The journal commit sequence that produced it.
    pub seq: u64,
    /// When the effective state it implies began; duplicates and
    /// same-class continuations preserve it rather than reset it.
    pub since_ms: u64,
    /// When this event itself was observed (producer time, else commit).
    pub observed_ms: u64,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub pid_start: Option<u64>,
}

/// A latched attention event: an `awaiting`, `end` or `error` a `start`
/// has not implicitly acknowledged. Seen-state acknowledgement applies at
/// derive time, not here - the journal never rewrites.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Retained {
    pub seq: u64,
    pub kind: NormEvent,
    pub at_ms: u64,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The reduced state of one conversation's journal records - what a
/// checkpoint persists so the whole log never needs replaying.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Fold {
    /// Newest committed record on the conversation.
    #[serde(default)]
    pub last_seq: u64,
    /// Newest mapped event, the lifecycle claim.
    #[serde(default)]
    pub last_event: Option<LastEvent>,
    /// Attention events no `start` has acknowledged. Compaction drops the
    /// ones seen-state acknowledged, so a provider without a `start` event
    /// does not carry every turn's latch forever.
    #[serde(default)]
    pub retained: Vec<Retained>,
    /// Newest unmapped ping `(seq, epoch ms)` - the weak `Busy` lease.
    #[serde(default)]
    pub ping: Option<(u64, u64)>,
    /// The producer-sequence high-water mark; older rejects as reordered.
    #[serde(default)]
    pub pseq_high: Option<u64>,
    /// The newest producer-supplied timestamp (`pts`) any folded record
    /// carried, epoch milliseconds - conversation activity as the hook
    /// producer dated it, never the journal's commit time.
    #[serde(default)]
    pub last_pts: Option<u64>,
}

/// How one record landed in a fold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// New information, folded in.
    Accepted,
    /// An already-seen producer sequence: the observation refreshes its
    /// time but the effective `since` stands.
    Duplicate,
    /// A producer sequence below the high-water mark: reordered source
    /// evidence, rejected.
    Stale,
}

impl Fold {
    /// Fold one committed record into the conversation's reduction.
    /// Deterministic in commit order: two writers cannot overwrite one
    /// another, and replaying the same sequence yields the same fold.
    pub fn apply(&mut self, record: &Record) -> Apply {
        // Producer sequences reject reordering when the source supplies
        // them; an equal sequence is a duplicate heartbeat, not new
        // evidence.
        if let Some(pseq) = record.pseq {
            match self.pseq_high {
                Some(high) if pseq < high => return Apply::Stale,
                Some(high) if pseq == high => {
                    self.refresh_observed(record);
                    return Apply::Duplicate;
                }
                _ => self.pseq_high = Some(pseq),
            }
        }
        self.last_seq = self.last_seq.max(record.seq);
        if let Some(pts) = record.pts {
            self.last_pts = Some(self.last_pts.map_or(pts, |p| p.max(pts)));
        }
        let observed = record.pts.unwrap_or(record.at);
        let Some(kind) = record.event else {
            self.ping = Some((record.seq, observed));
            return Apply::Accepted;
        };
        if kind == NormEvent::TeardownHint {
            // Diagnostic only: it never reaps and never claims execution.
            return Apply::Accepted;
        }
        if kind == NormEvent::Start {
            // A new prompt acknowledges everything retained: answering
            // implies the attention was seen.
            self.retained.clear();
        }
        if kind.latches() {
            self.retained.push(Retained {
                seq: record.seq,
                kind,
                at_ms: observed,
                reason: record
                    .reason
                    .clone()
                    .or_else(|| Some(record.native.clone())),
            });
        }
        // `since` survives inside one execution class: an `activity`
        // heartbeat continues the `start`'s Busy rather than restarting
        // the clock.
        let execution = kind.execution();
        let since = match (&self.last_event, execution) {
            (Some(prev), Some(exec)) if prev.kind.execution() == Some(exec) => prev.since_ms,
            _ => observed,
        };
        self.last_event = Some(LastEvent {
            kind,
            seq: record.seq,
            since_ms: since,
            observed_ms: observed,
            reason: record.reason.clone(),
            pid: record.pid,
            pid_start: record.pid_start,
        });
        Apply::Accepted
    }

    /// A duplicate heartbeat refreshes the observation time of whatever it
    /// duplicates - the lease's expiry moves, the `since` does not.
    fn refresh_observed(&mut self, record: &Record) {
        if let Some(pts) = record.pts {
            self.last_pts = Some(self.last_pts.map_or(pts, |p| p.max(pts)));
        }
        let observed = record.pts.unwrap_or(record.at);
        if let Some(event) = &mut self.last_event
            && (event.seq == record.seq
                || event.kind.execution() == record.event.and_then(|e| e.execution()))
        {
            event.observed_ms = event.observed_ms.max(observed);
        }
        if let Some((_, at)) = &mut self.ping
            && record.event.is_none()
        {
            *at = (*at).max(observed);
        }
        self.last_seq = self.last_seq.max(record.seq);
    }
}

/// One conversation's acknowledgement: the retained events seen through
/// a commit sequence, and the live wait episode seen - named by its
/// `effective_since`, since a wait the provider publishes carries no
/// sequence. A newer wait episode starts unseen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seen {
    /// Retained events at or below this commit sequence are seen.
    #[serde(default)]
    pub seq: u64,
    /// The `effective_since` of the newest live wait the user has seen,
    /// epoch ms; any wait that began at or before it is seen.
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

/// An authored not-busy mark: it names the `effective_since` of the `Busy`
/// it dismisses and the commit sequence at write time, so any newer event
/// or observation supersedes it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Mark {
    /// The dismissed `Busy`'s `effective_since`, epoch milliseconds.
    pub since_ms: u64,
    /// The journal commit sequence when the mark was written.
    pub seq: u64,
    /// When the mark was written, epoch milliseconds.
    pub at_ms: u64,
}

/// The observable lifecycle facts whose *change* is a proven transition:
/// dirty flag and tree shape, delivery evidence, forge state. Persisted
/// as the record's last reading so a restart does not re-report an
/// unchanged state, and a changed input lands an observation at its
/// detection time - diagnostics, never activity. An optional field is
/// `None` when unproven: it keeps the last proven value and observes
/// nothing, so an offline pass, a forge outage or a timed out probe is
/// never mistaken for a transition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleInputs {
    /// The worktree's dirty flag; `None` when unproven or not applicable.
    #[serde(default)]
    pub dirty: Option<bool>,
    /// Whether the anchor's checkout exists at all this pass: `Some` when
    /// the reader is the repository's own worktree inventory - `true` for
    /// a listed worktree, `false` for a branch with none - and `None` for
    /// a project-space sync, which cannot tell whether an uncollected
    /// repository still registers the path, so it never revokes a proven
    /// worktree.
    #[serde(default)]
    pub worktree: Option<bool>,
    /// Whether the worktree's `.git` exists this pass; `None` without a
    /// worktree anchor. A registered worktree whose `.git` file vanished
    /// stays a worktree - `git worktree list` still lists it, prunable -
    /// so presence and linkage are separate facts.
    #[serde(default)]
    pub git_dir: Option<bool>,
    /// The checkout path, when one exists - a move is a tree transition.
    #[serde(default)]
    pub worktree_path: Option<String>,
    /// The worktree's tmux-side admin id (`@wt_adminid`), when its anchor
    /// carried one - the edge a window's stored binding still names after
    /// the worktree itself is gone.
    #[serde(default)]
    pub admin_id: Option<String>,
    /// Commits on the tip not on the proven base.
    #[serde(default)]
    pub ahead: Option<u64>,
    /// Commits the configured upstream does not have.
    #[serde(default)]
    pub unpushed: Option<u64>,
    /// The proven upstream state's wire spelling plus its detail.
    #[serde(default)]
    pub upstream: Option<String>,
    /// The landed verdict's wire spelling.
    #[serde(default)]
    pub landed: Option<String>,
    /// The known forge work-item state's wire spelling.
    #[serde(default)]
    pub forge: Option<String>,
    /// The open item's pipeline state.
    #[serde(default)]
    pub pipeline: Option<String>,
    #[serde(default)]
    pub behind: Option<u64>,
    #[serde(default)]
    pub worktree_state: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub commit: Option<ObservedCommit>,
    #[serde(default)]
    pub working_tree: Option<WorkingTreeSnapshot>,
}

/// How many events one history keeps per source: enough that a busy
/// week's diagnostics survive, bounded so a churning worktree cannot
/// grow the file without limit.
pub const HISTORY_RETAIN_PER_SOURCE: usize = 100;

/// Where an [`ActivityEvent`]'s occurrence time came from: the source
/// that dated the work itself, never the scan that noticed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    /// A commit's committer time - a HEAD move whose stored metadata
    /// proves the tip's date, or the tip's `%(committerdate)`.
    Commit,
    /// A dated commit, merge, reset, rebase or amend reflog entry - real
    /// repository interaction, never bookkeeping lines.
    Reflog,
    /// The newest mtime among the working tree's current changed paths.
    WorkingTree,
    /// A forge work item's merge or close date.
    Forge,
    /// A conversation's transcript message or hook producer time.
    Conversation,
}

/// A source-backed work event: when the work *happened*, as the source
/// itself dated it. Occurrence times backfill freely - a first
/// collection may record work from long before it ran - but no missing
/// source timestamp ever falls back to scan time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub source: ActivitySource,
    /// When the work occurred, epoch milliseconds.
    pub occurred_at_ms: u64,
    pub reasons: Vec<String>,
}

/// A pointer from an [`ObservationEvent`] to the [`ActivityEvent`] the
/// same transition emitted: the source and occurrence identify the
/// counterpart, never a serial position, so a merged or retained event
/// still matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityReference {
    pub source: ActivitySource,
    /// The counterpart's occurrence, epoch milliseconds.
    pub occurred_at_ms: u64,
}

/// Where an [`ObservationEvent`]'s detection time came from: the pass
/// that learned a fact or proved a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    /// A HEAD move whose stored commit metadata could not date it.
    Commit,
    /// A changed working-tree fingerprint.
    WorkingTree,
    /// A forge state or pipeline transition.
    Forge,
    /// A conversation's discovery or placement.
    Conversation,
    /// A proven lifecycle-input transition: presence flips, moves, and
    /// every changed delivery or work-item reading.
    Lifecycle,
}

/// A scan-time diagnostic event: when the collector *learned* something,
/// not when it happened. Observations never count as activity - a recent
/// detection cannot make old work look new.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationEvent {
    pub source: ObservationSource,
    /// When the pass observed the fact, epoch milliseconds.
    pub observed_at_ms: u64,
    pub reasons: Vec<String>,
    /// The activity event this detection duplicates: set only by the
    /// transition that emitted both, never matched retroactively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covered_by: Option<ActivityReference>,
}

impl ObservationEvent {
    /// Whether the event's linked activity still covers it: the
    /// reference must name a WorkingTree observation's own counterpart -
    /// same source and occurrence - and that retained event must carry
    /// every observation reason. An unlinked event, a pruned counterpart
    /// and a partial match all keep the observation visible.
    pub fn is_covered_by(&self, activities: &[ActivityEvent]) -> bool {
        if self.source != ObservationSource::WorkingTree {
            return false;
        }
        let Some(reference) = self.covered_by else {
            return false;
        };
        if reference.source != ActivitySource::WorkingTree {
            return false;
        }
        activities.iter().any(|event| {
            event.source == reference.source
                && event.occurred_at_ms == reference.occurred_at_ms
                && self.reasons.iter().all(|r| event.reasons.contains(r))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedCommit {
    pub sha: String,
    pub subject: Option<String>,
    pub at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingTreeSnapshot {
    pub fingerprint: String,
    pub reasons: Vec<String>,
    /// The newest mtime among the changed paths' successfully read
    /// metadata, epoch milliseconds - the occurrence time a changed
    /// fingerprint may carry into an activity event. `None` when no
    /// changed path's metadata read (a deleted or clean tree), which is
    /// never proxied through scan time.
    #[serde(default)]
    pub newest_mtime_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum UpdateIdentity {
    Branch(String),
    Path(String),
}

/// The latest-known conversation context one record captured: the
/// provider title and a bounded excerpt of the latest submitted prompt -
/// context for the summary row, never a claim that the newest activity
/// occurrence dates the prompt, and never a full transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContext {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub prompt_excerpt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionUpdate {
    pub identity: UpdateIdentity,
    pub conversation: String,
    pub at_ms: u64,
    pub reason: String,
    /// The raw context the pass read; the store normalizes and bounds it
    /// before persisting so a caller cannot bypass the bound.
    pub context: SessionContext,
}

impl LifecycleInputs {
    /// This reading laid over `prior`: every unproven field keeps the
    /// prior proven value. The checkout's presence is proven only by the
    /// repository's own inventory; its path and admin id keep their last
    /// proven value, so a record outliving its workspace still names
    /// where the work was.
    fn over(&self, prior: &LifecycleInputs) -> LifecycleInputs {
        LifecycleInputs {
            dirty: self.dirty.or(prior.dirty),
            worktree: self.worktree.or(prior.worktree),
            git_dir: self.git_dir.or(prior.git_dir),
            worktree_path: self
                .worktree_path
                .clone()
                .or_else(|| prior.worktree_path.clone()),
            admin_id: self.admin_id.clone().or_else(|| prior.admin_id.clone()),
            ahead: self.ahead.or(prior.ahead),
            unpushed: self.unpushed.or(prior.unpushed),
            upstream: self.upstream.clone().or_else(|| prior.upstream.clone()),
            landed: self.landed.clone().or_else(|| prior.landed.clone()),
            forge: self.forge.clone().or_else(|| prior.forge.clone()),
            pipeline: self.pipeline.clone().or_else(|| prior.pipeline.clone()),
            behind: self.behind.or(prior.behind),
            worktree_state: self
                .worktree_state
                .clone()
                .or_else(|| prior.worktree_state.clone()),
            head: self.head.clone().or_else(|| prior.head.clone()),
            commit: self.commit.clone().or_else(|| prior.commit.clone()),
            working_tree: self
                .working_tree
                .clone()
                .or_else(|| prior.working_tree.clone()),
        }
    }

    /// Whether this reading is a lifecycle transition from `prior`: a
    /// field proven on both sides changed - the checkout appeared,
    /// vanished or moved, a probe's answer moved. A field's first proven
    /// value is adopted without an event or a date; only proven-to-
    /// proven transitions emit, at their observation time.
    fn transitions_from(&self, prior: &LifecycleInputs) -> Vec<String> {
        fn changed<T: PartialEq>(new: &Option<T>, old: &Option<T>) -> bool {
            matches!((new, old), (Some(n), Some(o)) if n != o)
        }
        fn opt<T: fmt::Display>(v: &Option<T>) -> String {
            v.as_ref()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".to_owned()) // coverage: off - a changed() pair is always Some
        }
        // Presence changes are events only when both sides were proven.
        // A first proven value establishes the baseline without pretending
        // that the state began when this pass observed it.
        let mut reasons = Vec::new();
        match (prior.worktree, self.worktree) {
            (Some(true), Some(false)) => reasons.push("worktree gone".to_owned()),
            (Some(false), Some(true)) => reasons.push("worktree found".to_owned()),
            _ => {}
        }
        match (prior.git_dir, self.git_dir) {
            (Some(false), Some(true)) => reasons.push(".git restored".to_owned()),
            (Some(true), Some(false)) => reasons.push(".git missing".to_owned()),
            _ => {}
        }
        if changed(&self.worktree_path, &prior.worktree_path) {
            reasons.push(format!(
                "path: {} -> {}",
                opt(&prior.worktree_path),
                opt(&self.worktree_path)
            ));
        }
        if changed(&self.admin_id, &prior.admin_id) {
            reasons.push(format!(
                "admin id: {} -> {}",
                opt(&prior.admin_id),
                opt(&self.admin_id)
            ));
        }
        if self.working_tree.is_none()
            && prior.working_tree.is_none()
            && changed(&self.dirty, &prior.dirty)
        {
            let word = |dirty: Option<bool>| {
                if dirty == Some(true) {
                    "dirty"
                } else {
                    "clean"
                }
            };
            reasons.push(format!(
                "dirty: {} -> {}",
                word(prior.dirty),
                word(self.dirty)
            ));
        }
        if changed(&self.ahead, &prior.ahead) {
            reasons.push(format!(
                "ahead: {} -> {}",
                opt(&prior.ahead),
                opt(&self.ahead)
            ));
        }
        if changed(&self.behind, &prior.behind) {
            reasons.push(format!(
                "behind: {} -> {}",
                opt(&prior.behind),
                opt(&self.behind)
            ));
        }
        if changed(&self.unpushed, &prior.unpushed) {
            reasons.push(format!(
                "unpushed: {} -> {}",
                opt(&prior.unpushed),
                opt(&self.unpushed)
            ));
        }
        if changed(&self.upstream, &prior.upstream) {
            reasons.push(format!(
                "upstream: {} -> {}",
                opt(&prior.upstream),
                opt(&self.upstream)
            ));
        }
        if changed(&self.landed, &prior.landed) {
            reasons.push(format!(
                "landed: {} -> {}",
                opt(&prior.landed),
                opt(&self.landed)
            ));
        }
        if changed(&self.forge, &prior.forge) {
            reasons.push(format!(
                "forge: {} -> {}",
                opt(&prior.forge),
                opt(&self.forge)
            ));
        }
        if changed(&self.pipeline, &prior.pipeline) {
            reasons.push(format!(
                "pipeline: {} -> {}",
                opt(&prior.pipeline),
                opt(&self.pipeline)
            ));
        }
        if changed(&self.worktree_state, &prior.worktree_state) {
            reasons.push(format!(
                "worktree state: {} -> {}",
                opt(&prior.worktree_state),
                opt(&self.worktree_state)
            ));
        }
        reasons
    }
}

/// A ref's creation as its reflog's newest null-old entry proves it: the
/// head the ref came to be at, and when, in epoch milliseconds. Two
/// incarnations sharing the name never share this evidence - a deleted
/// ref's log dies with it and a recreated ref starts a fresh null-old
/// line, so unequal creations are a proven boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefCreationEvidence {
    /// The tip the ref was created at.
    pub head: String,
    /// When the ref was created, epoch milliseconds.
    pub at_ms: u64,
}

/// Which evidence last established - or separated - an incarnation's
/// identity. `ProvenRename` and `Ambiguous` are terminal for the record:
/// they say how it began under this name, and later confirmations do not
/// rewrite that. The rest track the latest continuity decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuityEvidence {
    /// First sighting: nothing prior had to be reconciled.
    #[default]
    FirstObservation,
    /// The persisted creation evidence matched the ref's reflog.
    SameReflogCreation,
    /// A `Branch: renamed` reflog line moved the record to this name.
    ProvenRename,
    /// The tip moved while the creation evidence held.
    ForcePush,
    /// A boundary was detected but no evidence could prove which side of
    /// it the history belongs to: the record was separated rather than
    /// merged.
    Ambiguous,
}

impl ContinuityEvidence {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ContinuityEvidence::FirstObservation => "first_observation",
            ContinuityEvidence::SameReflogCreation => "same_reflog_creation",
            ContinuityEvidence::ProvenRename => "proven_rename",
            ContinuityEvidence::ForcePush => "force_push",
            ContinuityEvidence::Ambiguous => "ambiguous",
        }
    }
}

/// What a touch placement was derived from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchProvenance {
    /// The conversation's cwd resolved inside a checkout of the branch.
    Cwd,
    /// The provider declared the branch itself.
    ProviderBranch,
}

impl TouchProvenance {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            TouchProvenance::Cwd => "cwd",
            TouchProvenance::ProviderBranch => "provider_branch",
        }
    }
}

/// How exact a touch's placement claim is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Proven: the placement carries no inference.
    Exact,
}

impl Confidence {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Exact => "exact", // coverage: off - the only variant
        }
    }
}

/// One interval of a conversation's placement on a branch incarnation:
/// append-only history - a correction never rewrites an interval, it
/// closes it and opens the next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchTouch {
    /// `conversation_key(provider, session)`.
    pub conversation: String,
    /// The incarnation's `BranchRecord.id`.
    pub branch: String,
    /// The ref's tip the placement observed; `None` where the evidence
    /// names a branch but not its tip (a provider's dated record).
    #[serde(default)]
    pub head: Option<String>,
    /// Interval start, epoch milliseconds.
    pub valid_from: u64,
    /// Interval end, epoch ms; `None` while current.
    #[serde(default)]
    pub valid_until: Option<u64>,
    pub provenance: TouchProvenance,
    pub confidence: Confidence,
}

/// One conversation's exact placement on an incarnation, as a pass
/// observed it: the open interval [`Store::sync_touches`] reconciles
/// against.
#[derive(Debug, Clone)]
pub struct TouchPlacement {
    /// `conversation_key(provider, session)`.
    pub conversation: String,
    /// The active incarnation's `BranchRecord.id`.
    pub branch: String,
    /// The ref's tip at the observation.
    pub head: String,
    /// What the placement was derived from.
    pub provenance: TouchProvenance,
    /// How exact the placement is; only `Exact` exists today.
    pub confidence: Confidence,
}

/// One interval a provider's own dated records place a conversation on
/// an incarnation: `ProviderBranch` provenance, exact, with no tip - the
/// records name the branch, not its head. Re-derived from the same
/// append-only records every pass, so it is keyed by `(conversation,
/// branch, valid_from)`.
#[derive(Debug, Clone)]
pub struct DatedTouch {
    /// `conversation_key(provider, session)`.
    pub conversation: String,
    /// The incarnation's `BranchRecord.id`.
    pub branch: String,
    /// Interval start, epoch milliseconds.
    pub valid_from: u64,
    /// Interval end, epoch ms; `None` while the records' last mark holds.
    pub valid_until: Option<u64>,
}

/// One local ref a pass observed in a repository, with the facts the
/// record fingerprints and the lifecycle evidence continuity judges by.
#[derive(Debug, Clone)]
pub struct ObservedRef {
    /// The short branch name (`refs/heads/<name>`).
    pub name: String,
    /// The ref's tip OID this pass, when the collector proved it.
    pub head: Option<String>,
    /// The collector proved the tip moved off the record's last `head`
    /// without descending from it - a rewrite such as a rebase or reset
    /// that a force-push publishes. A fast-forward or an unprovable
    /// comparison is `false`.
    pub rewritten: bool,
    /// The newest null-old reflog entry's creation evidence.
    pub creation: Option<RefCreationEvidence>,
    /// The short name a `Branch: renamed` reflog line moved this ref
    /// from - the exact evidence that preserves identity across names.
    pub renamed_from: Option<String>,
    pub commit: Option<ObservedCommit>,
    /// The source-backed activity the pass can prove for this ref,
    /// deduplicated into the record's history - first collection may
    /// therefore backfill old occurrence times, but never promotes its
    /// own scan time into one.
    pub activities: Vec<ActivityEvent>,
    pub inputs: LifecycleInputs,
}

/// One branch incarnation: a single observed lifetime of `ref_name` inside
/// `repo`. A deleted ref closes the record (`ended_at`); a ref that
/// reappears opens a fresh record with a new `id` and `parked: false` - a
/// name is a label, not an identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRecord {
    /// Opaque stable identity of this incarnation.
    pub id: String,
    /// The canonical repository id (`$GIT_COMMON_DIR`).
    pub repo: String,
    /// The short ref name.
    pub ref_name: String,
    /// The first pass that observed the ref, epoch milliseconds.
    pub first_observed_at: u64,
    /// The last pass that observed the ref, epoch milliseconds. Every
    /// observation moves it, but a pass that changes nothing else writes
    /// nothing: an active record's stored value lags by the quiet passes
    /// since, and the snapshot reports its observation from the running
    /// collector instead. Closing stamps the pass before the close. Records
    /// written before this field existed carry `0` until their next write.
    #[serde(default)]
    pub last_observed_at: u64,
    /// The ref's tip OID as last proven - what the next observation's tip
    /// move is judged against (fast-forward or rewritten).
    #[serde(default)]
    pub head: Option<String>,
    /// The ref's creation evidence as last proven - what continuity is
    /// judged against.
    #[serde(default)]
    pub creation_evidence: Option<RefCreationEvidence>,
    /// The evidence that last established or separated this identity.
    #[serde(default)]
    pub continuity_evidence: ContinuityEvidence,
    /// When the ref was observed gone, epoch ms; `None` while active.
    #[serde(default)]
    pub ended_at: Option<u64>,
    /// The authored parked flag; suppresses only the `Forgotten` section.
    #[serde(default)]
    pub parked: bool,
    /// Source-backed work events, newest kept at
    /// [`HISTORY_RETAIN_PER_SOURCE`] per source. Their newest occurrence
    /// is the record's activity - derived, never persisted separately.
    #[serde(default)]
    pub activities: Vec<ActivityEvent>,
    /// Scan-time diagnostics: what the collector learned and when, kept
    /// independently of activity so a detection can never pass for work.
    #[serde(default)]
    pub observations: Vec<ObservationEvent>,
    /// The inputs transitions are judged against.
    #[serde(default)]
    pub inputs: LifecycleInputs,
    #[serde(default)]
    pub session_activity: BTreeMap<String, u64>,
    /// Latest-known context per `session_activity` key, captured for this
    /// record only - a conversation's later context never leaks into a
    /// record it moved away from.
    #[serde(default)]
    pub session_context: BTreeMap<String, SessionContext>,
}

/// A path-anchored record: a detached worktree or a non-Git project space,
/// keyed by its canonical path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathRecord {
    /// The repository the path belongs to - a detached worktree's repo,
    /// a project space's own path - so a row outliving the directory
    /// still lists under its repo. `None` on a record written before the
    /// field existed, until the next sync.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub parked: bool,
    #[serde(default)]
    pub activities: Vec<ActivityEvent>,
    #[serde(default)]
    pub observations: Vec<ObservationEvent>,
    #[serde(default)]
    pub inputs: LifecycleInputs,
    #[serde(default)]
    pub session_activity: BTreeMap<String, u64>,
    /// Latest-known context per `session_activity` key; see
    /// [`BranchRecord::session_context`].
    #[serde(default)]
    pub session_context: BTreeMap<String, SessionContext>,
}

/// The `work.json` payload: the authored Work state.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Work {
    /// Incarnation id -> the record, active or closed. Closed records are
    /// retained: they are the incarnation history a later pass numbers
    /// and reconciles against.
    #[serde(default)]
    pub branches: std::collections::BTreeMap<String, BranchRecord>,
    /// `branch_key(repo, ref_name)` -> the active incarnation's id.
    #[serde(default)]
    pub active_branches: std::collections::BTreeMap<String, String>,
    /// Canonical path -> the detached worktree or project space record.
    #[serde(default)]
    pub paths: std::collections::BTreeMap<String, PathRecord>,
    /// Every touch interval ever recorded, append-only: the conversation
    /// placements each pass proved, in the order they landed.
    #[serde(default)]
    pub touches: Vec<BranchTouch>,
}

impl Work {
    /// The active incarnation record for `(repo, ref_name)`, if one is.
    /// A closed record answers `None`: a reappeared ref gets its identity
    /// from the record a sync creates, never from the closed past.
    pub fn branch(&self, repo: &str, ref_name: &str) -> Option<&BranchRecord> {
        let id = self.active_branches.get(&branch_key(repo, ref_name))?;
        self.branches.get(id).filter(|r| r.ended_at.is_none())
    }

    /// The path record for `path`, if one exists.
    pub fn path(&self, path: &str) -> Option<&PathRecord> {
        self.paths.get(path)
    }
}

/// What [`Store::work_stamp`] compares: a `work.json` read is current
/// while its stamp is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkStamp {
    ino: u64,
    len: u64,
    modified: Option<SystemTime>,
}

/// The exact identity `p` toggles `parked` on: a branch incarnation's id,
/// or a canonical path for rows with no ref.
#[derive(Debug)]
pub enum WorkIdentity {
    /// `BranchRecord.id` of an active incarnation.
    Branch(String),
    /// Canonical path of a detached worktree or project space.
    Path(String),
}

/// The `branches` map key: repo and ref joined like `conversation_key`.
fn branch_key(repo: &str, ref_name: &str) -> String {
    format!("{repo}\u{0}{ref_name}")
}

/// An opaque incarnation id: unique per creation without a central
/// counter surviving across processes - observation time, process and a
/// per-process sequence name it.
fn incarnation_id(repo: &str, ref_name: &str, at_ms: u64) -> String {
    use std::hash::{Hash, Hasher};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (
        repo,
        ref_name,
        at_ms,
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    )
        .hash(&mut h);
    format!("i{:016x}", h.finish())
}

/// How long a closed incarnation record is kept: ninety days, or as long
/// as a touch references it - whichever outlives the other.
const RETAIN_CLOSED_MS: u64 = 90 * 24 * 3600 * 1000;

/// Whether the observation proves the still-present ref is not the
/// recorded incarnation, and how the new record labels the split: a
/// creation strictly newer than the stored one is a proven recreate
/// (`FirstObservation`), anything else detectable - an equal-time or
/// older creation the record cannot explain, or a creation dated after
/// the record's first observation while the record carried no evidence
/// at all - is a boundary without a proven side, `Ambiguous`. `None`
/// means no boundary is detectable: the observation continues the record.
fn creation_boundary(record: &BranchRecord, obs: &ObservedRef) -> Option<ContinuityEvidence> {
    match (&record.creation_evidence, &obs.creation) {
        (Some(have), Some(seen)) if have != seen => Some(if seen.at_ms > have.at_ms {
            ContinuityEvidence::FirstObservation
        } else {
            ContinuityEvidence::Ambiguous
        }),
        // The record was made before creation evidence existed, and the
        // observed creation postdates it: the ref it recorded is gone.
        (None, Some(seen)) if seen.at_ms > record.first_observed_at => {
            Some(ContinuityEvidence::FirstObservation)
        }
        _ => None,
    }
}

/// The continuity a confirmed-same record carries: a proven
/// non-fast-forward tip move under unchanged creation evidence is
/// `force_push`, which then holds for the record; otherwise matching
/// creation evidence is `same_reflog_creation`, however far the tip
/// fast-forwarded. `None` keeps the record's label: how it began
/// (`proven_rename`, `ambiguous`) is never rewritten, and without
/// creation evidence there is nothing to confirm.
fn continuity_of(record: &BranchRecord, obs: &ObservedRef) -> Option<ContinuityEvidence> {
    match record.continuity_evidence {
        ContinuityEvidence::ProvenRename | ContinuityEvidence::Ambiguous => None,
        _ if obs.rewritten => Some(ContinuityEvidence::ForcePush),
        ContinuityEvidence::ForcePush => None,
        _ => record
            .creation_evidence
            .as_ref()
            .map(|_| ContinuityEvidence::SameReflogCreation),
    }
}

/// Open a new incarnation for `obs` at `observed_ms`: new id, `parked:
/// false`, the observation's evidence, fingerprint and provable
/// activities stored - first collection backfills source occurrence
/// times without ever dating the baseline itself. Returns the new
/// record's id.
fn open_incarnation(
    work: &mut Work,
    repo: &str,
    obs: &ObservedRef,
    observed_ms: u64,
    continuity: ContinuityEvidence,
) -> String {
    let id = incarnation_id(repo, &obs.name, observed_ms);
    work.branches.insert(
        id.clone(),
        BranchRecord {
            id: id.clone(),
            repo: repo.to_owned(),
            ref_name: obs.name.clone(),
            first_observed_at: observed_ms,
            last_observed_at: observed_ms,
            head: obs.head.clone(),
            creation_evidence: obs.creation.clone(),
            continuity_evidence: continuity,
            ended_at: None,
            parked: false,
            activities: Vec::new(),
            observations: Vec::new(),
            inputs: obs.inputs.clone(),
            session_activity: BTreeMap::new(),
            session_context: BTreeMap::new(),
        },
    );
    let record = work.branches.get_mut(&id).expect("just inserted");
    for event in &obs.activities {
        append_activity(&mut record.activities, event.clone());
    }
    work.active_branches
        .insert(branch_key(repo, &obs.name), id.clone());
    id
}

fn short_sha(sha: &str) -> &str {
    let end = sha
        .char_indices()
        .nth(7)
        .map(|(i, _)| i)
        .unwrap_or(sha.len());
    &sha[..end]
}

/// The record's newest source-backed occurrence, epoch milliseconds -
/// derived from the history every read, so no stored aggregate can
/// drift from it. `None` reads as `?` everywhere it surfaces.
pub fn newest_activity(activities: &[ActivityEvent]) -> Option<u64> {
    activities.iter().map(|e| e.occurred_at_ms).max()
}

const ACTIVITY_SOURCES: [ActivitySource; 5] = [
    ActivitySource::Commit,
    ActivitySource::Reflog,
    ActivitySource::WorkingTree,
    ActivitySource::Forge,
    ActivitySource::Conversation,
];

const OBSERVATION_SOURCES: [ObservationSource; 5] = [
    ObservationSource::Commit,
    ObservationSource::WorkingTree,
    ObservationSource::Forge,
    ObservationSource::Conversation,
    ObservationSource::Lifecycle,
];

/// Append one activity, deduplicated on `(source, occurred_at_ms)`:
/// a second event at the same occurrence merges its reasons rather
/// than listing twice - the transition path and the pass's own
/// evidence can date the same work independently. Returns whether the
/// history changed.
pub fn append_activity(activities: &mut Vec<ActivityEvent>, event: ActivityEvent) -> bool {
    match activities
        .iter_mut()
        .find(|e| e.source == event.source && e.occurred_at_ms == event.occurred_at_ms)
    {
        Some(existing) => {
            let mut reasons = event.reasons;
            reasons.retain(|r| !existing.reasons.contains(r));
            if reasons.is_empty() {
                return false;
            }
            existing.reasons.extend(reasons);
            existing.reasons.sort_unstable();
            existing.reasons.dedup();
            true
        }
        None => {
            activities.push(event);
            activities.sort_by(|a, b| {
                a.occurred_at_ms
                    .cmp(&b.occurred_at_ms)
                    .then(a.source.cmp(&b.source))
                    .then(a.reasons.cmp(&b.reasons))
            });
            for source in ACTIVITY_SOURCES {
                let mut drop = activities
                    .iter()
                    .filter(|e| e.source == source)
                    .count()
                    .saturating_sub(HISTORY_RETAIN_PER_SOURCE);
                activities.retain(|e| {
                    if e.source == source && drop > 0 {
                        drop -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
            true
        }
    }
}

/// Append one observation; an identical event (same source, time and
/// reasons) is a repeated detection, not a new one. Returns whether the
/// history changed.
fn append_observation(observations: &mut Vec<ObservationEvent>, event: ObservationEvent) -> bool {
    if observations.contains(&event) {
        return false;
    }
    observations.push(event);
    observations.sort_by(|a, b| {
        a.observed_at_ms
            .cmp(&b.observed_at_ms)
            .then(a.source.cmp(&b.source))
            .then(a.reasons.cmp(&b.reasons))
    });
    for source in OBSERVATION_SOURCES {
        let mut drop = observations
            .iter()
            .filter(|e| e.source == source)
            .count()
            .saturating_sub(HISTORY_RETAIN_PER_SOURCE);
        observations.retain(|e| {
            if e.source == source && drop > 0 {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }
    true
}

/// A proven HEAD move: a Commit activity when the stored commit metadata
/// dates the new tip, a Commit observation when it cannot - a HEAD move
/// whose occurrence no source proves is detection, not work.
fn commit_transition(
    activities: &mut Vec<ActivityEvent>,
    observations: &mut Vec<ObservationEvent>,
    prior: &Option<String>,
    head: &Option<String>,
    commit: &Option<ObservedCommit>,
    observed_ms: u64,
) -> bool {
    let (Some(new), Some(old)) = (head, prior) else {
        return false;
    };
    if new == old {
        return false;
    };
    let reason = match commit {
        Some(c) if &c.sha == new => match &c.subject {
            Some(subject) => format!("{} {}", short_sha(new), subject),
            None => short_sha(new).to_owned(),
        },
        _ => short_sha(new).to_owned(),
    };
    match commit {
        Some(c) if &c.sha == new && c.at_ms.is_some() => append_activity(
            activities,
            ActivityEvent {
                source: ActivitySource::Commit,
                occurred_at_ms: c.at_ms.expect("the guard proves it"),
                reasons: vec![reason],
            },
        ),
        _ => append_observation(
            observations,
            ObservationEvent {
                covered_by: None,
                source: ObservationSource::Commit,
                observed_at_ms: observed_ms,
                reasons: vec![reason],
            },
        ),
    }
}

/// A proven working-tree fingerprint change: always a WorkingTree
/// observation at detection time, plus a WorkingTree activity at the
/// snapshot's newest changed-path mtime when one exists. A deleted-file
/// or clean transition carries no remaining path's mtime, so it is
/// observation only.
fn working_tree_transition(
    activities: &mut Vec<ActivityEvent>,
    observations: &mut Vec<ObservationEvent>,
    prior: &Option<WorkingTreeSnapshot>,
    current: &Option<WorkingTreeSnapshot>,
    observed_ms: u64,
) -> bool {
    let (Some(new), Some(old)) = (current, prior) else {
        return false;
    };
    if new.fingerprint == old.fingerprint {
        return false;
    }
    // The same transition that emits source-backed activity links the
    // observation to it: the occurrence the snapshot's newest mtime
    // dates. A transition whose remaining changed paths prove no mtime
    // - a clean tree, a lone deletion, unreadable metadata - stays
    // unlinked: nothing covered it.
    let mut changed = append_observation(
        observations,
        ObservationEvent {
            covered_by: new.newest_mtime_ms.map(|at_ms| ActivityReference {
                source: ActivitySource::WorkingTree,
                occurred_at_ms: at_ms,
            }),
            source: ObservationSource::WorkingTree,
            observed_at_ms: observed_ms,
            reasons: new.reasons.clone(),
        },
    );
    if let Some(at_ms) = new.newest_mtime_ms {
        changed |= append_activity(
            activities,
            ActivityEvent {
                source: ActivitySource::WorkingTree,
                occurred_at_ms: at_ms,
                reasons: new.reasons.clone(),
            },
        );
    }
    changed
}

/// Everything a live record owes one pass: the pass's provable
/// activities, the transition evidence, then the merged inputs. The
/// continuing-record arm and the proven-rename arm run the same
/// sequence, so a move cannot defer a transition to the pass after it.
fn absorb_evidence(record: &mut BranchRecord, obs: &ObservedRef, observed_ms: u64) -> bool {
    let mut changed = false;
    for event in &obs.activities {
        changed |= append_activity(&mut record.activities, event.clone());
    }
    changed |= commit_transition(
        &mut record.activities,
        &mut record.observations,
        &record.head,
        &obs.head,
        &obs.commit,
        observed_ms,
    );
    if obs.head.is_some() && record.head != obs.head {
        record.head = obs.head.clone();
        changed = true;
    }
    changed |= working_tree_transition(
        &mut record.activities,
        &mut record.observations,
        &record.inputs.working_tree,
        &obs.inputs.working_tree,
        observed_ms,
    );
    // A changed proven fingerprint is a transition; first observation
    // never lands here. A newly proven field is stored without observing
    // anything.
    let reasons = obs.inputs.transitions_from(&record.inputs);
    if !reasons.is_empty() {
        changed |= append_observation(
            &mut record.observations,
            ObservationEvent {
                covered_by: None,
                source: ObservationSource::Lifecycle,
                observed_at_ms: observed_ms,
                reasons,
            },
        );
    }
    let inputs = obs.inputs.over(&record.inputs);
    if record.inputs != inputs {
        record.inputs = inputs;
        changed = true;
    }
    changed
}

/// The same evidence a path record owes one pass: the provable
/// activities, HEAD and working-tree transitions, lifecycle
/// observations, then the merged inputs.
fn absorb_path_evidence(
    record: &mut PathRecord,
    inputs: &LifecycleInputs,
    activities: &[ActivityEvent],
    observed_ms: u64,
) -> bool {
    let mut changed = false;
    for event in activities {
        changed |= append_activity(&mut record.activities, event.clone());
    }
    changed |= commit_transition(
        &mut record.activities,
        &mut record.observations,
        &record.inputs.head,
        &inputs.head,
        &inputs.commit,
        observed_ms,
    );
    changed |= working_tree_transition(
        &mut record.activities,
        &mut record.observations,
        &record.inputs.working_tree,
        &inputs.working_tree,
        observed_ms,
    );
    let reasons = inputs.transitions_from(&record.inputs);
    if !reasons.is_empty() {
        changed |= append_observation(
            &mut record.observations,
            ObservationEvent {
                covered_by: None,
                source: ObservationSource::Lifecycle,
                observed_at_ms: observed_ms,
                reasons,
            },
        );
    }
    let inputs = inputs.over(&record.inputs);
    if record.inputs != inputs {
        record.inputs = inputs;
        changed = true;
    }
    changed
}

/// `text` where it carries content, `None` where it is empty or
/// whitespace-only - text that renders nothing is not context.
fn context_text(text: Option<&str>) -> Option<String> {
    text.filter(|t| !t.trim().is_empty()).map(str::to_owned)
}

fn session_update(
    activities: &mut Vec<ActivityEvent>,
    cursors: &mut BTreeMap<String, u64>,
    contexts: &mut BTreeMap<String, SessionContext>,
    update: &SessionUpdate,
) -> bool {
    // The update's context, normalized and bounded at the store boundary:
    // an empty text is missing, a prompt persists only as a raw excerpt
    // whose escaped rendering fits the bound.
    let title = context_text(update.context.title.as_deref());
    let prompt = update
        .context
        .prompt_excerpt
        .as_deref()
        .and_then(text::prompt_excerpt);
    match cursors.get(&update.conversation).copied() {
        // An update before the cursor is stale: the source-backed event is
        // already superseded, so it changes nothing - context included.
        Some(cursor) if update.at_ms < cursor => false,
        // At the cursor the event already landed: enrich context only,
        // filling fields the record still lacks - never replacing what an
        // earlier update captured, never touching cursor or activity.
        Some(cursor) if update.at_ms == cursor => {
            let existing = contexts.get(&update.conversation);
            if existing.is_some_and(|c| c.title.is_some() && c.prompt_excerpt.is_some())
                || (title.is_none() && prompt.is_none())
            {
                return false;
            }
            let context = contexts.entry(update.conversation.clone()).or_default();
            let mut changed = false;
            if context.title.is_none() && title.is_some() {
                context.title = title;
                changed = true;
            }
            if context.prompt_excerpt.is_none() && prompt.is_some() {
                context.prompt_excerpt = prompt;
                changed = true;
            }
            changed
        }
        // First sightings backfill like newer turns do: the conversation's own
        // timestamp is real source history, whether it predates this record or
        // this store. The incoming context wins where it carries a value;
        // absent fields keep what the record already knows.
        _ => {
            cursors.insert(update.conversation.clone(), update.at_ms);
            append_activity(
                activities,
                ActivityEvent {
                    source: ActivitySource::Conversation,
                    occurred_at_ms: update.at_ms,
                    reasons: vec![update.reason.clone()],
                },
            );
            // The cursor moved, so the record changed even when the event
            // merged into an identical one another conversation already
            // supplied.
            merge_context(contexts, &update.conversation, title, prompt);
            true
        }
    }
}

/// `title`/`prompt` over the conversation's stored context: incoming
/// values replace, absent ones retain. Only writes when something
/// actually changed, and never fabricates an empty entry.
fn merge_context(
    contexts: &mut BTreeMap<String, SessionContext>,
    conversation: &str,
    title: Option<String>,
    prompt: Option<String>,
) {
    if title.is_none() && prompt.is_none() && !contexts.contains_key(conversation) {
        return;
    }
    let context = contexts.entry(conversation.to_owned()).or_default();
    if title.is_some() {
        context.title = title;
    }
    if prompt.is_some() {
        context.prompt_excerpt = prompt;
    }
}

/// Close every open touch interval naming `branch` at `at_ms`: an
/// incarnation that ended cannot keep current placements.
fn close_touches(work: &mut Work, branch: &str, at_ms: u64) -> bool {
    let mut closed = false;
    for touch in &mut work.touches {
        if touch.branch == branch && touch.valid_until.is_none() {
            touch.valid_until = Some(at_ms);
            closed = true;
        }
    }
    closed
}

/// The touch interval a placement opens.
fn touch_of(placement: &TouchPlacement, at_ms: u64) -> BranchTouch {
    BranchTouch {
        conversation: placement.conversation.clone(),
        branch: placement.branch.clone(),
        head: Some(placement.head.clone()),
        valid_from: at_ms,
        valid_until: None,
        provenance: placement.provenance,
        confidence: placement.confidence,
    }
}

/// The checkpoint file: the reduction at `through` commit sequence.
#[derive(Debug, Serialize, Deserialize)]
struct Checkpoint {
    #[serde(default)]
    v: u32,
    through: u64,
    #[serde(default)]
    folds: HashMap<String, Fold>,
    /// The newest rejected records per conversation, in journal order,
    /// so the evidence view keeps them past compaction.
    #[serde(default)]
    rejected: Vec<RejectedRecord>,
}

/// A committed journal record the fold rejected as stale or duplicate
/// producer evidence, plus the reason - kept for the evidence view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedRecord {
    /// `conversation_key(provider, session)`.
    pub conversation: String,
    /// The journal commit sequence the record carries.
    pub seq: u64,
    /// The record's own time (`pts`, else `at`), epoch ms.
    pub at_ms: u64,
    /// The producer sequence the record carried, when it carried one.
    pub pseq: Option<u64>,
    /// The provider's native event name.
    pub native: String,
    /// Why the fold rejected it.
    pub reason: String,
}

/// The whole store, read: per-conversation folds plus the authored files.
/// A conversation's fold key is `provider\0session_id`.
#[derive(Debug, Default)]
pub struct Loaded {
    pub folds: HashMap<String, Fold>,
    /// Conversation key -> what the user has acknowledged.
    pub seen: HashMap<String, Seen>,
    /// Conversation key -> the authored not-busy mark.
    pub marks: HashMap<String, Mark>,
    /// The authored Work state: incarnation records and parked flags.
    pub work: Work,
    /// Committed records the folds rejected while applying, in journal
    /// order - the stale and duplicate observations the evidence view
    /// shows next to what won.
    pub rejected: Vec<RejectedRecord>,
    /// Records excluded or files unreadable - isolated, never fatal.
    pub errors: Vec<SourceError>,
    /// The highest committed sequence the store knows.
    pub max_seq: u64,
    /// Whether acknowledgement can be established: the checkpoint history
    /// and `seen.json` both answered (absent is an answer). When `false`,
    /// attention is `Unknown` rather than a guessed glyph.
    pub ack_readable: bool,
    /// Whether the checkpoint is usable; absent counts as usable.
    /// Compaction rewrites it, so an unusable one defers compaction rather
    /// than overwrite bytes a newer schema may have written.
    checkpoint_ok: bool,
}

/// The journal tail's read: every parseable record in committed order
/// plus whether the file stayed clean. `compactable` is false the moment
/// any frame is excluded or malformed - a compaction rewrites the file to
/// nothing, and bytes the reduction cannot carry are data loss.
struct JournalRead {
    records: Vec<Record>,
    compactable: bool,
    /// Where an incomplete trailing frame begins - the end of the last
    /// whole frame - when the file ends in one.
    torn_at: Option<usize>,
}

/// The newest `REJECTED_KEPT` of each conversation's rejected records,
/// still in journal order - what a checkpoint carries forward.
fn newest_rejected(rejected: &[RejectedRecord]) -> Vec<RejectedRecord> {
    let mut left: HashMap<&str, usize> = HashMap::new();
    for r in rejected {
        *left.entry(r.conversation.as_str()).or_default() += 1;
    }
    rejected
        .iter()
        .filter(|r| {
            let n = left
                .get_mut(r.conversation.as_str())
                .expect("counted above");
            *n -= 1;
            *n < REJECTED_KEPT
        })
        .cloned()
        .collect()
}

/// The journal record key a conversation's events fold under.
pub fn conversation_key(provider: &str, session_id: &str) -> String {
    format!("{provider}\u{0}{session_id}")
}

/// A store rooted at `dir` (`$XDG_STATE_HOME/agent-sessions/`). Every
/// method tolerates a missing directory: a first run has no store.
#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: PathBuf) -> Store {
        Store { dir }
    }

    /// Append one record as the next commit: lock, sequence, write, fsync.
    /// Returns the assigned commit sequence.
    ///
    /// A previous append that died partway - out of space, or killed
    /// between its writes - leaves an incomplete frame at the end, and
    /// appending after it would misframe every later record. So the
    /// journal is cut back to the last whole frame first. A corrupt length
    /// header mid-file reads the same way with committed records behind
    /// it, so the cut bytes are set aside beside the journal rather than
    /// deleted, and every read reports them. A complete frame that does
    /// not parse keeps the framing intact and stays.
    pub fn append(&self, mut record: Record) -> io::Result<u64> {
        fs::create_dir_all(&self.dir)?;
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let journal = self.dir.join(JOURNAL);
        let tail = self.read_journal(&mut Vec::new());
        if let Some(end) = tail.torn_at {
            let bytes = fs::read(&journal)?; // coverage: off - the journal was just read, so it reads again
            let aside = self.dir.join(format!("{CUT_PREFIX}{}", now_ms()));
            write_atomic(&aside, &bytes[end..])?; // coverage: off - a write failure needs a filesystem fault
            let file = fs::OpenOptions::new().write(true).open(&journal)?; // coverage: off - the journal was just read, so it opens
            file.set_len(end as u64)?; // coverage: off - a truncate failure needs a filesystem fault
            file.sync_all()?; // coverage: off - an fsync failure needs a broken filesystem
        }
        let seq = self.next_seq(&tail);
        record.seq = seq;
        record.at = now_ms();
        record.v = SCHEMA;
        record.writer = writer();
        let bytes = serde_json::to_vec(&record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?; // coverage: off - a Record always serializes
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&journal)?;
        let len = (bytes.len() as u32).to_le_bytes();
        file.write_all(&len)?; // coverage: off - a write failure needs the filesystem to fail under an open handle
        file.write_all(&bytes)?; // coverage: off - same
        file.sync_all()?; // coverage: off - an fsync failure needs a broken filesystem
        Ok(seq)
    }

    /// The commit sequence one past the current tip: the checkpoint's
    /// `through` plus the journal tail's newest record.
    fn next_seq(&self, tail: &JournalRead) -> u64 {
        let checkpoint = self.read_checkpoint(&mut Vec::new());
        let through = checkpoint.map_or(0, |c| c.through);
        let tail = tail.records.iter().map(|r| r.seq).max().unwrap_or(0);
        through.max(tail) + 1
    }

    /// Read the store: checkpoint, then the journal tail folded on top.
    /// A tail larger than the compaction bound is folded into a fresh
    /// checkpoint on the spot - reaping and maintenance run on read, but
    /// only under the store's mutation lock, and only when the tail re-read
    /// under that lock still exceeds the bound on a journal that stayed
    /// clean: an append or another compaction committed between the two
    /// reads is never compacted over, and an excluded frame never is
    /// either. Best-effort: a failed compaction is reported and the
    /// unfolded tail simply answers again next time.
    pub fn load(&self) -> Loaded {
        let (loaded, tail, compactable) = self.load_once();
        if tail <= COMPACT_AFTER || !compactable || !loaded.checkpoint_ok {
            return loaded;
        }
        match Lock::acquire(&self.dir.join(LOCK)) {
            Ok(_lock) => {
                let (mut loaded, tail, compactable) = self.load_once();
                if tail > COMPACT_AFTER && compactable && loaded.checkpoint_ok {
                    // A failed compaction is reported; the unfolded tail
                    // simply answers again next read.
                    let compacted = self.compact(
                        &loaded.folds,
                        &loaded.seen,
                        &loaded.rejected,
                        loaded.max_seq,
                    );
                    loaded.errors.extend(compacted.err().map(compaction_error));
                }
                loaded
            }
            Err(e) => {
                let mut loaded = loaded;
                loaded.errors.push(compaction_error(e));
                loaded
            }
        }
    }

    /// One full read of the store with no writes: checkpoint, journal
    /// tail folded on top, then the authored files. Returns the loaded
    /// view, the count of tail records a fold accepted, and whether the
    /// journal stayed clean enough that compacting it loses nothing.
    fn load_once(&self) -> (Loaded, usize, bool) {
        let mut errors = Vec::new();
        let mut checkpoint_ok = true;
        let mut folds = HashMap::new();
        let mut through = 0u64;
        // The checkpoint's carried rejections first; the tail's follow in
        // journal order.
        let mut rejected = Vec::new();
        match self.read_checkpoint(&mut errors) {
            Some(c) => {
                through = c.through;
                folds = c.folds;
                rejected = c.rejected;
            }
            None if self.dir.join(CHECKPOINT).exists() => {
                // A checkpoint that exists but cannot be used (malformed
                // or future) degrades the store: the tail alone is not the
                // history, and it must never be compacted over.
                checkpoint_ok = false;
            }
            None => {}
        }
        let mut tail = 0usize;
        let mut max_seq = through;
        let journal = self.read_journal(&mut errors);
        for record in &journal.records {
            max_seq = max_seq.max(record.seq);
            if record.seq <= through || record.v > SCHEMA || record.session.is_empty() {
                continue;
            }
            tail += 1;
            let key = conversation_key(&record.provider, &record.session);
            let fold: &mut Fold = folds.entry(key.clone()).or_default();
            match fold.apply(record) {
                Apply::Accepted => {}
                outcome => {
                    let high_water = fold.pseq_high.unwrap_or(0);
                    let pseq = record.pseq.unwrap_or(0);
                    rejected.push(RejectedRecord {
                        conversation: key,
                        seq: record.seq,
                        at_ms: record.pts.unwrap_or(record.at),
                        pseq: record.pseq,
                        native: record.native.clone(),
                        reason: match outcome {
                            Apply::Stale => format!(
                                "producer sequence {pseq} is below the high-water {high_water}"
                            ),
                            Apply::Duplicate => format!(
                                "producer sequence {pseq} already seen; the observation refreshes the retained one"
                            ),
                            Apply::Accepted => unreachable!(), // coverage: off - the match guards it
                        },
                    });
                }
            }
        }
        self.report_cuts(&mut errors);
        let seen = self.read_seen(&mut errors);
        let marks = self.read_marks(&mut errors);
        let work = self.read_work(&mut errors);
        // Acknowledgement stands on both halves reading: the history a
        // `seen` sequence indexes into, and `seen.json` itself - absent is
        // a first run, malformed is a guess refused.
        let ack_readable = checkpoint_ok && !errors.iter().any(|e| e.source == SEEN);
        (
            Loaded {
                folds,
                seen,
                marks,
                work,
                rejected,
                errors,
                max_seq,
                ack_readable,
                checkpoint_ok,
            },
            tail,
            journal.compactable,
        )
    }

    /// Acknowledge every retained event on each `key` through its
    /// `through_seq`, and the live wait episode `wait_ms` names when one
    /// shows - `(key, through_seq, wait_ms)` per entry. The batch takes
    /// the store lock once, reads `seen.json` once, and rewrites the map
    /// atomically at most once, only when an update changed it, so two
    /// acknowledgements cannot lose one another and one keypress cannot
    /// queue behind a lock per conversation. Updates merge monotonically,
    /// later entries seeing earlier ones under the same key, and the
    /// sequence never moves backwards. `space` and the focus observation
    /// both land here. Returns each entry's acknowledgement as stored, in
    /// input order. An empty batch is a no-op that touches nothing.
    pub fn acknowledge_many(&self, updates: &[(&str, u64, Option<u64>)]) -> io::Result<Vec<Seen>> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let mut seen = self.read_seen_for_update()?;
        let mut changed = false;
        let mut stored = Vec::with_capacity(updates.len());
        for &(key, through_seq, wait_ms) in updates {
            let old = seen.get(key).copied().unwrap_or_default();
            let new = Seen {
                seq: old.seq.max(through_seq),
                wait_ms: wait_ms.max(old.wait_ms),
            };
            if new != old {
                seen.insert(key.to_owned(), new);
                changed = true;
            }
            stored.push(new);
        }
        if changed {
            self.write_seen(&seen)?; // coverage: off - a seen-state write failure needs a filesystem fault
        }
        Ok(stored)
    }

    /// Acknowledge one conversation; delegates to
    /// [`Self::acknowledge_many`]. Returns its acknowledgement as stored.
    pub fn acknowledge(
        &self,
        key: &str,
        through_seq: u64,
        wait_ms: Option<u64>,
    ) -> io::Result<Seen> {
        Ok(self
            .acknowledge_many(&[(key, through_seq, wait_ms)])?
            .into_iter()
            .next()
            .expect("a one-entry batch returns one acknowledgement"))
    }

    /// Record authored not-busy marks - `(key, since_ms, seq)` per entry:
    /// `since_ms` names the dismissed `Busy`'s `effective_since`, `seq`
    /// the commit sequence at write time. The batch takes the store lock
    /// once, reads `marks.json` once, and rewrites it once, every mark
    /// sharing the one `at_ms` so the keypress lands as a single authored
    /// action. An empty batch is a no-op that touches nothing.
    pub fn mark_not_busy_many(&self, updates: &[(&str, u64, u64)]) -> io::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let mut marks = self.read_marks_for_update()?;
        let at_ms = now_ms();
        for &(key, since_ms, seq) in updates {
            marks.insert(
                key.to_owned(),
                Mark {
                    since_ms,
                    seq,
                    at_ms,
                },
            );
        }
        self.write_marks(&marks)
    }

    /// Mark one conversation not-busy; delegates to
    /// [`Self::mark_not_busy_many`].
    pub fn mark_not_busy(&self, key: &str, since_ms: u64, seq: u64) -> io::Result<()> {
        self.mark_not_busy_many(&[(key, since_ms, seq)])
    }

    /// Synchronize `repo`'s records with the refs one pass observed at
    /// `observed_ms` (epoch milliseconds): a ref that keeps its record
    /// updates it, a ref absent from the observation closes its active
    /// record (`ended_at`), and a ref with no active record opens a new
    /// incarnation - new id, `parked: false`, the current inputs
    /// fingerprinted silently. A changed fingerprint on a live record is
    /// a lifecycle transition observed at `observed_ms` - a detection,
    /// never activity. The activities each ref carries land on its
    /// record at their own proven occurrence times, first pass included.
    ///
    /// Continuity: a proven `Branch: renamed` line moves the record with
    /// its id; reflog creation evidence that no longer matches proves a
    /// boundary the ref's continued presence hid (a missed
    /// delete/recreate); a proven non-fast-forward tip move under
    /// unchanged creation evidence is continuous (`force_push`), never a
    /// boundary; and evidence that
    /// detects a boundary it cannot explain separates rather than merges
    /// (`ambiguous`). Closing a record closes its open touch intervals at
    /// the same time, and closed records are pruned only past
    /// [`RETAIN_CLOSED_MS`] and only while no touch references them.
    ///
    /// One lock and at most one rewrite; a sync that changes nothing -
    /// no refs to record and none to close - touches no file.
    pub fn sync_repo(&self, repo: &str, refs: &[ObservedRef], observed_ms: u64) -> io::Result<()> {
        self.sync_repo_after(repo, refs, observed_ms, None)
    }

    /// [`Self::sync_repo`] for a caller that knows `previous_ms`, the
    /// last pass that observed this repository's refs: a record this pass
    /// closes was last seen then, so its `last_observed_at` lands there
    /// even when the quiet passes in between wrote nothing.
    pub fn sync_repo_after(
        &self,
        repo: &str,
        refs: &[ObservedRef],
        observed_ms: u64,
        previous_ms: Option<u64>,
    ) -> io::Result<()> {
        let last_seen = |record: &mut BranchRecord| {
            if let Some(ms) = previous_ms {
                record.last_observed_at = record.last_observed_at.max(ms);
            }
        };
        // No refs to record and no file to close records in: the sync
        // touches nothing.
        if refs.is_empty() && !self.dir.join(WORK).exists() {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let mut work = self.read_work_for_update()?;
        let mut changed = false;
        let observed: std::collections::HashSet<&str> =
            refs.iter().map(|r| r.name.as_str()).collect();

        // A proven rename moves the record under the new name before
        // disappearance is judged: the ref's reflog - creation line,
        // history and all - moved with it, so id and `first_observed_at`
        // hold and only the label changes.
        for obs in refs {
            let Some(old) = obs.renamed_from.as_deref().filter(|o| *o != obs.name) else {
                continue;
            };
            let old_key = branch_key(repo, old);
            let Some(id) = work.active_branches.get(&old_key).cloned() else {
                continue;
            };
            // The rename line persists in the moved reflog forever: it
            // applies only while the old ref went unobserved this pass
            // and the destination is not already a different active
            // incarnation. A recreated old name opens its own record; a
            // taken destination never loses its lookup.
            if observed.contains(old)
                || work
                    .active_branches
                    .get(&branch_key(repo, &obs.name))
                    .is_some_and(|d| *d != id)
            {
                continue;
            }
            let Some(record) = work.branches.get_mut(&id) else {
                continue; // coverage: off - the lookup only ever names a stored id
            };
            work.active_branches.remove(&old_key);
            record.ref_name = obs.name.clone();
            record.last_observed_at = observed_ms;
            absorb_evidence(record, obs, observed_ms);
            record.continuity_evidence = ContinuityEvidence::ProvenRename;
            if record.creation_evidence.is_none() {
                record.creation_evidence = obs.creation.clone();
            }
            work.active_branches.insert(branch_key(repo, &obs.name), id);
            changed = true;
        }

        // Refs the pass did not observe close their active record and
        // leave the active lookup - the closed record itself is kept.
        let prefix = format!("{repo}\u{0}");
        let active: Vec<String> = work
            .active_branches
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for key in active {
            let Some(id) = work.active_branches.get(&key).cloned() else {
                continue; // coverage: off - `key` came from this map
            };
            let Some(record) = work.branches.get(&id) else {
                continue; // coverage: off - the lookup only ever names a stored id
            };
            if observed.contains(record.ref_name.as_str()) {
                continue;
            }
            let Some(record) = work.branches.get_mut(&id) else {
                continue; // coverage: off - the same lookup just answered it
            };
            record.ended_at = Some(observed_ms);
            // An established branch going absent is a proven presence
            // transition: observed, never activity.
            append_observation(
                &mut record.observations,
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::Lifecycle,
                    observed_at_ms: observed_ms,
                    reasons: vec!["branch gone".to_owned()],
                },
            );
            last_seen(record);
            work.active_branches.remove(&key);
            close_touches(&mut work, &id, observed_ms);
            changed = true;
        }
        for obs in refs {
            let key = branch_key(repo, &obs.name);
            let active = work
                .active_branches
                .get(&key)
                .and_then(|id| work.branches.get(id))
                .filter(|r| r.ended_at.is_none())
                .map(|r| r.id.clone());
            match active {
                Some(id) => {
                    let boundary = {
                        let Some(record) = work.branches.get(&id) else {
                            continue; // coverage: off - the lookup only ever names a stored id
                        };
                        creation_boundary(record, obs)
                    };
                    if let Some(continuity) = boundary {
                        // The evidence proves - or fails to rule out - a
                        // delete/recreate between polls: the observed ref
                        // is not the recorded incarnation. Close at the
                        // observation and open the next record; an
                        // unprovable boundary serializes `ambiguous`.
                        let Some(record) = work.branches.get_mut(&id) else {
                            continue; // coverage: off - the same lookup just answered it
                        };
                        record.ended_at = Some(observed_ms);
                        // The ref never went absent: this is a detected
                        // incarnation boundary, not a presence flip.
                        append_observation(
                            &mut record.observations,
                            ObservationEvent {
                                covered_by: None,
                                source: ObservationSource::Lifecycle,
                                observed_at_ms: observed_ms,
                                reasons: vec!["branch incarnation changed".to_owned()],
                            },
                        );
                        last_seen(record);
                        work.active_branches.remove(&key);
                        close_touches(&mut work, &id, observed_ms);
                        open_incarnation(&mut work, repo, obs, observed_ms, continuity);
                        changed = true;
                        continue;
                    }
                    let Some(record) = work.branches.get_mut(&id) else {
                        continue; // coverage: off - the lookup only ever names a stored id
                    };
                    // The observation itself moves `last_observed_at`
                    // but is no reason to write: an unchanged pass
                    // rewrites nothing, and the value lands with the
                    // next real change.
                    record.last_observed_at = observed_ms;
                    // Creation evidence adopts its first proven value
                    // without dating anything, like every other field.
                    if record.creation_evidence.is_none() && obs.creation.is_some() {
                        record.creation_evidence = obs.creation.clone();
                        changed = true;
                    }
                    if let Some(continuity) = continuity_of(record, obs)
                        && record.continuity_evidence != continuity
                    {
                        record.continuity_evidence = continuity;
                        changed = true;
                    }
                    changed |= absorb_evidence(record, obs, observed_ms);
                }
                None => {
                    // No active record for this ref: a first-ever sighting
                    // is a silent baseline; a ref that a closed record
                    // proves was already observed-and-gone is found again.
                    let returned = work
                        .branches
                        .values()
                        .any(|r| r.repo == repo && r.ref_name == obs.name && r.ended_at.is_some());
                    let id = open_incarnation(
                        &mut work,
                        repo,
                        obs,
                        observed_ms,
                        ContinuityEvidence::FirstObservation,
                    );
                    if returned {
                        let record = work.branches.get_mut(&id).expect("just opened");
                        append_observation(
                            &mut record.observations,
                            ObservationEvent {
                                covered_by: None,
                                source: ObservationSource::Lifecycle,
                                observed_at_ms: observed_ms,
                                reasons: vec!["branch found".to_owned()],
                            },
                        );
                    }
                    changed = true;
                }
            }
        }
        // Closed records keep 90 days, or as long as a touch references
        // them - the history an `h` view would name. Stale active lookups
        // never survive the record they named.
        let cutoff = observed_ms.saturating_sub(RETAIN_CLOSED_MS);
        let touched: std::collections::HashSet<&str> =
            work.touches.iter().map(|t| t.branch.as_str()).collect();
        let before = work.branches.len();
        work.branches.retain(|id, r| {
            r.ended_at.is_none_or(|e| e >= cutoff) || touched.contains(id.as_str())
        });
        changed |= work.branches.len() != before;
        let before = work.active_branches.len();
        work.active_branches
            .retain(|_, id| work.branches.contains_key(id));
        changed |= work.active_branches.len() != before;
        if changed {
            self.write_work(&work)?; // coverage: off - the error edge needs the atomic write to fail
        }
        Ok(())
    }

    /// Reconcile the persisted touch intervals with what one pass proved,
    /// at `observed_ms` (epoch milliseconds), under one lock and at most
    /// one rewrite.
    ///
    /// `placements` are live observations: a first placement opens an
    /// interval, an unchanged one is a no-op, a moved head or incarnation
    /// closes the conversation's open observed interval and appends the
    /// next at the same time. They land only on live incarnations.
    ///
    /// `dated` intervals come from a provider's own records: an unknown
    /// one appends as given, a known open one may close, nothing else
    /// moves. They may land on closed incarnations - that is history.
    ///
    /// History stays append-only, and a conversation absent from both
    /// keeps its open intervals: missing evidence closes nothing. An empty
    /// call with no file to reconcile touches nothing.
    pub fn sync_touches(
        &self,
        placements: &[TouchPlacement],
        dated: &[DatedTouch],
        observed_ms: u64,
    ) -> io::Result<()> {
        if placements.is_empty() && dated.is_empty() && !self.dir.join(WORK).exists() {
            return Ok(());
        }
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let mut work = self.read_work_for_update()?;
        let mut changed = false;
        for placement in placements {
            // A placement lands only on a live incarnation; a closed one
            // has its intervals closed at the boundary, never reopened.
            if !work
                .branches
                .get(&placement.branch)
                .is_some_and(|r| r.ended_at.is_none())
            {
                continue;
            }
            let open = work.touches.iter().rposition(|t| {
                t.conversation == placement.conversation
                    && t.provenance == placement.provenance
                    && t.valid_until.is_none()
            });
            match open {
                Some(i)
                    if work.touches[i].branch == placement.branch
                        && work.touches[i].head.as_deref() == Some(placement.head.as_str()) => {}
                Some(i) => {
                    work.touches[i].valid_until = Some(observed_ms);
                    work.touches.push(touch_of(placement, observed_ms));
                    changed = true;
                }
                None => {
                    work.touches.push(touch_of(placement, observed_ms));
                    changed = true;
                }
            }
        }
        let mut known: HashMap<(String, String, u64), usize> = work
            .touches
            .iter()
            .enumerate()
            .filter(|(_, t)| t.provenance == TouchProvenance::ProviderBranch)
            .map(|(i, t)| ((t.conversation.clone(), t.branch.clone(), t.valid_from), i))
            .collect();
        for d in dated {
            let Some(record) = work.branches.get(&d.branch) else {
                continue;
            };
            // A closed incarnation bounds every interval on it.
            let until = match (d.valid_until, record.ended_at) {
                (Some(u), Some(e)) => Some(u.min(e)),
                (u, e) => u.or(e),
            };
            let key = (d.conversation.clone(), d.branch.clone(), d.valid_from);
            match known.get(&key) {
                Some(&i) => {
                    if work.touches[i].valid_until.is_none() && until.is_some() {
                        work.touches[i].valid_until = until;
                        changed = true;
                    }
                }
                None => {
                    known.insert(key, work.touches.len());
                    work.touches.push(BranchTouch {
                        conversation: d.conversation.clone(),
                        branch: d.branch.clone(),
                        head: None,
                        valid_from: d.valid_from,
                        valid_until: until,
                        provenance: TouchProvenance::ProviderBranch,
                        confidence: Confidence::Exact,
                    });
                    changed = true;
                }
            }
        }
        if changed {
            self.write_work(&work)?; // coverage: off - the error edge needs the atomic write to fail
        }
        Ok(())
    }

    /// Synchronize the record for `path` - a detached worktree or a
    /// project space, owned by `repo` - with the fingerprint a pass observed at
    /// `observed_ms` and the source-backed `activities` it can prove.
    /// Creates it absent, lands changed fingerprints and field transitions
    /// as observations, and writes nothing when nothing changed.
    pub fn sync_path(
        &self,
        path: &str,
        repo: &str,
        inputs: &LifecycleInputs,
        activities: &[ActivityEvent],
        observed_ms: u64,
    ) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let mut work = self.read_work_for_update()?;
        let mut changed = false;
        match work.paths.get_mut(path) {
            Some(record) => {
                // A record written before it carried its repo adopts it.
                // A project space names only itself - where a repository
                // already claimed the record, the space does not displace
                // it; a repo always reasserts its own claim.
                if record.repo.as_deref() != Some(repo) && (repo != path || record.repo.is_none()) {
                    record.repo = Some(repo.to_owned());
                    changed = true;
                }
                changed |= absorb_path_evidence(record, inputs, activities, observed_ms);
            }
            None => {
                let mut record = PathRecord {
                    repo: Some(repo.to_owned()),
                    parked: false,
                    activities: Vec::new(),
                    observations: Vec::new(),
                    inputs: inputs.clone(),
                    session_activity: BTreeMap::new(),
                    session_context: BTreeMap::new(),
                };
                for event in activities {
                    append_activity(&mut record.activities, event.clone());
                }
                work.paths.insert(path.to_owned(), record);
                changed = true;
            }
        }
        if changed {
            self.write_work(&work)?; // coverage: off - the error edge needs the atomic write to fail
        }
        Ok(())
    }

    pub fn sync_session_updates(&self, updates: &[SessionUpdate]) -> io::Result<()> {
        if updates.is_empty() || !self.dir.join(WORK).exists() {
            return Ok(());
        }
        let _lock = Lock::acquire(&self.dir.join(LOCK))?; // coverage: off - the error edge needs a filesystem fault
        let mut work = self.read_work_for_update()?; // coverage: off - the error edge needs an unreadable work.json
        let mut changed = false;
        for update in updates {
            let applied = match &update.identity {
                UpdateIdentity::Branch(id) => work.branches.get_mut(id).map(|record| {
                    session_update(
                        &mut record.activities,
                        &mut record.session_activity,
                        &mut record.session_context,
                        update,
                    )
                }),
                UpdateIdentity::Path(path) => work.paths.get_mut(path).map(|record| {
                    session_update(
                        &mut record.activities,
                        &mut record.session_activity,
                        &mut record.session_context,
                        update,
                    )
                }),
            };
            changed |= applied.unwrap_or(false);
        }
        if changed {
            self.write_work(&work)?; // coverage: off - the error edge needs the atomic write to fail
        }
        Ok(())
    }

    /// Flip `parked` on the record `identity` names exactly - an active
    /// incarnation by id, or a path record by canonical path. A missing
    /// record is `NotFound`: the toggle never fabricates a row's state.
    /// Returns the stored flag after the flip.
    pub fn toggle_parked(&self, identity: &WorkIdentity) -> io::Result<bool> {
        // No file, no record: the toggle names nothing and writes nothing.
        if !self.dir.join(WORK).exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no work.json - nothing is parked",
            ));
        }
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let mut work = self.read_work_for_update()?;
        let parked = match identity {
            WorkIdentity::Branch(id) => {
                let record = work
                    .branches
                    .get_mut(id)
                    .filter(|r| r.ended_at.is_none())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("no active branch incarnation {id}"),
                        )
                    })?;
                record.parked = !record.parked;
                record.parked
            }
            WorkIdentity::Path(path) => {
                let record = work.paths.get_mut(path).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no work record for {path}"),
                    )
                })?;
                record.parked = !record.parked;
                record.parked
            }
        };
        self.write_work(&work)?; // coverage: off - the error edge needs the atomic write to fail
        Ok(parked)
    }

    /// The identity of `work.json` as it stands - inode, length and mtime -
    /// or `None` when absent. Every write is an atomic rename onto a new
    /// inode, so an unchanged stamp means an unchanged file and a reader
    /// can skip re-parsing it.
    pub fn work_stamp(&self) -> Option<WorkStamp> {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(self.dir.join(WORK)).ok()?;
        Some(WorkStamp {
            ino: meta.ino(),
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    /// The authored work state as the file reads today, plus any read
    /// errors - absent is an empty state, malformed or future reports.
    pub fn work(&self) -> (Work, Vec<SourceError>) {
        let mut errors = Vec::new();
        let work = self.read_work(&mut errors);
        (work, errors)
    }

    /// Write the checkpoint for `folds` at `through` and rewrite the
    /// journal to nothing past it - checkpoint first, so a crash between
    /// the two renames leaves stale tail records that simply re-skip.
    /// Latches `seen` acknowledges are dropped: derivation already ignores
    /// them, so the reduction it answers is unchanged.
    fn compact(
        &self,
        folds: &HashMap<String, Fold>,
        seen: &HashMap<String, Seen>,
        rejected: &[RejectedRecord],
        through: u64,
    ) -> io::Result<()> {
        let mut folds = folds.clone();
        for (key, fold) in &mut folds {
            if let Some(seen) = seen.get(key) {
                fold.retained.retain(|r| r.seq > seen.seq);
            }
        }
        let checkpoint = Checkpoint {
            v: SCHEMA,
            through,
            folds,
            rejected: newest_rejected(rejected),
        };
        let bytes = serde_json::to_vec_pretty(&checkpoint)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?; // coverage: off - a checkpoint always serializes
        write_atomic(&self.dir.join(CHECKPOINT), &bytes)?; // coverage: off - a checkpoint write failure needs a filesystem fault
        // The tail restarts empty; every record at or below `through` is
        // carried by the checkpoint and skipped on the next read.
        write_atomic(&self.dir.join(JOURNAL), &[])?; // coverage: off - same
        Ok(())
    }

    /// The checkpoint's folds, or `None` when absent/unreadable/future.
    /// Errors are retained rather than thrown.
    fn read_checkpoint(&self, errors: &mut Vec<SourceError>) -> Option<Checkpoint> {
        let path = self.dir.join(CHECKPOINT);
        let bytes = read_file(&path, "checkpoint", errors)?;
        match serde_json::from_slice::<Checkpoint>(&bytes) {
            Ok(c) if c.v <= SCHEMA => Some(c),
            Ok(c) => {
                errors.push(SourceError {
                    source: "checkpoint".to_owned(),
                    detail: format!(
                        "{}: schema v{} is newer than v{SCHEMA}",
                        path.display(),
                        c.v
                    ),
                });
                None
            }
            Err(e) => {
                errors.push(SourceError {
                    source: "checkpoint".to_owned(),
                    detail: format!("{}: {e}", path.display()),
                });
                None
            }
        }
    }

    /// The journal tail: every parseable record in committed order, plus
    /// whether the file stayed clean enough to compact. A framing error -
    /// a truncated length or payload - ends the tail there: the corrupt
    /// suffix can hide nothing of the valid prefix. `compactable` is
    /// false for any frame the reduction cannot carry back, since a
    /// rewrite would drop its bytes.
    fn read_journal(&self, errors: &mut Vec<SourceError>) -> JournalRead {
        let path = self.dir.join(JOURNAL);
        let Some(bytes) = read_file(&path, "journal", errors) else {
            return JournalRead {
                records: Vec::new(),
                compactable: true,
                torn_at: None,
            };
        };
        let mut records = Vec::new();
        let mut compactable = true;
        let mut torn_at = None;
        let mut cursor = 0usize;
        while cursor + 4 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            if cursor + len > bytes.len() {
                errors.push(SourceError {
                    source: "journal".to_owned(),
                    detail: format!(
                        "{}: truncated record at byte {cursor} ({len} bytes announced)",
                        path.display()
                    ),
                });
                compactable = false;
                torn_at = Some(cursor - 4);
                break;
            }
            let frame = &bytes[cursor..cursor + len];
            // A record is always a JSON object. serde would happily decode
            // an array positionally into the struct's defaults, which would
            // let corrupt bytes masquerade as a committed record.
            let is_object = frame
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|b| *b == b'{');
            match is_object
                .then(|| serde_json::from_slice::<Record>(frame))
                .and_then(|r| r.ok())
            {
                Some(record) => {
                    if record.v > SCHEMA {
                        // Excluded from derivation, retained on disk - and
                        // its commit sequence still counts, so a future
                        // record never lets a writer reissue `seq`.
                        compactable = false;
                        errors.push(SourceError {
                            source: "journal".to_owned(),
                            detail: format!(
                                "{}: record seq {} carries schema v{}, newer than v{SCHEMA}",
                                path.display(),
                                record.seq,
                                record.v
                            ),
                        });
                    }
                    if record.session.is_empty() {
                        // The record folds onto no conversation, so a
                        // checkpoint cannot carry it either.
                        compactable = false;
                    }
                    records.push(record);
                }
                None => {
                    compactable = false;
                    errors.push(SourceError {
                        source: "journal".to_owned(),
                        detail: format!(
                            "{}: record at byte {cursor} does not parse",
                            path.display()
                        ),
                    });
                }
            }
            cursor += len;
        }
        // Fewer than four bytes past the last whole frame: a length header
        // that never finished. A truncated payload was already reported.
        if torn_at.is_none() && bytes.len() - cursor > 0 && cursor + 4 > bytes.len() {
            compactable = false;
            torn_at = Some(cursor);
            errors.push(SourceError {
                source: "journal".to_owned(),
                detail: format!(
                    "{}: {} trailing bytes",
                    path.display(),
                    bytes.len() - cursor
                ),
            });
        }
        JournalRead {
            records,
            compactable,
            torn_at,
        }
    }

    /// Every journal fragment an append set aside, as an error: the bytes
    /// are kept for repair by hand, and their records count for nothing
    /// until then.
    fn report_cuts(&self, errors: &mut Vec<SourceError>) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut cuts: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(CUT_PREFIX))
            })
            .collect();
        cuts.sort();
        errors.extend(cuts.into_iter().map(|path| SourceError {
            source: "journal".to_owned(),
            detail: format!("{}: unframed bytes set aside by an append", path.display()),
        }));
    }

    /// The seen-state map: conversation key -> its acknowledgement.
    fn read_seen(&self, errors: &mut Vec<SourceError>) -> HashMap<String, Seen> {
        self.read_authored(SEEN, errors)
            .map(|authored: Authored<HashMap<String, Seen>>| authored.data)
            .unwrap_or_default()
    }

    /// The same map for a read-modify-write: the file must have read
    /// clean or be absent - an unreadable, malformed or future-schema
    /// file is refused as `InvalidData` so the rewrite never turns its
    /// bytes into an empty map.
    fn read_seen_for_update(&self) -> io::Result<HashMap<String, Seen>> {
        let mut errors = Vec::new();
        let seen = self.read_seen(&mut errors);
        update_read(errors)?;
        Ok(seen)
    }

    fn write_seen(&self, seen: &HashMap<String, Seen>) -> io::Result<()> {
        self.write_authored(SEEN, seen, SCHEMA)
    }

    fn read_marks(&self, errors: &mut Vec<SourceError>) -> HashMap<String, Mark> {
        self.read_authored(MARKS, errors)
            .map(|authored: Authored<HashMap<String, Mark>>| authored.data)
            .unwrap_or_default()
    }

    /// The marks map under the same update precondition as seen-state.
    fn read_marks_for_update(&self) -> io::Result<HashMap<String, Mark>> {
        let mut errors = Vec::new();
        let marks = self.read_marks(&mut errors);
        update_read(errors)?;
        Ok(marks)
    }

    fn write_marks(&self, marks: &HashMap<String, Mark>) -> io::Result<()> {
        self.write_authored(MARKS, marks, SCHEMA)
    }

    /// The work-state file: incarnation and path records. An envelope
    /// version older than `WORK_SCHEMA` is a pre-release contract the
    /// build no longer carries: it reads as empty state - not an error -
    /// so the next sync atomically replaces it. A future or malformed
    /// file still reports and reads as absent, never guessed.
    fn read_work(&self, errors: &mut Vec<SourceError>) -> Work {
        let path = self.dir.join(WORK);
        let Some(bytes) = read_file(&path, WORK, errors) else {
            return Work::default();
        };
        let mut malformed = |e: serde_json::Error| {
            errors.push(SourceError {
                source: WORK.to_owned(),
                detail: format!("{}: {e}", path.display()),
            });
            Work::default()
        };
        let envelope = match serde_json::from_slice::<Authored<serde_json::Value>>(&bytes) {
            Ok(envelope) => envelope,
            Err(e) => return malformed(e),
        };
        if envelope.v > WORK_SCHEMA {
            errors.push(SourceError {
                source: WORK.to_owned(),
                detail: format!(
                    "{}: schema v{} is newer than v{WORK_SCHEMA}",
                    path.display(),
                    envelope.v
                ),
            });
            return Work::default();
        }
        if envelope.v < WORK_SCHEMA {
            return Work::default();
        }
        match serde_json::from_slice::<Authored<Work>>(&bytes) {
            Ok(a) => a.data,
            Err(e) => malformed(e),
        }
    }

    /// The work state under the same update precondition as seen-state: a
    /// file that did not read clean refuses the read-modify-write, so a
    /// rewrite can never erase bytes it could not carry.
    fn read_work_for_update(&self) -> io::Result<Work> {
        let mut errors = Vec::new();
        let work = self.read_work(&mut errors);
        update_read(errors)?;
        Ok(work)
    }

    fn write_work(&self, work: &Work) -> io::Result<()> {
        self.write_authored(WORK, work, WORK_SCHEMA)
    }

    /// One authored file read: future or malformed content is reported and
    /// treated as absent - never guessed.
    fn read_authored<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
        errors: &mut Vec<SourceError>,
    ) -> Option<Authored<T>> {
        let path = self.dir.join(name);
        let bytes = read_file(&path, name, errors)?;
        match serde_json::from_slice::<Authored<T>>(&bytes) {
            Ok(a) if a.v <= SCHEMA => Some(a),
            Ok(a) => {
                errors.push(SourceError {
                    source: name.to_owned(),
                    detail: format!(
                        "{}: schema v{} is newer than v{SCHEMA}",
                        path.display(),
                        a.v
                    ),
                });
                None
            }
            Err(e) => {
                errors.push(SourceError {
                    source: name.to_owned(),
                    detail: format!("{}: {e}", path.display()),
                });
                None
            }
        }
    }

    /// Every caller holds the store lock, so the directory exists. The
    /// envelope carries the file's own schema: seen and marks share
    /// [`SCHEMA`], `work.json` carries `WORK_SCHEMA`.
    fn write_authored<T: Serialize>(&self, name: &str, data: &T, version: u32) -> io::Result<()> {
        let authored = Authored { v: version, data };
        let bytes = serde_json::to_vec_pretty(&authored)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?; // coverage: off - the envelope always serializes
        write_atomic(&self.dir.join(name), &bytes)
    }
}

/// The shared envelope for `seen.json` and `marks.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Authored<T> {
    #[serde(default)]
    v: u32,
    data: T,
}

/// The exclusive OS advisory lock on `journal.lock`. The kernel releases
/// it when the holder closes the file or dies, so a crashed writer never
/// leaves a lock behind to judge stale or steal. The file itself stays:
/// unlinking a lock file another process may be waiting on would let two
/// holders lock two different files.
struct Lock {
    _file: fs::File,
}

impl Lock {
    /// Take the lock, polling until [`LOCK_WAIT`] expires; then
    /// `WouldBlock`, so a hook reports the failure instead of hanging.
    fn acquire(path: &Path) -> io::Result<Lock> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let deadline = SystemTime::now() + LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Lock { _file: file }),
                Err(fs::TryLockError::WouldBlock) if SystemTime::now() <= deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("{}: lock held", path.display()),
                    ));
                }
                Err(fs::TryLockError::Error(e)) => return Err(e), // coverage: off - a lock call failing outright needs a filesystem that refuses locks
            }
        }
    }
}

/// A failed compaction, reported beside the read it was attempted on.
fn compaction_error(e: io::Error) -> SourceError {
    SourceError {
        source: "store".to_owned(),
        detail: format!("compaction: {e}"),
    }
}

/// A mutation's read precondition: a clean or absent authored file passes;
/// anything the reader reported becomes `InvalidData` carrying the same
/// details, so the rewrite never erases bytes it could not carry.
fn update_read(errors: Vec<SourceError>) -> io::Result<()> {
    if errors.is_empty() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        errors
            .iter()
            .map(|e| format!("{}: {}", e.source, e.detail))
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

/// `path`'s whole contents; `None` on a missing file (the normal first-run
/// case) and an error retained on any other failure.
fn read_file(path: &Path, source: &str, errors: &mut Vec<SourceError>) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            errors.push(SourceError {
                source: source.to_owned(),
                detail: format!("{}: {e}", path.display()),
            });
            None
        }
    }
}

/// `bytes` -> `path` atomically: sibling temp, fsync, rename, fsync the
/// directory so the rename itself survives. Callers hold the store lock,
/// which already created the directory.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp)?; // coverage: off - a create failure needs a filesystem fault
        file.write_all(bytes)?; // coverage: off - same
        file.sync_all()?; // coverage: off - an fsync failure needs a broken filesystem
    }
    fs::rename(&tmp, path)?; // coverage: off - a failed rename needs a filesystem fault
    if let Some(dir) = path.parent()
        && let Ok(dir) = fs::File::open(dir)
    {
        let _ = dir.sync_all();
    } // coverage: off - every store path has an openable parent: the lock was just taken in it
    Ok(())
}

/// Epoch milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// `system_time` -> epoch milliseconds.
pub fn epoch_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// The writer identity a record carries.
fn writer() -> String {
    format!("agent-sessions/{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempStore(PathBuf);
    impl TempStore {
        fn new() -> TempStore {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-sessions-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            TempStore(path)
        }
        fn store(&self) -> Store {
            Store::open(self.0.clone())
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record(provider: &str, session: &str, native: &str, event: NormEvent) -> Record {
        let mut r = Record::new(provider, session, native);
        r.event = Some(event);
        r
    }

    #[test]
    fn concurrent_writers_get_unique_monotonic_sequences() {
        let temp = TempStore::new();
        let store = temp.store();
        let mut handles = Vec::new();
        for i in 0..8 {
            let store = Store::open(temp.0.clone());
            handles.push(std::thread::spawn(move || {
                (0..8)
                    .map(|_| {
                        store
                            .append(record("claude", &format!("s{i}"), "Stop", NormEvent::End))
                            .expect("append")
                    })
                    .collect::<Vec<u64>>()
            }));
        }
        let mut seqs: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("writer joins"))
            .collect();
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=64).collect::<Vec<_>>());
        let loaded = store.load();
        assert_eq!(loaded.max_seq, 64);
        assert_eq!(loaded.folds.len(), 8);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    }

    #[test]
    fn a_corrupt_tail_never_hides_the_valid_prefix() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        // A torn write: an announced record that never arrives, then
        // trailing garbage.
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(b"{\"v\":1,\"seq\":2");
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.folds.len(), 1);
        assert!(loaded.errors.iter().any(|e| e.source == "journal"));
    }

    #[test]
    fn an_append_after_a_torn_write_lands_on_a_frame_boundary() {
        // A write that died partway - out of space, or the hook killed
        // between length and payload - leaves a partial frame. Appending
        // after it would misframe every later record, so the next append
        // cuts the uncommitted fragment off first.
        for torn in [
            [&100u32.to_le_bytes()[..], b"{\"v\":1,\"seq\":2"].concat(),
            [&100u32.to_le_bytes()[..], b"{\""].concat(),
            vec![7, 0],
        ] {
            let temp = TempStore::new();
            let store = temp.store();
            store
                .append(record("claude", "s1", "Stop", NormEvent::End))
                .unwrap();
            let journal = temp.path(JOURNAL);
            let clean = fs::read(&journal).unwrap();
            fs::write(&journal, [clean.as_slice(), &torn].concat()).unwrap();
            let seqs: Vec<u64> = ["s2", "s3"]
                .iter()
                .map(|s| {
                    store
                        .append(record("claude", s, "Stop", NormEvent::End))
                        .unwrap()
                })
                .collect();
            assert_eq!(seqs, [2, 3]);
            let loaded = store.load();
            assert_eq!(loaded.folds.len(), 3, "{:?}", loaded.folds);
            // The journal reads clean; only the set-aside fragment is
            // reported.
            assert_eq!(loaded.errors.len(), 1, "{:?}", loaded.errors);
            assert!(loaded.errors[0].detail.contains(CUT_PREFIX));
            assert_eq!(loaded.max_seq, 3);
        }
    }

    #[test]
    fn a_cut_journal_tail_is_set_aside_not_deleted() {
        // What the reader cannot frame need not be a fresh tear: a
        // corrupt length header mid-file, here followed by a committed
        // record, reads the same. The append sets the cut bytes aside,
        // byte for byte, and every later read reports where they went.
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        let journal = temp.path(JOURNAL);
        let clean = fs::read(&journal).unwrap();
        let other = TempStore::new();
        other
            .store()
            .append(record("claude", "s9", "Stop", NormEvent::End))
            .unwrap();
        let behind = fs::read(other.path(JOURNAL)).unwrap();
        let cut = [&100u32.to_le_bytes()[..], b"{\"v\"", &behind].concat();
        let corrupt = [clean.as_slice(), &cut].concat();
        fs::write(&journal, &corrupt).unwrap();
        store
            .append(record("claude", "s2", "Stop", NormEvent::End))
            .unwrap();
        let aside: Vec<PathBuf> = fs::read_dir(&temp.0)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(CUT_PREFIX))
            })
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        // Where the reader lost the framing is its call; whatever it cut
        // is set aside, so the kept journal plus the fragment is every
        // byte that was there.
        let fragment = fs::read(&aside[0]).unwrap();
        let kept = corrupt.len() - fragment.len();
        assert!(kept >= clean.len() && corrupt.ends_with(&fragment));
        assert_eq!(&fs::read(&journal).unwrap()[..kept], &corrupt[..kept]);
        let loaded = store.load();
        assert_eq!(loaded.folds.len(), 2, "{:?}", loaded.folds);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.source == "journal" && e.detail.contains(CUT_PREFIX)),
            "{:?}",
            loaded.errors
        );
    }

    #[test]
    fn an_append_keeps_a_complete_frame_it_cannot_parse() {
        // Only an incomplete trailing frame is cut: a whole frame that does
        // not parse is retained byte for byte, and appends go after it.
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        let journal = temp.path(JOURNAL);
        let bad = b"not json";
        let mut bytes = fs::read(&journal).unwrap();
        bytes.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        bytes.extend_from_slice(bad);
        fs::write(&journal, &bytes).unwrap();
        store
            .append(record("claude", "s2", "Stop", NormEvent::End))
            .unwrap();
        let after = fs::read(&journal).unwrap();
        assert_eq!(&after[..bytes.len()], bytes.as_slice());
        assert_eq!(store.load().folds.len(), 2);
    }

    #[test]
    fn a_malformed_record_is_isolated_not_fatal() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        // A record whose payload is JSON but not a Record shape: an array.
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        let bad = b"[1,2,3]";
        bytes.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        bytes.extend_from_slice(bad);
        fs::write(&journal, &bytes).unwrap();
        store
            .append(record("claude", "s2", "Stop", NormEvent::End))
            .unwrap();
        let loaded = store.load();
        assert_eq!(loaded.folds.len(), 2, "{:?}", loaded.folds);
        assert!(loaded.errors.iter().any(|e| e.source == "journal"));
    }

    #[test]
    fn a_future_schema_record_is_excluded_and_reported() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        let future = b"{\"v\":99,\"seq\":2,\"provider\":\"claude\",\"session\":\"s2\",\"native\":\"Stop\",\"event\":\"end\"}";
        bytes.extend_from_slice(&(future.len() as u32).to_le_bytes());
        bytes.extend_from_slice(future);
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        // s2 reduces to nothing - the record stays on disk, reported.
        assert_eq!(loaded.folds.len(), 1);
        assert!(loaded.max_seq >= 2);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("newer than"))
        );
    }

    #[test]
    fn compaction_replays_to_the_same_reduction() {
        let temp = TempStore::new();
        let store = temp.store();
        for i in 0..COMPACT_AFTER {
            let event = match i % 4 {
                0 => NormEvent::Start,
                1 => NormEvent::Activity,
                2 => NormEvent::Awaiting,
                _ => NormEvent::End,
            };
            let mut r = record("claude", "s1", "Evt", event);
            r.reason = Some("why".to_owned());
            store.append(r).unwrap();
        }
        // An `end` as the very last record, so the reduction has a latch.
        let mut r = record("claude", "s1", "Stop", NormEvent::End);
        r.reason = None;
        store.append(r).unwrap();
        let before = store.load();
        assert_eq!(before.folds.len(), 1, "{:?}", before.folds);
        let through = serde_json::from_slice::<serde_json::Value>(
            &fs::read(temp.path(CHECKPOINT)).expect("a checkpoint was written"),
        )
        .unwrap();
        assert_eq!(through["through"], before.max_seq);
        assert!(fs::read(temp.path(JOURNAL)).unwrap().is_empty());
        let after = store.load();
        let key = conversation_key("claude", "s1");
        for (label, loaded) in [("before", &before), ("after", &after)] {
            let fold = &loaded.folds[&key];
            assert_eq!(fold.last_seq, before.max_seq, "{label}");
            assert_eq!(
                fold.last_event.as_ref().map(|e| e.kind),
                Some(NormEvent::End),
                "{label}"
            );
            // The retained latch set is identical through compaction: an
            // awaiting the last `start` never cleared, then two `end`s.
            let kinds: Vec<NormEvent> = fold.retained.iter().map(|r| r.kind).collect();
            assert_eq!(
                kinds,
                vec![NormEvent::Awaiting, NormEvent::End, NormEvent::End],
                "{label}"
            );
        }
        assert!(after.errors.is_empty(), "{:?}", after.errors);
    }

    #[test]
    fn compaction_drops_acknowledged_latches() {
        // A provider with no `start` event: every turn's `end` latches, and
        // only seen-state acknowledges them.
        let temp = TempStore::new();
        let store = temp.store();
        let key = conversation_key("vibe", "s1");
        for _ in 0..COMPACT_AFTER {
            store
                .append(record("vibe", "s1", "post_agent", NormEvent::End))
                .unwrap();
        }
        store.acknowledge(&key, 100, None).unwrap();
        store
            .append(record("vibe", "s1", "post_agent", NormEvent::End))
            .unwrap();
        let before = store.load();
        assert!(temp.path(CHECKPOINT).exists(), "the tail compacted");
        let after = store.load();
        let kept: Vec<u64> = after.folds[&key].retained.iter().map(|r| r.seq).collect();
        assert_eq!(kept, (101..=before.max_seq).collect::<Vec<_>>());
        // What derivation answers is the same: only the unseen latches.
        let unseen = |loaded: &Loaded| {
            loaded.folds[&key]
                .retained
                .iter()
                .filter(|r| r.seq > loaded.seen[&key].seq)
                .count()
        };
        assert_eq!(unseen(&before), unseen(&after));
    }

    #[test]
    fn a_previous_schema_checkpoint_still_reads() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        let loaded = store.load();
        let fold = loaded.folds[&conversation_key("claude", "s1")].clone();
        // Hand-write a checkpoint at the pre-versioned schema (v absent).
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(
            temp.path(CHECKPOINT),
            serde_json::to_string(&serde_json::json!({
                "through": loaded.max_seq,
                "folds": { conversation_key("claude", "s1"): fold },
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(temp.path(JOURNAL), b"").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.folds.len(), 1);
        assert_eq!(loaded.max_seq, 1);
    }

    #[test]
    fn a_future_checkpoint_is_unreadable_and_reported() {
        let temp = TempStore::new();
        let store = temp.store();
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(
            temp.path(CHECKPOINT),
            serde_json::json!({"v": 99, "through": 5, "folds": {}}).to_string(),
        )
        .unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        assert!(loaded.folds.is_empty());
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("newer than"))
        );
    }

    #[test]
    fn seen_and_marks_round_trip_atomically() {
        let temp = TempStore::new();
        let store = temp.store();
        let key = conversation_key("claude", "s1");
        store.acknowledge(&key, 7, None).unwrap();
        store.acknowledge(&key, 3, None).unwrap(); // backwards is a no-op
        assert_eq!(store.load().seen[&key].seq, 7);
        // A wait episode joins the sequence without moving it back, and a
        // later sequence-only acknowledgement keeps the episode.
        let seen = store.acknowledge(&key, 0, Some(42_000)).unwrap();
        assert_eq!(
            seen,
            Seen {
                seq: 7,
                wait_ms: Some(42_000)
            }
        );
        store.acknowledge(&key, 9, None).unwrap();
        assert_eq!(
            store.load().seen[&key],
            Seen {
                seq: 9,
                wait_ms: Some(42_000)
            }
        );
        store.mark_not_busy(&key, 123_000, 7).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.marks[&key].since_ms, 123_000);
        assert_eq!(loaded.marks[&key].seq, 7);
        // A malformed authored file reports and reads empty.
        fs::write(temp.path(SEEN), "{oops").unwrap();
        let loaded = store.load();
        assert!(loaded.seen.is_empty());
        assert!(loaded.errors.iter().any(|e| e.source == SEEN));
        fs::write(
            temp.path(MARKS),
            serde_json::json!({"v": 9, "data": {}}).to_string(),
        )
        .unwrap();
        let loaded = store.load();
        assert!(loaded.marks.is_empty());
        assert!(loaded.errors.iter().any(|e| e.source == MARKS));
    }

    #[test]
    fn authored_batches_merge_every_update_in_one_operation() {
        let temp = TempStore::new();
        let store = temp.store();
        // Empty batches touch nothing: no directory, no lock, no file.
        assert!(store.acknowledge_many(&[]).unwrap().is_empty());
        store.mark_not_busy_many(&[]).unwrap();
        assert!(!temp.0.exists(), "an empty batch creates nothing");
        let a = conversation_key("claude", "a");
        let b = conversation_key("claude", "b");
        // Two keys in one batch, the first repeated later: later entries
        // see earlier ones, so the merge stays monotonic within the batch
        // and every update answers its stored value in input order.
        let stored = store
            .acknowledge_many(&[
                (a.as_str(), 7, Some(42_000)),
                (b.as_str(), 3, None),
                (a.as_str(), 5, Some(50_000)),
            ])
            .unwrap();
        assert_eq!(
            stored,
            vec![
                Seen {
                    seq: 7,
                    wait_ms: Some(42_000)
                },
                Seen {
                    seq: 3,
                    wait_ms: None
                },
                Seen {
                    seq: 7,
                    wait_ms: Some(50_000)
                },
            ]
        );
        let loaded = store.load();
        assert_eq!(
            loaded.seen[&a],
            Seen {
                seq: 7,
                wait_ms: Some(50_000)
            }
        );
        assert_eq!(
            loaded.seen[&b],
            Seen {
                seq: 3,
                wait_ms: None
            }
        );
        // Two marks land in one operation sharing the authored `at_ms`.
        store
            .mark_not_busy_many(&[(a.as_str(), 100_000, 7), (b.as_str(), 200_000, 3)])
            .unwrap();
        let marks = store.load().marks;
        assert_eq!(marks[&a].since_ms, 100_000);
        assert_eq!(marks[&a].seq, 7);
        assert_eq!(marks[&b].since_ms, 200_000);
        assert_eq!(marks[&b].seq, 3);
        assert_eq!(marks[&a].at_ms, marks[&b].at_ms);
    }

    #[test]
    fn a_mutation_never_overwrites_an_authored_file_it_cannot_read() {
        let temp = TempStore::new();
        fs::create_dir_all(&temp.0).unwrap();
        let store = temp.store();
        let key = conversation_key("claude", "s1");
        for bytes in [
            "{oops".to_owned(),
            serde_json::json!({"v": 99, "data": {}}).to_string(),
        ] {
            fs::write(temp.path(SEEN), &bytes).unwrap();
            let err = store.acknowledge(&key, 3, None).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert_eq!(fs::read(temp.path(SEEN)).unwrap(), bytes.as_bytes());
            fs::write(temp.path(MARKS), &bytes).unwrap();
            let err = store.mark_not_busy(&key, 1_000, 3).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert_eq!(fs::read(temp.path(MARKS)).unwrap(), bytes.as_bytes());
        }
        // The same bytes stay a best-effort read: reported, not fatal.
        let loaded = store.load();
        assert!(loaded.errors.iter().any(|e| e.source == SEEN));
        assert!(loaded.errors.iter().any(|e| e.source == MARKS));
    }

    #[test]
    fn fold_rejects_reordered_and_duplicates_refresh_observation() {
        let mut fold = Fold::default();
        let key = |pseq: Option<u64>, seq: u64, at: u64, event: NormEvent| {
            let mut r = record("claude", "s1", "Evt", event);
            r.pseq = pseq;
            r.seq = seq;
            r.at = at;
            r
        };
        // A stale producer sequence is rejected; a duplicate refreshes the
        // observation without resetting `since`.
        assert_eq!(
            fold.apply(&key(Some(5), 1, 1000, NormEvent::Start)),
            Apply::Accepted
        );
        assert_eq!(
            fold.apply(&key(Some(3), 2, 1100, NormEvent::Activity)),
            Apply::Stale
        );
        assert_eq!(
            fold.apply(&key(Some(5), 3, 1200, NormEvent::Start)),
            Apply::Duplicate
        );
        let last = fold.last_event.as_ref().unwrap();
        assert_eq!(last.since_ms, 1000);
        assert_eq!(last.observed_ms, 1200);
        // And an event without a producer sequence always lands.
        assert_eq!(
            fold.apply(&key(None, 4, 1300, NormEvent::End)),
            Apply::Accepted
        );
        assert_eq!(fold.retained.len(), 1);
        assert_eq!(fold.retained[0].seq, 4);
    }

    #[test]
    fn a_start_acknowledges_everything_retained() {
        let mut fold = Fold::default();
        for (seq, event) in [
            (1, NormEvent::Start),
            (2, NormEvent::End),
            (3, NormEvent::Error),
            (4, NormEvent::Awaiting),
        ] {
            let mut r = record("claude", "s1", "Evt", event);
            r.seq = seq;
            r.at = seq * 1000;
            fold.apply(&r);
        }
        assert_eq!(fold.retained.len(), 3);
        let mut r = record("claude", "s1", "UserPromptSubmit", NormEvent::Start);
        r.seq = 5;
        r.at = 5000;
        fold.apply(&r);
        assert!(fold.retained.is_empty());
        // A teardown hint is diagnostic only: it latches nothing and does
        // not move the execution claim.
        let mut r = record("claude", "s1", "SessionEnd", NormEvent::TeardownHint);
        r.seq = 6;
        fold.apply(&r);
        assert_eq!(fold.last_event.unwrap().kind, NormEvent::Start);
    }

    #[test]
    fn an_empty_session_id_reduces_to_no_conversation() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "", "Stop", NormEvent::End))
            .unwrap();
        let loaded = store.load();
        assert!(loaded.folds.is_empty());
        assert_eq!(loaded.max_seq, 1);
    }

    #[test]
    fn a_teardown_hint_claims_no_execution() {
        assert_eq!(NormEvent::TeardownHint.execution(), None);
        assert_eq!(NormEvent::End.execution(), Some(Exec::Idle));
        assert_eq!(NormEvent::Awaiting.execution(), Some(Exec::Waiting));
        assert!(!NormEvent::Activity.latches());
    }

    #[test]
    fn a_duplicate_ping_refreshes_its_lease() {
        let mut fold = Fold::default();
        let mut ping = Record::new("claude", "s1", "OddEvent");
        ping.seq = 1;
        ping.at = 1_000;
        ping.pts = Some(900);
        ping.pseq = Some(9);
        assert_eq!(fold.apply(&ping), Apply::Accepted);
        assert_eq!(fold.last_pts, Some(900));
        // The same producer sequence again is a heartbeat: the observation
        // refreshes and the ping's clock moves, but nothing latches. An
        // older `pts` cannot drag the producer watermark back.
        ping.seq = 2;
        ping.at = 1_500;
        ping.pts = Some(800);
        assert_eq!(fold.apply(&ping), Apply::Duplicate);
        assert_eq!(fold.ping, Some((1, 900)));
        assert_eq!(fold.last_pts, Some(900));
        // A newer producer sequence folds in; an older `pts` still loses
        // to the watermark.
        ping.seq = 3;
        ping.at = 2_000;
        ping.pts = Some(850);
        ping.pseq = Some(10);
        assert_eq!(fold.apply(&ping), Apply::Accepted);
        assert_eq!(fold.last_pts, Some(900));
        // And a newer `pts` moves it.
        ping.seq = 4;
        ping.pts = Some(1_100);
        ping.pseq = Some(11);
        assert_eq!(fold.apply(&ping), Apply::Accepted);
        assert_eq!(fold.last_pts, Some(1_100));
    }

    #[test]
    fn a_malformed_checkpoint_reports_and_reads_as_absent() {
        let temp = TempStore::new();
        let store = temp.store();
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(temp.path(CHECKPOINT), b"{oops").unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.source == "checkpoint" && e.detail.contains("checkpoint.json"))
        );
    }

    #[test]
    fn a_sub_frame_tail_reports_the_trailing_bytes() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .unwrap();
        // Three trailing bytes: not even a length fits.
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        bytes.extend_from_slice(b"\x01\x00\x00");
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.folds.len(), 1);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("trailing bytes"))
        );
    }

    #[test]
    fn a_dead_holders_lock_frees_a_held_one_waits_out() {
        let temp = TempStore::new();
        let store = temp.store();
        let lock = temp.path(LOCK);
        // A lock file a crashed writer left behind holds no lock: the
        // kernel released it with the holder, so the next writer commits.
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(&lock, "12345\n").unwrap();
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect("an unheld lock file is taken");
        // A live holder - another open file description - is waited out
        // until the deadline, then reported, and its lock stays intact.
        let held = Lock::acquire(&lock).unwrap();
        let err = store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect_err("a held lock is not taken");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(lock.exists());
        drop(held);
        store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect("a released lock is taken");
    }

    #[test]
    fn an_unreadable_store_file_is_one_error_not_a_crash() {
        let temp = TempStore::new();
        let store = temp.store();
        fs::create_dir_all(&temp.0).unwrap();
        // A directory where the journal should be: the read fails, the
        // error is retained, the rest of the store still answers.
        fs::create_dir_all(temp.path(JOURNAL)).unwrap();
        let loaded = store.load();
        assert!(
            loaded.errors.iter().any(|e| e.source == "journal"),
            "{:?}",
            loaded.errors
        );
        // Appending with a journal directory fails the open - the error
        // propagates as an io::Error, not a panic.
        let err = store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect_err("a journal directory fails the open");
        assert_eq!(err.kind(), io::ErrorKind::IsADirectory);
        // And a lock path that is a directory makes acquire fail outright:
        // the open's own error, not a wait for a holder.
        fs::remove_file(temp.path(LOCK)).unwrap();
        fs::create_dir_all(temp.path(LOCK)).unwrap();
        let err = store
            .append(record("claude", "s1", "Stop", NormEvent::End))
            .expect_err("a lock directory fails the open");
        assert_ne!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn compaction_keeps_a_future_frame_untouched() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        let future = b"{\"v\":99,\"seq\":130,\"provider\":\"claude\",\"session\":\"s2\",\"native\":\"Stop\",\"event\":\"end\"}";
        bytes.extend_from_slice(&(future.len() as u32).to_le_bytes());
        bytes.extend_from_slice(future);
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        // The tail is past the bound, but the future frame is not this
        // build's to rewrite: the journal stays byte-for-byte and no
        // checkpoint appears.
        assert_eq!(fs::read(&journal).unwrap(), bytes);
        assert!(!temp.path(CHECKPOINT).exists());
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("newer than"))
        );
    }

    #[test]
    fn compaction_keeps_a_malformed_frame_untouched() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        let journal = temp.path(JOURNAL);
        let mut bytes = fs::read(&journal).unwrap();
        let bad = b"[1,2,3]";
        bytes.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        bytes.extend_from_slice(bad);
        fs::write(&journal, &bytes).unwrap();
        let loaded = store.load();
        assert_eq!(fs::read(&journal).unwrap(), bytes);
        assert!(!temp.path(CHECKPOINT).exists());
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("does not parse"))
        );
    }

    #[test]
    fn compaction_never_rewrites_an_unusable_checkpoint() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        fs::write(temp.path(CHECKPOINT), b"{oops").unwrap();
        let journal = fs::read(temp.path(JOURNAL)).unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        // The malformed checkpoint is evidence of a schema this build may
        // not read: compaction must not overwrite it, so the journal
        // stays too.
        assert_eq!(fs::read(temp.path(CHECKPOINT)).unwrap(), b"{oops");
        assert_eq!(fs::read(temp.path(JOURNAL)).unwrap(), journal);
    }

    #[test]
    fn concurrent_acknowledgements_preserve_every_key() {
        let temp = TempStore::new();
        let store = temp.store();
        fs::create_dir_all(&temp.0).unwrap();
        let key_a = conversation_key("claude", "a");
        let key_b = conversation_key("claude", "b");
        // While the shared lock is held, an authored update cannot slip
        // its read-modify-write past it: it waits out the deadline and
        // fails rather than clobbering a concurrent write.
        let held = Lock::acquire(&temp.path(LOCK)).unwrap();
        let err = store
            .acknowledge(&key_a, 1, None)
            .expect_err("a held lock blocks an acknowledgement");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        let err = store
            .mark_not_busy(&key_a, 1, 1)
            .expect_err("a held lock blocks a mark");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        // Two acknowledgements parked on the lock each take it over the
        // whole read-modify-write once it frees, so both keys survive.
        let a = std::thread::spawn({
            let store = Store::open(temp.0.clone());
            let key = key_a.clone();
            move || store.acknowledge(&key, 5, None)
        });
        let b = std::thread::spawn({
            let store = Store::open(temp.0.clone());
            let key = key_b.clone();
            move || store.acknowledge(&key, 9, None)
        });
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
        a.join().expect("ack a joins").expect("ack a");
        b.join().expect("ack b joins").expect("ack b");
        let seen = store.load().seen;
        assert_eq!(seen[&key_a].seq, 5);
        assert_eq!(seen[&key_b].seq, 9);
    }

    #[test]
    fn seen_state_readability_gates_acknowledgement() {
        let temp = TempStore::new();
        let store = temp.store();
        // Absent is a first run: nothing acknowledged, nothing distrusted.
        assert!(store.load().ack_readable);
        let key = conversation_key("claude", "s1");
        store.acknowledge(&key, 3, None).unwrap();
        assert!(store.load().ack_readable);
        // Malformed seen-state cannot prove what was acknowledged.
        fs::write(temp.path(SEEN), "{oops").unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        assert!(loaded.errors.iter().any(|e| e.source == SEEN));
        // Nor can a schema written by a newer build.
        fs::write(
            temp.path(SEEN),
            serde_json::json!({"v": 99, "data": {}}).to_string(),
        )
        .unwrap();
        let loaded = store.load();
        assert!(!loaded.ack_readable);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("newer than"))
        );
    }

    #[test]
    fn compaction_and_append_share_the_lock() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        let journal = temp.path(JOURNAL);
        let before = fs::read(&journal).unwrap();
        // A held lock blocks compaction itself: the read still answers,
        // reports the failure, and leaves every byte on disk.
        let held = Lock::acquire(&journal.with_file_name(LOCK)).unwrap();
        let loaded = store.load();
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("compaction")),
            "{:?}",
            loaded.errors
        );
        assert_eq!(fs::read(&journal).unwrap(), before);
        // Now an append and two reads contend for the same lock. Whichever
        // order they run in, the append commits the next monotonic
        // sequence and the second read sees the first's compaction.
        let appender = std::thread::spawn({
            let store = Store::open(temp.0.clone());
            move || store.append(record("claude", "appended", "Stop", NormEvent::End))
        });
        let mut loaders = Vec::new();
        for _ in 0..2 {
            loaders.push(std::thread::spawn({
                let store = Store::open(temp.0.clone());
                move || store.load().max_seq
            }));
        }
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
        let seq = appender
            .join()
            .expect("append joins")
            .expect("append commits");
        assert_eq!(seq, (COMPACT_AFTER + 2) as u64);
        for loader in loaders {
            assert!(loader.join().expect("load joins") <= seq);
        }
        let loaded = store.load();
        let fold = &loaded.folds[&conversation_key("claude", "appended")];
        assert_eq!(fold.last_seq, seq);
        assert_eq!(loaded.max_seq, seq);
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    }

    #[test]
    fn a_frame_arriving_between_the_compaction_reads_still_stops_it() {
        let temp = TempStore::new();
        let store = temp.store();
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s1", "Evt", NormEvent::End))
                .unwrap();
        }
        let journal = temp.path(JOURNAL);
        let held = Lock::acquire(&temp.path(LOCK)).unwrap();
        let loader = std::thread::spawn({
            let store = Store::open(temp.0.clone());
            move || store.load()
        });
        std::thread::sleep(Duration::from_millis(200));
        // The frame lands while the read waits on the lock: the re-read
        // under the lock sees it and must not rewrite it away.
        let mut bytes = fs::read(&journal).unwrap();
        let bad = b"[1,2,3]";
        bytes.extend_from_slice(&(bad.len() as u32).to_le_bytes());
        bytes.extend_from_slice(bad);
        fs::write(&journal, &bytes).unwrap();
        drop(held);
        let loaded = loader.join().expect("load joins");
        assert_eq!(fs::read(&journal).unwrap(), bytes);
        assert!(
            loaded
                .errors
                .iter()
                .any(|e| e.detail.contains("does not parse"))
        );
    }

    /// A ref observation to sync: `name` with a dirty flag, so a single
    /// field's change stands in for a fingerprint transition.
    fn obs(name: &str, dirty: bool) -> ObservedRef {
        ObservedRef {
            name: name.to_owned(),
            head: None,
            creation: None,
            renamed_from: None,
            rewritten: false,
            commit: None,
            activities: Vec::new(),
            inputs: LifecycleInputs {
                dirty: Some(dirty),
                ..LifecycleInputs::default()
            },
        }
    }

    #[test]
    fn work_records_sync_close_and_reopen_incarnations() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        // First sync: two incarnations open, each with its own stable id.
        store
            .sync_repo(repo, &[obs("main", false), obs("feat", false)], 1_000)
            .unwrap();
        let work = store.load().work;
        let main = work.branch(repo, "main").expect("main is active");
        let feat = work.branch(repo, "feat").expect("feat is active");
        assert_ne!(main.id, feat.id);
        assert_eq!(main.first_observed_at, 1_000);
        assert_eq!(
            newest_activity(&main.activities),
            None,
            "first observation dates nothing"
        );
        assert!(
            main.observations.is_empty() && feat.observations.is_empty(),
            "the first-ever baseline is silent - no presence events"
        );
        assert!(!main.parked);
        // The same observation on a later pass is a no-op: the file does
        // not move, so a dashboard left open rewrites nothing per refresh.
        let bytes = fs::read(temp.path(WORK)).unwrap();
        store
            .sync_repo(repo, &[obs("main", false), obs("feat", false)], 1_500)
            .unwrap();
        assert_eq!(fs::read(temp.path(WORK)).unwrap(), bytes);
        // A fingerprint change lands the transition as an observation at
        // detection time - it never passes for work activity.
        store
            .sync_repo(repo, &[obs("main", false), obs("feat", true)], 2_000)
            .unwrap();
        let work = store.load().work;
        let feat = work.branch(repo, "feat").unwrap();
        assert_eq!(
            feat.observations,
            vec![ObservationEvent {
                covered_by: None,
                source: ObservationSource::Lifecycle,
                observed_at_ms: 2_000,
                reasons: vec!["dirty: clean -> dirty".to_owned()],
            }]
        );
        assert_eq!(newest_activity(&feat.activities), None);
        assert_eq!(feat.inputs.dirty, Some(true));
        // `feat` gone from the observation: the record closes and stays -
        // it leaves the active lookup but remains in the history, and the
        // established presence flip lands a `branch gone` observation.
        let old_id = feat.id.clone();
        store.sync_repo(repo, &[obs("main", false)], 3_000).unwrap();
        let work = store.load().work;
        assert!(work.branch(repo, "feat").is_none());
        assert!(!work.active_branches.contains_key(&branch_key(repo, "feat")));
        let closed = &work.branches[&old_id];
        assert_eq!(closed.ended_at, Some(3_000));
        assert_eq!(closed.first_observed_at, 1_000);
        assert_eq!(
            closed.observations,
            vec![
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::Lifecycle,
                    observed_at_ms: 2_000,
                    reasons: vec!["dirty: clean -> dirty".to_owned()],
                },
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::Lifecycle,
                    observed_at_ms: 3_000,
                    reasons: vec!["branch gone".to_owned()],
                },
            ]
        );
        assert_eq!(newest_activity(&closed.activities), None);
        // Reappearance opens a new incarnation: new id, unparked, and the
        // closed record survives beside it. The prior absent interval
        // makes this sighting a `branch found` - an observation, so the
        // new record still carries no activity.
        store
            .sync_repo(repo, &[obs("main", false), obs("feat", false)], 4_000)
            .unwrap();
        let work = store.load().work;
        let feat = work.branch(repo, "feat").expect("feat reincarnated");
        assert_ne!(feat.id, old_id);
        assert_eq!(feat.first_observed_at, 4_000);
        assert_eq!(feat.ended_at, None);
        assert_eq!(
            feat.observations,
            vec![ObservationEvent {
                covered_by: None,
                source: ObservationSource::Lifecycle,
                observed_at_ms: 4_000,
                reasons: vec!["branch found".to_owned()],
            }]
        );
        assert!(feat.activities.is_empty());
        assert_eq!(work.branches.len(), 3, "{:?}", work.branches);
        assert_eq!(work.branches[&old_id].ended_at, Some(3_000));
        // Another repo's records are not touched by this repo's sync.
        assert_eq!(work.branch(repo, "main").unwrap().id, main.id);
        let other = store.load().work;
        store.sync_repo("/other/.git", &[], 5_000).unwrap();
        let work = store.load().work;
        assert_eq!(work.branches.len(), 3, "{:?}", work.branches);
        assert!(other.branch(repo, "main").is_some());
    }

    #[test]
    fn a_creation_boundary_close_records_incarnation_changed_not_gone() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        let creation = |head: &str, at_ms: u64| {
            Some(RefCreationEvidence {
                head: head.to_owned(),
                at_ms,
            })
        };
        let mut o = obs("feat", false);
        o.creation = creation("aaaa", 500);
        store.sync_repo(repo, &[o], 1_000).unwrap();
        // A different creation proven under a never-absent name is an
        // incarnation boundary: the old record closes with `incarnation
        // changed` - it was never gone - and the same-pass open is no
        // `found`, since no absent interval preceded it.
        let mut o = obs("feat", false);
        o.creation = creation("bbbb", 800);
        store.sync_repo(repo, &[o], 2_000).unwrap();
        let work = store.load().work;
        assert_eq!(work.branches.len(), 2);
        let closed = work
            .branches
            .values()
            .find(|r| r.ended_at == Some(2_000))
            .expect("the closed incarnation");
        assert_eq!(
            closed.observations.last().expect("the boundary").reasons,
            ["branch incarnation changed".to_owned()]
        );
        let open = work.branch(repo, "feat").expect("the new incarnation");
        assert!(
            open.observations.is_empty(),
            "no absent interval, so no `branch found`: {:?}",
            open.observations
        );
    }

    #[test]
    fn an_unproven_reading_keeps_the_last_proven_value_and_dates_nothing() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        let proven = LifecycleInputs {
            dirty: Some(false),
            upstream: Some("tracked origin/feat".to_owned()),
            forge: Some("open".to_owned()),
            ..LifecycleInputs::default()
        };
        let observe = |inputs: &LifecycleInputs, at| {
            store
                .sync_repo(
                    repo,
                    &[ObservedRef {
                        name: "feat".to_owned(),
                        head: None,
                        creation: None,
                        renamed_from: None,
                        rewritten: false,
                        commit: None,
                        activities: Vec::new(),
                        inputs: inputs.clone(),
                    }],
                    at,
                )
                .unwrap();
            store.load().work.branch(repo, "feat").unwrap().clone()
        };
        observe(&proven, 1_000);
        // Offline: upstream and forge go unproven. Nothing dates, and the
        // record keeps the proven readings.
        let offline = LifecycleInputs {
            upstream: None,
            forge: None,
            ..proven.clone()
        };
        let record = observe(&offline, 2_000);
        assert!(record.observations.is_empty());
        assert_eq!(record.inputs, proven);
        // Back online with the same answers: still nothing to observe.
        assert!(observe(&proven, 3_000).observations.is_empty());
        // A proven change observes, even when other fields are unproven -
        // and the observation never passes for work.
        let merged = LifecycleInputs {
            forge: Some("closed".to_owned()),
            upstream: None,
            ..proven.clone()
        };
        let record = observe(&merged, 4_000);
        assert_eq!(
            record.observations,
            vec![ObservationEvent {
                covered_by: None,
                source: ObservationSource::Lifecycle,
                observed_at_ms: 4_000,
                reasons: vec!["forge: open -> closed".to_owned()],
            }]
        );
        assert_eq!(newest_activity(&record.activities), None);
        assert_eq!(record.inputs.forge.as_deref(), Some("closed"));
        assert_eq!(
            record.inputs.upstream.as_deref(),
            Some("tracked origin/feat")
        );
        // The same holds for a path record.
        store
            .sync_path("/space", "/space", &proven, &[], 1_000)
            .unwrap();
        store
            .sync_path("/space", "/space", &offline, &[], 2_000)
            .unwrap();
        let work = store.load().work;
        assert!(work.path("/space").unwrap().observations.is_empty());

        // A record made while a field was unproven - a first run offline -
        // adopts the field's first proven value without dating it, then
        // dates a later proven change.
        let repo = "/fresh/.git";
        let first = |inputs: &LifecycleInputs, at| {
            store
                .sync_repo(
                    repo,
                    &[ObservedRef {
                        name: "feat".to_owned(),
                        head: None,
                        creation: None,
                        renamed_from: None,
                        rewritten: false,
                        commit: None,
                        activities: Vec::new(),
                        inputs: inputs.clone(),
                    }],
                    at,
                )
                .unwrap();
            store.load().work.branch(repo, "feat").unwrap().clone()
        };
        first(&offline, 1_000);
        let record = first(&proven, 2_000);
        assert!(record.observations.is_empty());
        assert_eq!(record.inputs, proven);
        let record = first(&merged, 3_000);
        assert_eq!(
            record.observations,
            vec![ObservationEvent {
                covered_by: None,
                source: ObservationSource::Lifecycle,
                observed_at_ms: 3_000,
                reasons: vec!["forge: open -> closed".to_owned()],
            }]
        );
        store
            .sync_path("/fresh-space", "/fresh-space", &offline, &[], 1_000)
            .unwrap();
        store
            .sync_path("/fresh-space", "/fresh-space", &proven, &[], 2_000)
            .unwrap();
        let work = store.load().work;
        let space = work.path("/fresh-space").unwrap();
        assert!(space.observations.is_empty());
        assert_eq!(space.inputs, proven);
    }

    #[test]
    fn a_path_record_carries_its_repo_and_a_legacy_one_adopts_it() {
        let temp = TempStore::new();
        let store = temp.store();
        store
            .sync_path("/wt", "/r/.git", &LifecycleInputs::default(), &[], 1_000)
            .unwrap();
        assert_eq!(
            store.load().work.paths["/wt"].repo.as_deref(),
            Some("/r/.git")
        );
        // A record from before the field reads `None`, then adopts the
        // repo on the next sync without dating a transition.
        let mut work = store.load().work;
        work.paths.get_mut("/wt").unwrap().repo = None;
        store.write_work(&work).unwrap();
        assert_eq!(store.load().work.paths["/wt"].repo, None);
        store
            .sync_path("/wt", "/r/.git", &LifecycleInputs::default(), &[], 2_000)
            .unwrap();
        let record = &store.load().work.paths["/wt"];
        assert_eq!(record.repo.as_deref(), Some("/r/.git"));
        assert!(record.observations.is_empty());
        // A project-space sync names only itself: a repo-less record
        // adopts it, but where a repo already claimed the record the
        // space displaces nothing.
        let mut work = store.load().work;
        work.paths.get_mut("/wt").unwrap().repo = None;
        store.write_work(&work).unwrap();
        store
            .sync_path("/wt", "/wt", &LifecycleInputs::default(), &[], 3_000)
            .unwrap();
        assert_eq!(store.load().work.paths["/wt"].repo.as_deref(), Some("/wt"));
        store
            .sync_path("/wt", "/r/.git", &LifecycleInputs::default(), &[], 4_000)
            .unwrap();
        store
            .sync_path("/wt", "/wt", &LifecycleInputs::default(), &[], 5_000)
            .unwrap();
        assert_eq!(
            store.load().work.paths["/wt"].repo.as_deref(),
            Some("/r/.git"),
            "the space did not displace the repo's claim"
        );
    }

    #[test]
    fn the_work_stamp_moves_with_every_write_and_only_then() {
        let temp = TempStore::new();
        let store = temp.store();
        assert_eq!(store.work_stamp(), None, "no file, no stamp");
        store
            .sync_path("/p", "/p", &LifecycleInputs::default(), &[], 1_000)
            .unwrap();
        let first = store.work_stamp().expect("the file exists");
        // A no-op sync leaves the file, and so the stamp, alone.
        store
            .sync_path("/p", "/p", &LifecycleInputs::default(), &[], 2_000)
            .unwrap();
        assert_eq!(store.work_stamp(), Some(first));
        // A same-length rewrite still moves it: the rename is a new inode.
        store
            .toggle_parked(&WorkIdentity::Path("/p".to_owned()))
            .unwrap();
        let parked = store.work_stamp().expect("the file exists");
        assert_ne!(parked, first);
        store
            .toggle_parked(&WorkIdentity::Path("/p".to_owned()))
            .unwrap();
        assert_ne!(store.work_stamp(), Some(parked));
    }

    #[test]
    fn a_sync_that_records_nothing_writes_nothing() {
        let temp = TempStore::new();
        let store = temp.store();
        // No refs observed and none on record: not even the directory is
        // created - a no-op touches nothing.
        store.sync_repo("/r/.git", &[], 1_000).unwrap();
        assert!(!temp.0.exists());
        // Identical fingerprints likewise: the file stays byte-identical.
        store
            .sync_path("/space", "/space", &LifecycleInputs::default(), &[], 1_000)
            .unwrap();
        let bytes = fs::read(temp.path(WORK)).unwrap();
        store
            .sync_path("/space", "/space", &LifecycleInputs::default(), &[], 2_000)
            .unwrap();
        assert_eq!(fs::read(temp.path(WORK)).unwrap(), bytes);
        // A changed proven fingerprint is a transition: it lands a
        // Lifecycle observation - never activity - and rewrites once.
        let clean = LifecycleInputs {
            dirty: Some(false),
            ..LifecycleInputs::default()
        };
        store
            .sync_path("/space", "/space", &clean, &[], 2_500)
            .unwrap();
        let inputs = LifecycleInputs {
            dirty: Some(true),
            ..LifecycleInputs::default()
        };
        store
            .sync_path("/space", "/space", &inputs, &[], 3_000)
            .unwrap();
        let work = store.load().work;
        let record = work.path("/space").unwrap();
        assert_eq!(
            record.observations,
            vec![ObservationEvent {
                covered_by: None,
                source: ObservationSource::Lifecycle,
                observed_at_ms: 3_000,
                reasons: vec!["dirty: clean -> dirty".to_owned()],
            }]
        );
        assert_eq!(newest_activity(&record.activities), None);
        // Toggling an identity no file can name is NotFound before a
        // single byte is written.
        let empty = TempStore::new();
        let err = Store::open(empty.0.clone())
            .toggle_parked(&WorkIdentity::Path("/space".to_owned()))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn parked_toggles_by_exact_identity_and_survives_restart() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        store
            .sync_repo(repo, &[obs("main", false), obs("feat", true)], 1_000)
            .unwrap();
        store
            .sync_path("/space", "/space", &LifecycleInputs::default(), &[], 1_000)
            .unwrap();
        let work = store.load().work;
        let id = work.branch(repo, "feat").unwrap().id.clone();
        // Branch by id, path by its canonical spelling.
        assert!(
            store
                .toggle_parked(&WorkIdentity::Branch(id.clone()))
                .unwrap()
        );
        assert!(
            store
                .toggle_parked(&WorkIdentity::Path("/space".to_owned()))
                .unwrap()
        );
        // A fresh Store over the same dir - the "restart" - reads them back.
        let reloaded = Store::open(temp.0.clone()).load().work;
        assert!(reloaded.branch(repo, "feat").unwrap().parked);
        assert!(!reloaded.branch(repo, "main").unwrap().parked);
        assert!(reloaded.path("/space").unwrap().parked);
        // Toggling back clears it.
        assert!(
            !store
                .toggle_parked(&WorkIdentity::Branch(id.clone()))
                .unwrap()
        );
        // Unknown identities are refused, not fabricated.
        let err = store
            .toggle_parked(&WorkIdentity::Branch("inonesuch".to_owned()))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let err = store
            .toggle_parked(&WorkIdentity::Path("/nowhere".to_owned()))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        // Park the incarnation, then close it: the closed record keeps
        // its parked flag, the old id refuses the toggle, and the
        // reappeared ref comes back as a new unparked incarnation.
        assert!(
            store
                .toggle_parked(&WorkIdentity::Branch(id.clone()))
                .unwrap()
        );
        store.sync_repo(repo, &[obs("main", false)], 2_000).unwrap();
        let err = store
            .toggle_parked(&WorkIdentity::Branch(id.clone()))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let work = store.load().work;
        assert!(work.branches[&id].parked, "the closed record keeps it");
        assert_eq!(work.branches[&id].ended_at, Some(2_000));
        store
            .sync_repo(repo, &[obs("main", false), obs("feat", true)], 3_000)
            .unwrap();
        let work = store.load().work;
        let feat = work.branch(repo, "feat").expect("feat reincarnated");
        assert_ne!(feat.id, id);
        assert!(!feat.parked, "the new incarnation starts unparked");
        assert!(work.branches.contains_key(&id));
    }

    #[test]
    fn work_mutations_hold_the_lock_and_refuse_an_unreadable_file() {
        let temp = TempStore::new();
        fs::create_dir_all(&temp.0).unwrap();
        let store = temp.store();
        // A record exists so the toggle reaches the lock rather than
        // answering NotFound off the missing file.
        store
            .sync_path("/p", "/p", &LifecycleInputs::default(), &[], 1_000)
            .unwrap();
        // A held lock fails every work mutation rather than clobbering.
        let held = Lock::acquire(&temp.path(LOCK)).unwrap();
        assert_eq!(
            store
                .sync_repo("/r", &[obs("a", false)], 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            store
                .sync_path("/p", "/p", &LifecycleInputs::default(), &[], 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            store
                .toggle_parked(&WorkIdentity::Path("/p".to_owned()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            store
                .sync_touches(
                    &[TouchPlacement {
                        conversation: "claude:s1".to_owned(),
                        branch: "i-x".to_owned(),
                        head: "a".to_owned(),
                        provenance: TouchProvenance::Cwd,
                        confidence: Confidence::Exact,
                    }],
                    &[],
                    1
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(held);
        // Malformed, malformed-at-every-schema and future-schema files
        // refuse the read-modify-write and keep their bytes.
        for bytes in [
            "{oops".to_owned(),
            serde_json::json!({"v": 99, "data": {}}).to_string(),
            serde_json::json!({"v": WORK_SCHEMA, "data": "bogus"}).to_string(),
        ] {
            fs::write(temp.path(WORK), &bytes).unwrap();
            let err = store
                .sync_repo("/r", &[obs("a", false)], 1_000)
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            let err = store
                .sync_path("/p", "/p", &LifecycleInputs::default(), &[], 1_000)
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            let err = store
                .toggle_parked(&WorkIdentity::Path("/p".to_owned()))
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            let err = store
                .sync_touches(
                    &[TouchPlacement {
                        conversation: "claude:s1".to_owned(),
                        branch: "i-x".to_owned(),
                        head: "a".to_owned(),
                        provenance: TouchProvenance::Cwd,
                        confidence: Confidence::Exact,
                    }],
                    &[],
                    1_000,
                )
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert_eq!(fs::read(temp.path(WORK)).unwrap(), bytes.as_bytes());
            // The best-effort read reports and answers an empty state.
            let loaded = store.load();
            assert!(loaded.work.branches.is_empty());
            assert!(loaded.errors.iter().any(|e| e.source == WORK));
        }
    }

    #[test]
    fn concurrent_work_updates_never_lose_a_record() {
        let temp = TempStore::new();
        fs::create_dir_all(&temp.0).unwrap();
        let store = temp.store();
        // Two repos syncing and two parked toggles racing: every write
        // lands whole under the one lock.
        let mut handles = Vec::new();
        for i in 0..4 {
            let store = Store::open(temp.0.clone());
            handles.push(std::thread::spawn(move || {
                store
                    .sync_repo(&format!("/r{i}"), &[obs("main", i % 2 == 0)], 1_000)
                    .expect("sync")
            }));
        }
        for h in handles {
            h.join().expect("sync joins");
        }
        let ids: Vec<String> = (0..4)
            .map(|i| {
                store
                    .load()
                    .work
                    .branch(&format!("/r{i}"), "main")
                    .unwrap()
                    .id
                    .clone()
            })
            .collect();
        let mut handles = Vec::new();
        for id in ids {
            let store = Store::open(temp.0.clone());
            handles.push(std::thread::spawn(move || {
                store
                    .toggle_parked(&WorkIdentity::Branch(id))
                    .expect("toggle")
            }));
        }
        for h in handles {
            assert!(h.join().expect("toggle joins"));
        }
        let work = store.load().work;
        assert_eq!(work.branches.len(), 4);
        assert!(work.branches.values().all(|r| r.parked));
    }

    #[test]
    fn enum_labels_spell_their_wire_names() {
        for (v, word) in [
            (Exec::Busy, "busy"),
            (Exec::Idle, "idle"),
            (Exec::Waiting, "waiting"),
            (Exec::Unknown, "unknown"),
        ] {
            assert_eq!(v.as_str(), word);
        }
        for (v, word) in [
            (NormEvent::Start, "start"),
            (NormEvent::Activity, "activity"),
            (NormEvent::Awaiting, "awaiting"),
            (NormEvent::End, "end"),
            (NormEvent::Error, "error"),
            (NormEvent::TeardownHint, "teardown_hint"),
        ] {
            assert_eq!(v.as_str(), word);
        }
        for (v, word) in [
            (ContinuityEvidence::FirstObservation, "first_observation"),
            (
                ContinuityEvidence::SameReflogCreation,
                "same_reflog_creation",
            ),
            (ContinuityEvidence::ProvenRename, "proven_rename"),
            (ContinuityEvidence::ForcePush, "force_push"),
            (ContinuityEvidence::Ambiguous, "ambiguous"),
        ] {
            assert_eq!(v.as_str(), word);
        }
        assert_eq!(TouchProvenance::Cwd.as_str(), "cwd");
        assert_eq!(TouchProvenance::ProviderBranch.as_str(), "provider_branch");
        assert_eq!(Confidence::Exact.as_str(), "exact");
    }

    #[test]
    fn rejected_records_survive_compaction_bounded_per_conversation() {
        let temp = TempStore::new();
        let store = temp.store();
        let mut high = record("claude", "s1", "Stop", NormEvent::End);
        high.pseq = Some(1_000);
        store.append(high).expect("append");
        // More stale records than a conversation keeps, then enough
        // traffic elsewhere to compact the tail.
        for pseq in 0..(REJECTED_KEPT as u64 + 4) {
            let mut stale = record("claude", "s1", "Stop", NormEvent::End);
            stale.pseq = Some(pseq);
            store.append(stale).expect("append");
        }
        for _ in 0..=COMPACT_AFTER {
            store
                .append(record("claude", "s2", "Stop", NormEvent::End))
                .expect("append");
        }
        let before = store.load();
        assert_eq!(before.rejected.len(), REJECTED_KEPT + 4);
        assert_eq!(
            fs::metadata(temp.path(JOURNAL)).unwrap().len(),
            0,
            "the tail compacted"
        );
        // The checkpoint carries the newest of them, in journal order.
        let after = store.load();
        assert_eq!(after.rejected.len(), REJECTED_KEPT, "{:?}", after.rejected);
        let kept: Vec<u64> = after.rejected.iter().filter_map(|r| r.pseq).collect();
        let want: Vec<u64> = (4..(REJECTED_KEPT as u64 + 4)).collect();
        assert_eq!(kept, want);
        // A later rejection appends after the carried ones.
        let mut stale = record("claude", "s1", "Stop", NormEvent::End);
        stale.pseq = Some(7);
        store.append(stale).expect("append");
        let later = store.load();
        assert_eq!(later.rejected.len(), REJECTED_KEPT + 1);
        assert_eq!(later.rejected.last().and_then(|r| r.pseq), Some(7));
    }

    #[test]
    fn a_load_reports_every_record_the_fold_rejected() {
        let temp = TempStore::new();
        let store = temp.store();
        let mut high = record("claude", "s1", "Stop", NormEvent::End);
        high.pseq = Some(5);
        store.append(high).expect("append");
        let mut dup = record("claude", "s1", "Stop", NormEvent::End);
        dup.pseq = Some(5);
        store.append(dup).expect("append");
        let mut stale = record("claude", "s1", "Stop", NormEvent::End);
        stale.pseq = Some(3);
        store.append(stale).expect("append");
        let loaded = store.load();
        assert_eq!(loaded.rejected.len(), 2, "{:?}", loaded.rejected);
        assert!(
            loaded
                .rejected
                .iter()
                .all(|r| r.conversation == conversation_key("claude", "s1"))
        );
        let reasons: Vec<&str> = loaded.rejected.iter().map(|r| r.reason.as_str()).collect();
        assert!(
            reasons.iter().any(|r| r.contains("below the high-water")),
            "{reasons:?}"
        );
        assert!(
            reasons.iter().any(|r| r.contains("already seen")),
            "{reasons:?}"
        );
    }

    fn obs_with_inputs(head: Option<&str>, inputs: LifecycleInputs) -> ObservedRef {
        ObservedRef {
            name: "feat".to_owned(),
            head: head.map(str::to_owned),
            creation: None,
            renamed_from: None,
            rewritten: false,
            commit: head.map(|sha| ObservedCommit {
                sha: sha.to_owned(),
                subject: Some("landed subject".to_owned()),
                at_ms: Some(7_777),
            }),
            activities: Vec::new(),
            inputs,
        }
    }

    #[test]
    fn first_observation_seeds_sources_without_emitting_an_event() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        let inputs = || LifecycleInputs {
            dirty: Some(true),
            worktree: Some(true),
            worktree_state: Some("healthy".to_owned()),
            working_tree: Some(WorkingTreeSnapshot {
                fingerprint: "ab".to_owned(),
                reasons: vec!["modified a.rs".to_owned()],
                newest_mtime_ms: None,
            }),
            ..LifecycleInputs::default()
        };
        store
            .sync_repo(
                repo,
                &[obs_with_inputs(Some("aaaaaaabbbbbbbb"), inputs())],
                1_000,
            )
            .unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat").expect("active");
        assert!(
            record.observations.is_empty(),
            "first proven values seed the baseline without an event: {:?}",
            record.observations
        );
        assert!(record.activities.is_empty(), "seeding is no work activity");
        let stamp = fs::metadata(temp.path(WORK)).unwrap().modified().unwrap();
        store
            .sync_repo(
                repo,
                &[obs_with_inputs(Some("aaaaaaabbbbbbbb"), inputs())],
                2_000,
            )
            .unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat").expect("active");
        assert!(
            record.observations.is_empty(),
            "an unchanged poll appends nothing"
        );
        assert_eq!(
            fs::metadata(temp.path(WORK)).unwrap().modified().unwrap(),
            stamp,
            "an unchanged poll writes nothing"
        );
    }

    #[test]
    fn presence_seeds_silently_and_only_proven_transitions_emit() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        let inputs = |worktree: Option<bool>, git_dir: Option<bool>| LifecycleInputs {
            worktree,
            git_dir,
            ..LifecycleInputs::default()
        };
        let sync = |inputs: LifecycleInputs, at: u64| {
            let mut o = obs("feat", false);
            o.inputs = inputs;
            store.sync_repo(repo, &[o], at).unwrap();
            store.load().work.branch(repo, "feat").unwrap().clone()
        };
        // Unproven, then proven: the first known values seed the
        // baseline without an event on either pass.
        let record = sync(inputs(None, None), 500);
        assert!(record.observations.is_empty(), "{:?}", record.observations);
        let record = sync(inputs(Some(true), Some(true)), 1_000);
        assert!(
            record.observations.is_empty(),
            "first proven values emit nothing: {:?}",
            record.observations
        );
        assert!(record.activities.is_empty());
        // Both sides proven on both passes: gone and missing land an
        // observation at the pass, found and restored the reverse - and
        // a presence transition can never pass for activity.
        sync(inputs(Some(false), Some(false)), 2_000);
        let record = sync(inputs(Some(true), Some(true)), 3_000);
        assert_eq!(
            record.observations,
            vec![
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::Lifecycle,
                    observed_at_ms: 2_000,
                    reasons: vec!["worktree gone".to_owned(), ".git missing".to_owned()],
                },
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::Lifecycle,
                    observed_at_ms: 3_000,
                    reasons: vec!["worktree found".to_owned(), ".git restored".to_owned()],
                },
            ]
        );
        assert_eq!(newest_activity(&record.activities), None);
        // A path record behaves the same.
        let path = "/wt";
        let sync_path = |inputs: LifecycleInputs, at: u64| {
            store.sync_path(path, "/r/.git", &inputs, &[], at).unwrap();
            store.load().work.path(path).unwrap().clone()
        };
        let record = sync_path(inputs(Some(true), Some(true)), 1_000);
        assert!(record.observations.is_empty(), "{:?}", record.observations);
        sync_path(inputs(Some(false), Some(false)), 2_000);
        let record = sync_path(inputs(Some(true), Some(true)), 3_000);
        assert_eq!(
            record.observations,
            vec![
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::Lifecycle,
                    observed_at_ms: 2_000,
                    reasons: vec!["worktree gone".to_owned(), ".git missing".to_owned()],
                },
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::Lifecycle,
                    observed_at_ms: 3_000,
                    reasons: vec!["worktree found".to_owned(), ".git restored".to_owned()],
                },
            ]
        );
    }

    #[test]
    fn each_changed_source_lands_its_own_event() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        let inputs = |dirty: bool, fingerprint: &str, state: &str| LifecycleInputs {
            dirty: Some(dirty),
            worktree: Some(true),
            worktree_state: Some(state.to_owned()),
            working_tree: Some(WorkingTreeSnapshot {
                fingerprint: fingerprint.to_owned(),
                reasons: vec![format!("modified {fingerprint}.rs")],
                newest_mtime_ms: None,
            }),
            ..LifecycleInputs::default()
        };
        store
            .sync_repo(
                repo,
                &[obs_with_inputs(
                    Some("aaaaaaabbbbbbbb"),
                    inputs(true, "aa", "healthy"),
                )],
                1_000,
            )
            .unwrap();
        store
            .sync_repo(
                repo,
                &[obs_with_inputs(
                    Some("bbbbbbbbcccccccc"),
                    inputs(true, "bb", "broken: .git missing"),
                )],
                2_000,
            )
            .unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat").expect("active");
        assert_eq!(
            record.observations,
            vec![
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::WorkingTree,
                    observed_at_ms: 2_000,
                    reasons: vec!["modified bb.rs".to_owned()],
                },
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::Lifecycle,
                    observed_at_ms: 2_000,
                    reasons: vec!["worktree state: healthy -> broken: .git missing".to_owned()],
                },
            ]
        );
        // The commit metadata proves its own occurrence: 7_777 is when
        // the commit happened, not when the pass saw it.
        assert_eq!(
            record.activities,
            vec![ActivityEvent {
                source: ActivitySource::Commit,
                occurred_at_ms: 7_777,
                reasons: vec!["bbbbbbb landed subject".to_owned()],
            }]
        );
        assert_eq!(newest_activity(&record.activities), Some(7_777));
        store
            .sync_repo(
                repo,
                &[obs_with_inputs(
                    Some("bbbbbbbbcccccccc"),
                    inputs(true, "bb", "broken: .git missing"),
                )],
                3_000,
            )
            .unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat").expect("active");
        assert_eq!(
            record.activities.len() + record.observations.len(),
            3,
            "unchanged polls append no duplicate"
        );
    }

    #[test]
    fn a_path_record_drives_the_same_events_from_its_inputs() {
        let temp = TempStore::new();
        let store = temp.store();
        let path = "/repos/detached";
        let inputs = |head: &str, fingerprint: &str| LifecycleInputs {
            worktree: Some(true),
            head: Some(head.to_owned()),
            commit: Some(ObservedCommit {
                sha: head.to_owned(),
                subject: None,
                at_ms: None,
            }),
            working_tree: Some(WorkingTreeSnapshot {
                fingerprint: fingerprint.to_owned(),
                reasons: vec![format!("modified {fingerprint}.rs")],
                newest_mtime_ms: None,
            }),
            ..LifecycleInputs::default()
        };
        store
            .sync_path(path, path, &inputs("aaaaaaa1", "aa"), &[], 1_000)
            .unwrap();
        store
            .sync_path(path, path, &inputs("bbbbbbb2", "bb"), &[], 2_000)
            .unwrap();
        let work = store.load().work;
        let record = work.path(path).expect("the record");
        assert_eq!(
            record.observations,
            vec![
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::Commit,
                    observed_at_ms: 2_000,
                    reasons: vec!["bbbbbbb".to_owned()],
                },
                ObservationEvent {
                    covered_by: None,
                    source: ObservationSource::WorkingTree,
                    observed_at_ms: 2_000,
                    reasons: vec!["modified bb.rs".to_owned()],
                },
            ]
        );
        // A HEAD move whose commit metadata carries no time fails
        // closed: it is a detection, never work.
        assert_eq!(newest_activity(&record.activities), None);
    }

    #[test]
    fn histories_dedupe_order_and_retain_per_source_independently() {
        // Activities deduplicate on (source, occurrence): a repeated
        // sighting of the same work merges reasons, never lists twice.
        let mut activities = Vec::new();
        let activity = |source: ActivitySource, at_ms: u64, reason: &str| ActivityEvent {
            source,
            occurred_at_ms: at_ms,
            reasons: vec![reason.to_owned()],
        };
        let commit = activity(ActivitySource::Commit, 5_000, "c");
        assert!(append_activity(&mut activities, commit.clone()));
        assert!(!append_activity(&mut activities, commit));
        assert_eq!(activities.len(), 1);
        assert!(append_activity(
            &mut activities,
            activity(ActivitySource::Commit, 5_000, "c2")
        ));
        assert_eq!(activities.len(), 1);
        assert_eq!(
            activities[0].reasons,
            vec!["c".to_owned(), "c2".to_owned()],
            "same work, merged reasons"
        );
        assert!(append_activity(
            &mut activities,
            activity(ActivitySource::Conversation, 1_000, "s")
        ));
        assert_eq!(activities[0].source, ActivitySource::Conversation);
        for i in 0..101u64 {
            append_activity(
                &mut activities,
                activity(ActivitySource::Commit, 10_000 + i, &format!("c{i}")),
            );
            append_activity(
                &mut activities,
                activity(ActivitySource::WorkingTree, 10_000 + i, &format!("w{i}")),
            );
        }
        let commits = activities
            .iter()
            .filter(|e| e.source == ActivitySource::Commit)
            .count();
        let trees = activities
            .iter()
            .filter(|e| e.source == ActivitySource::WorkingTree)
            .count();
        assert_eq!(commits, HISTORY_RETAIN_PER_SOURCE);
        assert_eq!(trees, HISTORY_RETAIN_PER_SOURCE);
        assert!(
            activities
                .iter()
                .any(|e| e.source == ActivitySource::Commit && e.occurred_at_ms == 10_100),
            "the newest commit survives"
        );
        assert!(
            !activities
                .iter()
                .any(|e| e.source == ActivitySource::Commit && e.occurred_at_ms == 5_000),
            "the oldest commit ages out"
        );

        // Observations deduplicate on identity: the same detection again
        // is a no-op, a different detection at the same time is kept.
        let mut observations = Vec::new();
        let observation = |source: ObservationSource, at_ms: u64, reason: &str| ObservationEvent {
            covered_by: None,
            source,
            observed_at_ms: at_ms,
            reasons: vec![reason.to_owned()],
        };
        let gone = observation(ObservationSource::Lifecycle, 5_000, "worktree gone");
        assert!(append_observation(&mut observations, gone.clone()));
        assert!(!append_observation(&mut observations, gone));
        assert_eq!(observations.len(), 1);
        assert!(append_observation(
            &mut observations,
            observation(ObservationSource::WorkingTree, 1_000, "modified a")
        ));
        assert_eq!(observations[0].source, ObservationSource::WorkingTree);
        for i in 0..101u64 {
            append_observation(
                &mut observations,
                observation(ObservationSource::Lifecycle, 10_000 + i, &format!("l{i}")),
            );
        }
        let lifecycle = observations
            .iter()
            .filter(|e| e.source == ObservationSource::Lifecycle)
            .count();
        assert_eq!(lifecycle, HISTORY_RETAIN_PER_SOURCE);
        assert_eq!(
            observations
                .iter()
                .filter(|e| e.source == ObservationSource::WorkingTree)
                .count(),
            1,
            "the other source's bound is untouched"
        );
        // Newest activity is derived, never stored: a fresh observation
        // cannot change it.
        let newest = newest_activity(&activities);
        assert!(append_observation(
            &mut observations,
            observation(ObservationSource::Lifecycle, u64::MAX - 1, "just now")
        ));
        assert_eq!(newest_activity(&activities), newest);
    }

    #[test]
    fn event_sources_round_trip_their_wire_names() {
        for (source, name) in [
            (ActivitySource::Commit, "commit"),
            (ActivitySource::Reflog, "reflog"),
            (ActivitySource::WorkingTree, "working_tree"),
            (ActivitySource::Forge, "forge"),
            (ActivitySource::Conversation, "conversation"),
        ] {
            let json = serde_json::to_string(&source).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<ActivitySource>(&json).unwrap(),
                source
            );
        }
        for (source, name) in [
            (ObservationSource::Commit, "commit"),
            (ObservationSource::WorkingTree, "working_tree"),
            (ObservationSource::Forge, "forge"),
            (ObservationSource::Conversation, "conversation"),
            (ObservationSource::Lifecycle, "lifecycle"),
        ] {
            let json = serde_json::to_string(&source).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<ObservationSource>(&json).unwrap(),
                source
            );
        }
    }

    #[test]
    fn a_lifecycle_transition_names_every_changed_field() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        let inputs = |worktree: bool,
                      path: &str,
                      admin: &str,
                      dirty: bool,
                      ahead: u64,
                      behind: u64,
                      unpushed: u64,
                      upstream: &str,
                      landed: &str,
                      forge: &str,
                      pipeline: &str,
                      state: &str| {
            LifecycleInputs {
                dirty: Some(dirty),
                worktree: Some(worktree),
                worktree_path: Some(path.to_owned()),
                admin_id: Some(admin.to_owned()),
                ahead: Some(ahead),
                behind: Some(behind),
                unpushed: Some(unpushed),
                upstream: Some(upstream.to_owned()),
                landed: Some(landed.to_owned()),
                forge: Some(forge.to_owned()),
                pipeline: Some(pipeline.to_owned()),
                worktree_state: Some(state.to_owned()),
                ..LifecycleInputs::default()
            }
        };
        let first = |inputs: LifecycleInputs, at: u64| {
            let mut o = obs("feat", false);
            o.inputs = inputs;
            store.sync_repo(repo, &[o], at)
        };
        first(
            inputs(
                true,
                "/a",
                "a1",
                false,
                1,
                0,
                2,
                "tracked origin/feat",
                "no",
                "open",
                "success",
                "healthy",
            ),
            1_000,
        )
        .unwrap();
        first(
            inputs(
                false,
                "/b",
                "a2",
                true,
                3,
                1,
                0,
                "gone",
                "merged",
                "merged",
                "failed",
                "broken: missing",
            ),
            2_000,
        )
        .unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat").expect("the record");
        assert_eq!(
            record.observations,
            vec![ObservationEvent {
                covered_by: None,
                source: ObservationSource::Lifecycle,
                observed_at_ms: 2_000,
                reasons: vec![
                    "worktree gone".to_owned(),
                    "path: /a -> /b".to_owned(),
                    "admin id: a1 -> a2".to_owned(),
                    "dirty: clean -> dirty".to_owned(),
                    "ahead: 1 -> 3".to_owned(),
                    "behind: 0 -> 1".to_owned(),
                    "unpushed: 2 -> 0".to_owned(),
                    "upstream: tracked origin/feat -> gone".to_owned(),
                    "landed: no -> merged".to_owned(),
                    "forge: open -> merged".to_owned(),
                    "pipeline: success -> failed".to_owned(),
                    "worktree state: healthy -> broken: missing".to_owned(),
                ],
            }]
        );
        // A transition is detection, not work: the derived activity
        // stays empty.
        assert_eq!(newest_activity(&record.activities), None);
    }

    #[test]
    fn a_proven_rename_with_a_moved_tip_writes_a_commit_observation() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        let mut o = obs("feat", false);
        o.head = Some("aaaaaaa0000000".to_owned());
        store.sync_repo(repo, &[o], 1_000).unwrap();
        let mut renamed = obs("feat2", false);
        renamed.head = Some("bbbbbbb1111111".to_owned());
        renamed.renamed_from = Some("feat".to_owned());
        store.sync_repo(repo, &[renamed], 2_000).unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat2").expect("the record");
        assert_eq!(
            record.observations,
            vec![ObservationEvent {
                covered_by: None,
                source: ObservationSource::Commit,
                observed_at_ms: 2_000,
                reasons: vec!["bbbbbbb".to_owned()],
            }],
            "a head move without commit metadata lands a detection"
        );
        assert_eq!(
            newest_activity(&record.activities),
            None,
            "an undated tip move fails closed - never activity"
        );
    }

    #[test]
    fn a_proven_rename_lands_the_passes_own_transitions() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        store.sync_repo(repo, &[obs("feat", false)], 1_000).unwrap();
        // The rename pass is an observation like any other: a proven
        // lifecycle change dates this pass, not the next one to find
        // the moved record.
        let mut renamed = obs("feat2", true);
        renamed.renamed_from = Some("feat".to_owned());
        store.sync_repo(repo, &[renamed], 2_000).unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat2").expect("the record");
        assert_eq!(
            record.observations,
            vec![ObservationEvent {
                covered_by: None,
                source: ObservationSource::Lifecycle,
                observed_at_ms: 2_000,
                reasons: vec!["dirty: clean -> dirty".to_owned()],
            }]
        );
        assert_eq!(record.inputs.dirty, Some(true));
    }

    #[test]
    fn session_updates_backfill_then_emit_once_per_newer_turn() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        store.sync_repo(repo, &[obs("feat", false)], 1_000).unwrap();
        store
            .sync_path(
                "/spaces/a",
                "/spaces/a",
                &LifecycleInputs::default(),
                &[],
                1_000,
            )
            .unwrap();
        let id = store
            .load()
            .work
            .branch(repo, "feat")
            .expect("active")
            .id
            .clone();
        let absent = TempStore::new();
        absent
            .store()
            .sync_session_updates(&[SessionUpdate {
                identity: UpdateIdentity::Path("/never".to_owned()),
                conversation: "c".to_owned(),
                at_ms: 1,
                reason: "r".to_owned(),
                context: SessionContext::default(),
            }])
            .unwrap();
        let conv = conversation_key("claude", "s1");
        let update = |at_ms: u64, reason: &str, identity: &UpdateIdentity| SessionUpdate {
            identity: identity.clone(),
            conversation: conv.clone(),
            at_ms,
            reason: reason.to_owned(),
            context: SessionContext::default(),
        };
        let branch = UpdateIdentity::Branch(id.clone());
        let path = UpdateIdentity::Path("/spaces/a".to_owned());
        // A first sighting backfills at its own source time: the cursor
        // alone never proved when the turn happened.
        store
            .sync_session_updates(&[update(5_000, "8f423bbb old", &branch)])
            .unwrap();
        let work = store.load().work;
        let record = work.branches.get(&id).expect("the record");
        assert_eq!(record.session_activity.get(&conv), Some(&5_000));
        assert_eq!(
            record.activities,
            vec![ActivityEvent {
                source: ActivitySource::Conversation,
                occurred_at_ms: 5_000,
                reasons: vec!["8f423bbb old".to_owned()],
            }],
            "the first sighting is source-backed history too"
        );
        store
            .sync_session_updates(&[update(6_000, "8f423bbb new", &branch)])
            .unwrap();
        let work = store.load().work;
        let record = work.branches.get(&id).expect("the record");
        assert_eq!(
            record.activities,
            vec![
                ActivityEvent {
                    source: ActivitySource::Conversation,
                    occurred_at_ms: 5_000,
                    reasons: vec!["8f423bbb old".to_owned()],
                },
                ActivityEvent {
                    source: ActivitySource::Conversation,
                    occurred_at_ms: 6_000,
                    reasons: vec!["8f423bbb new".to_owned()],
                },
            ]
        );
        assert_eq!(newest_activity(&record.activities), Some(6_000));
        store
            .sync_session_updates(&[
                update(6_000, "8f423bbb new", &branch),
                update(5_500, "stale", &branch),
            ])
            .unwrap();
        let work = store.load().work;
        assert_eq!(
            work.branches.get(&id).expect("the record").activities.len(),
            2,
            "equal or older turns append nothing"
        );
        // First sightings on a path record backfill the same way; three
        // conversations at one instant merge into one event's reasons.
        let conv2 = conversation_key("claude", "s2");
        let update2 = |at_ms: u64, reason: &str| SessionUpdate {
            identity: path.clone(),
            conversation: conv2.clone(),
            at_ms,
            reason: reason.to_owned(),
            context: SessionContext::default(),
        };
        let conv3 = conversation_key("claude", "s3");
        let update3 = |at_ms: u64, reason: &str| SessionUpdate {
            identity: path.clone(),
            conversation: conv3.clone(),
            at_ms,
            reason: reason.to_owned(),
            context: SessionContext::default(),
        };
        store
            .sync_session_updates(&[
                update(6_500, "first", &path),
                update2(6_500, "first"),
                update3(6_500, "first"),
            ])
            .unwrap();
        let record = store
            .load()
            .work
            .path("/spaces/a")
            .expect("the record")
            .clone();
        assert_eq!(
            record.activities,
            vec![ActivityEvent {
                source: ActivitySource::Conversation,
                occurred_at_ms: 6_500,
                reasons: vec!["first".to_owned()],
            }],
            "one event per identity and timestamp, reasons deduped"
        );
        store
            .sync_session_updates(&[
                update(7_000, "bbbb", &path),
                update2(7_000, "aaaa"),
                update3(7_000, "aaaa"),
            ])
            .unwrap();
        let work = store.load().work;
        let record = work.path("/spaces/a").expect("the record");
        assert_eq!(
            record.activities,
            vec![
                ActivityEvent {
                    source: ActivitySource::Conversation,
                    occurred_at_ms: 6_500,
                    reasons: vec!["first".to_owned()],
                },
                ActivityEvent {
                    source: ActivitySource::Conversation,
                    occurred_at_ms: 7_000,
                    reasons: vec!["aaaa".to_owned(), "bbbb".to_owned()],
                },
            ],
            "one event per identity and timestamp, reasons sorted and deduped"
        );
        // A repeat of an already-seen instant lands nothing twice.
        store
            .sync_session_updates(&[update2(7_000, "aaaa")])
            .unwrap();
        let work = store.load().work;
        assert_eq!(
            work.path("/spaces/a").expect("the record").activities.len(),
            2,
            "a replayed update is a no-op"
        );
        // The same instant and reason arriving from a new conversation in
        // a *separate* transaction: the event dedupes into the one
        // already stored, but the cursor must still persist - it is the
        // record's own change.
        let conv4 = conversation_key("claude", "s4");
        let update4 = |at_ms: u64, reason: &str| SessionUpdate {
            identity: path.clone(),
            conversation: conv4.clone(),
            at_ms,
            reason: reason.to_owned(),
            context: SessionContext::default(),
        };
        store
            .sync_session_updates(&[update4(7_000, "aaaa")])
            .unwrap();
        let work = store.load().work;
        let record = work.path("/spaces/a").expect("the record");
        assert_eq!(record.session_activity.get(&conv4), Some(&7_000));
        assert_eq!(
            record.activities.len(),
            2,
            "the identical event merged, not duplicated"
        );
        // Repeating that same update is a full no-op: cursor and event
        // already recorded, the file's bytes unchanged.
        let bytes = fs::read(temp.path(WORK)).unwrap();
        store
            .sync_session_updates(&[update4(7_000, "aaaa")])
            .unwrap();
        assert_eq!(fs::read(temp.path(WORK)).unwrap(), bytes);
        store.sync_session_updates(&[]).unwrap();
        store
            .sync_session_updates(&[update(
                9_000,
                "gone",
                &UpdateIdentity::Branch("nope".to_owned()),
            )])
            .unwrap();
    }

    #[test]
    fn records_written_before_the_event_histories_deserialize() {
        let temp = TempStore::new();
        fs::create_dir_all(&temp.0).unwrap();
        fs::write(
            temp.path(WORK),
            serde_json::json!({
                "v": WORK_SCHEMA,
                "data": {
                    "branches": {
                        "i1": {
                            "id": "i1",
                            "repo": "/r/.git",
                            "ref_name": "feat",
                            "first_observed_at": 1,
                            "last_observed_at": 1,
                            "continuity_evidence": "first_observation"
                        }
                    },
                    "paths": {"/p": {}}
                }
            })
            .to_string(),
        )
        .unwrap();
        let (work, errors) = temp.store().work();
        assert!(errors.is_empty(), "{errors:?}");
        let record = work.branches.get("i1").expect("the record");
        assert!(record.activities.is_empty() && record.observations.is_empty());
        assert!(record.session_activity.is_empty());
        assert!(record.session_context.is_empty());
        let path = work.path("/p").expect("the path");
        assert!(path.activities.is_empty() && path.observations.is_empty());
        assert!(path.session_context.is_empty());
    }

    /// A record with a conversation cursor and context written by this
    /// build reads back identically - additive fields round-trip.
    #[test]
    fn session_context_roundtrips_through_the_file() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        store.sync_repo(repo, &[obs("feat", false)], 1_000).unwrap();
        let id = store
            .load()
            .work
            .branch(repo, "feat")
            .expect("active")
            .id
            .clone();
        let conv = conversation_key("claude", "s1");
        store
            .sync_session_updates(&[SessionUpdate {
                identity: UpdateIdentity::Branch(id.clone()),
                conversation: conv.clone(),
                at_ms: 5_000,
                reason: "8f423bbb title".to_owned(),
                context: SessionContext {
                    title: Some("a title".to_owned()),
                    prompt_excerpt: Some("do the thing".to_owned()),
                },
            }])
            .unwrap();
        let record = store
            .load()
            .work
            .branches
            .get(&id)
            .expect("the record")
            .clone();
        assert_eq!(
            record.session_context.get(&conv),
            Some(&SessionContext {
                title: Some("a title".to_owned()),
                prompt_excerpt: Some("do the thing".to_owned()),
            })
        );
    }

    #[test]
    fn session_context_enriches_equal_time_and_yields_to_newer() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        store.sync_repo(repo, &[obs("feat", false)], 1_000).unwrap();
        store
            .sync_path(
                "/spaces/a",
                "/spaces/a",
                &LifecycleInputs::default(),
                &[],
                1_000,
            )
            .unwrap();
        let id = store
            .load()
            .work
            .branch(repo, "feat")
            .expect("active")
            .id
            .clone();
        let conv = conversation_key("claude", "s1");
        let update = |at_ms: u64, title: Option<&str>, prompt: Option<&str>| SessionUpdate {
            identity: UpdateIdentity::Branch(id.clone()),
            conversation: conv.clone(),
            at_ms,
            reason: "turn".to_owned(),
            context: SessionContext {
                title: title.map(str::to_owned),
                prompt_excerpt: prompt.map(str::to_owned),
            },
        };
        // A first sighting captures what it carries - raw, never the
        // whole prompt beyond the bound.
        let long = "p".repeat(200);
        store
            .sync_session_updates(&[update(5_000, Some("first title"), Some(&long))])
            .unwrap();
        let record = store
            .load()
            .work
            .branches
            .get(&id)
            .expect("the record")
            .clone();
        let context = record.session_context.get(&conv).expect("context");
        assert_eq!(context.title.as_deref(), Some("first title"));
        let stored = context.prompt_excerpt.as_deref().expect("excerpt");
        assert!(
            unicode_width::UnicodeWidthStr::width(text::escape_text(stored).as_str())
                <= text::PROMPT_EXCERPT_CELLS,
            "the stored excerpt renders within the bound: {stored}"
        );
        assert!(stored.ends_with('…'));
        assert!(stored.len() < long.len(), "the full prompt never persists");
        assert_eq!(record.activities.len(), 1);

        // Equal-time updates fill only missing fields: the stored
        // context stays, no event lands, the cursor never moves.
        store
            .sync_session_updates(&[update(5_000, Some("other title"), Some("other prompt"))])
            .unwrap();
        let record = store.load().work.branches.get(&id).unwrap().clone();
        assert_eq!(
            record.session_context.get(&conv).unwrap().title.as_deref(),
            Some("first title"),
            "equal-time context never replaces"
        );
        assert_eq!(record.activities.len(), 1, "enrichment lands no event");
        assert_eq!(newest_activity(&record.activities), Some(5_000));

        // A stale update is a complete no-op, context included: the
        // file's bytes do not move.
        let bytes = fs::read(temp.path(WORK)).unwrap();
        store
            .sync_session_updates(&[update(4_000, Some("stale"), Some("stale prompt"))])
            .unwrap();
        assert_eq!(fs::read(temp.path(WORK)).unwrap(), bytes);

        // A newer turn carrying no prompt keeps the captured excerpt as
        // fallback while a changed title replaces.
        store
            .sync_session_updates(&[update(6_000, Some("second title"), None)])
            .unwrap();
        let record = store.load().work.branches.get(&id).unwrap().clone();
        let context = record.session_context.get(&conv).unwrap();
        assert_eq!(context.title.as_deref(), Some("second title"));
        assert_eq!(context.prompt_excerpt.as_deref(), Some(stored));
        assert_eq!(newest_activity(&record.activities), Some(6_000));

        // The whitespace-only and empty texts count as missing.
        store
            .sync_session_updates(&[update(7_000, Some("  \t "), Some("   "))])
            .unwrap();
        let record = store.load().work.branches.get(&id).unwrap().clone();
        let context = record.session_context.get(&conv).unwrap();
        assert_eq!(context.title.as_deref(), Some("second title"));
        assert_eq!(context.prompt_excerpt.as_deref(), Some(stored));

        // An equal-time update fills a still-missing field without
        // disturbing the rest: a fresh conversation on the same record
        // gains context while its cursor and event land once.
        let conv2 = conversation_key("claude", "s2");
        let update2 = |at_ms: u64, title: Option<&str>, prompt: Option<&str>| SessionUpdate {
            identity: UpdateIdentity::Path("/spaces/a".to_owned()),
            conversation: conv2.clone(),
            at_ms,
            reason: "turn".to_owned(),
            context: SessionContext {
                title: title.map(str::to_owned),
                prompt_excerpt: prompt.map(str::to_owned),
            },
        };
        store
            .sync_session_updates(&[update2(8_000, Some("space title"), None)])
            .unwrap();
        store
            .sync_session_updates(&[update2(8_000, None, Some("late prompt"))])
            .unwrap();
        let work = store.load().work;
        let path = work.path("/spaces/a").expect("the path record");
        let context = path.session_context.get(&conv2).expect("context");
        assert_eq!(context.title.as_deref(), Some("space title"));
        assert_eq!(context.prompt_excerpt.as_deref(), Some("late prompt"));
        assert_eq!(path.activities.len(), 1, "one event for the sighting");
        assert_eq!(path.session_activity.get(&conv2), Some(&8_000));

        // The mirror fill order: a record holding only a prompt gains the
        // title at equal time, its prompt untouched by the update's own.
        let conv3 = conversation_key("claude", "s3");
        let update3 = |title: Option<&str>, prompt: Option<&str>| SessionUpdate {
            identity: UpdateIdentity::Path("/spaces/a".to_owned()),
            conversation: conv3.clone(),
            at_ms: 9_000,
            reason: "turn".to_owned(),
            context: SessionContext {
                title: title.map(str::to_owned),
                prompt_excerpt: prompt.map(str::to_owned),
            },
        };
        store
            .sync_session_updates(&[update3(None, Some("first prompt"))])
            .unwrap();
        store
            .sync_session_updates(&[update3(Some("added title"), Some("other"))])
            .unwrap();
        let work = store.load().work;
        let path = work.path("/spaces/a").expect("the path record");
        let context = path.session_context.get(&conv3).expect("context");
        assert_eq!(context.title.as_deref(), Some("added title"));
        assert_eq!(context.prompt_excerpt.as_deref(), Some("first prompt"));

        // Context is per record: the same conversation key on the path
        // record never read the branch record's capture, and neither
        // record's context shows under the other.
        assert!(!path.session_context.contains_key(&conv));
        let branch = work.branches.get(&id).unwrap();
        assert!(!branch.session_context.contains_key(&conv2));
    }

    #[test]
    fn session_context_fills_a_cursor_only_record_without_new_activity() {
        // A record written before context existed carries the cursor but
        // no capture: the next equal-time update compacts it in place.
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        store.sync_repo(repo, &[obs("feat", false)], 1_000).unwrap();
        let id = store
            .load()
            .work
            .branch(repo, "feat")
            .expect("active")
            .id
            .clone();
        let conv = conversation_key("claude", "s1");
        let update = |context: SessionContext| SessionUpdate {
            identity: UpdateIdentity::Branch(id.clone()),
            conversation: conv.clone(),
            at_ms: 5_000,
            reason: "turn".to_owned(),
            context,
        };
        store
            .sync_session_updates(&[update(SessionContext::default())])
            .unwrap();
        let record = store.load().work.branches.get(&id).unwrap().clone();
        assert_eq!(record.session_activity.get(&conv), Some(&5_000));
        assert_eq!(record.activities.len(), 1);
        assert!(!record.session_context.contains_key(&conv));
        let last = newest_activity(&record.activities);
        // The equal-time enrichment: context lands, cursor and activity
        // stay exactly where they were.
        store
            .sync_session_updates(&[update(SessionContext {
                title: Some("the title".to_owned()),
                prompt_excerpt: Some("the prompt".to_owned()),
            })])
            .unwrap();
        let record = store.load().work.branches.get(&id).unwrap().clone();
        assert_eq!(
            record.session_context.get(&conv),
            Some(&SessionContext {
                title: Some("the title".to_owned()),
                prompt_excerpt: Some("the prompt".to_owned()),
            })
        );
        assert_eq!(record.activities.len(), 1, "no event for enrichment");
        assert_eq!(newest_activity(&record.activities), last);
        // A repeated identical enrichment is a byte-exact no-op.
        let bytes = fs::read(temp.path(WORK)).unwrap();
        store
            .sync_session_updates(&[update(SessionContext {
                title: Some("the title".to_owned()),
                prompt_excerpt: Some("the prompt".to_owned()),
            })])
            .unwrap();
        assert_eq!(fs::read(temp.path(WORK)).unwrap(), bytes);
    }

    #[test]
    fn pass_supplied_activities_backfill_and_dedupe() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        // The first pass proves old work outright: the events it supplies
        // land on the new record at their own occurrence times.
        let mut o = obs("feat", false);
        o.activities = vec![
            ActivityEvent {
                source: ActivitySource::Commit,
                occurred_at_ms: 500,
                reasons: vec!["aaaaaaa old".to_owned()],
            },
            ActivityEvent {
                source: ActivitySource::Reflog,
                occurred_at_ms: 600,
                reasons: vec!["reflog work".to_owned()],
            },
        ];
        store.sync_repo(repo, &[o.clone()], 1_000).unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat").expect("active");
        assert_eq!(record.activities, o.activities);
        assert_eq!(newest_activity(&record.activities), Some(600));
        // The same pass seen again is a dedupe, not a second event.
        store.sync_repo(repo, &[o], 2_000).unwrap();
        let record = store.load().work.branch(repo, "feat").unwrap().clone();
        assert_eq!(record.activities.len(), 2);
        // The same occurrence from a different angle merges reasons.
        let mut o = obs("feat", false);
        o.activities = vec![ActivityEvent {
            source: ActivitySource::Commit,
            occurred_at_ms: 500,
            reasons: vec!["other view".to_owned()],
        }];
        store.sync_repo(repo, &[o], 3_000).unwrap();
        let record = store.load().work.branch(repo, "feat").unwrap().clone();
        assert_eq!(record.activities.len(), 2);
        assert_eq!(
            record.activities[0].reasons,
            vec!["aaaaaaa old".to_owned(), "other view".to_owned()]
        );
        // Path records seed the same way on creation.
        store
            .sync_path(
                "/p",
                "/p",
                &LifecycleInputs::default(),
                &[ActivityEvent {
                    source: ActivitySource::Reflog,
                    occurred_at_ms: 700,
                    reasons: vec!["reflog work".to_owned()],
                }],
                4_000,
            )
            .unwrap();
        let work = store.load().work;
        let record = work.path("/p").expect("the record");
        assert_eq!(newest_activity(&record.activities), Some(700));
    }

    #[test]
    fn a_changed_tree_activity_dates_at_the_mtime_not_the_pass() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        let inputs = |fingerprint: &str, mtime: Option<u64>| LifecycleInputs {
            working_tree: Some(WorkingTreeSnapshot {
                fingerprint: fingerprint.to_owned(),
                reasons: vec!["modified a.rs".to_owned()],
                newest_mtime_ms: mtime,
            }),
            ..LifecycleInputs::default()
        };
        let mut o = obs("feat", false);
        o.inputs = inputs("aa", Some(100));
        store.sync_repo(repo, &[o], 1_000).unwrap();
        // A dirty tree's first reading seeds the baseline; the pinned
        // mtime still lands as provable work at its own time.
        let mut o = obs("feat", false);
        o.inputs = inputs("bb", Some(300));
        store.sync_repo(repo, &[o], 2_000).unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat").expect("active");
        assert_eq!(
            record.activities,
            vec![ActivityEvent {
                source: ActivitySource::WorkingTree,
                occurred_at_ms: 300,
                reasons: vec!["modified a.rs".to_owned()],
            }]
        );
        assert_eq!(
            record.observations,
            vec![ObservationEvent {
                covered_by: Some(ActivityReference {
                    source: ActivitySource::WorkingTree,
                    occurred_at_ms: 300,
                }),
                source: ObservationSource::WorkingTree,
                observed_at_ms: 2_000,
                reasons: vec!["modified a.rs".to_owned()],
            }]
        );
        // A transition with no remaining changed-path mtime - a deletion
        // or a return to clean - observes at the pass and adds no work.
        let mut o = obs("feat", false);
        o.inputs = inputs("cc", None);
        store.sync_repo(repo, &[o], 3_000).unwrap();
        let work = store.load().work;
        let record = work.branch(repo, "feat").expect("active");
        assert_eq!(
            record
                .observations
                .iter()
                .filter(|e| e.source == ObservationSource::WorkingTree)
                .count(),
            2
        );
        assert_eq!(newest_activity(&record.activities), Some(300));
        // The same holds for a path record.
        let inputs = |f: &str, m: Option<u64>| {
            let mut i = inputs(f, m);
            i.dirty = Some(true);
            i
        };
        store
            .sync_path("/wt", "/r/.git", &inputs("aa", None), &[], 1_000)
            .unwrap();
        store
            .sync_path("/wt", "/r/.git", &inputs("bb", None), &[], 2_000)
            .unwrap();
        let work = store.load().work;
        let record = work.path("/wt").expect("the record");
        assert!(record.activities.is_empty());
        assert_eq!(
            record
                .observations
                .iter()
                .filter(|e| e.source == ObservationSource::WorkingTree)
                .count(),
            1
        );
    }

    #[test]
    fn a_dated_dirty_transition_links_its_observation_to_the_activity() {
        let temp = TempStore::new();
        let store = temp.store();
        let repo = "/repo/.git";
        // A fixed reason under changing fingerprints: the same changed
        // path recurs, each occurrence its own link.
        let inputs = |fingerprint: &str, mtime: Option<u64>| LifecycleInputs {
            working_tree: Some(WorkingTreeSnapshot {
                fingerprint: fingerprint.to_owned(),
                reasons: vec!["modified repeated.rs".to_owned()],
                newest_mtime_ms: mtime,
            }),
            ..LifecycleInputs::default()
        };
        // The first reading seeds the baseline; every later fingerprint
        // is a transition.
        let mut o = obs("feat", false);
        o.inputs = inputs("aa", Some(100));
        store.sync_repo(repo, &[o], 1_000).unwrap();
        // A dated dirty transition emits both events in one pass: the
        // observation links to the activity it duplicates by source and
        // occurrence, while its own time stays the pass's - never the
        // mtime's.
        let mut o = obs("feat", false);
        o.inputs = inputs("bb", Some(300));
        store.sync_repo(repo, &[o], 2_000).unwrap();
        let record = store.load().work.branch(repo, "feat").unwrap().clone();
        let observation = &record.observations[0];
        assert_eq!(observation.observed_at_ms, 2_000);
        assert_eq!(
            observation.covered_by,
            Some(ActivityReference {
                source: ActivitySource::WorkingTree,
                occurred_at_ms: 300,
            })
        );
        assert!(observation.is_covered_by(&record.activities));
        // The link round-trips through the file, and a legacy event with
        // no `covered_by` reads as unlinked under the same schema.
        let json = serde_json::to_value(observation).unwrap();
        assert_eq!(
            json["covered_by"],
            serde_json::json!({"source": "working_tree", "occurred_at_ms": 300})
        );
        let mut legacy = json.clone();
        legacy.as_object_mut().unwrap().remove("covered_by");
        let legacy: ObservationEvent = serde_json::from_value(legacy).unwrap();
        assert_eq!(legacy.covered_by, None);
        assert_eq!(legacy.observed_at_ms, 2_000);
        // An unlinked event serializes no key at all.
        assert!(serde_json::to_value(&legacy).unwrap()["covered_by"].is_null());
        // The same dirty path again at a newer mtime links to its own
        // occurrence: two same-named observations stay distinguishable.
        let mut o = obs("feat", false);
        o.inputs = inputs("cc", Some(500));
        store.sync_repo(repo, &[o], 3_000).unwrap();
        let record = store.load().work.branch(repo, "feat").unwrap().clone();
        let links: Vec<u64> = record
            .observations
            .iter()
            .map(|e| e.covered_by.unwrap().occurred_at_ms)
            .collect();
        assert_eq!(links, vec![300, 500], "{:?}", record.observations);
        assert_eq!(
            record.observations[0].reasons, record.observations[1].reasons,
            "the repeated path reads identically; only the link times differ"
        );
        assert!(
            record
                .observations
                .iter()
                .all(|e| e.is_covered_by(&record.activities))
        );
        // A clean, deleted or metadata-less transition proves no mtime:
        // its observation stays unlinked.
        let mut o = obs("feat", false);
        o.inputs = inputs("dd", None);
        store.sync_repo(repo, &[o], 4_000).unwrap();
        let record = store.load().work.branch(repo, "feat").unwrap().clone();
        let last = record.observations.last().unwrap();
        assert_eq!(last.observed_at_ms, 4_000);
        assert_eq!(last.covered_by, None);
        assert!(!last.is_covered_by(&record.activities));
        // An unchanged fingerprint emits nothing at all.
        let mut o = obs("feat", false);
        o.inputs = inputs("dd", None);
        store.sync_repo(repo, &[o], 5_000).unwrap();
        let record = store.load().work.branch(repo, "feat").unwrap().clone();
        assert_eq!(record.observations.len(), 3, "{:?}", record.observations);
        // A path record links the same way.
        let path = "/wt";
        let path_inputs = |f: &str, m: Option<u64>| {
            let mut i = inputs(f, m);
            i.dirty = Some(true);
            i
        };
        store
            .sync_path(path, "/r/.git", &path_inputs("aa", Some(700)), &[], 1_000)
            .unwrap();
        store
            .sync_path(path, "/r/.git", &path_inputs("bb", Some(900)), &[], 2_000)
            .unwrap();
        let record = store.load().work.path(path).unwrap().clone();
        let observation = &record.observations[0];
        assert_eq!(observation.observed_at_ms, 2_000);
        assert_eq!(
            observation.covered_by,
            Some(ActivityReference {
                source: ActivitySource::WorkingTree,
                occurred_at_ms: 900,
            })
        );
        assert!(observation.is_covered_by(&record.activities));
    }

    #[test]
    fn an_observation_is_covered_only_by_a_counterpart_holding_every_reason() {
        let activities = vec![
            ActivityEvent {
                source: ActivitySource::WorkingTree,
                occurred_at_ms: 300,
                // A merge kept the emitted reasons and added another:
                // the superset still covers.
                reasons: vec!["modified a.rs".to_owned(), "untracked b.rs".to_owned()],
            },
            ActivityEvent {
                source: ActivitySource::Commit,
                occurred_at_ms: 300,
                reasons: vec!["modified a.rs".to_owned()],
            },
        ];
        let covered = ObservationEvent {
            covered_by: Some(ActivityReference {
                source: ActivitySource::WorkingTree,
                occurred_at_ms: 300,
            }),
            source: ObservationSource::WorkingTree,
            observed_at_ms: 2_000,
            reasons: vec!["modified a.rs".to_owned()],
        };
        assert!(covered.is_covered_by(&activities));
        // A reason the counterpart never carried is a partial match.
        let mut extra = covered.clone();
        extra.reasons.push("deleted c.rs".to_owned());
        assert!(!extra.is_covered_by(&activities));
        // A non-working-tree observation is never covered, link or not.
        let mut lifecycle = covered.clone();
        lifecycle.source = ObservationSource::Lifecycle;
        assert!(!lifecycle.is_covered_by(&activities));
        // A link naming another source fails even at a matching time.
        let mut wrong_source = covered.clone();
        wrong_source.covered_by = Some(ActivityReference {
            source: ActivitySource::Commit,
            occurred_at_ms: 300,
        });
        assert!(!wrong_source.is_covered_by(&activities));
        // A link naming an occurrence no retained event holds fails.
        let mut missing = covered.clone();
        missing.covered_by = Some(ActivityReference {
            source: ActivitySource::WorkingTree,
            occurred_at_ms: 999,
        });
        assert!(!missing.is_covered_by(&activities));
        // No link at all stays visible, and so does a link whose
        // counterpart was pruned from the retained history.
        let mut unlinked = covered.clone();
        unlinked.covered_by = None;
        assert!(!unlinked.is_covered_by(&activities));
        assert!(!covered.is_covered_by(&[]));
        assert!(!covered.is_covered_by(&activities[1..]));
    }

    #[test]
    fn a_pruned_counterpart_uncovers_its_observation() {
        // The linked occurrence is real and retained: the observation is
        // covered while it lives.
        let mut activities = vec![ActivityEvent {
            source: ActivitySource::WorkingTree,
            occurred_at_ms: 300,
            reasons: vec!["modified a.rs".to_owned()],
        }];
        let observation = ObservationEvent {
            covered_by: Some(ActivityReference {
                source: ActivitySource::WorkingTree,
                occurred_at_ms: 300,
            }),
            source: ObservationSource::WorkingTree,
            observed_at_ms: 2_000,
            reasons: vec!["modified a.rs".to_owned()],
        };
        assert!(observation.is_covered_by(&activities));
        // Newer occurrences push the linked one past the per-source
        // retention bound: the observation reappears rather than
        // staying hidden behind a pruned counterpart.
        for i in 0..HISTORY_RETAIN_PER_SOURCE as u64 {
            append_activity(
                &mut activities,
                ActivityEvent {
                    source: ActivitySource::WorkingTree,
                    occurred_at_ms: 400 + i,
                    reasons: vec![format!("modified {i}.rs")],
                },
            );
        }
        assert_eq!(
            activities
                .iter()
                .filter(|e| e.source == ActivitySource::WorkingTree)
                .count(),
            HISTORY_RETAIN_PER_SOURCE
        );
        assert!(!observation.is_covered_by(&activities));
        // The retained history kept the newest occurrences: the oldest
        // link target is exactly what aged out.
        assert_eq!(
            activities[0].occurred_at_ms, 400,
            "the linked occurrence aged out first: {activities:?}"
        );
    }

    #[test]
    fn an_outdated_work_file_resets_and_regenerates() {
        let temp = TempStore::new();
        fs::create_dir_all(&temp.0).unwrap();
        let store = temp.store();
        // Seen-state, a mark and a committed journal record written
        // beside the work file prove the other stores are never touched
        // by the reset.
        let key = conversation_key("claude", "s1");
        store.acknowledge(&key, 3, None).unwrap();
        store.mark_not_busy(&key, 123_000, 7).unwrap();
        store
            .append(record("claude", "s1", "Evt", NormEvent::End))
            .unwrap();
        let journal_bytes = fs::read(temp.path(JOURNAL)).unwrap();
        let marks_bytes = fs::read(temp.path(MARKS)).unwrap();
        let seen_bytes = fs::read(temp.path(SEEN)).unwrap();
        for v in [0u32, 1, 2, 3] {
            // Every pre-release contract this build dropped: an
            // incarnation id, parked flag, touch interval and a session
            // cursor seated before first-sighting backfill, plus a
            // reflog event dated absurdly far out - the maintenance-mtime
            // artifact v4 exists to drop. None of it may survive.
            let bytes = serde_json::json!({
                "v": v,
                "data": {
                    "branches": {
                        "i-old": {
                            "id": "i-old",
                            "repo": "/r/.git",
                            "ref_name": "feat",
                            "first_observed_at": 1,
                            "last_observed_at": 1,
                            "parked": true,
                            "session_activity": {"claude:s1": 9_999_999},
                            "activities": [{
                                "source": "reflog",
                                "occurred_at_ms": 9_999_999_999_999_u64,
                                "reasons": ["maintenance"]
                            }]
                        }
                    },
                    "active_branches": {"/r/.git\u{0}feat": "i-old"},
                    "paths": {
                        "/p": {
                            "repo": "/p",
                            "parked": true,
                            "session_activity": {"claude:s1": 9_999_999},
                            "activities": [{
                                "source": "reflog",
                                "occurred_at_ms": 9_999_999_999_999_u64,
                                "reasons": ["maintenance"]
                            }]
                        }
                    },
                    "touches": [{
                        "conversation": "claude:s1",
                        "branch": "i-old",
                        "head": "aaaaaaaaaaaaaaaa",
                        "provenance": "cwd",
                        "confidence": "exact",
                        "valid_from": 1
                    }]
                }
            })
            .to_string();
            fs::write(temp.path(WORK), &bytes).unwrap();
            // A read alone reports nothing and rewrites nothing: the old
            // contract is not malformed, it is dropped state.
            let (work, errors) = store.work();
            assert!(errors.is_empty(), "v{v}: {errors:?}");
            assert!(work.branches.is_empty() && work.paths.is_empty());
            assert!(work.touches.is_empty());
            assert_eq!(fs::read(temp.path(WORK)).unwrap(), bytes.as_bytes());
            // Journal, seen and marks are untouched by a reset that a
            // plain read performs.
            assert_eq!(fs::read(temp.path(JOURNAL)).unwrap(), journal_bytes);
            assert_eq!(fs::read(temp.path(MARKS)).unwrap(), marks_bytes);
            assert_eq!(fs::read(temp.path(SEEN)).unwrap(), seen_bytes);
            // The next sync rebuilds the record under the current schema.
            store
                .sync_repo("/r/.git", &[obs("feat", false)], 1_000)
                .unwrap();
            let json: serde_json::Value =
                serde_json::from_slice(&fs::read(temp.path(WORK)).unwrap()).unwrap();
            assert_eq!(json["v"], WORK_SCHEMA);
            let work = store.load().work;
            let record = work.branch("/r/.git", "feat").expect("regenerated");
            assert_ne!(record.id, "i-old", "a fresh incarnation id");
            assert!(!record.parked, "the old parked flag does not carry");
            assert!(work.touches.is_empty(), "old touches do not carry");
            assert!(
                !record.session_activity.contains_key("claude:s1"),
                "the old cursor does not suppress backfill"
            );
            assert!(
                record.activities.is_empty(),
                "the bogus reflog date is gone"
            );
            // First-sighting backfill lands on the regenerated record.
            store
                .sync_session_updates(&[SessionUpdate {
                    identity: UpdateIdentity::Branch(record.id.clone()),
                    conversation: "claude:s1".to_owned(),
                    at_ms: 5_000,
                    reason: "old turn".to_owned(),
                    context: SessionContext::default(),
                }])
                .unwrap();
            let work = store.load().work;
            let record = work.branch("/r/.git", "feat").expect("regenerated");
            assert_eq!(newest_activity(&record.activities), Some(5_000));
            // The dropped path record rebuilds the same way: none of the
            // old repo claim, parked flag, cursor or event survives.
            store
                .sync_path("/p", "/p", &LifecycleInputs::default(), &[], 2_000)
                .unwrap();
            store
                .sync_session_updates(&[SessionUpdate {
                    identity: UpdateIdentity::Path("/p".to_owned()),
                    conversation: "claude:s1".to_owned(),
                    at_ms: 5_000,
                    reason: "old turn".to_owned(),
                    context: SessionContext::default(),
                }])
                .unwrap();
            let work = store.load().work;
            let path = work.path("/p").expect("the path regenerated");
            assert_eq!(path.repo.as_deref(), Some("/p"));
            assert!(!path.parked);
            assert_eq!(newest_activity(&path.activities), Some(5_000));
            // The other authored files kept their own schema and data.
            let loaded = store.load();
            assert!(loaded.seen.contains_key(&key));
            assert!(loaded.marks.contains_key(&key));
            assert_eq!(fs::read(temp.path(JOURNAL)).unwrap(), journal_bytes);
            assert_eq!(fs::read(temp.path(MARKS)).unwrap(), marks_bytes);
            assert_eq!(fs::read(temp.path(SEEN)).unwrap(), seen_bytes);
        }
    }
}
