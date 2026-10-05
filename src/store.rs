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
//! Every record carries a schema version; readers accept the current and
//! immediately previous one (an absent `v` reads as the pre-versioned
//! schema). A future-versioned or malformed record is excluded from
//! derivation, retained on disk and reported for the evidence view, and a
//! corrupt journal tail never hides the valid prefix.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::provider::SourceError;

/// The record schema this build reads and writes. `0` - an unversioned
/// record from before the field existed - reads as the previous schema.
pub const SCHEMA: u32 = 1;

/// Compact once the journal's un-checkpointed tail passes this many
/// records: enough that a busy day never rewrites, small enough that a
/// scan stays trivial.
const COMPACT_AFTER: usize = 128;

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

/// The observable lifecycle facts whose *change* is meaningful work
/// activity: dirty flag and tree shape, delivery evidence, forge state.
/// Persisted as the record's last reading so a restart does not redate an
/// unchanged state, and a changed input dates at its observation time.
/// An optional field is `None` when unproven: it keeps the last proven
/// value and dates nothing, so an offline pass, a forge outage or a timed
/// out probe is never mistaken for work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleInputs {
    /// The worktree's dirty flag; `None` when unproven or not applicable.
    #[serde(default)]
    pub dirty: Option<bool>,
    /// Whether the anchor's checkout exists at all this pass.
    #[serde(default)]
    pub worktree: bool,
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
}

impl LifecycleInputs {
    /// This reading laid over `prior`: every unproven field keeps the
    /// prior proven value. The checkout's presence is always observed;
    /// its path and admin id keep their last proven value, so a record
    /// outliving its workspace still names where the work was.
    fn over(&self, prior: &LifecycleInputs) -> LifecycleInputs {
        LifecycleInputs {
            dirty: self.dirty.or(prior.dirty),
            worktree: self.worktree,
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
        }
    }

    /// Whether this reading is a lifecycle transition from `prior`: the
    /// checkout appeared, vanished or moved, or a field proven on both
    /// sides changed. A field's first proven value - unproven when the
    /// record was made - is adopted without dating anything.
    fn transitions_from(&self, prior: &LifecycleInputs) -> bool {
        fn changed<T: PartialEq>(new: &Option<T>, old: &Option<T>) -> bool {
            matches!((new, old), (Some(n), Some(o)) if n != o)
        }
        // The checkout appearing or vanishing is a transition in itself;
        // path and admin id only transition on a proven move (Some vs
        // Some) - adopting a first value, like losing one, dates nothing
        // twice.
        self.worktree != prior.worktree
            || changed(&self.worktree_path, &prior.worktree_path)
            || changed(&self.admin_id, &prior.admin_id)
            || changed(&self.dirty, &prior.dirty)
            || changed(&self.ahead, &prior.ahead)
            || changed(&self.unpushed, &prior.unpushed)
            || changed(&self.upstream, &prior.upstream)
            || changed(&self.landed, &prior.landed)
            || changed(&self.forge, &prior.forge)
            || changed(&self.pipeline, &prior.pipeline)
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
    pub inputs: LifecycleInputs,
}

/// One branch incarnation: a single observed lifetime of `ref_name` inside
/// `repo`. A deleted ref closes the record (`ended_at`); a ref that
/// reappears opens a fresh record with a new `id` and `parked: false` - a
/// name is a label, not an identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// When the last lifecycle transition was observed, epoch ms. Source
    /// timestamps (commit, reflog) are re-derived each pass and never
    /// stored; first observation fingerprints without dating anything.
    #[serde(default)]
    pub activity_at: Option<u64>,
    /// The inputs `activity_at` was judged against.
    #[serde(default)]
    pub inputs: LifecycleInputs,
}

/// A path-anchored record: a detached worktree or a non-Git project space,
/// keyed by its canonical path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathRecord {
    #[serde(default)]
    pub parked: bool,
    #[serde(default)]
    pub activity_at: Option<u64>,
    #[serde(default)]
    pub inputs: LifecycleInputs,
}

/// The `work.json` payload: the authored Work state.
#[derive(Debug, Default, Serialize, Deserialize)]
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
/// false`, the observation's evidence and fingerprint stored.
fn open_incarnation(
    work: &mut Work,
    repo: &str,
    obs: &ObservedRef,
    observed_ms: u64,
    continuity: ContinuityEvidence,
) {
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
            activity_at: None,
            inputs: obs.inputs.clone(),
        },
    );
    work.active_branches.insert(branch_key(repo, &obs.name), id);
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
}

/// A committed journal record the fold rejected as stale or duplicate
/// producer evidence, plus the reason - kept for the evidence view.
#[derive(Debug, Clone, Serialize)]
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
                    let compacted = self.compact(&loaded.folds, &loaded.seen, loaded.max_seq);
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
        match self.read_checkpoint(&mut errors) {
            Some(c) => {
                through = c.through;
                folds = c.folds;
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
        let mut rejected = Vec::new();
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
    /// fingerprinted without dating activity. A changed fingerprint on a
    /// live record is a lifecycle transition dated `observed_ms`.
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
            if obs.head.is_some() {
                record.head = obs.head.clone();
            }
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
                    if obs.head.is_some() && record.head != obs.head {
                        record.head = obs.head.clone();
                        changed = true;
                    }
                    // A changed proven fingerprint is a transition; first
                    // observation never lands here. A newly proven field
                    // is stored without dating anything.
                    if obs.inputs.transitions_from(&record.inputs) {
                        record.activity_at = Some(observed_ms);
                        changed = true;
                    }
                    let inputs = obs.inputs.over(&record.inputs);
                    if record.inputs != inputs {
                        record.inputs = inputs;
                        changed = true;
                    }
                }
                None => {
                    open_incarnation(
                        &mut work,
                        repo,
                        obs,
                        observed_ms,
                        ContinuityEvidence::FirstObservation,
                    );
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
    /// project space - with the fingerprint a pass observed at
    /// `observed_ms`. Creates it absent, dates a changed fingerprint as a
    /// transition, and writes nothing when nothing changed.
    pub fn sync_path(
        &self,
        path: &str,
        inputs: &LifecycleInputs,
        observed_ms: u64,
    ) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?; // coverage: off - a directory-creation failure needs a filesystem fault
        let _lock = Lock::acquire(&self.dir.join(LOCK))?;
        let mut work = self.read_work_for_update()?;
        let mut changed = false;
        match work.paths.get_mut(path) {
            Some(record) => {
                if inputs.transitions_from(&record.inputs) {
                    record.activity_at = Some(observed_ms);
                    changed = true;
                }
                let inputs = inputs.over(&record.inputs);
                if record.inputs != inputs {
                    record.inputs = inputs;
                    changed = true;
                }
            }
            None => {
                work.paths.insert(
                    path.to_owned(),
                    PathRecord {
                        parked: false,
                        activity_at: None,
                        inputs: inputs.clone(),
                    },
                );
                changed = true;
            }
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
        self.write_authored(SEEN, seen)
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
        self.write_authored(MARKS, marks)
    }

    /// The work-state file: incarnation and path records. Malformed or
    /// future content reports and reads as absent - never guessed.
    fn read_work(&self, errors: &mut Vec<SourceError>) -> Work {
        self.read_authored(WORK, errors)
            .map(|authored: Authored<Work>| authored.data)
            .unwrap_or_default()
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
        self.write_authored(WORK, work)
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

    /// Every caller holds the store lock, so the directory exists.
    fn write_authored<T: Serialize>(&self, name: &str, data: &T) -> io::Result<()> {
        let authored = Authored { v: SCHEMA, data };
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
        ping.pseq = Some(9);
        assert_eq!(fold.apply(&ping), Apply::Accepted);
        // The same producer sequence again is a heartbeat: the observation
        // refreshes and the ping's clock moves, but nothing latches.
        ping.seq = 2;
        ping.at = 1_500;
        assert_eq!(fold.apply(&ping), Apply::Duplicate);
        assert_eq!(fold.ping, Some((1, 1_500)));
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
        assert_eq!(main.activity_at, None, "first observation dates nothing");
        assert!(!main.parked);
        // The same observation on a later pass is a no-op: the file does
        // not move, so a dashboard left open rewrites nothing per refresh.
        let bytes = fs::read(temp.path(WORK)).unwrap();
        store
            .sync_repo(repo, &[obs("main", false), obs("feat", false)], 1_500)
            .unwrap();
        assert_eq!(fs::read(temp.path(WORK)).unwrap(), bytes);
        // A fingerprint change dates the transition at its observation.
        store
            .sync_repo(repo, &[obs("main", false), obs("feat", true)], 2_000)
            .unwrap();
        let work = store.load().work;
        let feat = work.branch(repo, "feat").unwrap();
        assert_eq!(feat.activity_at, Some(2_000));
        assert_eq!(feat.inputs.dirty, Some(true));
        // `feat` gone from the observation: the record closes and stays -
        // it leaves the active lookup but remains in the history.
        let old_id = feat.id.clone();
        store.sync_repo(repo, &[obs("main", false)], 3_000).unwrap();
        let work = store.load().work;
        assert!(work.branch(repo, "feat").is_none());
        assert!(!work.active_branches.contains_key(&branch_key(repo, "feat")));
        let closed = &work.branches[&old_id];
        assert_eq!(closed.ended_at, Some(3_000));
        assert_eq!(closed.first_observed_at, 1_000);
        // Reappearance opens a new incarnation: new id, unparked, and the
        // closed record survives beside it.
        store
            .sync_repo(repo, &[obs("main", false), obs("feat", false)], 4_000)
            .unwrap();
        let work = store.load().work;
        let feat = work.branch(repo, "feat").expect("feat reincarnated");
        assert_ne!(feat.id, old_id);
        assert_eq!(feat.first_observed_at, 4_000);
        assert_eq!(feat.ended_at, None);
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
        assert_eq!(record.activity_at, None);
        assert_eq!(record.inputs, proven);
        // Back online with the same answers: still nothing to date.
        assert_eq!(observe(&proven, 3_000).activity_at, None);
        // A proven change dates, even when other fields are unproven.
        let merged = LifecycleInputs {
            forge: Some("closed".to_owned()),
            upstream: None,
            ..proven.clone()
        };
        let record = observe(&merged, 4_000);
        assert_eq!(record.activity_at, Some(4_000));
        assert_eq!(record.inputs.forge.as_deref(), Some("closed"));
        assert_eq!(
            record.inputs.upstream.as_deref(),
            Some("tracked origin/feat")
        );
        // The same holds for a path record.
        store.sync_path("/space", &proven, 1_000).unwrap();
        store.sync_path("/space", &offline, 2_000).unwrap();
        let work = store.load().work;
        assert_eq!(work.path("/space").unwrap().activity_at, None);

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
                        inputs: inputs.clone(),
                    }],
                    at,
                )
                .unwrap();
            store.load().work.branch(repo, "feat").unwrap().clone()
        };
        first(&offline, 1_000);
        let record = first(&proven, 2_000);
        assert_eq!(record.activity_at, None);
        assert_eq!(record.inputs, proven);
        assert_eq!(first(&merged, 3_000).activity_at, Some(3_000));
        store.sync_path("/fresh-space", &offline, 1_000).unwrap();
        store.sync_path("/fresh-space", &proven, 2_000).unwrap();
        let work = store.load().work;
        let space = work.path("/fresh-space").unwrap();
        assert_eq!(space.activity_at, None);
        assert_eq!(space.inputs, proven);
    }

    #[test]
    fn the_work_stamp_moves_with_every_write_and_only_then() {
        let temp = TempStore::new();
        let store = temp.store();
        assert_eq!(store.work_stamp(), None, "no file, no stamp");
        store
            .sync_path("/p", &LifecycleInputs::default(), 1_000)
            .unwrap();
        let first = store.work_stamp().expect("the file exists");
        // A no-op sync leaves the file, and so the stamp, alone.
        store
            .sync_path("/p", &LifecycleInputs::default(), 2_000)
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
            .sync_path("/space", &LifecycleInputs::default(), 1_000)
            .unwrap();
        let bytes = fs::read(temp.path(WORK)).unwrap();
        store
            .sync_path("/space", &LifecycleInputs::default(), 2_000)
            .unwrap();
        assert_eq!(fs::read(temp.path(WORK)).unwrap(), bytes);
        // A changed proven fingerprint is a transition: it lands
        // `activity_at` and rewrites once.
        let clean = LifecycleInputs {
            dirty: Some(false),
            ..LifecycleInputs::default()
        };
        store.sync_path("/space", &clean, 2_500).unwrap();
        let inputs = LifecycleInputs {
            dirty: Some(true),
            ..LifecycleInputs::default()
        };
        store.sync_path("/space", &inputs, 3_000).unwrap();
        let work = store.load().work;
        assert_eq!(work.path("/space").unwrap().activity_at, Some(3_000));
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
            .sync_path("/space", &LifecycleInputs::default(), 1_000)
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
            .sync_path("/p", &LifecycleInputs::default(), 1_000)
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
                .sync_path("/p", &LifecycleInputs::default(), 1)
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
        // Malformed and future-schema files refuse the read-modify-write
        // and keep their bytes.
        for bytes in [
            "{oops".to_owned(),
            serde_json::json!({"v": 99, "data": {}}).to_string(),
        ] {
            fs::write(temp.path(WORK), &bytes).unwrap();
            let err = store
                .sync_repo("/r", &[obs("a", false)], 1_000)
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            let err = store
                .sync_path("/p", &LifecycleInputs::default(), 1_000)
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
}
