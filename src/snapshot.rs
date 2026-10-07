//! The immutable snapshot the TUI renders and `list --json` prints.
//!
//! One collect pass produces one snapshot: repos, work rows, conversations,
//! the effective evidence behind them and every source error a collector
//! retained. Nothing in it is derived lazily later - the TUI holds only
//! cursor and filters on top of it, and no subprocess ever runs on the
//! render path because all reading happened here.
//!
//! `list --json` prints this same complete, unfiltered snapshot. It carries
//! `schema_version`; additive fields preserve the version, while removing,
//! renaming or changing a field's meaning requires a version increment.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::attention::{self, Attention};
use crate::claude::{Claude, Conversation};
use crate::config;
use crate::evidence::Evidence;
use crate::fanout;
use crate::forge::{self, ForgeStatus, Pipeline, WorkItem};
use crate::git::{self, Head, RemoteHead, RemoteListing, Resolved};
use crate::process::{Liveness, ProcessInstance, ProcessStart};
use crate::provider::{SourceError, StateEvidence};
use crate::runtime::{EvidenceSource, PaneSource, Placement, Provider, Runtime};
use crate::store::{self, Exec, Store};
use crate::tmux::{self, PaneRef};
use crate::vector::{
    self, Anchor, Landed as LandedVerdict, RemoteCache, RuntimeFacts, UpstreamState, WindowCount,
};
use crate::verdict::{self, Verdict};

/// The JSON contract version. Additive changes keep it; a field's removal,
/// rename or change of meaning bumps it.
pub const SCHEMA_VERSION: u32 = 1;

/// One refresh's complete, unfiltered view.
#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub schema_version: u32,
    /// When the evidence was collected, epoch seconds.
    pub observed_at: u64,
    /// The pane the dashboard itself occupies (`$TMUX_PANE`), when it runs
    /// inside tmux - carried so focus observation can exclude it.
    pub own_pane: Option<String>,
    /// Whether every collection stage has landed. `false` while evidence
    /// is still arriving: unlanded fields read as unknowns (`?`) and the
    /// status bar keeps its spinner until this turns `true`.
    pub complete: bool,
    pub repos: Vec<RepoRow>,
    pub work: Vec<WorkRow>,
    pub conversations: Vec<ConversationRow>,
    /// Collector failures, isolated per record.
    pub errors: Vec<SourceError>,
    /// Entries a provider's safety rules rejected without ever parsing -
    /// non-UUID names, symlinks, non-regular or empty files. Retained for
    /// the evidence view, as lossy display strings.
    pub skipped: Vec<String>,
    /// tmux socket files no server listens on. tmux never unlinks its
    /// socket, so these pile up; each is skipped without a `tmux` spawn,
    /// and the count keeps a socket dir full of them visible.
    pub stale_sockets: usize,
}

impl Snapshot {
    /// The pre-collection view the TUI draws first: an empty, incomplete
    /// snapshot so the dashboard paints its frame before the collector's
    /// first stage lands.
    pub fn empty() -> Snapshot {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            observed_at: epoch(SystemTime::now()),
            own_pane: None,
            complete: false,
            repos: Vec::new(),
            work: Vec::new(),
            conversations: Vec::new(),
            errors: Vec::new(),
            skipped: Vec::new(),
            stale_sockets: 0,
        }
    }
}

/// A repository - or a non-git project space - as the `[1]` list sees it.
/// Identity is the canonical `$GIT_COMMON_DIR` for a repo, the canonical
/// path for a project space.
#[derive(Debug, Clone, Serialize)]
pub struct RepoRow {
    pub id: String,
    /// Display name: the main checkout's basename, or the space's basename.
    pub name: String,
    /// The main checkout path, or the project space itself.
    pub path: PathBuf,
    pub git: bool,
    /// Work rows and live conversations in scope of this entry.
    pub work: usize,
    pub live: usize,
    /// The work rows' rolled-up attention: the glyph [1] renders.
    pub attention: Attention,
    /// Rows asking for work - NeedsYou, Active, FollowUp, Forgotten.
    pub open: usize,
    /// Rows provably done - ReadyToClean and CleanupReview.
    pub clean: usize,
    /// Latest meaningful activity across its work rows; `None` renders `?`.
    pub last_activity: Option<u64>,
    /// The proven default branch: the remote's HEAD that every probed
    /// base agreed on, `None` when nothing proves it (`?`).
    pub default_branch: Option<String>,
    /// The remote the repo's work anchors on, when proven or lone; `None`
    /// is `?`.
    pub remote: Option<String>,
    /// The repo's work rows per section.
    pub counts: RepoCounts,
}

/// `RepoRow.counts`: the section breakdown the detail view prints.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoCounts {
    pub needs_you: usize,
    pub active: usize,
    pub follow_up: usize,
    pub forgotten: usize,
    pub ready_to_clean: usize,
    pub cleanup_review: usize,
}

impl RepoRow {
    /// Recompute the rollup from the repo's rows in `work` - attention,
    /// the `N open · M clean` counts, the section breakdown and the
    /// latest activity. The emit path and a `p` reclassification share
    /// it, so both read the same way.
    pub fn roll_up(&mut self, work: &[WorkRow]) {
        let rows: Vec<&WorkRow> = work.iter().filter(|w| w.repo == self.id).collect();
        self.work = rows.len();
        self.attention = attention::rollup(rows.iter().map(|w| &w.attention));
        self.open = rows.iter().filter(|w| w.section.open()).count();
        self.clean = rows.len() - self.open;
        self.last_activity = rows.iter().filter_map(|w| w.last_activity).max();
        let mut counts = RepoCounts::default();
        for w in &rows {
            match w.section {
                WorkSection::NeedsYou => counts.needs_you += 1,
                WorkSection::Active => counts.active += 1,
                WorkSection::FollowUp => counts.follow_up += 1,
                WorkSection::Forgotten => counts.forgotten += 1,
                WorkSection::ReadyToClean => counts.ready_to_clean += 1,
                WorkSection::CleanupReview => counts.cleanup_review += 1,
            }
        }
        self.counts = counts;
    }
}

/// `WorkRow.kind`: which anchor shape the row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    /// A branch checkout, or a branch with no checkout at all.
    Branch,
    /// A detached-HEAD checkout.
    Detached,
    /// The repository's own checkout.
    Worktree,
    /// A non-git project space.
    ProjectSpace,
}

impl WorkKind {
    /// The wire spelling; the detail view renders `project_space` as
    /// "project space".
    pub fn as_str(self) -> &'static str {
        match self {
            WorkKind::Branch => "branch",
            WorkKind::Detached => "detached",
            WorkKind::Worktree => "worktree",
            WorkKind::ProjectSpace => "project_space",
        }
    }
}

/// `WorkRow.upstream`: what the branch's configured upstream proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Upstream {
    /// The upstream ref exists on the remote.
    Tracked,
    /// No upstream configured.
    NeverPushed,
    /// Configured, and the remote provably no longer carries it.
    RemoteGone,
    /// No branch to track (detached HEAD, project space).
    NotApplicable,
    /// Configured partially, or the remote could not be asked.
    Unknown,
}

impl Upstream {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Upstream::Tracked => "tracked",
            Upstream::NeverPushed => "never_pushed",
            Upstream::RemoteGone => "remote_gone",
            Upstream::NotApplicable => "not_applicable",
            Upstream::Unknown => "unknown",
        }
    }
}

/// `WorkRow.landed`: whether HEAD's content already lives on the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Landed {
    /// HEAD is an ancestor of the base ref.
    Ancestor,
    /// Every path HEAD changed is identical on the base - the squash- or
    /// rebase-landed shape.
    Content,
    /// Proven not landed (including the no-delta case).
    No,
}

impl Landed {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Landed::Ancestor => "ancestor",
            Landed::Content => "content",
            Landed::No => "no",
        }
    }
}

/// `AttachmentRow.liveness`: the claim's proven process state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentLiveness {
    /// The running process is the claimed instance.
    Instance,
    /// The pid is alive but the instance is unproven.
    PidOnly,
    /// The pid is gone or provably belongs to another instance.
    Dead,
    /// Nothing could be checked - the process table failed to read.
    Unverifiable,
}

/// `ConversationRow.state`: the arbitrated effective execution state -
/// published state fused with the journal's events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationState {
    Busy,
    Idle,
    Waiting,
    /// No applicable evidence, or one the mapping does not know.
    Unknown,
}

/// `WorkRow.section`: the first-match next-action section. Attention wins
/// over delivery and cleanup state; the losing evidence still shows in the
/// row's summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSection {
    /// Waiting, failed unseen or completed unseen.
    NeedsYou,
    /// A live busy process with no higher attention and no authored
    /// not-busy mark.
    Active,
    /// Resumable idle work, pending or failed delivery, dirty or unpushed
    /// work, a blocked cleanup verdict - or simply open work.
    FollowUp,
    /// Unfinished work with no live process and no meaningful activity in
    /// `forgotten_after`; `parked` suppresses only this placement.
    Forgotten,
    /// Every applicable cleanup action proved safe.
    ReadyToClean,
    /// An applicable action needs an explicit human review.
    CleanupReview,
}

impl WorkSection {
    /// The section header as [2] renders it.
    pub fn title(self) -> &'static str {
        match self {
            WorkSection::NeedsYou => "Needs you",
            WorkSection::Active => "Active",
            WorkSection::FollowUp => "Follow up",
            WorkSection::Forgotten => "Forgotten",
            WorkSection::ReadyToClean => "Ready to clean",
            WorkSection::CleanupReview => "Cleanup review",
        }
    }

    /// Whether the section counts as `open` in a repo rollup's
    /// `N open · M clean` - the cleanup sections count as `clean`.
    pub fn open(self) -> bool {
        !matches!(self, WorkSection::ReadyToClean | WorkSection::CleanupReview)
    }
}

impl ConversationState {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            ConversationState::Busy => "busy",
            ConversationState::Idle => "idle",
            ConversationState::Waiting => "waiting",
            ConversationState::Unknown => "unknown",
        }
    }
}

/// One branch incarnation as the snapshot serializes it: a row of the
/// persisted record, numbered oldest-first within its `(repo, ref_name)`.
/// Times are epoch seconds - the store's milliseconds converted.
#[derive(Debug, Clone, Serialize)]
pub struct IncarnationRow {
    /// The persisted `BranchRecord.id`.
    pub id: String,
    /// 1-based within `(repo, ref_name)`, oldest first.
    pub number: usize,
    pub repo: String,
    pub ref_name: String,
    pub first_observed_at: u64,
    pub last_observed_at: u64,
    /// The head the ref was created at, when the reflog proved it.
    pub creation_head: Option<String>,
    /// When the ref was created, when the reflog proved it.
    pub creation_at: Option<u64>,
    /// The tip the ref last proved, when it carried one.
    pub head: Option<String>,
    pub ended_at: Option<u64>,
    /// The evidence that last established or separated this identity.
    pub continuity: store::ContinuityEvidence,
    /// `true` on retained earlier same-name history: it scopes and lists
    /// but never contributes to the current row.
    pub excluded: bool,
}

/// One persisted touch interval as the snapshot serializes it: the
/// conversation's placement on an incarnation with HEAD, provenance and
/// confidence. Times are epoch seconds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TouchRow {
    /// The touched `BranchRecord.id`.
    pub incarnation_id: String,
    pub repo: String,
    pub ref_name: String,
    /// The incarnation's 1-based number within `(repo, ref_name)`.
    pub incarnation: usize,
    /// The ref's tip the placement observed; `None` when the evidence
    /// named the branch only.
    pub head: Option<String>,
    pub valid_from: u64,
    /// `None` while the placement is current.
    pub valid_until: Option<u64>,
    pub provenance: store::TouchProvenance,
    pub confidence: store::Confidence,
}

/// One Work row: a checkout on a branch, a detached checkout, a local branch
/// without one, or a non-git project space.
#[derive(Debug, Clone, Serialize)]
pub struct WorkRow {
    /// The owning repo's (or project space's) identity.
    pub repo: String,
    /// The repo's display name; rows under `all` carry it as a prefix.
    pub repo_name: String,
    pub kind: WorkKind,
    /// The row's label: the branch name, `detached @sha`, or the path name.
    pub name: String,
    /// The checkout path when there is one (`None` -> `no wt`).
    pub worktree: Option<PathBuf>,
    /// The checked-out branch name, when any.
    pub branch: Option<String>,
    pub dirty: Option<bool>,
    pub broken: Option<String>,
    /// Commits on the row's tip not on the proven base; `None` is `?`.
    pub commits_ahead: Option<u64>,
    pub unpushed: Option<u64>,
    /// Detail in `upstream_detail`.
    pub upstream: Upstream,
    pub upstream_detail: Option<String>,
    /// `None` is unproven.
    pub landed: Option<Landed>,
    /// The proven base's `remote/branch` label.
    pub base: Option<String>,
    pub windows: usize,
    /// Live agent processes bound to the row.
    pub live_pids: usize,
    /// Live conversations attached to the row.
    pub live_sessions: usize,
    /// Conversations ever recorded against the row's path.
    pub past_sessions: usize,
    /// Latest meaningful activity: the newest real work in the row's
    /// reflogs (never creation or checkout bookkeeping), the authored
    /// record's last proven transition and bound conversations' turns;
    /// `None` is `?`.
    pub last_activity: Option<u64>,
    /// The rolled-up attention of the conversations bound to the row.
    pub attention: Attention,
    /// The row's work identity: the active branch incarnation's id for
    /// branch rows, the canonical path for a detached worktree or a
    /// project space. `None` for a branch that has no record yet.
    pub identity: Option<String>,
    /// The active incarnation the identity names - its `#N` number and
    /// continuity evidence. `None` where `identity` is not an incarnation.
    pub incarnation: Option<IncarnationRow>,
    /// Retained earlier incarnations of the same `(repo, ref_name)`,
    /// numbered oldest first and excluded from everything this row
    /// derives - `h` lists them below the row.
    pub same_name_history: Vec<IncarnationRow>,
    /// The authored parked flag from `work.json`; suppresses only the
    /// `Forgotten` placement and reads back in the summary as `· parked`.
    pub parked: bool,
    /// The forge's work-item state for the branch.
    pub forge: WorkItem,
    /// Pipeline state of an open work item.
    pub pipeline: Pipeline,
    /// `PR #191`/`MR !7`-style label when the forge named one.
    pub forge_label: Option<String>,
    /// The work item's URL when known.
    pub forge_url: Option<String>,
    /// Commits on the proven base the row's tip lacks; `None` is `?`.
    pub commits_behind: Option<u64>,
    /// The newest commits on the tip the proven base lacks, newest first,
    /// at most `vector::COMMIT_LIST_LIMIT` (`commits_ahead` counts them
    /// all); `None` is `?` - the base is unproven or the read failed.
    pub commits: Option<Vec<CommitRow>>,
    /// The panes bound to the row's worktree; empty where there is no
    /// worktree or none bound.
    pub panes: Vec<PaneRow>,
    /// What vanished, when the row is a gone-work row: its branch or
    /// folder no longer exists but something live still references it.
    pub gone: Option<String>,
    /// The live references retaining a gone row - every pane, window,
    /// session, process and agent session that still names it. Empty on
    /// live rows.
    pub references: Vec<ReferenceRow>,
    /// The authored record's last proven lifecycle transition, epoch
    /// seconds; `None` is `?`.
    pub transition_at: Option<u64>,
    /// The newest real work the row's reflogs recorded, epoch seconds;
    /// `None` is `?`.
    pub git_activity_at: Option<u64>,
    pub updates: Vec<store::UpdateEvent>,
    /// `git worktree remove` verdict and reasons; `None` for project
    /// spaces, which carry no cleanup verdicts at all.
    pub worktree_removal: Option<verdict::ActionVerdict>,
    /// `git branch -d/-D` verdict and reasons.
    pub branch_deletion: Option<verdict::ActionVerdict>,
    /// The first-match section the row sits in.
    pub section: WorkSection,
    /// The section reason followed by the compact evidence tail (`↑3`,
    /// `~dirty`, `no remote`, `no wt`, `merged`, a PR/MR label).
    pub summary: String,
}

/// One commit on a row's tip that its proven base lacks.
#[derive(Debug, Clone, Serialize)]
pub struct CommitRow {
    pub sha: String,
    pub subject: String,
    /// Committer time, epoch seconds.
    pub at: u64,
    /// The short id of the one conversation whose touch to this
    /// incarnation was open at the commit time; `None` (`?`) when no
    /// touch or more than one covers it.
    pub conversation: Option<String>,
}

/// One tmux pane bound to a worktree, for the Work detail's `tmux` line.
#[derive(Debug, Clone, Serialize)]
pub struct PaneRow {
    /// `session_name:@window.%pane` - the handle shape a provider
    /// publishes.
    pub handle: String,
    /// `pane_current_command`.
    pub command: String,
}

/// `ReferenceRow.kind`: which shape of live entity still names the gone
/// work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Pane,
    Window,
    /// A tmux session.
    TmuxSession,
    /// A live process.
    Process,
    /// A live agent session.
    AgentSession,
}

/// One live reference retaining a gone work row - what it is and how it
/// identifies itself. Read-only; dismissal is not this surface.
#[derive(Debug, Clone, Serialize)]
pub struct ReferenceRow {
    pub kind: ReferenceKind,
    /// `workmux:@149.%162`, `workmux:@149`, `workmux`, `pid 4200`,
    /// `claude:8f423bbb`.
    pub label: String,
}

/// `RelatedRow.strength`, in proven-strength order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationStrength {
    /// The provider declared the relation itself.
    ProviderLineage,
    /// One live process sits in the other's ancestor chain.
    ProcessAncestry,
    /// Both conversations touched the same exact incarnation.
    SameIncarnation,
}

/// One conversation related to this row, with the evidence that proved
/// the relation. Only proven relations exist: a shared ref name, a pane
/// shared at different times, or mere time proximity never produces one.
#[derive(Debug, Clone, Serialize)]
pub struct RelatedRow {
    /// The strength group the relation belongs to.
    pub strength: RelationStrength,
    /// What the relation claims about the other conversation:
    /// `ancestor`, `descendant` or `same incarnation`.
    pub label: String,
    /// The evidence that proved it, for the view's provenance column.
    pub provenance: String,
    pub provider: Provider,
    pub session_id: String,
    /// First eight of the related session's id.
    pub short_id: String,
    pub title: Option<String>,
    /// The related row's attention glyph.
    pub attention: Attention,
    /// Its effective-state age basis, epoch seconds.
    pub state_since: Option<u64>,
}

/// One retained latch event with its acknowledgement state.
#[derive(Debug, Clone, Serialize)]
pub struct LatchRow {
    /// The journal commit sequence.
    pub seq: u64,
    pub kind: store::NormEvent,
    /// The observation's own time, epoch ms.
    pub at_ms: u64,
    /// The record's reason or native name.
    pub reason: Option<String>,
    /// Whether seen-state already acknowledges it.
    pub acknowledged: bool,
}

/// The conversation's arbitration and store evidence, for the `e` view:
/// what claimed the state, what lost and why, and what the journal and
/// the authored files recorded.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EvidenceRow {
    /// The execution claims considered, in evaluation order.
    pub claims: Vec<attention::ClaimRow>,
    /// Every retained latch event and its acknowledgement state.
    pub latches: Vec<LatchRow>,
    /// Committed records the fold rejected as stale or duplicate, with
    /// the fold's reason.
    pub rejected: Vec<store::RejectedRecord>,
    /// What acknowledgement the user has recorded: retained events
    /// through `seen_seq`, and the newest seen wait episode's `since`.
    pub seen_seq: Option<u64>,
    pub seen_wait_ms: Option<u64>,
    /// The authored not-busy mark, when one applies.
    pub mark: Option<store::Mark>,
    /// Whether the mark suppressed a live `Busy` claim.
    pub mark_suppressed: bool,
    /// The newest journal commit sequence on the conversation.
    pub journal_seq: Option<u64>,
    /// The producer-sequence high-water records are ordered against.
    pub producer_seq: Option<u64>,
}

/// What a conversation's process claim resolved to this refresh.
#[derive(Debug, Clone, Serialize)]
pub struct AttachmentRow {
    pub pid: u32,
    /// The claimed start, epoch seconds; `None` when the provider did not
    /// date its process.
    pub pid_start: Option<u64>,
    pub liveness: AttachmentLiveness,
    pub liveness_detail: Option<String>,
    /// The bound pane as `session:window.pane`, when bound.
    pub pane: Option<String>,
    /// How the pane was bound; `None` when unbound.
    pub pane_source: Option<PaneSource>,
    /// Why the pane is unbound, when it is (`dead`, `superseded`, or the
    /// failed-closed reason).
    pub placement_detail: Option<String>,
    /// Where the claim was observed - a published file, a lock, a hook or
    /// derivation.
    pub source: EvidenceSource,
    /// When the claim's evidence was produced, epoch seconds.
    pub observed_at: u64,
}

/// One conversation in the snapshot - live, restorable or transcript-only.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationRow {
    /// The provider plugin the conversation came from.
    pub provider: Provider,
    pub session_id: String,
    /// First eight of the id - what list rows display.
    pub short_id: String,
    /// Provider title, or `None` -> `?`.
    pub title: Option<String>,
    /// The arbitrated effective reading: published state fused with the
    /// journal's events, marks and liveness.
    pub state: ConversationState,
    /// The provider's raw status string, kept for the evidence view.
    pub state_raw: Option<String>,
    /// The wait reason while waiting - the provider's `waitingFor` or the
    /// `awaiting` event's, verbatim.
    pub waiting_for: Option<String>,
    /// When the effective state began, epoch seconds; without a live claim,
    /// the provider's status time or the transcript's newest record.
    pub state_since: Option<u64>,
    /// The same instant in epoch milliseconds - what the not-busy mark
    /// names.
    pub state_since_ms: Option<u64>,
    /// The row's attention: the precedence winner among unacknowledged
    /// latches and the live claim.
    pub attention: Attention,
    /// The winning attention's reason (`permission prompt`,
    /// `StopFailure`), for the detail pane.
    pub attention_detail: Option<String>,
    /// The journal sequence an acknowledgement would write through: the
    /// highest unacknowledged latch. `None` when nothing awaits.
    pub attention_seq: Option<u64>,
    /// The live wait episode an acknowledgement records: the `since` of a
    /// `waiting` the user has not seen, epoch milliseconds.
    pub attention_wait_ms: Option<u64>,
    /// The newest event sequence the conversation has; a not-busy mark
    /// written at it is superseded by anything newer.
    pub journal_seq: Option<u64>,
    /// Most recent evidence of the conversation at all.
    pub last_activity: Option<u64>,
    /// Whether a live session record exists.
    pub live: bool,
    pub attachment: Option<AttachmentRow>,
    /// The cwd the conversation's Work identity derives from.
    pub cwd: Option<PathBuf>,
    /// The transcript file, for history that survives everything.
    pub transcript: Option<PathBuf>,
    /// Lines of the transcript that did not parse; `None` when there is no
    /// transcript. Retained for the evidence view.
    pub malformed_lines: Option<usize>,
    /// `claude --resume <id>` as an argv vector - never a shell string.
    pub resume_argv: Vec<String>,
    /// The latest provider-parsed user prompt and reply, verbatim.
    pub latest_prompt: Option<String>,
    pub latest_reply: Option<String>,
    /// Resolved Work identity: the repo id, checkout and branch the cwd
    /// landed in. `None` where the cwd resolves to nothing on disk.
    pub repo: Option<String>,
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
    /// The conversation's persisted touch intervals, oldest first - the
    /// exact placements live observation and the provider's records
    /// proved.
    pub touches: Vec<TouchRow>,
    /// The incarnation the newest open interval names, if one is current.
    pub current_incarnation: Option<String>,
    /// When the conversation began as its own records date it: the
    /// transcript's first record, else the live process's start; `None`
    /// is `?`.
    pub started_at: Option<u64>,
    /// The conversations proven related to this one, strongest group
    /// first - lineage, then shared incarnations.
    pub related: Vec<RelatedRow>,
    /// The arbitration and store evidence the `e` view renders.
    pub evidence: EvidenceRow,
}

/// One provider branch mark, placed: from `at_ms` (epoch ms) on, the
/// conversation's project in `repo` was on `branch`; `None` is a detached
/// stretch with no placeable branch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrailMark {
    at_ms: u64,
    repo: String,
    branch: Option<String>,
}

impl ConversationRow {
    /// Whether a live process is bound right now: the claim resolved to an
    /// attachment whose instance verdict is not `Dead`. Distinct from
    /// `live`, which only says a live session record exists - a crashed
    /// agent's stale file is a record, not a process.
    pub fn running(&self) -> bool {
        self.attachment
            .as_ref()
            .is_some_and(|a| a.liveness != AttachmentLiveness::Dead)
    }
}

/// The collector: owns the plugins (and so their incremental indexes), the
/// caches reused across passes until their freshness deadline, the store
/// the journal lives in, and the retained view every stage merges into. A
/// collect's only write is seen-state into our own store: the focus
/// observation acknowledges a watched conversation.
pub struct Collector {
    claude: Claude,
    remotes: RemoteCache,
    /// The forge CLI handle and its five-minute answer cache; stage 4
    /// serves every branch ask through them.
    forge: forge::Forge,
    forge_cache: forge::ForgeCache,
    /// The configured `forgotten_after`: how old a quiet unfinished row
    /// must be before it sinks into `Forgotten`.
    forgotten_after: Duration,
    /// Config-load warnings, retained in `errors` like any read failure.
    warnings: Vec<SourceError>,
    /// The event journal and authored records; `None` where no state dir
    /// could be placed (no `$HOME`, no `$XDG_STATE_HOME`).
    store: Option<Store>,
    /// Per-conversation weak `Busy -> Idle` stabilizer state, carried
    /// across passes so confirmations accumulate between refreshes.
    idles: HashMap<String, attention::WeakIdle>,
    /// When each live record that carries no time of its own was first read
    /// in its current content - its stand-in `since` and observation time,
    /// so polling the same record never moves them.
    undated: HashMap<String, Undated>,
    /// The last-published view: every field keeps its last value until the
    /// stage that owns it lands a replacement.
    model: Model,
    /// Pool width for per-repository and per-remote fan-out.
    workers: usize,
}

/// The retained view one refresh stages into place. Published snapshots
/// are rebuilt from it, so nothing visible ever regresses to `?` once it
/// was proven - the only exception is evidence this pass has already
/// replaced or dropped.
#[derive(Default)]
struct Model {
    conversations: Vec<ConversationRow>,
    repos: BTreeMap<String, RepoModel>,
    /// The authored Work state - incarnation ids, parked flags, lifecycle
    /// fingerprints - loaded at stage 1 and re-read whenever `work.json`
    /// changed: by the pass's own sync, or by a `p` landing mid-pass.
    work: store::Work,
    /// The `work.json` stamp `work` was read at.
    work_stamp: Option<store::WorkStamp>,
    /// Conversation key -> its provider branch trail, placed in the
    /// repository its cwd resolved to; replaced by every stage 2.
    trails: HashMap<String, Vec<TrailMark>>,
    /// Repo id -> the epoch ms of this process's last ref sync over it:
    /// an active incarnation's last observation, which a quiet pass does
    /// not write to `work.json`.
    ref_observed: HashMap<String, u64>,
    errors: Vec<SourceError>,
    skipped: Vec<String>,
    stale_sockets: usize,
    complete: bool,
}

/// One repository - or a non-git project space - as the model holds it.
struct RepoModel {
    /// The repository itself; `None` for a project space.
    repo: Option<git::Repo>,
    name: String,
    path: PathBuf,
    data: RepoData,
}

enum RepoData {
    /// The anchors with their local facts and merged vector state.
    Git(vector::RepoLocal),
    /// The one row a non-git space carries.
    Space(Box<WorkRow>),
}

impl Collector {
    /// A collector over the Claude store at `claude_root` (`~/.claude`).
    #[rustfmt::skip]
    pub fn new(claude_root: PathBuf) -> Collector {
        let claude = Claude::new(claude_root);
        let remotes = RemoteCache::default();
        let forge = forge::Forge::from_env();
        let forge_cache = forge::ForgeCache::new(vector::REMOTE_DEADLINE); // coverage: off - the unexecuted instantiation's region edge
        let forgotten_after = config::DEFAULT_FORGOTTEN_AFTER;
        let model = Model::default(); // coverage: off - the unexecuted instantiation's region edge
        let workers = fanout::WORKERS; // coverage: off - same
        Collector { claude, remotes, forge, forge_cache, forgotten_after, warnings: Vec::new(), store: None, idles: HashMap::new(), undated: HashMap::new(), model, workers }
    }

    /// Read (and acknowledge through) the store at `dir` - the journal of
    /// hook events and the authored records. Without it the snapshot holds
    /// published evidence only.
    pub fn with_store(mut self, dir: PathBuf) -> Collector {
        self.store = Some(Store::open(dir));
        self
    }

    /// The resolved configuration's collector-relevant fields: today the
    /// `forgotten_after` threshold; load warnings are retained in the
    /// snapshot's `errors` like every other read failure.
    pub fn with_config(mut self, loaded: config::Loaded) -> Collector {
        self.forgotten_after = loaded.config.forgotten_after;
        self.warnings = loaded
            .warnings
            .into_iter()
            .map(|detail| SourceError {
                source: "config".to_owned(),
                detail,
            })
            .collect();
        self
    }

    /// The forge handle the collector asks - `Forge::with_path` in tests
    /// routes stage 4's lookups at a stub `gh`/`glab` directory.
    pub fn with_forge(mut self, forge: forge::Forge) -> Collector {
        self.forge = forge;
        self
    }

    /// The pool width the staged stages fan out at; `1` makes the pass
    /// strictly sequential, which tests use to make the published order
    /// deterministic.
    pub fn with_workers(mut self, workers: usize) -> Collector {
        self.workers = workers.max(1);
        self
    }

    /// One complete pass, staged: runtime and provider inventory first,
    /// then cwd resolution and local Git per repository newest-activity
    /// first, then remote evidence, then forge. Every stage merges into the
    /// retained model and publishes one complete immutable snapshot through
    /// `publish`; a field whose stage has not landed yet reads `?`, like
    /// any unknown. Returning `false` from `publish` stops the pass early -
    /// the receiver is gone and more evidence has nowhere to go.
    pub fn collect_staged(
        &mut self,
        runtime: &Runtime,
        own_pane: Option<&PaneRef>,
        publish: &mut dyn FnMut(Snapshot) -> bool,
    ) {
        // Stage 1 - runtime and provider inventory: conversations with
        // their published state, attachments and runtime evidence, before
        // any Git subprocess runs. The store loads here too - the journal
        // and authored records are stage-1 evidence.
        let observed_at = runtime.observed_at;
        let inventory = self.claude.scan();
        self.model.errors = inventory.errors;
        // Stamp before loading: a write racing the load then shows as a
        // changed stamp at the next publish, never as a missed one.
        let work_stamp = self.store.as_ref().and_then(Store::work_stamp);
        let mut loaded = self.store.as_ref().map(Store::load).unwrap_or_default();
        self.model.errors.append(&mut loaded.errors);
        self.model.errors.extend(self.warnings.iter().cloned());
        self.model.work = std::mem::take(&mut loaded.work);
        self.model.work_stamp = work_stamp;
        self.model.skipped = inventory
            .skipped
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        self.model.stale_sockets = runtime.panes.stale_sockets;
        self.model.complete = false;

        // Claims -> resolved attachments, kept parallel so each live
        // conversation gets its own liveness/placement verdict back.
        let mut claims = Vec::new();
        let mut claim_of: Vec<usize> = Vec::new();
        for (i, conv) in inventory.conversations.iter().enumerate() {
            if let Some(claim) = conv.claim(observed_at) {
                claims.push(claim);
                claim_of.push(i);
            }
        }
        let resolved = runtime.resolve_attachments(&claims);
        let mut attachment_of: Vec<Option<usize>> = vec![None; inventory.conversations.len()];
        for (slot, conv_index) in claim_of.iter().enumerate() {
            attachment_of[*conv_index] = Some(slot);
        }
        // Which conversations are actually running: a claim was resolved and
        // the instance verdict is not `Dead`. A live file left behind by a
        // crashed agent is stale evidence, not a live process.
        let running: Vec<bool> = (0..inventory.conversations.len())
            .map(|i| attachment_of[i].is_some_and(|slot| resolved[slot].liveness.may_be_live()))
            .collect();

        // Timestamp-less dates belong to the conversation's current live
        // instance: once a claim resolves dead - or stops claiming - the
        // entry is dropped so an observed dead interval cannot carry its
        // episode into a resume under another process.
        let live_keys: HashSet<String> = (0..inventory.conversations.len())
            .filter(|&i| running[i])
            .map(|i| store::conversation_key("claude", &inventory.conversations[i].session_id))
            .collect();
        retain_live_undated(&mut self.undated, &live_keys);

        // Focus: a poll that observes a bound pane active, its window
        // current and its session attached proves the user saw the agent -
        // unless the pane is the dashboard's own, which cannot.
        let watched: Vec<bool> = (0..inventory.conversations.len())
            .map(|i| {
                attachment_of[i]
                    .and_then(|slot| resolved[slot].attachment.pane.as_ref())
                    .is_some_and(|pref| {
                        !is_own_pane(own_pane, pref)
                            && runtime.panes.panes.iter().any(|p| {
                                p.id == pref.pane
                                    && p.socket == pref.socket
                                    && p.active
                                    && p.window_active
                                    && p.session_attached > 0
                            })
                    })
            })
            .collect();

        // The conversation rows keep their previous Work placement until
        // stage 2 resolves this pass's cwds - a moved checkout shows its
        // last proven anchor rather than flickering to `?` every refresh.
        // The model keeps inventory order so `placements[i]` stays aligned;
        // the attention sort happens per publish.
        let now_ms = store::epoch_ms(observed_at);
        let mut conversations: Vec<ConversationRow> = Vec::new();
        for (i, conv) in inventory.conversations.iter().enumerate() {
            let resolved_claim = attachment_of[i].map(|slot| &resolved[slot]);
            let attachment = resolved_claim.map(|r| attachment_row(r, &runtime.panes));
            let key = store::conversation_key("claude", &conv.session_id);
            // The published claim applies only while it is bound to a live
            // attachment; a dead `(pid, pid_start)` leaves it as history.
            let instance = resolved_claim
                .filter(|r| r.liveness.may_be_live())
                .map(|r| observed_instance(r, runtime.processes.as_ref()));
            let live = instance.map(|i| (i.pid, process_start(i.pid_start)));
            let published = conv.live.as_ref().and_then(|l| {
                // A record with no time of its own is dated when first read
                // and keeps that date while later polls read it unchanged.
                let first = first_read(&mut self.undated, &key, l, instance?, now_ms);
                Some(attention::Published {
                    status: l.status,
                    waiting_for: l.waiting_for.clone(),
                    observed_ms: l.updated_at.map_or(first, store::epoch_ms),
                    since_ms: Some(
                        l.status_updated_at
                            .or(l.updated_at)
                            .map_or(first, store::epoch_ms),
                    ),
                })
            });
            let idle = self.idles.entry(key.clone()).or_default();
            let derive = |seen: store::Seen, idle: &mut attention::WeakIdle| {
                attention::derive(attention::Inputs {
                    fold: loaded.folds.get(&key),
                    seen,
                    mark: loaded.marks.get(&key),
                    published: published.clone(),
                    live,
                    now_ms,
                    ack_ok: loaded.ack_readable,
                    idle,
                })
            };
            let mut derived = derive(loaded.seen.get(&key).copied().unwrap_or_default(), idle);
            // A watched conversation's pending attention is acknowledged on
            // the spot - every unseen latch and the live wait - and the row
            // derives again on what was stored.
            if watched[i]
                && (derived.ack_through > 0 || derived.wait_ms.is_some())
                && let Some(store) = &self.store
            {
                match store.acknowledge(&key, derived.ack_through, derived.wait_ms) {
                    Ok(seen) => {
                        loaded.seen.insert(key.clone(), seen);
                        derived = derive(seen, idle);
                    }
                    Err(e) => self.model.errors.push(seen_state_error(&key, e)), // coverage: off - a seen-state write failure needs a store fault mid-pass; the attention shows again next pass
                }
            }
            let carried = self
                .model
                .conversations
                .iter()
                .find(|c| c.session_id == conv.session_id);
            let (repo, worktree, branch) = carried
                .map(|c| (c.repo.clone(), c.worktree.clone(), c.branch.clone()))
                .unwrap_or_default();
            conversations.push(conversation_row(
                conv,
                attachment,
                derived,
                repo,
                worktree,
                branch,
                EvidenceIn {
                    fold: loaded.folds.get(&key),
                    seen: loaded.seen.get(&key).copied().unwrap_or_default(),
                    mark: loaded.marks.get(&key).copied(),
                    rejected: loaded
                        .rejected
                        .iter()
                        .filter(|r| r.conversation == key)
                        .cloned()
                        .collect(),
                },
            ));
        }
        self.model.conversations = conversations;
        if !self.emit(runtime, own_pane, publish) {
            return;
        }

        // Stage 2 - cwd resolution, then local Git per repository ordered
        // by newest conversation activity. Work identity per conversation:
        // the cwd resolves to a checkout, a bare repo, a project space, or
        // nothing still on disk. Distinct cwds are few while conversations
        // are many, so each resolves once per pass - a failure is one
        // error, not one per conversation.
        let mut placements = resolve_cwds(&inventory.conversations, &mut self.model.errors);
        // The provider trails stay in the model: the evidence dated
        // touches derive from, not part of the published rows.
        self.model.trails = self
            .model
            .conversations
            .iter()
            .zip(resolve_trails(&inventory.conversations, &placements))
            .map(|(c, trail)| {
                (
                    store::conversation_key(c.provider.as_str(), &c.session_id),
                    trail,
                )
            })
            .collect();
        for (conv, place) in self.model.conversations.iter_mut().zip(placements.iter()) {
            let (repo, worktree, branch) = match place {
                Some(CwdPlacement::Checkout {
                    repo_id,
                    root,
                    branch,
                }) => (Some(repo_id.clone()), Some(root.clone()), branch.clone()),
                Some(CwdPlacement::ProjectSpace { path }) => {
                    (Some(path.display().to_string()), None, None)
                }
                None => (None, None, None),
            };
            conv.repo = repo;
            conv.worktree = worktree;
            conv.branch = branch;
        }

        // Non-git project spaces become rows on their own pseudo-repo -
        // one per distinct space, not one per conversation sitting in it.
        let mut spaces = std::collections::HashSet::new();
        for place in placements.iter().flatten() {
            if let CwdPlacement::ProjectSpace { path } = place {
                if !spaces.insert(path) {
                    continue;
                }
                let id = path.display().to_string();
                self.model
                    .repos
                    .entry(id.clone())
                    .or_insert_with(|| RepoModel {
                        repo: None,
                        name: display_name(path),
                        path: path.clone(),
                        data: RepoData::Space(Box::new(space_row(&id, path))),
                    });
            }
        }

        // The repos conversations resolved into, newest activity first.
        let order = repo_order(&inventory.conversations, &placements);
        // Repos that fell out of scope leave with this pass's stage 2 -
        // the stage replaces their rows with nothing, which is an answer.
        let keep: std::collections::HashSet<String> = order
            .iter()
            .cloned()
            .chain(spaces.iter().map(|p| p.display().to_string()))
            .collect();
        self.model.repos.retain(|id, _| keep.contains(id));
        if !self.emit(runtime, own_pane, publish) {
            return;
        }

        {
            let conversations = &inventory.conversations;
            let running = &running;
            let placements = &placements;
            let mut alive = true;
            fanout::fan_out(
                &order,
                self.workers,
                |repo_id| {
                    let repo = git::Repo {
                        common_dir: PathBuf::from(repo_id),
                    };
                    vector::collect_local_repo(&repo, |anchor| {
                        runtime_facts(runtime, conversations, running, placements, anchor, repo_id)
                    })
                    .map_err(|e| anchor_error(repo_id, e)) // coverage: off - needs a repo whose worktree read fails mid-pass
                },
                |i, result| {
                    let repo_id = &order[i];
                    match result {
                        Ok(local) => self.merge_repo(repo_id, local),
                        Err(error) => self.fail_repo(repo_id, error), // coverage: off - needs a repo's worktree list to fail after its cwd resolved, mid-pass
                    }
                    alive = self.emit(runtime, own_pane, publish);
                },
            );
            if !alive {
                return;
            }
        }
        self.reconcile_worktree_spaces(
            runtime,
            &inventory.conversations,
            &running,
            &mut placements,
        );
        // Stage 3 - remote evidence, one `ls-remote --symref` per repo and
        // remote per deadline, fanned out; then the local probes each
        // remote answer unlocks (bases, ahead/behind, landed, unpushed).
        let mut asks: Vec<(git::Repo, String)> = Vec::new();
        for model in self.model.repos.values() {
            let (Some(repo), RepoData::Git(local)) = (&model.repo, &model.data) else {
                continue;
            };
            for remote in &local.asks {
                if !self.remotes.fresh(repo, remote) {
                    asks.push((repo.clone(), remote.clone()));
                }
            }
        }
        fanout::fan_out(
            &asks,
            self.workers,
            |(repo, remote)| repo.remote_listing(remote),
            |i, listing| {
                #[rustfmt::skip]
                let (repo, remote) = &asks[i]; // coverage: off - the bounds arm never fires: `i` enumerates `asks` itself
                self.remotes.seed(repo, remote, listing); // coverage: off - seed's cached-stat arm never fires: the entry was just written
            }, // coverage: off - the closure edge of the unexecuted instantiation
        ); // coverage: off - same
        // The listings apply reads: every ask, whether the pool just fetched  // coverage: off - the line's zero region is an instantiation edge, not code
        // it or the cache still held it. A `peek` miss means the asks  // coverage: off - same
        // enumeration is wrong; it fails closed like an unreachable remote. // coverage: off - the unexecuted instantiation's region edge
        let listings = &self.current_listings(); // coverage: off - the unexecuted instantiation's region edge
        let model = &self.model; // coverage: off - same
        let jobs: Vec<&String> = order
            .iter()
            .filter(|id| {
                model
                    .repos
                    .get(*id)
                    .is_some_and(|m| matches!(m.data, RepoData::Git(_)))
            })
            .collect();
        let mut applied: Vec<Option<Vec<vector::RemoteApplied>>> =
            jobs.iter().map(|_| None).collect();
        fanout::fan_out(
            &jobs,
            self.workers,
            |id| {
                let model = &model.repos[*id];
                let (Some(repo), RepoData::Git(local)) = (&model.repo, &model.data) else {
                    unreachable!("jobs holds Git models only") // coverage: off - filtered above
                };
                vector::apply_remote(repo, local, |repo, remote| {
                    listings
                        .get(&(repo.common_dir().to_owned(), remote.to_owned()))
                        .cloned()
                        .unwrap_or_else(|| unprobed_listing(remote)) // coverage: off - apply only consults remotes the asks enumeration seeded
                })
            },
            |i, a| applied[i] = Some(a),
        );
        for (i, repo_id) in jobs.iter().enumerate() {
            let Some(applied) = applied[i].take() else {
                continue; // coverage: off - fan_out delivers every index
            };
            self.apply_to_repo(repo_id, applied);
            if !self.emit(runtime, own_pane, publish) {
                return;
            }
        }

        // Stage 4 - forge enrichment: one status lookup per branch whose
        // remote parses as a forge remote, through the five-minute cache,
        // fanned out like the remote evidence before it.
        self.collect_forge();
        // Authored work state syncs once the pass's evidence is final:
        // incarnation records, parked flags and the lifecycle fingerprints
        // transitions date from. Best-effort - a refused write reports and
        // the rows keep their last-known authored values.
        if let Some(store) = self.store.clone() {
            self.sync_work(&store, now_ms);
        }
        self.model.complete = true;
        self.emit(runtime, own_pane, publish);
    }

    /// The forge fan-out: enumerate every branch anchor's `(remote_url,
    /// branch)` ask, skip what the cache still holds, fetch the rest in
    /// parallel, then apply a cached status to every anchor that asked.
    /// Anchors without a forge remote get their `Unknown` reason here.
    fn collect_forge(&mut self) {
        let now = std::time::Instant::now();
        let mut asks: Vec<(String, String)> = Vec::new();
        let mut settled: Vec<(String, usize, ForgeStatus)> = Vec::new();
        for (repo_id, model) in &self.model.repos {
            let RepoData::Git(local) = &model.data else {
                continue;
            };
            for (i, work) in local.anchors.iter().enumerate() {
                let Some(branch) = work.state.anchor.branch().map(str::to_owned) else {
                    settled.push((repo_id.clone(), i, unknown_forge("no branch")));
                    continue;
                };
                match &work.state.remote_url {
                    Some(url) if forge::parse_remote(url).is_some() => {
                        if !self.forge_cache.fresh(url, &branch, now) {
                            asks.push((url.clone(), branch));
                        }
                    }
                    Some(_) => {
                        settled.push((repo_id.clone(), i, unknown_forge("not a forge remote")))
                    }
                    None => settled.push((repo_id.clone(), i, unknown_forge("no upstream remote"))),
                }
            }
        }
        asks.sort();
        asks.dedup();
        let mut answers: Vec<Option<ForgeStatus>> = vec![None; asks.len()];
        fanout::fan_out(
            &asks,
            self.workers,
            |(url, branch)| self.forge.status(url, branch),
            |i, status| answers[i] = Some(status), // coverage: off - the closure edge of the unexecuted instantiation
        );
        for ((url, branch), status) in asks.iter().zip(answers) {
            let Some(status) = status else {
                continue; // coverage: off - fan_out delivers every index
            };
            self.forge_cache.seed(url, branch, status, now);
        }
        for (repo_id, i, status) in settled {
            let Some(RepoModel {
                data: RepoData::Git(local),
                ..
            }) = self.model.repos.get_mut(&repo_id)
            else {
                continue; // coverage: off - the model cannot change underneath one pass
            };
            if let Some(work) = local.anchors.get_mut(i) {
                work.state.forge = status;
            } // coverage: off - the get-miss edge is unreachable: `i` indexes this same vec
        }
        for model in self.model.repos.values_mut() {
            let RepoData::Git(local) = &mut model.data else {
                continue;
            };
            for work in &mut local.anchors {
                let (Some(url), Some(branch)) =
                    (&work.state.remote_url, work.state.anchor.branch())
                else {
                    continue;
                };
                if let Some(status) = self.forge_cache.peek(url, branch) {
                    work.state.forge = status.clone();
                }
            }
        }
    }

    /// Sync `work.json` with what the pass observed: one ref sync per Git
    /// repo (which closes records for vanished refs and opens new
    /// incarnations), one path sync per detached anchor and project
    /// space, then one touch sync over every conversation's exact
    /// placement. `model.work` re-reads after each write so the published
    /// snapshot's identities, intervals and parked flags are this
    /// pass's, not the stage-1 load's.
    fn sync_work(&mut self, store: &Store, observed_ms: u64) {
        // `(repo, branch)` -> the tip the pass proved: what a touch's
        // `head` carries.
        let mut heads: HashMap<(String, String), String> = HashMap::new();
        let git_worktrees: HashSet<PathBuf> = self
            .model
            .repos
            .values()
            .flat_map(|model| match &model.data {
                RepoData::Git(local) => local
                    .anchors
                    .iter()
                    .filter_map(|w| match &w.state.anchor {
                        Anchor::Worktree { path, .. } => Some(path.clone()),
                        Anchor::Branch { .. } => None,
                    })
                    .collect::<Vec<_>>(),
                RepoData::Space(_) => Vec::new(),
            })
            .collect();
        let mut observed = Vec::new();
        for (repo_id, model) in &self.model.repos {
            match &model.data {
                RepoData::Git(local) => {
                    let mut seen = HashSet::new();
                    let refs: Vec<store::ObservedRef> = local
                        .anchors
                        .iter()
                        .filter_map(|w| {
                            let name = w.state.anchor.branch()?;
                            if let Some(head) = w.ref_head() {
                                heads.insert((repo_id.clone(), name.to_owned()), head.to_owned());
                            }
                            seen.insert(name.to_owned()).then(|| store::ObservedRef {
                                name: name.to_owned(),
                                head: w.ref_head().map(str::to_owned),
                                rewritten: rewritten(&self.model.work, repo_id, name, w),
                                creation: w.ref_creation().map(|c| store::RefCreationEvidence {
                                    head: c.head.clone(),
                                    at_ms: store::epoch_ms(c.at),
                                }),
                                renamed_from: w.renamed_from().map(str::to_owned),
                                commit: observed_commit(&w.state, w.ref_head()),
                                inputs: lifecycle_inputs(&w.state),
                            })
                        })
                        .collect();
                    let previous = self.model.ref_observed.get(repo_id).copied();
                    match store.sync_repo_after(repo_id, &refs, observed_ms, previous) {
                        Ok(()) => observed.push(repo_id.clone()),
                        Err(e) => self.model.errors.push(work_state_error(repo_id, e)),
                    }
                    for w in &local.anchors {
                        if let Anchor::Worktree {
                            path,
                            head: Head::Detached(_),
                            ..
                        } = &w.state.anchor
                            && let Err(e) = store.sync_path(
                                &path.display().to_string(),
                                repo_id,
                                &lifecycle_inputs(&w.state),
                                observed_ms,
                            )
                        {
                            self.model.errors.push(work_state_error(repo_id, e));
                        }
                    }
                }
                RepoData::Space(row) => {
                    let claimed = git_worktrees.iter().any(|wt| inside_path(&model.path, wt));
                    if !claimed
                        && let Err(e) = store.sync_path(
                            &row.repo,
                            &row.repo,
                            &store::LifecycleInputs::default(),
                            observed_ms,
                        )
                    {
                        self.model.errors.push(work_state_error(repo_id, e));
                    }
                }
            }
        }
        for repo_id in observed {
            self.model.ref_observed.insert(repo_id, observed_ms);
        }
        // Repo reconciliation first: placements name the active
        // incarnation ids the sync just settled.
        let mut errors = self.refresh_work();
        self.model.errors.append(&mut errors);
        let mut placements = Vec::new();
        let mut dated = Vec::new();
        for c in &self.model.conversations {
            let key = store::conversation_key(c.provider.as_str(), &c.session_id);
            let trail = self.model.trails.get(&key).map_or(&[][..], Vec::as_slice);
            dated.extend(dated_touches(&key, trail, &self.model.work));
            // A cwd places only a live conversation: where a dead one's
            // directory points now says nothing about where it worked.
            if !c.running() {
                continue;
            }
            // No checkout on a branch, no record of it, or an unproven tip
            // (a touch would name a head it cannot swear to): no exact
            // placement.
            let (Some(repo), Some(branch)) = (&c.repo, &c.branch) else {
                continue;
            };
            let (Some(record), Some(head)) = (
                self.model.work.branch(repo, branch),
                heads.get(&(repo.clone(), branch.clone())),
            ) else {
                continue;
            };
            placements.push(store::TouchPlacement {
                conversation: key,
                branch: record.id.clone(),
                head: head.clone(),
                provenance: store::TouchProvenance::Cwd,
                confidence: store::Confidence::Exact,
            });
        }
        if let Err(e) = store.sync_touches(&placements, &dated, observed_ms) {
            self.model.errors.push(SourceError {
                source: "work.json".to_owned(),
                detail: format!("touches: {e}"),
            });
        }
        let mut errors = self.refresh_work();
        self.model.errors.append(&mut errors);
        let mut bound: HashMap<String, store::UpdateIdentity> = HashMap::new();
        for touch in &self.model.work.touches {
            if touch.valid_until.is_none() && touch.confidence == store::Confidence::Exact {
                bound.insert(
                    touch.conversation.clone(),
                    store::UpdateIdentity::Branch(touch.branch.clone()),
                );
            }
        }
        let mut updates = Vec::new();
        for conv in &self.model.conversations {
            let key = store::conversation_key(conv.provider.as_str(), &conv.session_id);
            let identity = bound.get(&key).cloned().or_else(|| {
                if conv.branch.is_some() {
                    return None;
                }
                conv.worktree
                    .as_ref()
                    .map(|path| store::UpdateIdentity::Path(path.display().to_string()))
                    .or_else(|| conv.repo.clone().map(store::UpdateIdentity::Path))
            });
            let (Some(identity), Some(at)) = (identity, conv.last_activity) else {
                continue;
            };
            updates.push(store::SessionUpdate {
                identity,
                conversation: key,
                at_ms: at.saturating_mul(1000),
                reason: conv.title.as_ref().map_or_else(
                    || conv.short_id.clone(),
                    |title| format!("{} {}", conv.short_id, title),
                ),
            });
        }
        match store.sync_session_updates(&updates) {
            Ok(()) => {}
            Err(e) => self.model.errors.push(work_state_error("sessions", e)), // coverage: off - a refused write needs a filesystem fault
        }
        let mut errors = self.refresh_work();
        self.model.errors.append(&mut errors);
    }

    /// Re-read `work.json` into the model when its stamp moved since the
    /// last read - the pass's own sync or a `p` from the TUI - and return
    /// that read's errors. An unchanged file is not parsed again.
    fn refresh_work(&mut self) -> Vec<SourceError> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        let stamp = store.work_stamp();
        if stamp == self.model.work_stamp {
            return Vec::new();
        }
        let (work, errors) = store.work();
        self.model.work = work;
        self.model.work_stamp = stamp;
        errors
    }

    /// One pass run to completion: the staged collect's final snapshot,
    /// identical in classification to an unstaged collect because it is the
    /// same collection streamed rather than buffered.
    pub fn collect(&mut self, runtime: &Runtime, own_pane: Option<&PaneRef>) -> Snapshot {
        let mut last = None;
        self.collect_staged(runtime, own_pane, &mut |snapshot| {
            last = Some(snapshot);
            true
        });
        last.expect("a staged pass always publishes") // coverage: off - stage 1 always publishes
    } // coverage: off - the unexecuted instantiation's exit edge

    /// Rebuild the publishable snapshot from the retained model: work rows
    /// projected from every anchor's state, repo rollups recomputed, and
    /// the completeness flag as it currently stands.
    fn emit(
        &mut self,
        runtime: &Runtime,
        own_pane: Option<&PaneRef>,
        publish: &mut dyn FnMut(Snapshot) -> bool,
    ) -> bool {
        let now = epoch(runtime.observed_at);
        // Authored state as the store holds it now, not as stage 1 loaded
        // it: a `p` that lands mid-pass must hold in every later stage.
        // Stage 1 and the sync already report the file's read errors.
        let _ = self.refresh_work();
        let authored = &self.model.work;
        // Touches re-derive per publish like the Work identities: stage 1
        // may show last pass's persisted intervals, the publish after the
        // sync shows the reconciled ones. The model carries them so
        // `binds` sees the same rows classification does.
        let numbers = incarnation_numbers(authored);
        for c in &mut self.model.conversations {
            let key = store::conversation_key(c.provider.as_str(), &c.session_id);
            c.touches = authored
                .touches
                .iter()
                .filter(|t| t.conversation == key)
                .filter_map(|t| touch_row(t, authored, &numbers))
                .collect();
            // Provider intervals can land after the observed ones they
            // predate: the row reads in time order, landing order within.
            c.touches.sort_by_key(|t| t.valid_from);
            c.current_incarnation = c
                .touches
                .iter()
                .filter(|t| t.valid_until.is_none())
                .max_by_key(|t| t.valid_from)
                .map(|t| t.incarnation_id.clone());
        }
        relate(&mut self.model.conversations, runtime);
        let mut work = Vec::new();
        for (id, model) in &self.model.repos {
            match &model.data {
                RepoData::Git(local) => {
                    for anchor in &local.anchors {
                        let mut row = work_row(id, &model.name, &anchor.state, authored);
                        row.panes = anchor_panes(&anchor.state.anchor, &runtime.panes);
                        work.push(row);
                    }
                }
                RepoData::Space(row) => {
                    let mut row = (**row).clone();
                    apply_path_record(&mut row, authored);
                    work.push(row);
                }
            }
        }
        // The paths the live rows still anchor: a closed record whose
        // last workspace is one of them leaves its references to the live
        // row, and produces no gone row at all.
        let live_paths: HashSet<String> = work
            .iter()
            .filter_map(|w| w.worktree.as_ref().map(|p| p.display().to_string()))
            .collect();
        work.extend(gone_rows(&self.model, authored, runtime, &live_paths));
        // A branch row's session counts are its incarnation's, not its
        // location's: runtime facts count every conversation under the
        // worktree or repo, but the row claims only the exact touches to
        // its incarnation - closed same-name intervals count toward the
        // excluded history they name, never toward the current row.
        for w in &mut work {
            // An active incarnation was observed by this process's last
            // ref sync over its repo, even when that quiet pass wrote
            // nothing.
            if let Some(inc) = &mut w.incarnation
                && let Some(ms) = self.model.ref_observed.get(&w.repo)
            {
                inc.last_observed_at = inc.last_observed_at.max(ms / 1000);
            }
            let Some(id) = w.incarnation.as_ref().map(|i| i.id.as_str()) else {
                continue;
            };
            w.past_sessions = self
                .model
                .conversations
                .iter()
                .filter(|c| c.touches.iter().any(|t| t.incarnation_id == id))
                .count();
            w.live_sessions = self
                .model
                .conversations
                .iter()
                .filter(|c| {
                    c.running()
                        && c.touches.iter().any(|t| {
                            t.incarnation_id == id
                                && t.valid_until.is_none()
                                && t.confidence == store::Confidence::Exact
                        })
                })
                .count();
            w.live_pids = w.live_sessions;
            attribute_commits(&mut w.commits, id, &self.model.conversations);
        }
        // Attention, section and summary are derived per publish from the
        // conversations bound to the row; sections order first, newest
        // activity inside a section, a stable identity last.
        for w in &mut work {
            classify_work(w, &self.model.conversations, self.forgotten_after, now);
        }
        sort_work(&mut work);
        let mut repos = Vec::new();
        for (id, model) in &self.model.repos {
            let live = self
                .model
                .conversations
                .iter()
                .filter(|c| c.running() && c.repo.as_deref() == Some(id.as_str()))
                .count();
            let (default_branch, remote) = repo_default(model);
            let mut row = RepoRow {
                id: id.clone(),
                name: model.name.clone(),
                path: model.path.clone(),
                git: model.repo.is_some(),
                work: 0,
                live,
                attention: Attention::None,
                open: 0,
                clean: 0,
                last_activity: None,
                default_branch,
                remote,
                counts: RepoCounts::default(),
            };
            row.roll_up(&work);
            repos.push(row);
        }
        sort_repos(&mut repos);
        let mut conversations = self.model.conversations.clone();
        sort_conversations(&mut conversations, runtime.observed_at);
        publish(Snapshot {
            schema_version: SCHEMA_VERSION,
            observed_at: epoch(runtime.observed_at),
            own_pane: own_pane.map(|p| p.pane.as_str().to_owned()),
            complete: self.model.complete,
            repos,
            work,
            conversations,
            errors: self.model.errors.clone(),
            skipped: self.model.skipped.clone(),
            stale_sockets: self.model.stale_sockets,
        })
    }

    /// Every ask's last-known listing: fetched this pass, or still fresh
    /// in the cache. A `peek` miss means the asks enumeration is wrong; it
    /// fails closed like an unreachable remote.
    #[rustfmt::skip]
    fn current_listings(&self) -> HashMap<(PathBuf, String), git::RemoteListing> {
        let mut listings = HashMap::new();
        for model in self.model.repos.values() { // coverage: off - the unexecuted instantiation's region edge
            let (Some(repo), RepoData::Git(local)) = (&model.repo, &model.data) else { continue; }; // coverage: off - the model cannot change underneath one pass
            for remote in &local.asks {
                if let Some(listing) = self.remotes.peek(repo, remote) { listings.insert((repo.common_dir().to_owned(), remote.clone()), listing.clone()); } // coverage: off - the None arm is unreachable: every ask was seeded by the pool or the deadline cache
            }
        }
        listings
    } // coverage: off - the unexecuted instantiation's exit edge

    /// Apply one repo's remote-phase results into its local state.
    #[rustfmt::skip]
    fn apply_to_repo(&mut self, repo_id: &str, applied: Vec<vector::RemoteApplied>) {
        let Some(RepoModel { data: RepoData::Git(local), .. }) = self.model.repos.get_mut(repo_id) else { return }; // coverage: off - the model cannot change underneath one pass
        for (work, a) in local.anchors.iter_mut().zip(applied) {
            work.apply(a);
        }
    }

    /// A repo whose local read failed keeps its error and drops its row.
    #[rustfmt::skip]
    fn fail_repo(&mut self, repo_id: &str, error: SourceError) { self.model.errors.push(error); self.model.repos.remove(repo_id); } // coverage: off - the caller's arm needs a gitdir to vanish mid-pass

    fn reconcile_worktree_spaces(
        &mut self,
        runtime: &Runtime,
        conversations: &[Conversation],
        running: &[bool],
        placements: &mut [Option<CwdPlacement>],
    ) {
        let mut claimed: Vec<(PathBuf, String, Option<String>)> = Vec::new();
        for (repo_id, model) in &self.model.repos {
            let RepoData::Git(local) = &model.data else {
                continue;
            };
            for w in &local.anchors {
                let Anchor::Worktree { path, head, .. } = &w.state.anchor else {
                    continue;
                };
                let branch = match head {
                    Head::Branch(name) | Head::Unborn(name) => Some(name.clone()),
                    Head::Detached(_) => None,
                };
                claimed.push((path.clone(), repo_id.clone(), branch));
            }
        }
        if claimed.is_empty() {
            return;
        }
        self.model.repos.retain(|_, model| match &model.data {
            RepoData::Space(_) => !claimed
                .iter()
                .any(|(path, ..)| inside_path(&model.path, path)),
            RepoData::Git(_) => true,
        });
        let mut touched: HashSet<String> = HashSet::new();
        for (conv, place) in self
            .model
            .conversations
            .iter_mut()
            .zip(placements.iter_mut())
        {
            let Some(CwdPlacement::ProjectSpace { path }) = place else {
                continue;
            };
            let Some((root, repo_id, branch)) = claimed
                .iter()
                .filter(|(root, ..)| inside_path(path, root))
                .max_by_key(|(root, ..)| root.components().count())
            else {
                continue;
            };
            *place = Some(CwdPlacement::Checkout {
                repo_id: repo_id.clone(),
                root: root.clone(),
                branch: branch.clone(),
            });
            conv.repo = Some(repo_id.clone());
            conv.worktree = Some(root.clone());
            conv.branch = branch.clone();
            touched.insert(repo_id.clone());
        }
        if touched.is_empty() {
            return;
        }
        let trails = resolve_trails(conversations, placements);
        for (conv, trail) in self.model.conversations.iter().zip(trails) {
            self.model.trails.insert(
                store::conversation_key(conv.provider.as_str(), &conv.session_id),
                trail,
            );
        }
        for repo_id in &touched {
            let Some(RepoModel {
                data: RepoData::Git(local),
                ..
            }) = self.model.repos.get_mut(repo_id)
            else {
                continue; // coverage: off - touched only ever names a collected Git repo
            };
            for work in &mut local.anchors {
                let facts = runtime_facts(
                    runtime,
                    conversations,
                    running,
                    placements,
                    &work.state.anchor,
                    repo_id,
                );
                work.state.vector.windows = facts.windows;
                work.state.vector.live_pids = facts.live_pids;
                work.state.vector.live_agent_sessions = facts.live_agent_sessions;
                work.state.vector.past_agent_sessions = facts.past_agent_sessions;
            }
        }
    }

    /// Merge one finished repository into the model. Remote-owned fields
    /// carry their last-pass values over into the fresh local state - they
    /// keep their last value until stage 3 replaces it.
    fn merge_repo(&mut self, repo_id: &str, mut local: vector::RepoLocal) {
        let repo = git::Repo {
            common_dir: PathBuf::from(repo_id),
        };
        let prior: HashMap<String, &vector::WorkState> = match self.model.repos.get(repo_id) {
            Some(RepoModel {
                data: RepoData::Git(old),
                ..
            }) => old
                .anchors
                .iter()
                .map(|w| (anchor_key(&w.state.anchor), &w.state))
                .collect(),
            _ => HashMap::new(),
        };
        for work in &mut local.anchors {
            if let Some(old) = prior.get(&anchor_key(&work.state.anchor)) {
                carry_remote(&mut work.state, old);
            }
        }
        let (name, path) = repo_display(local.anchors.iter().map(|w| &w.state.anchor), &repo);
        self.model.repos.insert(
            repo_id.to_owned(),
            RepoModel {
                repo: Some(repo),
                name,
                path,
                data: RepoData::Git(local),
            },
        );
    }
}

/// The internal spelling of a resolved cwd - repo identity plus where in it.
#[derive(Debug, Clone)]
enum CwdPlacement {
    Checkout {
        repo_id: String,
        root: PathBuf,
        branch: Option<String>,
    },
    ProjectSpace {
        path: PathBuf,
    },
}

/// Every conversation's cwd resolved, one `git::resolve` per distinct path:
/// conversations share cwds, and the same failure would otherwise emit an
/// identical error per conversation sharing it.
fn resolve_cwds(
    conversations: &[Conversation],
    errors: &mut Vec<SourceError>,
) -> Vec<Option<CwdPlacement>> {
    let mut memo: HashMap<PathBuf, Option<CwdPlacement>> = HashMap::new();
    conversations
        .iter()
        .map(|conv| match conv.cwd() {
            Some(cwd) => memo
                .entry(cwd.to_owned())
                .or_insert_with(|| resolve_cwd(cwd, errors))
                .clone(),
            None => None,
        })
        .collect()
}

/// Every conversation's provider branch trail, placed in the repository
/// its own cwd resolved to: the branch names the provider records are the
/// project directory's, so they belong to that repository and no other.
/// A conversation whose cwd is no checkout has no placeable trail.
fn resolve_trails(
    conversations: &[Conversation],
    placements: &[Option<CwdPlacement>],
) -> Vec<Vec<TrailMark>> {
    conversations
        .iter()
        .zip(placements)
        .map(|(conv, place)| {
            let Some(CwdPlacement::Checkout { repo_id, .. }) = place else {
                return Vec::new();
            };
            conv.branch_trail()
                .iter()
                .map(|mark| TrailMark {
                    at_ms: store::epoch_ms(mark.at),
                    repo: repo_id.clone(),
                    branch: mark.branch.clone(),
                })
                .collect()
        })
        .collect()
}

/// `cwd` -> checkout / project space / nothing. An unreadable or vanished
/// path is no anchor at all; the error is retained, the row gets no claim.
fn resolve_cwd(cwd: &Path, errors: &mut Vec<SourceError>) -> Option<CwdPlacement> {
    if !cwd.is_dir() {
        return None;
    }
    match git::resolve(cwd) {
        Ok(Resolved::Checkout(checkout)) => Some(CwdPlacement::Checkout {
            repo_id: checkout.repo.common_dir().display().to_string(),
            root: checkout.root,
            branch: match checkout.head {
                Head::Branch(name) | Head::Unborn(name) => Some(name),
                Head::Detached(_) => None,
            },
        }),
        Ok(Resolved::ProjectSpace(path)) => Some(CwdPlacement::ProjectSpace { path }),
        // A cwd inside `.git` or a bare repo has no checkout to anchor on;
        // it anchors on the repository itself through its common dir.
        Ok(Resolved::RepoOnly(repo)) => Some(CwdPlacement::ProjectSpace {
            path: repo.common_dir().to_owned(),
        }),
        Err(e) => {
            errors.push(SourceError {
                source: "git resolve".to_owned(),
                detail: format!("{}: {e}", cwd.display()),
            });
            None
        }
    }
}

/// The error a touched repo's failed anchor scan records.
fn anchor_error(repo_id: &str, e: git::Error) -> SourceError /* // coverage: off - needs a repo deleted mid-collection */
{
    let source = "git".to_owned(); // coverage: off - needs a repo deleted mid-collection
    let detail = format!("{repo_id}: {e}"); // coverage: off - same
    SourceError { source, detail } // coverage: off - same
} // coverage: off - same
/// The repo row's name and path: the main checkout's, or - when no anchor
/// is a main checkout, a bare repo for instance - the common dir itself.
fn repo_display<'a>(
    mut anchors: impl Iterator<Item = &'a Anchor>,
    repo: &git::Repo,
) -> (String, PathBuf) {
    anchors
        .find_map(|a| match a {
            Anchor::Worktree { path, main, .. } if *main => {
                Some((display_name(path), path.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            (
                display_name(repo.common_dir()),
                repo.common_dir().to_owned(),
            )
        })
}

/// A path's own name, with `.git`-dir and root edges rendered sanely.
fn display_name(path: &Path) -> String {
    if path.file_name().is_none_or(|n| n == ".git")
        && let Some(parent) = path.parent()
        && parent.file_name().is_some()
    {
        return display_name(parent);
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Agent-visible runtime facts for one anchor's worktree: windows bound to
/// it, and the live/total conversations whose cwd resolves under it. A
/// branch-only anchor has no worktree - nothing binds to it by path.
fn runtime_facts(
    runtime: &Runtime,
    conversations: &[Conversation],
    running: &[bool],
    placements: &[Option<CwdPlacement>],
    anchor: &Anchor,
    repo_id: &str,
) -> RuntimeFacts {
    let (path, admin_id) = match anchor {
        Anchor::Worktree { path, admin_id, .. } => (Some(path.as_path()), admin_id.as_deref()),
        Anchor::Branch { .. } => (None, None),
    };
    let mut facts = RuntimeFacts {
        windows: WindowCount::default(),
        live_pids: 0,
        live_agent_sessions: 0,
        past_agent_sessions: 0,
    };
    if let Some(path) = path {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned()); // coverage: off - a reported path canonicalizes
        facts.windows.total = runtime.panes.windows_bound(admin_id, path);
        // Orphaned windows are bound by derived evidence alone: a window
        // carrying no stored worktree edge whose pane cwds land inside.
        let mut orphaned = std::collections::HashSet::new();
        for pane in &runtime.panes.panes {
            if pane.wt_adminid.is_none() && pane.binds_worktree(admin_id, path) {
                orphaned.insert((&pane.socket, &pane.window));
            }
        }
        facts.windows.orphaned = orphaned.len();
        for (i, (_, place)) in conversations.iter().zip(placements.iter()).enumerate() {
            let Some(CwdPlacement::Checkout { root, .. }) = place else {
                continue;
            };
            if root != &canonical {
                continue;
            }
            facts.past_agent_sessions += 1;
            if running[i] {
                facts.live_agent_sessions += 1;
                facts.live_pids += 1;
            }
        }
    } else {
        // A branch-only row still counts conversations in its repository.
        for (i, (_, place)) in conversations.iter().zip(placements.iter()).enumerate() {
            if matches!(place, Some(CwdPlacement::Checkout { repo_id: id, .. }) if id == repo_id) {
                facts.past_agent_sessions += 1;
                if running[i] {
                    facts.live_agent_sessions += 1;
                    facts.live_pids += 1;
                }
            }
        }
    }
    facts
}

/// The `vector::WorkState` collapsed into the row's display contract.
/// Fields whose stage has not landed read `Unknown` and render `?`, like
/// any other unknown. `authored` carries the persisted record: a branch
/// row's work identity is its active incarnation's id, a detached row's
/// its canonical path.
fn work_row(
    repo_id: &str,
    repo_name: &str,
    state: &vector::WorkState,
    authored: &store::Work,
) -> WorkRow {
    let anchor = &state.anchor;
    let v = &state.vector;
    let (kind, name, branch) = match anchor {
        Anchor::Worktree { head, main, .. } => {
            let branch = match head {
                Head::Branch(name) | Head::Unborn(name) => Some(name.clone()),
                Head::Detached(_) => None,
            };
            let name = match head {
                Head::Branch(name) | Head::Unborn(name) => name.clone(),
                Head::Detached(sha) => format!("detached @{}", short_sha(sha)),
            };
            let kind = match (branch.is_some(), main) {
                (_, true) => WorkKind::Worktree,
                (true, false) => WorkKind::Branch,
                (false, false) => WorkKind::Detached,
            };
            (kind, name, branch)
        }
        Anchor::Branch { name } => (WorkKind::Branch, name.clone(), Some(name.clone())),
    };
    let (upstream, upstream_detail) = upstream_of(&v.upstream_state);
    // Work identity: an active incarnation's id for a branch row, the
    // canonical path for a detached one. A branch whose record the sync
    // has not written yet carries no identity rather than a guess.
    let (identity, parked, authored_ms, authored_updates) = match &branch {
        Some(name) => match authored.branch(repo_id, name) {
            Some(r) => (
                Some(r.id.clone()),
                r.parked,
                r.activity_at,
                r.updates.clone(),
            ),
            None => (None, false, None, Vec::new()),
        },
        None => {
            let path = v.worktree.as_ref().map(|p| p.display().to_string());
            match path.as_deref().and_then(|p| authored.path(p)) {
                Some(r) => (path, r.parked, r.activity_at, r.updates.clone()),
                None => (path, false, None, Vec::new()),
            }
        }
    };
    // The incarnation rows: every retained record of this `(repo,
    // ref_name)`, numbered oldest first - the active one on the row, the
    // closed ones as excluded same-name history.
    let mut incarnation = None;
    let mut same_name_history = Vec::new();
    if let Some(name) = &branch {
        let mut records: Vec<&store::BranchRecord> = authored
            .branches
            .values()
            .filter(|r| r.repo == repo_id && &r.ref_name == name)
            .collect();
        sort_incarnations(&mut records);
        for (number, record) in records.into_iter().enumerate() {
            let row = incarnation_row(record, number + 1);
            if record.ended_at.is_some() {
                same_name_history.push(row);
            } else {
                incarnation = Some(row);
            }
        }
    }
    let (removal, deletion) = verdict::cleanup(state, &state.forge);
    WorkRow {
        repo: repo_id.to_owned(),
        repo_name: repo_name.to_owned(),
        kind,
        name,
        worktree: v.worktree.clone(),
        branch,
        dirty: v.dirty.known().copied(),
        broken: state.broken.as_ref().map(|reason| match &v.worktree {
            Some(path) if !path.join(".git").exists() => {
                format!(".git missing; metadata retained by {repo_name}")
            }
            _ => reason.clone(),
        }),
        commits_ahead: v.commits_ahead_of_base.known().copied(),
        unpushed: v.unpushed_commits.known().copied(),
        upstream,
        upstream_detail,
        landed: landed_of(v),
        base: state.base.known().map(|b| b.label()),
        windows: v.windows.total,
        live_pids: v.live_pids,
        live_sessions: v.live_agent_sessions,
        past_sessions: v.past_agent_sessions,
        // Meaningful activity: the Git time the pass observed, folded with
        // the persisted transition time. Bound conversations' activity
        // joins in `classify_work`.
        last_activity: v
            .last_git_activity
            .map(epoch)
            .into_iter()
            .chain(authored_ms.map(|ms| ms / 1000))
            .max(),
        attention: Attention::None,
        identity,
        incarnation,
        same_name_history,
        parked,
        forge: state.forge.item,
        pipeline: state.forge.pipeline,
        forge_label: state.forge.label.clone(),
        forge_url: state.forge.url.clone(),
        commits_behind: v.commits_behind_of_base.known().copied(),
        commits: v.commits_not_on_base.known().map(|list| {
            list.iter()
                .map(|c| CommitRow {
                    sha: c.sha.clone(),
                    subject: c.subject.clone(),
                    at: c.at,
                    // Attributed per publish, once touches are this pass's.
                    conversation: None,
                })
                .collect()
        }),
        // Filled by the caller with the runtime's pane inventory.
        panes: Vec::new(),
        gone: None,
        references: Vec::new(),
        transition_at: authored_ms.map(|ms| ms / 1000),
        git_activity_at: v.last_git_activity.map(epoch),
        updates: authored_updates,
        worktree_removal: Some(removal),
        branch_deletion: Some(deletion),
        section: WorkSection::FollowUp,
        summary: String::new(),
    }
}

/// The row's upstream reading and its detail: `remote/merge_ref` for a
/// configured upstream, the reason for an unknown one.
fn upstream_of(state: &UpstreamState) -> (Upstream, Option<String>) {
    match state {
        UpstreamState::NeverPushed => (Upstream::NeverPushed, None),
        UpstreamState::Tracked { remote, merge_ref } => {
            (Upstream::Tracked, Some(format!("{remote}/{merge_ref}")))
        }
        UpstreamState::RemoteGone { remote, merge_ref } => {
            (Upstream::RemoteGone, Some(format!("{remote}/{merge_ref}")))
        }
        UpstreamState::Unknown(reason) => (Upstream::Unknown, Some(reason.clone())),
        UpstreamState::NotApplicable => (Upstream::NotApplicable, None),
    }
}

/// The row's proven landed verdict, `None` while unproven.
fn landed_of(v: &vector::StateVector) -> Option<Landed> {
    v.landed.known().map(|l| match l {
        LandedVerdict::AncestorMerged => Landed::Ancestor,
        LandedVerdict::ContentMerged => Landed::Content,
        LandedVerdict::No => Landed::No,
    })
}

/// The lifecycle fingerprint one anchor's state produces for the authored
/// record: every field a `work.json` record compares next pass to date a
/// transition at its observation. Unproven readings are `None`, so they
/// keep the record's last proven value instead of dating a transition.
fn lifecycle_inputs(state: &vector::WorkState) -> store::LifecycleInputs {
    let v = &state.vector;
    let (upstream, detail) = upstream_of(&v.upstream_state);
    let head = match &state.anchor {
        Anchor::Worktree {
            head: Head::Detached(sha),
            ..
        } => Some(sha.clone()),
        _ => None,
    };
    store::LifecycleInputs {
        dirty: v.dirty.known().copied(),
        worktree: Some(v.worktree.is_some()),
        git_dir: match &state.anchor {
            // A stat failure is unproven, not missing: the record keeps
            // its last value instead of dating a false ".git missing".
            Anchor::Worktree { path, .. } => path.join(".git").try_exists().ok(),
            Anchor::Branch { .. } => None,
        },
        worktree_path: v.worktree.as_ref().map(|p| p.display().to_string()),
        admin_id: match &state.anchor {
            Anchor::Worktree { admin_id, .. } => admin_id.clone(),
            Anchor::Branch { .. } => None,
        },
        ahead: v.commits_ahead_of_base.known().copied(),
        behind: v.commits_behind_of_base.known().copied(),
        unpushed: v.unpushed_commits.known().copied(),
        upstream: (upstream != Upstream::Unknown).then(|| match detail {
            Some(detail) => format!("{} {detail}", upstream.as_str()),
            None => upstream.as_str().to_owned(),
        }),
        landed: landed_of(v).map(|l| l.as_str().to_owned()),
        forge: (state.forge.item != WorkItem::Unknown)
            .then(|| state.forge.item.as_str().to_owned()),
        pipeline: (state.forge.pipeline != Pipeline::Unknown)
            .then(|| state.forge.pipeline.as_str().to_owned()),
        worktree_state: match &state.anchor {
            Anchor::Worktree { prunable, .. } => Some(match prunable {
                Some(reason) => format!("broken: {reason}"),
                None => "healthy".to_owned(),
            }),
            Anchor::Branch { .. } => None,
        },
        commit: observed_commit(state, head.as_deref()),
        head,
        working_tree: v
            .worktree
            .as_deref()
            .zip(v.working_tree.known())
            .map(|(root, bytes)| {
                let status = git::status(bytes, root);
                store::WorkingTreeSnapshot {
                    fingerprint: status.fingerprint,
                    reasons: status.reasons,
                }
            }),
    }
}

fn observed_commit(state: &vector::WorkState, head: Option<&str>) -> Option<store::ObservedCommit> {
    let sha = head?;
    let listed = state
        .vector
        .commits_not_on_base
        .known()
        .and_then(|commits| commits.iter().find(|c| c.sha == sha));
    Some(store::ObservedCommit {
        sha: sha.to_owned(),
        subject: listed.map(|c| c.subject.clone()),
        at_ms: listed.map(|c| c.at.saturating_mul(1000)),
    })
}

/// The `Unknown` forge status anchors without a forge remote settle to.
fn unknown_forge(reason: &str) -> ForgeStatus {
    ForgeStatus {
        item: WorkItem::Unknown,
        pipeline: Pipeline::Unknown,
        label: None,
        url: None,
        reason: Some(reason.to_owned()),
    }
}

/// The error a failed `work.json` write records: it lands in the
/// snapshot's `errors`, never silently.
fn work_state_error(repo_id: &str, e: std::io::Error) -> SourceError {
    SourceError {
        source: "work.json".to_owned(),
        detail: format!("{repo_id}: {e}"),
    }
}

/// The dated touches a provider's branch trail proves: each mark that
/// names a branch in a resolved repo becomes an interval until the next
/// mark, on the one incarnation of that name that provably existed at the
/// mark. A mark no single incarnation covers places nothing.
fn dated_touches(
    conversation: &str,
    trail: &[TrailMark],
    authored: &store::Work,
) -> Vec<store::DatedTouch> {
    let mut touches = Vec::new();
    for (i, mark) in trail.iter().enumerate() {
        let Some(branch) = &mark.branch else {
            continue;
        };
        let repo = &mark.repo;
        let Some(id) = incarnation_at(authored, repo, branch, mark.at_ms) else {
            continue;
        };
        touches.push(store::DatedTouch {
            conversation: conversation.to_owned(),
            branch: id,
            valid_from: mark.at_ms,
            valid_until: trail.get(i + 1).map(|next| next.at_ms),
        });
    }
    touches
}

/// The incarnation of `(repo, ref_name)` that provably existed at `at_ms`:
/// from its reflog creation (or, without one, its first observation)
/// through its end. Zero or several candidates is no answer - a time
/// before any proven start, or inside a boundary the records cannot
/// order, fails closed.
fn incarnation_at(
    authored: &store::Work,
    repo: &str,
    ref_name: &str,
    at_ms: u64,
) -> Option<String> {
    let mut covering = authored.branches.values().filter(|r| {
        let start = r
            .creation_evidence
            .as_ref()
            .map_or(r.first_observed_at, |c| c.at_ms.min(r.first_observed_at));
        r.repo == repo
            && r.ref_name == ref_name
            && start <= at_ms
            && r.ended_at.is_none_or(|e| at_ms <= e)
    });
    let found = covering.next()?;
    covering.next().is_none().then(|| found.id.clone())
}

/// Whether the anchor's branch tip moved off the active record's last
/// proven head without descending from it - one `merge-base
/// --is-ancestor` only when the tip moved. A first sighting, an unproven
/// tip or a comparison git could not answer (the old commit pruned) is
/// not a proven rewrite.
fn rewritten(authored: &store::Work, repo_id: &str, name: &str, w: &vector::AnchorWork) -> bool {
    let (Some(old), Some(new)) = (
        authored
            .branch(repo_id, name)
            .and_then(|r| r.head.as_deref()),
        w.ref_head(),
    ) else {
        return false;
    };
    old != new && w.state.repo.is_ancestor(old, new) == Evidence::Known(false)
}

/// Incarnation order within a `(repo, ref_name)`: oldest first,
/// `(first_observed_at, id)` - the id is a stable tiebreak for two
/// records a single pass both dated.
fn sort_incarnations(records: &mut [&store::BranchRecord]) {
    records.sort_by(|a, b| {
        a.first_observed_at
            .cmp(&b.first_observed_at)
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// The `#N` numbers: every retained incarnation's 1-based rank within its
/// `(repo, ref_name)`, ordered `(first_observed_at, id)` - the numbering a
/// `#1`/`#2` label and a touch path share.
fn incarnation_numbers(authored: &store::Work) -> HashMap<String, usize> {
    let mut by_name: HashMap<(&str, &str), Vec<&store::BranchRecord>> = HashMap::new();
    for record in authored.branches.values() {
        by_name
            .entry((record.repo.as_str(), record.ref_name.as_str()))
            .or_default()
            .push(record);
    }
    let mut numbers = HashMap::new();
    for records in by_name.values_mut() {
        sort_incarnations(records);
        for (i, record) in records.iter().enumerate() {
            numbers.insert(record.id.clone(), i + 1);
        }
    }
    numbers
}

/// One persisted record as its serialized row, numbered within its
/// `(repo, ref_name)` and marked excluded when closed.
fn incarnation_row(record: &store::BranchRecord, number: usize) -> IncarnationRow {
    IncarnationRow {
        id: record.id.clone(),
        number,
        repo: record.repo.clone(),
        ref_name: record.ref_name.clone(),
        first_observed_at: record.first_observed_at / 1000,
        last_observed_at: record.last_observed_at / 1000,
        creation_head: record.creation_evidence.as_ref().map(|c| c.head.clone()),
        creation_at: record.creation_evidence.as_ref().map(|c| c.at_ms / 1000),
        head: record.head.clone(),
        ended_at: record.ended_at.map(|ms| ms / 1000),
        continuity: record.continuity_evidence,
        excluded: record.ended_at.is_some(),
    }
}

/// One persisted touch interval as its serialized row: the incarnation's
/// repo, name and number resolved from the authored records. `None` when
/// the record itself is gone.
fn touch_row(
    touch: &store::BranchTouch,
    authored: &store::Work,
    numbers: &HashMap<String, usize>,
) -> Option<TouchRow> {
    let record = authored.branches.get(&touch.branch)?;
    Some(TouchRow {
        incarnation_id: touch.branch.clone(),
        repo: record.repo.clone(),
        ref_name: record.ref_name.clone(),
        incarnation: numbers.get(&touch.branch).copied().unwrap_or(0), // coverage: off - numbering covers every stored record
        head: touch.head.clone(),
        valid_from: touch.valid_from / 1000,
        valid_until: touch.valid_until.map(|ms| ms / 1000),
        provenance: touch.provenance,
        confidence: touch.confidence,
    })
}

/// Refresh a path-keyed row's authored fields - a project space's row is
/// stored in the model, so parked and transition dates apply per publish.
fn apply_path_record(row: &mut WorkRow, authored: &store::Work) {
    let Some(identity) = &row.identity else {
        return; // coverage: off - a space row's identity is its path, always present
    };
    if let Some(record) = authored.path(identity) {
        row.parked = record.parked;
        row.updates = record.updates.clone();
        row.last_activity = row
            .last_activity
            .into_iter()
            .chain(record.activity_at.map(|ms| ms / 1000))
            .max();
    }
}

/// The panes bound to an anchor's worktree - a branch-only anchor binds
/// nothing by path.
fn anchor_panes(anchor: &Anchor, panes: &tmux::PaneInventory) -> Vec<PaneRow> {
    let Anchor::Worktree { path, admin_id, .. } = anchor else {
        return Vec::new();
    };
    panes
        .panes
        .iter()
        .filter(|p| p.binds_worktree(admin_id.as_deref(), path))
        .map(|p| PaneRow {
            handle: format!("{}:{}.{}", p.session_name, p.window, p.id),
            command: p.command.clone(),
        })
        .collect()
}

/// The repo's proven default branch and remote: the remote HEAD every
/// anchor's proven base agreed on - disagreeing or unprobed anchors
/// prove nothing and read `?`. A lone configured remote still names
/// itself for the `remote` field.
fn repo_default(model: &RepoModel) -> (Option<String>, Option<String>) {
    let RepoData::Git(local) = &model.data else {
        return (None, None);
    };
    let mut bases: HashSet<(String, String)> = HashSet::new();
    for anchor in &local.anchors {
        if let Some(base) = anchor.state.base.known() {
            bases.insert((base.remote.clone(), base.branch.clone()));
        }
    }
    let (branch, remote) = match bases.len() {
        1 => {
            let (remote, branch) = bases.into_iter().next().unwrap_or_default();
            (Some(branch), Some(remote))
        }
        _ => (None, None),
    };
    let remote = remote.or_else(|| match local.remote_names() {
        Some([one]) => Some(one.clone()),
        _ => None,
    });
    (branch, remote)
}

/// Fill every conversation's `related` from this pass's own evidence:
/// live process ancestry and shared exact incarnations. A shared ref
/// name, a pane shared at different times or mere time proximity is no
/// relation. Neither is pane-creation correlation: a tmux pane's root
/// process is a child of the daemonized tmux server, never of the agent
/// that asked for the pane, so the process table cannot prove it.
fn relate(conversations: &mut [ConversationRow], runtime: &Runtime) {
    let n = conversations.len();
    let pid_of = |i: usize| -> Option<u32> {
        let c = &conversations[i];
        c.running()
            .then(|| c.attachment.as_ref().map(|a| a.pid))
            .flatten()
    };
    let row = |strength: RelationStrength,
               label: &str,
               provenance: String,
               other: &ConversationRow| RelatedRow {
        strength,
        label: label.to_owned(),
        provenance,
        provider: other.provider,
        session_id: other.session_id.clone(),
        short_id: other.short_id.clone(),
        title: other.title.clone(),
        attention: other.attention,
        state_since: other.state_since,
    };
    let mut related: Vec<Vec<RelatedRow>> = (0..n).map(|_| Vec::new()).collect();
    for a in 0..n {
        for b in 0..n {
            if a == b {
                continue;
            }
            // Observed live process ancestry: b's process provably
            // spawned a's. Both sides list it - a names its ancestor, b
            // its descendant.
            if let (Some(pa), Some(pb)) = (pid_of(a), pid_of(b))
                && let Some(table) = runtime.processes.as_ref()
                && table.ancestors(pa).iter().skip(1).any(|&p| p == pb)
            {
                related[a].push(row(
                    RelationStrength::ProcessAncestry,
                    "ancestor",
                    "live process ancestry".to_owned(),
                    &conversations[b],
                ));
                related[b].push(row(
                    RelationStrength::ProcessAncestry,
                    "descendant",
                    "live process ancestry".to_owned(),
                    &conversations[a],
                ));
            }
            // Every exact incarnation both conversations touched.
            for t in &conversations[a].touches {
                if conversations[b]
                    .touches
                    .iter()
                    .any(|s| s.incarnation_id == t.incarnation_id)
                {
                    related[a].push(row(
                        RelationStrength::SameIncarnation,
                        "same incarnation",
                        format!("touch {}#{}", t.ref_name, t.incarnation),
                        &conversations[b],
                    ));
                }
            }
        }
    }
    // A descendant lands on its ancestor's list out of loop order: keep
    // the documented strongest-group-first order.
    for (c, mut rel) in conversations.iter_mut().zip(related) {
        rel.sort_by_key(|r| r.strength);
        c.related = rel;
    }
}

/// Name the conversation behind each listed commit of incarnation `id`:
/// the one conversation whose touch to it was open at the commit time.
/// No covering touch, or several, leaves `?` - a guess between them is
/// no attribution.
fn attribute_commits(
    commits: &mut Option<Vec<CommitRow>>,
    id: &str,
    conversations: &[ConversationRow],
) {
    let Some(commits) = commits else {
        return;
    };
    for commit in commits {
        let mut covering = conversations.iter().filter(|c| {
            c.touches.iter().any(|t| {
                t.incarnation_id == id
                    && t.valid_from <= commit.at
                    && t.valid_until.is_none_or(|until| commit.at < until)
            })
        });
        commit.conversation = match (covering.next(), covering.next()) {
            (Some(one), None) => Some(one.short_id.clone()),
            _ => None,
        };
    }
}

/// Gone work still referenced by something live: a closed branch
/// incarnation whose last proven workspace no longer anchors a live row,
/// or a path record whose directory lost every claim. It stays under
/// `Cleanup review` only while a pane, window, tmux session, live
/// process or live agent session still names it - a restorable or
/// transcript-only conversation is never a reference.
fn gone_rows(
    model: &Model,
    authored: &store::Work,
    runtime: &Runtime,
    live_paths: &HashSet<String>,
) -> Vec<WorkRow> {
    let mut rows = Vec::new();
    let numbers = incarnation_numbers(authored);
    // One vanished workspace is one row, however many closed records
    // name it - a same-name branch recreated there, a pooled directory
    // other branches reused. The newest closed record speaks for it. A
    // branch-only incarnation records nothing a reference could name; a
    // still-anchored workspace's references belong to the live row.
    let mut newest: std::collections::BTreeMap<&str, &store::BranchRecord> =
        std::collections::BTreeMap::new();
    for record in authored.branches.values().filter(|r| r.ended_at.is_some()) {
        let Some(path) = record.inputs.worktree_path.as_deref() else {
            continue;
        };
        if live_paths.contains(path) {
            continue;
        }
        let key = |r: &store::BranchRecord| (r.ended_at, r.last_observed_at, r.id.clone());
        newest
            .entry(path)
            .and_modify(|kept| {
                if key(record) > key(kept) {
                    *kept = record;
                }
            })
            .or_insert(record);
    }
    let mut emitted: HashSet<&str> = HashSet::new();
    for (path_str, record) in newest {
        let path = PathBuf::from(path_str);
        let refs = references(
            &runtime.panes,
            record.inputs.admin_id.as_deref(),
            &path,
            &model.conversations,
        );
        if refs.is_empty() {
            continue;
        }
        let gone = if path.is_dir() {
            "branch deleted".to_owned()
        } else {
            "branch and worktree gone".to_owned()
        };
        let number = numbers.get(&record.id).copied().unwrap_or(0); // coverage: off - numbering covers every stored record
        rows.push(WorkRow {
            repo: record.repo.clone(),
            repo_name: display_name(Path::new(&record.repo)),
            kind: WorkKind::Branch,
            name: record.ref_name.clone(),
            worktree: Some(path.clone()),
            branch: Some(record.ref_name.clone()),
            identity: Some(record.id.clone()),
            incarnation: Some(incarnation_row(record, number)),
            last_activity: record
                .ended_at
                .map(|ms| ms / 1000)
                .or_else(|| record.activity_at.map(|ms| ms / 1000)), // coverage: off - the loop's `ended_at.is_some()` filter proves the map
            gone: Some(gone.clone()),
            summary: format!("{gone} · {}", reference_summary(&refs)),
            references: refs,
            updates: record.updates.clone(),
            section: WorkSection::CleanupReview,
            ..space_row(&record.repo, &path)
        });
        emitted.insert(path_str);
    }
    // A path record for a workspace a closed branch already speaks for
    // adds no second row.
    for (path_str, record) in &authored.paths {
        if live_paths.contains(path_str) || emitted.contains(path_str.as_str()) {
            continue;
        }
        let path = PathBuf::from(path_str);
        let refs = references(&runtime.panes, None, &path, &model.conversations);
        if refs.is_empty() {
            continue;
        }
        let (kind, gone) = match record.inputs.worktree {
            Some(true) => (WorkKind::Detached, "worktree gone"),
            _ => (WorkKind::ProjectSpace, "project folder gone"),
        };
        // The repo the record carries, so a detached row lists under its
        // repository; a record from before the field names itself.
        let repo = record.repo.as_deref().unwrap_or(path_str);
        rows.push(WorkRow {
            kind,
            repo: repo.to_owned(),
            repo_name: display_name(Path::new(repo)),
            gone: Some(gone.to_owned()),
            summary: format!("{gone} · {}", reference_summary(&refs)),
            references: refs,
            updates: record.updates.clone(),
            last_activity: record.activity_at.map(|ms| ms / 1000),
            section: WorkSection::CleanupReview,
            ..space_row(path_str, &path)
        });
    }
    rows
}

/// The live references retaining a gone workspace `path`: every pane
/// bound to it - by a stored `wt_adminid` edge or a cwd inside it - the
/// windows and tmux sessions containing them, and the live agent
/// sessions (and their processes) still anchored inside it.
fn references(
    panes: &tmux::PaneInventory,
    admin_id: Option<&str>,
    path: &Path,
    conversations: &[ConversationRow],
) -> Vec<ReferenceRow> {
    let mut refs = Vec::new();
    let mut windows = std::collections::BTreeSet::new();
    let mut sessions = std::collections::BTreeSet::new();
    for pane in &panes.panes {
        if !pane.binds_worktree(admin_id, path) {
            continue;
        }
        refs.push(ReferenceRow {
            kind: ReferenceKind::Pane,
            label: format!("{}:{}.{}", pane.session_name, pane.window, pane.id),
        });
        windows.insert((
            pane.socket.clone(),
            pane.session_name.clone(),
            pane.window.to_string(),
        ));
        sessions.insert((pane.socket.clone(), pane.session_name.clone()));
    }
    refs.extend(
        windows
            .into_iter()
            .map(|(_, session, window)| ReferenceRow {
                kind: ReferenceKind::Window,
                label: format!("{session}:{window}"),
            }),
    );
    refs.extend(sessions.into_iter().map(|(_, session)| ReferenceRow {
        kind: ReferenceKind::TmuxSession,
        label: session,
    }));
    for c in conversations.iter().filter(|c| c.running()) {
        let inside = c.cwd.as_deref().is_some_and(|cwd| inside_path(cwd, path))
            || c.worktree.as_deref().is_some_and(|w| inside_path(w, path));
        if !inside {
            continue;
        }
        if let Some(a) = &c.attachment {
            refs.push(ReferenceRow {
                kind: ReferenceKind::Process,
                label: format!("pid {}", a.pid),
            });
        } // coverage: off - the `None` edge: `running()` proves the attachment
        refs.push(ReferenceRow {
            kind: ReferenceKind::AgentSession,
            label: format!("{}:{}", c.provider.as_str(), c.short_id),
        });
    }
    refs
}

/// Whether `cwd` sits at or below `root`, canonicalizing each side when
/// it can - a gone path falls back to its literal spelling, matching how
/// pane bindings read.
fn inside_path(cwd: &Path, root: &Path) -> bool {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    cwd.starts_with(&root)
}

/// The reference counts a gone row's summary names.
fn reference_summary(refs: &[ReferenceRow]) -> String {
    [
        (ReferenceKind::Pane, "pane"),
        (ReferenceKind::Window, "window"),
        (ReferenceKind::TmuxSession, "session"),
        (ReferenceKind::Process, "process"),
        (ReferenceKind::AgentSession, "agent session"),
    ]
    .into_iter()
    .filter_map(|(kind, word)| {
        let n = refs.iter().filter(|r| r.kind == kind).count();
        (n > 0).then(|| format!("{n} {word}{}", if n == 1 { "" } else { "s" }))
    })
    .collect::<Vec<_>>()
    .join(" · ")
}

/// Whether the conversation is bound to the work row: an open exact
/// touch to the row's incarnation for a branch-bearing row - the name
/// and the worktree path are only labels, the touch is the binding - a
/// checkout path for a detached row, or its repo identity for a project
/// space. A branch row with no recorded incarnation binds nothing
/// rather than guessing.
pub fn binds(row: &WorkRow, c: &ConversationRow) -> bool {
    match (row.kind, row.worktree.as_deref(), row.branch.as_deref()) {
        (WorkKind::ProjectSpace, _, _) => c.repo.as_deref() == Some(row.repo.as_str()),
        (WorkKind::Detached, Some(root), _) => {
            c.repo.as_deref() == Some(row.repo.as_str()) && c.worktree.as_deref() == Some(root)
        }
        (_, _, Some(_)) => match &row.identity {
            Some(id) => c.touches.iter().any(|t| {
                t.incarnation_id == *id
                    && t.valid_until.is_none()
                    && t.confidence == store::Confidence::Exact
            }),
            None => false,
        },
        // A row with neither worktree nor branch binds nothing; anchors
        // always carry one.
        _ => false, // coverage: off - a fabricated row shape: anchors always carry a worktree or a branch
    }
}

/// Fold the bound conversations' attention into the row's rollup, merge
/// their activity into the row's, then place the row in its first-match
/// section and compose its summary. The same call the pass applies is
/// what a `p` toggle re-runs on the in-memory snapshot, so a parked row
/// moves without waiting for a refresh.
pub fn classify_work(
    row: &mut WorkRow,
    conversations: &[ConversationRow],
    forgotten_after: Duration,
    now: u64,
) {
    let bound: Vec<&ConversationRow> = conversations.iter().filter(|c| binds(row, c)).collect();
    row.attention = attention::rollup(bound.iter().map(|c| &c.attention));
    for c in &bound {
        row.last_activity = row.last_activity.max(c.last_activity);
    }
    // A gone row keeps its own placement: it sits in `Cleanup review`
    // for as long as something live references it, and no lifecycle
    // verdict moves it.
    if row.gone.is_some() {
        return;
    }
    let (section, reason) = section_reason(row, &bound, forgotten_after, now);
    row.section = section;
    row.summary = summarize(row, reason);
}

/// Order the work rows as [2] renders them: section first, newest
/// meaningful activity inside it (unknown last), then a stable identity.
pub fn sort_work(work: &mut [WorkRow]) {
    work.sort_by(|a, b| {
        section_order(a)
            .cmp(&section_order(b))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
            .then_with(|| a.identity.cmp(&b.identity))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// First match in section order: attention, working, the FollowUp
/// triggers, `Forgotten`, then the cleanup verdicts.
fn section_reason(
    row: &WorkRow,
    bound: &[&ConversationRow],
    forgotten_after: Duration,
    now: u64,
) -> (WorkSection, String) {
    match row.attention {
        Attention::Waiting | Attention::Error | Attention::CompletedUnseen => {
            return (WorkSection::NeedsYou, attention_reason(row, bound));
        }
        Attention::Working => return (WorkSection::Active, attention_reason(row, bound)),
        Attention::Unknown | Attention::None => {}
    }
    let finished = matches!(row.landed, Some(Landed::Ancestor | Landed::Content));
    let quiet_beyond = row
        .last_activity
        .is_some_and(|a| now.saturating_sub(a) > forgotten_after.as_secs());
    // A resumable conversation asks for a pick-up only while its work is
    // unfinished and recent: landed work goes on to cleanup and quiet work
    // to `Forgotten`, with the conversation still listed in [3]. A project
    // space can be neither, so its resumable conversation always counts.
    let pick_up = row.kind == WorkKind::ProjectSpace || (!finished && !quiet_beyond);
    if let Some(reason) = follow_up(row, bound, pick_up) {
        return (WorkSection::FollowUp, reason);
    }
    // Forgotten: unfinished, nothing running, quiet strictly beyond the
    // configured threshold - and parked suppresses only this.
    if row.kind != WorkKind::ProjectSpace
        && !row.parked
        && !finished
        && row.live_pids == 0
        && quiet_beyond
    {
        let age = age_at(now, row.last_activity.unwrap_or(now));
        return (WorkSection::Forgotten, format!("idle {age}"));
    }
    if let Some(pair) = cleanup_section(row) {
        return pair;
    }
    match row.kind {
        // A non-actionable space is never forgotten or cleaned: it either
        // holds a resumable conversation or it is simply there.
        WorkKind::ProjectSpace => (WorkSection::FollowUp, "idle project".to_owned()),
        _ => (WorkSection::FollowUp, "open".to_owned()),
    }
}

/// The FollowUp triggers in order: known failed or pending checks, a dirty
/// tree, unpushed commits, a resumable idle conversation when `pick_up`
/// says the work is still unfinished and recent, or a blocked cleanup
/// verdict. Unknown forge state triggers nothing by itself.
fn follow_up(row: &WorkRow, bound: &[&ConversationRow], pick_up: bool) -> Option<String> {
    if row.forge == WorkItem::Open {
        match row.pipeline {
            Pipeline::Failed => return Some("checks failed".to_owned()),
            Pipeline::Busy => return Some("checks pending".to_owned()),
            Pipeline::Succeeded | Pipeline::Unknown => {}
        }
    }
    let blocked = [row.worktree_removal.as_ref(), row.branch_deletion.as_ref()]
        .into_iter()
        .flatten()
        .find(|a| a.verdict == Verdict::Blocked);
    // Landed work whose cleanup is blocked names both facts - its
    // remaining dirt is part of why it is blocked, not a separate ask.
    if blocked.is_some() && matches!(row.landed, Some(Landed::Ancestor | Landed::Content)) {
        return Some("merged · blocked".to_owned());
    }
    if row.dirty == Some(true) {
        return Some("dirty".to_owned());
    }
    if let Some(n) = row.unpushed.filter(|n| *n > 0) {
        return Some(format!("unpushed {n}"));
    }
    if pick_up
        && bound
            .iter()
            .any(|c| !c.running() && !c.resume_argv.is_empty())
    {
        return Some("resumable idle".to_owned());
    }
    if let Some(blocked) = blocked {
        // A blocked row that is not landed names its first concrete
        // blocker and is never a cleanup candidate.
        let first = blocked.reasons.first().map(String::as_str).unwrap_or("?");
        return Some(format!("blocked: {first}"));
    }
    None
}

/// The cleanup sections: every applicable action provably safe is `Ready
/// to clean` and names what goes; an applicable `Review` is `Cleanup
/// review` and names its first reason. Blocked actions never land here -
/// they are FollowUp.
fn cleanup_section(row: &WorkRow) -> Option<(WorkSection, String)> {
    let removal = row.worktree_removal.as_ref();
    let deletion = row.branch_deletion.as_ref();
    let applicable: Vec<&verdict::ActionVerdict> = [removal, deletion]
        .into_iter()
        .flatten()
        .filter(|a| a.verdict != Verdict::NotApplicable)
        .collect();
    if applicable.is_empty() {
        return None;
    }
    if applicable
        .iter()
        .all(|a| matches!(a.verdict, Verdict::Safe | Verdict::SafeAfterWorktreeRemoval))
    {
        let what = match (
            removal.is_some_and(|a| a.verdict != Verdict::NotApplicable),
            deletion.is_some_and(|a| a.verdict != Verdict::NotApplicable),
        ) {
            (true, true) => "wt + branch",
            (true, false) => "worktree only",
            (false, true) => "branch only",
            (false, false) => "?", // coverage: off - `applicable` was proven non-empty above
        };
        return Some((WorkSection::ReadyToClean, what.to_owned()));
    }
    if let Some(review) = applicable.iter().find(|a| a.verdict == Verdict::Review) {
        let first = review.reasons.first().map(String::as_str).unwrap_or("?");
        return Some((WorkSection::CleanupReview, format!("review: {first}")));
    } // coverage: off - the find-miss edge: `Blocked` verdicts exit at `FollowUp`, so a `Review` is always found
    None // coverage: off - the same edge
}

/// The attention sections' reason: `waiting: permission prompt`,
/// `error: StopFailure`, `done` - with `working` beside a retained latch
/// while the agent grinds on.
fn attention_reason(row: &WorkRow, bound: &[&ConversationRow]) -> String {
    let mut parts = Vec::new();
    let detail = bound
        .iter()
        .find(|c| c.attention == row.attention)
        .and_then(|c| c.attention_detail.as_deref());
    parts.push(match detail {
        Some(d) => format!("{}: {d}", row.attention.label()),
        None => row.attention.label().to_owned(),
    });
    if row.attention != Attention::Working
        && bound.iter().any(|c| c.state == ConversationState::Busy)
    {
        parts.push("working".to_owned());
    }
    parts.join(" · ")
}

/// The row's final summary: the section reason, then the compact evidence
/// tail with any phrase the reason already made dropped, then `parked`.
fn summarize(row: &WorkRow, reason: String) -> String {
    let mut parts = vec![reason.clone()];
    let reason_norm = normalized(&reason);
    let facts: Vec<String> = work_facts(row)
        .into_iter()
        .filter(|f| {
            // Only a pure-word fact can duplicate the reason (`~dirty`
            // beside `dirty`); `?`/`↑?` normalize to nothing and counts
            // like `↑2` may disagree with the reason's own count - both
            // always stay.
            let f = normalized(f);
            f.is_empty() || f.bytes().any(|b| b.is_ascii_digit()) || !reason_norm.contains(&f)
        })
        .collect();
    if !facts.is_empty() {
        parts.push(facts.join(" "));
    }
    if row.parked {
        parts.push("parked".to_owned());
    }
    parts.join(" · ")
}

/// `a` contains `b` as a phrase, compared on lowercase alphanumerics only,
/// so `~dirty` dedupes against the `dirty` reason and `PR #191` against
/// `open PR #191`.
fn normalized(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ')
        .collect::<String>()
        .to_lowercase()
}

/// The compact evidence tail: `no wt`, `no remote`/`remote gone`/`?`,
/// `↑n`, `~dirty`, `merged`, the PR/MR label and the pipeline read-out.
/// A project space says `no git` and nothing else.
fn work_facts(row: &WorkRow) -> Vec<String> {
    if row.kind == WorkKind::ProjectSpace {
        return vec!["no git".to_owned()];
    }
    let mut parts = Vec::new();
    if row.worktree.is_none() {
        parts.push("no wt".to_owned());
    }
    match row.upstream {
        Upstream::NeverPushed => parts.push("no remote".to_owned()),
        Upstream::RemoteGone => parts.push("remote gone".to_owned()),
        Upstream::Unknown => parts.push("?".to_owned()),
        Upstream::Tracked | Upstream::NotApplicable => {}
    }
    match row.commits_ahead {
        Some(n) if n > 0 => parts.push(format!("↑{n}")),
        None => parts.push("↑?".to_owned()),
        _ => {}
    }
    if row.dirty == Some(true) {
        parts.push("~dirty".to_owned());
    }
    if matches!(row.landed, Some(Landed::Ancestor | Landed::Content)) {
        parts.push("merged".to_owned());
    }
    if let Some(label) = &row.forge_label {
        parts.push(label.clone());
    }
    if row.forge == WorkItem::Open {
        match row.pipeline {
            Pipeline::Failed => parts.push("checks failed".to_owned()),
            Pipeline::Busy => parts.push("checks pending".to_owned()),
            Pipeline::Succeeded => parts.push("checks ok".to_owned()),
            Pipeline::Unknown => {}
        }
    }
    parts
}

/// Repos order by latest meaningful activity, unknown last, then a
/// stable name/id.
fn sort_repos(repos: &mut [RepoRow]) {
    repos.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// The section's sort slot, in the enum's declared order.
fn section_order(row: &WorkRow) -> u8 {
    match row.section {
        WorkSection::NeedsYou => 0,
        WorkSection::Active => 1,
        WorkSection::FollowUp => 2,
        WorkSection::Forgotten => 3,
        WorkSection::ReadyToClean => 4,
        WorkSection::CleanupReview => 5,
    }
}

/// How long ago `then` was at `now`, in the TUI's age buckets.
fn age_at(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// A non-git space's single row: the space anchors conversations but has
/// no Git evidence, so every Git cell is a plain unknown or n/a. Its work
/// identity is its canonical path; parked and transition dates apply per
/// publish through `apply_path_record`.
fn space_row(repo_id: &str, path: &Path) -> WorkRow {
    WorkRow {
        repo: repo_id.to_owned(),
        repo_name: display_name(path),
        kind: WorkKind::ProjectSpace,
        name: display_name(path),
        // The row's workspace is the space itself: no checkout, but the
        // path is what its conversations anchor on.
        worktree: Some(path.to_owned()),
        branch: None,
        dirty: None,
        broken: None,
        commits_ahead: None,
        unpushed: None,
        upstream: Upstream::NotApplicable,
        upstream_detail: None,
        landed: None,
        base: None,
        windows: 0,
        live_pids: 0,
        live_sessions: 0,
        past_sessions: 0,
        last_activity: None,
        attention: Attention::None,
        identity: Some(path.display().to_string()),
        incarnation: None,
        same_name_history: Vec::new(),
        parked: false,
        forge: WorkItem::Unknown,
        pipeline: Pipeline::Unknown,
        forge_label: None,
        forge_url: None,
        commits_behind: None,
        commits: None,
        panes: Vec::new(),
        gone: None,
        references: Vec::new(),
        transition_at: None,
        git_activity_at: None,
        updates: Vec::new(),
        worktree_removal: None,
        branch_deletion: None,
        section: WorkSection::FollowUp,
        summary: String::new(),
    }
}

/// The identity a work anchor keeps across passes: a branch anchor is its
/// name; a worktree anchor is its path plus the head it sits on, so a
/// worktree that switches branch counts as a different anchor - carrying
/// the old branch's remote evidence onto the new one would lie.
fn anchor_key(anchor: &Anchor) -> String {
    match anchor {
        Anchor::Branch { name } => format!("b\u{0}{name}"),
        Anchor::Worktree { path, head, .. } => {
            let head = match head {
                Head::Branch(name) | Head::Unborn(name) => name.as_str(),
                Head::Detached(sha) => sha.as_str(),
            };
            format!("w\u{0}{}\u{0}{head}", path.display())
        }
    }
}

/// Remote-owned fields keep their last-pass value while stage 3 has not
/// replaced them. Most have no local answer at all - `upstream_state`,
/// `base`, `commits_ahead` and `landed` are always `collection pending`
/// after the local phase, so the old answer carries unconditionally.
/// `unpushed_commits` is the exception: a detached HEAD already counted
/// its unreachable commits locally, and carrying a stale answer over that
/// fresh fact would lie.
fn carry_remote(new: &mut vector::WorkState, old: &vector::WorkState) {
    new.vector.upstream_state = old.vector.upstream_state.clone();
    new.base = old.base.clone();
    new.vector.commits_ahead_of_base = old.vector.commits_ahead_of_base.clone();
    new.vector.commits_behind_of_base = old.vector.commits_behind_of_base.clone();
    new.vector.commits_not_on_base = old.vector.commits_not_on_base.clone();
    new.vector.landed = old.vector.landed.clone();
    if pending(&new.vector.unpushed_commits) {
        new.vector.unpushed_commits = old.vector.unpushed_commits.clone();
    }
    // The forge overlay is stage-4-owned: it keeps its last answer until
    // stage 4 lands this pass's.
    if new.forge.reason.as_deref() == Some(vector::PENDING) {
        new.forge = old.forge.clone();
    } // coverage: off - the else edge: a fresh stage-3 state is always `collection pending`
}

/// Whether an `Evidence` is the not-yet-landed placeholder.
fn pending<T>(evidence: &Evidence<T>) -> bool {
    matches!(evidence, Evidence::Unknown(reason) if reason == vector::PENDING)
}

/// The remote listing for an ask the pool or cache somehow missed: it
/// fails closed like an unreachable remote, naming the miss.
#[rustfmt::skip]
fn unprobed_listing(remote: &str) -> RemoteListing { RemoteListing { head: RemoteHead::Unreachable(format!("remote {remote} was not probed")), refs: Evidence::Unknown(format!("remote {remote} was not probed")) } } // coverage: off - apply only consults remotes the asks enumeration seeded

/// A process start as its known epoch seconds; `None` when undated.
fn process_start(start: ProcessStart) -> Option<u64> {
    match start {
        ProcessStart::At(at) => Some(at),
        ProcessStart::Unavailable => None,
    }
}

/// `resolved` -> the row's attachment view.
fn attachment_row(
    r: &crate::runtime::ResolvedAttachment,
    panes: &tmux::PaneInventory,
) -> AttachmentRow {
    let (liveness, liveness_detail) = match &r.liveness {
        Liveness::Instance => (AttachmentLiveness::Instance, None),
        Liveness::PidOnly(reason) => (AttachmentLiveness::PidOnly, Some(reason.clone())),
        Liveness::Unverifiable(reason) => (AttachmentLiveness::Unverifiable, Some(reason.clone())),
        Liveness::Dead(reason) => (AttachmentLiveness::Dead, Some(reason.clone())),
    };
    let (pane_source, placement_detail) = match &r.placement {
        Placement::Bound(source) => (Some(*source), None),
        Placement::Unknown(reason) | Placement::Dead(reason) => (None, Some(reason.clone())),
        Placement::Superseded => (None, Some("superseded".to_owned())),
    };
    AttachmentRow {
        pid: r.attachment.process.pid,
        pid_start: process_start(r.attachment.process.pid_start),
        liveness,
        liveness_detail,
        pane: r.attachment.pane.as_ref().map(|p| display_pane(p, panes)),
        pane_source,
        placement_detail,
        source: r.attachment.source,
        observed_at: epoch(r.attachment.observed_at),
    }
}

/// A resolved attachment's pane as `session:window.pane` - the handle shape
/// a provider would publish - when the pane record is findable; the
/// socket-qualified id otherwise.
fn display_pane(pane: &PaneRef, panes: &tmux::PaneInventory) -> String {
    panes
        .panes
        .iter()
        .find(|p| p.id == pane.pane && p.socket == pane.socket)
        .map(|p| format!("{}:{}.{}", p.session_name, p.window, p.id))
        .unwrap_or_else(|| pane.to_string())
}

/// A focus acknowledgement the store refused, reported for the evidence
/// view. Its own function, like `anchor_error`, so each line carries one
/// fault path rustfmt cannot reflow away from its marker.
fn seen_state_error(key: &str, e: std::io::Error) -> SourceError /* // coverage: off - a seen-state write failure needs a store fault mid-pass */
{
    let source = "store".to_owned(); // coverage: off - a seen-state write failure needs a store fault mid-pass
    let detail = format!("seen-state for {key:?}: {e}"); // coverage: off - same
    SourceError { source, detail } // coverage: off - same
} // coverage: off - same

/// A live record's process instance and content as far as its stand-in date
/// goes: a change to any of it is a new reading, dated afresh.
#[derive(Debug, PartialEq)]
struct Undated {
    pid: u32,
    pid_start: ProcessStart,
    status: Option<String>,
    waiting_for: Option<String>,
    first_ms: u64,
}

/// The attachment's process instance, dated. The provider's own `pid_start`
/// wins when it carries one; otherwise the already-collected OS process row
/// dates the same pid without another system call. When neither can date
/// it, the pid-only limitation stands.
fn observed_instance(
    resolved: &crate::runtime::ResolvedAttachment,
    processes: Option<&crate::process::ProcessTable>,
) -> ProcessInstance {
    let mut instance = resolved.attachment.process;
    if instance.pid_start == ProcessStart::Unavailable
        && let Some(row) = processes.and_then(|table| table.get(instance.pid))
    {
        instance.pid_start = row.start;
    }
    instance
}

/// Forget timestamp-less dates whose conversation is not live this pass: a
/// dead interval observed between polls cannot carry the episode into a
/// resume.
fn retain_live_undated(undated: &mut HashMap<String, Undated>, live_keys: &HashSet<String>) {
    undated.retain(|key, _| live_keys.contains(key));
}

/// Whether two readings of one pid's start are the same instance. A start
/// may come from the OS or from a provider's quantized record, so it
/// compares with `ProcessStart`'s tolerance rather than exactly; two
/// undated readings stay pid-only and equal.
fn same_start(a: ProcessStart, b: ProcessStart) -> bool {
    a.matches(&b).unwrap_or(a == b)
}

/// When `live` was first read under `instance` with its current content,
/// epoch ms: `now_ms` for a new or changed record, the remembered time for
/// the same one.
fn first_read(
    seen: &mut HashMap<String, Undated>,
    key: &str,
    live: &crate::claude::Live,
    instance: ProcessInstance,
    now_ms: u64,
) -> u64 {
    let same = seen.get(key).is_some_and(|u| {
        u.pid == instance.pid
            && same_start(u.pid_start, instance.pid_start)
            && u.status == live.status_raw
            && u.waiting_for == live.waiting_for
    });
    if !same {
        seen.insert(
            key.to_owned(),
            Undated {
                pid: instance.pid,
                pid_start: instance.pid_start,
                status: live.status_raw.clone(),
                waiting_for: live.waiting_for.clone(),
                first_ms: now_ms,
            },
        );
    }
    seen[key].first_ms
}

/// Whether `pane` is the dashboard's own: the socket qualifies the id, so
/// the same `%N` on another server is a watched pane, not the dashboard -
/// and still acknowledges. Sockets compare as files: `$TMUX` carries
/// tmux's resolved path, discovery the spelling it listed, and the two
/// differ under a symlinked socket directory.
fn is_own_pane(own_pane: Option<&PaneRef>, pane: &PaneRef) -> bool {
    let Some(own) = own_pane else {
        return false;
    };
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    own.pane == pane.pane
        && (own.socket == pane.socket || canonical(&own.socket) == canonical(&pane.socket))
}

/// The store evidence a conversation row carries into its `e` view,
/// bundled so `conversation_row`'s parameter list stays readable.
#[derive(Default)]
struct EvidenceIn<'a> {
    /// The journal fold for the conversation.
    fold: Option<&'a store::Fold>,
    /// Its acknowledgement state.
    seen: store::Seen,
    /// The authored not-busy mark.
    mark: Option<store::Mark>,
    /// Records the fold rejected while loading this pass.
    rejected: Vec<store::RejectedRecord>,
}

fn conversation_row(
    conv: &Conversation,
    attachment: Option<AttachmentRow>,
    derived: attention::Derived,
    repo: Option<String>,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    evidence: EvidenceIn<'_>,
) -> ConversationRow {
    let state = match derived.exec {
        Exec::Busy => ConversationState::Busy,
        Exec::Idle => ConversationState::Idle,
        Exec::Waiting => ConversationState::Waiting,
        Exec::Unknown => ConversationState::Unknown,
    };
    let state_raw = match conv.state() {
        StateEvidence::Published(p) => p.raw,
        StateEvidence::Absent => None,
    };
    // Read before `derived.claims` and `attachment` move into the row.
    let marked = derived.marked;
    let jseq = (derived.journal_seq > 0).then_some(derived.journal_seq);
    let process_start = attachment.as_ref().and_then(|a| a.pid_start);
    ConversationRow {
        provider: Provider::Claude,
        session_id: conv.session_id.clone(),
        short_id: conv.session_id.chars().take(8).collect(),
        title: conv.title(),
        state,
        state_raw,
        waiting_for: derived.waiting_for,
        // Without a live claim arbitration proves no `since`; the row keeps
        // the provider's own time so history still has an age.
        state_since: derived
            .since_ms
            .map(|ms| ms / 1000)
            .or_else(|| conv.state_since().map(epoch)),
        state_since_ms: derived.since_ms,
        attention: derived.attention,
        attention_detail: derived.attention_detail,
        attention_seq: (derived.ack_through > 0).then_some(derived.ack_through),
        attention_wait_ms: derived.wait_ms,
        journal_seq: jseq,
        last_activity: conv.last_activity().map(epoch),
        live: conv.live.is_some(),
        attachment,
        cwd: conv.cwd().map(Path::to_owned),
        transcript: conv.transcript.as_ref().map(|t| t.file.clone()),
        malformed_lines: conv.transcript.as_ref().map(|t| t.malformed_lines),
        resume_argv: conv
            .resume_argv()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        latest_prompt: conv
            .transcript
            .as_ref()
            .and_then(|t| t.latest_prompt.clone()),
        latest_reply: conv
            .transcript
            .as_ref()
            .and_then(|t| t.latest_reply.clone()),
        repo,
        worktree,
        branch,
        touches: Vec::new(),
        current_incarnation: None,
        started_at: conv
            .transcript
            .as_ref()
            .and_then(|t| t.first_at)
            .map(epoch)
            .or(process_start),
        // Filled per publish by `relate`, once every row's touches are
        // this pass's.
        related: Vec::new(),
        evidence: EvidenceRow {
            claims: derived.claims,
            latches: evidence.fold.map_or(Vec::new(), |f| {
                f.retained
                    .iter()
                    .map(|r| LatchRow {
                        seq: r.seq,
                        kind: r.kind,
                        at_ms: r.at_ms,
                        reason: r.reason.clone(),
                        acknowledged: r.seq <= evidence.seen.seq,
                    })
                    .collect()
            }),
            rejected: evidence.rejected,
            seen_seq: (evidence.seen.seq > 0).then_some(evidence.seen.seq),
            seen_wait_ms: evidence.seen.wait_ms,
            mark: evidence.mark,
            mark_suppressed: marked,
            journal_seq: jseq,
            producer_seq: evidence.fold.and_then(|f| f.pseq_high),
        },
    }
}

/// Ordering for the sorted conversation list: the cross-row attention
/// rank then time-in-state, so a waiting row outranks everything older.
/// Retained attention on a dead conversation keeps its rank - an
/// unacknowledged `end` or `error` is durable. Work rows sort per publish,
/// by section then meaningful activity.
fn sort_conversations(conversations: &mut [ConversationRow], at: SystemTime) {
    conversations.sort_by(|a, b| {
        a.attention
            .rank()
            .cmp(&b.attention.rank())
            .then_with(|| age_of(a, at).cmp(&age_of(b, at)))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
}

/// The repos stage 2 collects, newest conversation activity first - the
/// work the user touched most recently fills in first. Ties break on repo
/// id so the order is deterministic.
fn repo_order(conversations: &[Conversation], placements: &[Option<CwdPlacement>]) -> Vec<String> {
    let mut activity: BTreeMap<String, Option<SystemTime>> = BTreeMap::new();
    for (conv, place) in conversations.iter().zip(placements.iter()) {
        let Some(CwdPlacement::Checkout { repo_id, .. }) = place else {
            continue;
        };
        let entry = activity.entry(repo_id.clone()).or_default();
        if *entry < conv.last_activity() {
            *entry = conv.last_activity();
        }
    }
    let mut ids: Vec<String> = activity.keys().cloned().collect();
    ids.sort_by_key(|id| std::cmp::Reverse(activity[id]));
    ids
}

/// How long the row has carried its effective state; unknown sorts last.
fn age_of(c: &ConversationRow, at: SystemTime) -> Duration {
    match c.state_since {
        Some(since) => at
            .duration_since(UNIX_EPOCH + Duration::from_secs(since))
            .unwrap_or_default(),
        None => Duration::MAX,
    }
}

fn epoch(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn short_sha(sha: &str) -> &str {
    let end = sha
        .char_indices()
        .nth(7)
        .map(|(i, _)| i)
        .unwrap_or(sha.len());
    &sha[..end]
}

/// The snapshot as the `list --json` document: complete and unfiltered.
/// A serialization failure (a non-UTF-8 path anywhere in the rows) is an
/// operational error - never a plausible-looking empty document.
pub fn to_json(snapshot: &Snapshot) -> serde_json::Result<String> {
    serde_json::to_string_pretty(snapshot) // coverage: off - `list --json` runs as a subprocess in tests
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::{Live, Transcript}; // coverage: off - the unexecuted instantiation's region edge
    use crate::process::{ProcessInstance, ProcessRow, ProcessTable};
    use crate::provider::PublishedStatus;
    use crate::runtime::{EvidenceSource, LiveAttachment, ResolvedAttachment}; // coverage: off - the unexecuted instantiation's region edge
    use crate::tmux::PaneId;
    use std::fs;

    /// One conversation fabricated to order: `live` and `transcript` each
    /// optional, so every merge shape can be built. // coverage: off - the line's zero region is an instantiation edge, not code
    fn conversation(live: Option<Live>, transcript: Option<Transcript>) -> Conversation {
        Conversation {
            session_id: "11111111-2222-3333-4444-555555555555".to_owned(),
            live,
            transcript: transcript.map(std::sync::Arc::new),
        }
    }

    /// A live record publishing `busy`; tests override what they need.
    fn live() -> Live {
        Live {
            file: PathBuf::from("/root/sessions/1.json"),
            pid: 1,
            pid_start: ProcessStart::Unavailable,
            cwd: None,
            tmux: None,
            status_raw: Some("busy".to_owned()),
            status: Some(PublishedStatus::Busy),
            waiting_for: None,
            updated_at: None,
            status_updated_at: None,
            name: None,
        }
    }

    /// A transcript whose newest record is at `last_at`.
    fn transcript_at(last_at: SystemTime) -> Transcript {
        Transcript {
            file: PathBuf::from("/root/projects/-r/1.jsonl"),
            slug: "-r".to_owned(),
            session_id: "11111111-2222-3333-4444-555555555555".to_owned(),
            project_cwd: None,
            summary: None,
            latest_prompt: None,
            latest_reply: None,
            first_at: None,
            last_at: Some(last_at),
            malformed_lines: 0,
            branch_trail: Vec::new(),
        }
    }

    /// A resolved attachment fabricated to order.
    fn attachment(liveness: Liveness, placement: Placement, pane: bool) -> ResolvedAttachment {
        ResolvedAttachment {
            attachment: LiveAttachment {
                session: None,
                process: ProcessInstance {
                    pid: 42,
                    pid_start: ProcessStart::At(1_800_000_000),
                },
                pane: pane.then(|| PaneRef {
                    socket: PathBuf::from("/tmp/socket"),
                    pane: PaneId::parse("%7").unwrap(),
                }),
                observed_at: SystemTime::now(),
                source: EvidenceSource::Published,
            },
            liveness,
            placement,
        }
    }

    /// A branch record fabricated to order.
    fn branch_record(id: &str, first_ms: u64, ended_ms: Option<u64>) -> store::BranchRecord {
        store::BranchRecord {
            id: id.to_owned(),
            repo: "/r/.git".to_owned(),
            ref_name: "feat".to_owned(),
            first_observed_at: first_ms,
            last_observed_at: first_ms,
            head: None,
            creation_evidence: None,
            continuity_evidence: store::ContinuityEvidence::FirstObservation,
            ended_at: ended_ms,
            parked: false,
            activity_at: None,
            inputs: store::LifecycleInputs::default(),
            updates: Vec::new(),
            session_activity: Default::default(),
        }
    }

    /// An open exact touch to `id`, as a fabricated conversation carries
    /// it.
    fn open_touch(id: &str) -> TouchRow {
        TouchRow {
            incarnation_id: id.to_owned(),
            repo: "/r/.git".to_owned(),
            ref_name: "feat".to_owned(),
            incarnation: 1,
            head: Some("aaaaaa".to_owned()),
            valid_from: 1_000,
            valid_until: None,
            provenance: store::TouchProvenance::Cwd,
            confidence: store::Confidence::Exact,
        }
    }

    /// A derived verdict fabricated to order.
    fn derived(exec: Exec, attention: Attention) -> attention::Derived {
        attention::Derived {
            exec,
            since_ms: None,
            waiting_for: None,
            attention,
            attention_detail: None,
            ack_through: 0,
            wait_ms: None,
            journal_seq: 0,
            marked: false,
            claims: Vec::new(),
        }
    }

    #[test]
    fn incarnation_numbering_is_oldest_first_with_a_stable_tiebreak() {
        // Two records one pass both dated order by id - deterministic,
        // never insertion order.
        let mut work = store::Work::default();
        work.branches
            .insert("i-zz".to_owned(), branch_record("i-zz", 1_000, Some(2_000)));
        work.branches
            .insert("i-aa".to_owned(), branch_record("i-aa", 1_000, None));
        work.branches
            .insert("i-old".to_owned(), branch_record("i-old", 500, None));
        let numbers = incarnation_numbers(&work);
        assert_eq!(numbers["i-old"], 1);
        assert_eq!(numbers["i-aa"], 2);
        assert_eq!(numbers["i-zz"], 3);
    }

    #[test]
    fn a_touch_whose_record_is_gone_serializes_to_nothing() {
        // A dangling interval - a record pruned while the touch that
        // referenced it was lost - is dropped from the view, not guessed.
        let work = store::Work::default();
        let numbers = incarnation_numbers(&work);
        let touch = store::BranchTouch {
            conversation: "claude:s1".to_owned(),
            branch: "i-gone".to_owned(),
            head: Some("a".to_owned()),
            valid_from: 1_000,
            valid_until: None,
            provenance: store::TouchProvenance::Cwd,
            confidence: store::Confidence::Exact,
        };
        assert!(touch_row(&touch, &work, &numbers).is_none());
        // With the record present, the interval resolves its context.
        let mut work = store::Work::default();
        work.branches
            .insert("i-1".to_owned(), branch_record("i-1", 1_000, Some(2_000)));
        let numbers = incarnation_numbers(&work);
        let touch = store::BranchTouch {
            conversation: "claude:s1".to_owned(),
            branch: "i-1".to_owned(),
            head: Some("a".to_owned()),
            valid_from: 1_500,
            valid_until: Some(2_000),
            provenance: store::TouchProvenance::Cwd,
            confidence: store::Confidence::Exact,
        };
        let row = touch_row(&touch, &work, &numbers).expect("the row resolves");
        assert_eq!(row.incarnation, 1);
        assert_eq!(row.ref_name, "feat");
        assert_eq!(row.valid_from, 1);
        assert_eq!(row.valid_until, Some(2));
    }

    #[test]
    fn work_row_splits_active_incarnation_from_excluded_history() {
        // The same-name records split across the row: the active one is
        // the numbered incarnation, the closed ones are excluded history.
        let mut work = store::Work::default();
        work.branches
            .insert("i-1".to_owned(), branch_record("i-1", 1_000, Some(2_000)));
        work.branches
            .insert("i-2".to_owned(), branch_record("i-2", 3_000, Some(4_000)));
        work.branches
            .insert("i-3".to_owned(), branch_record("i-3", 5_000, None));
        work.active_branches
            .insert("/r/.git\0feat".to_owned(), "i-3".to_owned());
        let state = vector::WorkState {
            repo: git::Repo {
                common_dir: PathBuf::from("/r/.git"),
            },
            anchor: Anchor::Branch {
                name: "feat".to_owned(),
            },
            remote_url: None,
            base: Evidence::Unknown("no base asked".to_owned()),
            forge: ForgeStatus {
                item: WorkItem::Unknown,
                pipeline: Pipeline::Unknown,
                label: None,
                url: None,
                reason: None,
            },
            broken: None,
            vector: vector::StateVector {
                worktree: None,
                windows: WindowCount::default(),
                live_pids: 0,
                live_agent_sessions: 0,
                past_agent_sessions: 0,
                dirty: Evidence::Known(false),
                working_tree: Evidence::Unknown("no worktree".to_owned()),
                commits_ahead_of_base: Evidence::Unknown("none asked".to_owned()),
                commits_behind_of_base: Evidence::Unknown("none asked".to_owned()),
                commits_not_on_base: Evidence::Unknown("none asked".to_owned()),
                upstream_state: UpstreamState::NotApplicable,
                unpushed_commits: Evidence::Unknown("none asked".to_owned()),
                landed: Evidence::Unknown("none asked".to_owned()),
                last_git_activity: None,
            },
        };
        let row = work_row("/r/.git", "r", &state, &work);
        let inc = row.incarnation.expect("the active incarnation");
        assert_eq!(inc.id, "i-3");
        assert_eq!(inc.number, 3);
        assert!(!inc.excluded);
        let ids: Vec<&str> = row
            .same_name_history
            .iter()
            .map(|h| h.id.as_str())
            .collect();
        assert_eq!(ids, ["i-1", "i-2"]);
        assert!(row.same_name_history.iter().all(|h| h.excluded));
        assert_eq!(row.identity.as_deref(), Some("i-3"));
    }

    #[test]
    fn a_broken_worktree_row_describes_its_break() {
        let dir = std::env::temp_dir().join(format!("asd-broken-wt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let state = |broken: &str| vector::WorkState {
            repo: git::Repo {
                common_dir: PathBuf::from("/r/.git"),
            },
            anchor: Anchor::Worktree {
                path: dir.clone(),
                admin_id: Some("wt".to_owned()),
                head: Head::Detached("deadbeef".to_owned()),
                locked: false,
                main: false,
                prunable: Some(broken.to_owned()),
            },
            remote_url: None,
            base: Evidence::Unknown("no base asked".to_owned()),
            forge: ForgeStatus {
                item: WorkItem::Unknown,
                pipeline: Pipeline::Unknown,
                label: None,
                url: None,
                reason: None,
            },
            broken: Some(broken.to_owned()),
            vector: vector::StateVector {
                worktree: Some(dir.clone()),
                windows: WindowCount::default(),
                live_pids: 0,
                live_agent_sessions: 0,
                past_agent_sessions: 0,
                dirty: Evidence::Known(false),
                working_tree: Evidence::Unknown("no worktree".to_owned()),
                commits_ahead_of_base: Evidence::Unknown("none asked".to_owned()),
                commits_behind_of_base: Evidence::Unknown("none asked".to_owned()),
                commits_not_on_base: Evidence::Unknown("none asked".to_owned()),
                upstream_state: UpstreamState::NotApplicable,
                unpushed_commits: Evidence::Unknown("none asked".to_owned()),
                landed: Evidence::Unknown("none asked".to_owned()),
                last_git_activity: None,
            },
        };
        let authored = store::Work::default();
        let row = work_row(
            "/r/.git",
            "r",
            &state("gitdir file points to non-existent location"),
            &authored,
        );
        assert_eq!(
            row.broken.as_deref(),
            Some(".git missing; metadata retained by r")
        );
        fs::write(dir.join(".git"), "gitdir: /elsewhere\n").unwrap();
        let row = work_row("/r/.git", "r", &state("checkout moved"), &authored);
        assert_eq!(row.broken.as_deref(), Some("checkout moved"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_row_exists_for_every_conversation_shape() {
        // Live-only, transcript-only and merged each produce one row; the
        // state column is the arbitrated verdict, not the published claim.
        for (exec, want) in [
            (Exec::Busy, ConversationState::Busy),
            (Exec::Idle, ConversationState::Idle),
            (Exec::Waiting, ConversationState::Waiting),
            (Exec::Unknown, ConversationState::Unknown),
        ] {
            let row = conversation_row(
                &conversation(Some(live()), None),
                None,
                derived(exec, Attention::None),
                None,
                None,
                None,
                EvidenceIn::default(),
            );
            assert_eq!(row.state, want, "{want:?}");
            assert!(row.live);
        }
        let row = conversation_row(
            &conversation(None, None),
            None,
            derived(Exec::Unknown, Attention::None),
            None,
            None,
            None,
            EvidenceIn::default(),
        );
        assert_eq!(row.state, ConversationState::Unknown);
        assert!(!row.live);
        assert_eq!(row.short_id, "11111111");
        assert_eq!(row.attention, Attention::None);
    }

    #[test]
    fn a_row_without_a_live_claim_keeps_the_provider_time() {
        // Arbitration proves no `since` without a live process, but the
        // row still has an age: the published status time, else the
        // transcript's newest record - so history keeps its age column and
        // survives an `age:` filter.
        let at = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let row = conversation_row(
            &conversation(None, Some(transcript_at(at))),
            None,
            derived(Exec::Unknown, Attention::None),
            None,
            None,
            None,
            EvidenceIn::default(),
        );
        assert_eq!(row.state_since, Some(1_800_000_000));
        // A not-busy mark names only an arbitrated `since`.
        assert_eq!(row.state_since_ms, None);
        let mut live = live();
        live.status_updated_at = Some(at + Duration::from_secs(60));
        let row = conversation_row(
            &conversation(Some(live), Some(transcript_at(at))),
            None,
            derived(Exec::Unknown, Attention::None),
            None,
            None,
            None,
            EvidenceIn::default(),
        );
        assert_eq!(row.state_since, Some(1_800_000_060));
        // An arbitrated `since` wins over the provider's.
        let mut d = derived(Exec::Busy, Attention::Working);
        d.since_ms = Some(1_700_000_000_000);
        let row = conversation_row(
            &conversation(None, Some(transcript_at(at))),
            None,
            d,
            None,
            None,
            None,
            EvidenceIn::default(),
        );
        assert_eq!(row.state_since, Some(1_700_000_000));
    }

    #[test]
    fn attachment_rows_spell_out_every_verdict() {
        for (liveness, placement, want_state, want_source) in [
            (
                Liveness::Instance,
                Placement::Bound(PaneSource::Published),
                AttachmentLiveness::Instance,
                Some(PaneSource::Published),
            ),
            (
                Liveness::PidOnly("no start".to_owned()),
                Placement::Bound(PaneSource::Ancestry),
                AttachmentLiveness::PidOnly,
                Some(PaneSource::Ancestry),
            ),
            (
                Liveness::Unverifiable("no table".to_owned()),
                Placement::Bound(PaneSource::Tty),
                AttachmentLiveness::Unverifiable,
                Some(PaneSource::Tty),
            ),
            (
                Liveness::Dead("gone".to_owned()),
                Placement::Dead("gone".to_owned()),
                AttachmentLiveness::Dead,
                None,
            ),
            (
                Liveness::Instance,
                Placement::Unknown("contradicted".to_owned()),
                AttachmentLiveness::Instance,
                None,
            ),
            (
                Liveness::Instance,
                Placement::Superseded,
                AttachmentLiveness::Instance,
                None,
            ),
        ] {
            let row = attachment_row(
                &attachment(liveness, placement, true),
                &tmux::PaneInventory::default(),
            );
            assert_eq!(row.liveness, want_state);
            assert_eq!(row.pane_source, want_source);
        }
        // A bound pane renders its handle; a dead one renders the reason.
        let dead = attachment_row(
            &attachment(
                Liveness::Dead("gone".to_owned()),
                Placement::Dead("gone".to_owned()),
                true,
            ),
            &tmux::PaneInventory::default(),
        );
        assert_eq!(dead.placement_detail.as_deref(), Some("gone"));
        assert!(dead.pane.is_some(), "the pane ref stays for evidence");
        let bare = attachment_row(
            &attachment(
                Liveness::Instance,
                Placement::Bound(PaneSource::Ancestry),
                false,
            ),
            &tmux::PaneInventory::default(),
        );
        assert!(bare.pane.is_none());
        assert_eq!(bare.pid_start, Some(1_800_000_000));
        let undated = attachment_row(
            &attachment(
                Liveness::Instance,
                Placement::Bound(PaneSource::Ancestry),
                false,
            ),
            &tmux::PaneInventory::default(),
        );
        let _ = undated;
    }

    #[test]
    fn classification_rolls_attention_up_to_first_match_sections() {
        // A work row's section is the bound conversations' best rank:
        // waiting/error/done -> `Needs you`, working -> `Active`, and
        // anything else goes on to the FollowUp and cleanup sections.
        // A branch-only row, fabricated: no checkout, its Git evidence
        // unread (`no wt ↑?` in the facts tail).
        let threshold = Duration::from_secs(14 * 24 * 3600);
        let now = 2_000_000_000;
        let conv = |attention, state| {
            let mut c = conversation_row(
                &conversation(None, None),
                None,
                derived(Exec::Unknown, attention),
                Some("/r/.git".to_owned()),
                None,
                Some("feat".to_owned()),
                EvidenceIn::default(),
            );
            c.state = state;
            // The fabricated row's identity is its path; the touch to it
            // is what binds the conversation to the row.
            c.touches = vec![open_touch("/r")];
            c
        };
        // `classify_work` composes the summary once per publish; the test
        // rebuilds the row between classifications to keep it honest.
        let fresh = || WorkRow {
            repo: "/r/.git".to_owned(),
            kind: WorkKind::Branch,
            worktree: None,
            branch: Some("feat".to_owned()),
            ..space_row("r", Path::new("/r"))
        };
        let mut row = fresh();
        classify_work(
            &mut row,
            &[conv(Attention::Working, ConversationState::Busy)],
            threshold,
            now,
        );
        assert_eq!(row.section, WorkSection::Active);
        assert_eq!(row.attention, Attention::Working);
        assert_eq!(row.summary, "working · no wt ↑?");

        // A retained error with the agent back at work: `error · working`.
        let mut row = fresh();
        let mut err = conv(Attention::Error, ConversationState::Busy);
        err.attention_detail = Some("StopFailure".to_owned());
        classify_work(&mut row, &[err], threshold, now);
        assert_eq!(row.section, WorkSection::NeedsYou);
        assert_eq!(row.summary, "error: StopFailure · working · no wt ↑?");

        // A quiet, clean tree adds no facts beside the attention; the row
        // has a checkout, so the conversation binds by its path.
        let mut row = fresh();
        row.worktree = Some(PathBuf::from("/r/wt"));
        row.dirty = Some(false);
        row.commits_ahead = Some(0);
        let mut done = conv(Attention::CompletedUnseen, ConversationState::Idle);
        done.worktree = Some(PathBuf::from("/r/wt"));
        classify_work(&mut row, &[done], threshold, now);
        assert_eq!(row.summary, "done");

        // No attention and nothing actionable: `Follow up`, resumable
        // when a bound conversation can be resumed, plain `open` when not.
        let mut row = fresh();
        classify_work(
            &mut row,
            &[conv(Attention::None, ConversationState::Idle)],
            threshold,
            now,
        );
        assert_eq!(row.section, WorkSection::FollowUp);
        assert_eq!(row.summary, "resumable idle · no wt ↑?");
        let mut row = fresh();
        classify_work(&mut row, &[], threshold, now);
        assert_eq!(row.summary, "open · no wt ↑?");
        // A conversation on another branch does not bind: its touch
        // names a different incarnation.
        let mut other = conv(Attention::Waiting, ConversationState::Waiting);
        other.branch = Some("elsewhere".to_owned());
        other.touches = vec![open_touch("i-elsewhere")];
        classify_work(&mut row, &[other], threshold, now);
        assert_eq!(row.section, WorkSection::FollowUp);
        assert_eq!(row.attention, Attention::None);
        // And the ordering puts Needs you first.
        assert!(
            section_order(&WorkRow {
                section: WorkSection::NeedsYou,
                ..space_row("s", Path::new("/s"))
            }) < section_order(&WorkRow {
                section: WorkSection::Active,
                ..space_row("s", Path::new("/s"))
            })
        );
    }

    #[test]
    fn summaries_spell_git_shape_and_unknowns() {
        // The evidence tail a WorkRow's fields produce, independent of the
        // section reason that prefixes it.
        let row = |f: &dyn Fn(&mut WorkRow)| {
            let mut row = WorkRow {
                kind: WorkKind::Branch,
                worktree: Some(PathBuf::from("/w")),
                branch: Some("b".to_owned()),
                dirty: Some(false),
                commits_ahead: Some(0),
                unpushed: Some(0),
                upstream: Upstream::Tracked,
                landed: Some(Landed::No),
                ..space_row("r", Path::new("/r"))
            };
            f(&mut row);
            row
        };
        let facts = |row: &WorkRow| work_facts(row).join(" ");

        assert_eq!(facts(&row(&|_| {})), "");

        let r = row(&|r| {
            r.worktree = None;
            r.commits_ahead = None;
            r.upstream = Upstream::NotApplicable;
        });
        assert_eq!(facts(&r), "no wt ↑?");

        let r = row(&|r| {
            r.dirty = Some(true);
            r.commits_ahead = Some(3);
            r.upstream = Upstream::NeverPushed;
        });
        assert_eq!(facts(&r), "no remote ↑3 ~dirty");

        let r = row(&|r| {
            r.dirty = Some(true);
            r.commits_ahead = Some(3);
        });
        assert_eq!(facts(&r), "↑3 ~dirty");

        let r = row(&|r| {
            r.dirty = Some(true);
            r.commits_ahead = Some(3);
            r.upstream = Upstream::RemoteGone;
        });
        assert_eq!(facts(&r), "remote gone ↑3 ~dirty");

        let r = row(&|r| {
            r.dirty = Some(true);
            r.commits_ahead = Some(3);
            r.upstream = Upstream::Unknown;
        });
        assert_eq!(facts(&r), "? ↑3 ~dirty");

        // Delivery and forge facts join the tail.
        let r = row(&|r| {
            r.landed = Some(Landed::Ancestor);
            r.forge = WorkItem::Open;
            r.forge_label = Some("PR #191".to_owned());
            r.pipeline = Pipeline::Busy;
        });
        assert_eq!(facts(&r), "merged PR #191 checks pending");

        // A project space says `no git` and nothing else.
        assert_eq!(
            work_facts(&space_row("s", Path::new("/s"))).join(" "),
            "no git"
        );

        // The summary keeps every fact the reason did not already say.
        let mut r = row(&|r| {
            r.dirty = Some(true);
            r.commits_ahead = Some(3);
        });
        r.summary = summarize(&r, "dirty".to_owned());
        assert_eq!(r.summary, "dirty · ↑3");
        r.summary = summarize(&r, "merged · blocked".to_owned());
        assert_eq!(r.summary, "merged · blocked · ↑3 ~dirty");
        r.landed = Some(Landed::Ancestor);
        r.summary = summarize(&r, "merged · blocked".to_owned());
        assert_eq!(r.summary, "merged · blocked · ↑3 ~dirty");
        r.parked = true;
        r.summary = summarize(&r, "merged · blocked".to_owned());
        assert_eq!(r.summary, "merged · blocked · ↑3 ~dirty · parked");
    }

    #[test]
    fn forgotten_idle_project_and_cleanup_reasons_cover_their_arms() {
        let now = 2_000_000_000u64;
        // The age buckets a `Forgotten` reason reads - one per width.
        for (threshold, age, want) in [
            (Duration::from_secs(10), 30u64, "idle 30s"),
            (Duration::from_secs(10), 300, "idle 5m"),
            (Duration::from_secs(10), 7_200, "idle 2h"),
            (Duration::from_secs(10), 100_000, "idle 1d"),
        ] {
            let mut row = WorkRow {
                kind: WorkKind::Branch,
                branch: Some("b".to_owned()),
                dirty: Some(false),
                commits_ahead: Some(1),
                unpushed: Some(0),
                upstream: Upstream::Tracked,
                landed: Some(Landed::No),
                last_activity: Some(now - age),
                // Verdicts that would otherwise claim the row for review:
                // not applicable here, so `Forgotten` proves it ran first.
                worktree_removal: None,
                branch_deletion: None,
                ..space_row("r", Path::new("/r"))
            };
            classify_work(&mut row, &[], threshold, now);
            assert_eq!(row.section, WorkSection::Forgotten, "{row:?}");
            assert!(
                row.summary.starts_with(want),
                "{want:?} vs {:?}",
                row.summary
            );
        }
        // A parked row of the same age cannot be forgotten.
        let mut row = WorkRow {
            kind: WorkKind::Branch,
            branch: Some("b".to_owned()),
            dirty: Some(false),
            commits_ahead: Some(1),
            unpushed: Some(0),
            upstream: Upstream::Tracked,
            landed: Some(Landed::No),
            last_activity: Some(now - 100_000),
            parked: true,
            ..space_row("r", Path::new("/r"))
        };
        classify_work(&mut row, &[], Duration::from_secs(10), now);
        assert_eq!(row.section, WorkSection::FollowUp, "{row:?}");
        assert!(row.summary.ends_with("· parked"), "{}", row.summary);

        // A project space with nothing resumable is `idle project`.
        let mut row = space_row("s", Path::new("/s"));
        classify_work(&mut row, &[], Duration::from_secs(10), now);
        assert_eq!(row.section, WorkSection::FollowUp);
        assert_eq!(row.summary, "idle project · no git");

        // The cleanup sections' reason names what the verdicts allow:
        // only a worktree, only a branch, both, and a review's first
        // reason.
        let verdict = |v, rs: &[&str]| crate::verdict::ActionVerdict {
            verdict: v,
            reasons: rs.iter().map(|&s| s.to_owned()).collect(),
        };
        let mut row = WorkRow {
            kind: WorkKind::Worktree,
            worktree: Some(PathBuf::from("/r/wt")),
            branch: Some("b".to_owned()),
            dirty: Some(false),
            commits_ahead: Some(0),
            unpushed: Some(0),
            upstream: Upstream::Tracked,
            landed: Some(Landed::Ancestor),
            worktree_removal: Some(verdict(Verdict::Safe, &["clean"])),
            branch_deletion: Some(verdict(Verdict::NotApplicable, &[])),
            ..space_row("r", Path::new("/r"))
        };
        classify_work(&mut row, &[], Duration::from_secs(10), now);
        assert_eq!(row.section, WorkSection::ReadyToClean);
        assert_eq!(row.summary, "worktree only · merged");
        row.branch_deletion = Some(verdict(Verdict::SafeAfterWorktreeRemoval, &[]));
        classify_work(&mut row, &[], Duration::from_secs(10), now);
        assert_eq!(row.summary, "wt + branch · merged");
        row.worktree_removal = Some(verdict(Verdict::NotApplicable, &[]));
        classify_work(&mut row, &[], Duration::from_secs(10), now);
        assert_eq!(row.summary, "branch only · merged");
        row.branch_deletion = Some(verdict(Verdict::Review, &["needs -D"]));
        classify_work(&mut row, &[], Duration::from_secs(10), now);
        assert_eq!(row.section, WorkSection::CleanupReview);
        assert!(
            row.summary.starts_with("review: needs -D"),
            "{}",
            row.summary
        );
        row.branch_deletion = Some(verdict(Verdict::Review, &[]));
        classify_work(&mut row, &[], Duration::from_secs(10), now);
        assert!(row.summary.starts_with("review: ?"), "{}", row.summary);
        // Nothing applicable at all is neither section.
        row.worktree_removal = Some(verdict(Verdict::NotApplicable, &[]));
        row.branch_deletion = Some(verdict(Verdict::NotApplicable, &[]));
        classify_work(&mut row, &[], Duration::from_secs(10), now);
        assert_eq!(row.section, WorkSection::FollowUp, "{row:?}");
        assert!(row.summary.starts_with("open"), "{}", row.summary);
    }

    #[test]
    fn repos_order_activity_then_name_then_id() {
        let row = |name: &str, id: &str, last: Option<u64>| RepoRow {
            name: name.to_owned(),
            id: id.to_owned(),
            path: PathBuf::from("/s"),
            git: true,
            work: 0,
            live: 0,
            attention: Attention::None,
            open: 0,
            clean: 0,
            last_activity: last,
            default_branch: None,
            remote: None,
            counts: RepoCounts::default(),
        };
        let mut repos = vec![
            row("b", "/z", Some(2)),
            row("a", "/b", Some(1)),
            row("a", "/a", Some(1)),
            row("a", "/c", None),
        ];
        sort_repos(&mut repos);
        let ids: Vec<&str> = repos.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["/z", "/a", "/b", "/c"], "{repos:?}");
    }

    #[test]
    fn a_repo_without_a_main_checkout_names_itself() {
        let repo = git::Repo {
            common_dir: PathBuf::from("/repos/app.git"),
        };
        // No anchors at all, or only non-main ones: the common dir's name.
        assert_eq!(repo_display([].iter(), &repo).0, "app.git");
        let anchors = [Anchor::Branch {
            name: "keep".to_owned(),
        }];
        assert_eq!(repo_display(anchors.iter(), &repo).0, "app.git");
        let anchors = [Anchor::Worktree {
            path: PathBuf::from("/repos/app"),
            admin_id: None,
            head: Head::Branch("main".to_owned()),
            locked: false,
            main: true,
            prunable: None,
        }];
        assert_eq!(
            repo_display(anchors.iter(), &repo),
            ("app".to_owned(), PathBuf::from("/repos/app"))
        );
    }

    #[test]
    fn the_empty_snapshot_is_an_incomplete_nothing() {
        // What the TUI draws before the first stage lands: renderable, and
        // honestly incomplete.
        let empty = Snapshot::empty();
        assert!(!empty.complete);
        assert_eq!(empty.schema_version, SCHEMA_VERSION);
        assert!(empty.repos.is_empty() && empty.work.is_empty());
        assert!(empty.conversations.is_empty() && empty.errors.is_empty());
        // The JSON document is how `list --json` prints it.
        let json = to_json(&empty).expect("an empty snapshot serializes");
        assert!(json.contains("\"complete\": false"), "{json}");
    }

    #[test]
    fn names_and_shas_render_sanely() {
        assert_eq!(display_name(Path::new("/repo/x/.git")), "x");
        assert_eq!(display_name(Path::new("/repo/x")), "x");
        assert_eq!(display_name(Path::new("/")), "/");
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("abc"), "abc");
    }

    #[test]
    fn attention_orders_waiting_then_busy_then_idle_then_unknown() {
        let row = |attention: Attention, since: Option<u64>| ConversationRow {
            provider: Provider::Claude,
            session_id: String::new(),
            short_id: String::new(),
            title: None,
            state: ConversationState::Unknown,
            state_raw: None,
            waiting_for: None,
            state_since: since,
            state_since_ms: since.map(|s| s * 1000),
            attention,
            attention_detail: None,
            attention_seq: None,
            attention_wait_ms: None,
            journal_seq: None,
            last_activity: None,
            live: false,
            attachment: None,
            cwd: None,
            transcript: None,
            malformed_lines: None,
            resume_argv: Vec::new(),
            latest_prompt: None,
            latest_reply: None,
            repo: None,
            worktree: None,
            branch: None,
            touches: Vec::new(),
            current_incarnation: None,
            started_at: None,
            related: Vec::new(),
            evidence: EvidenceRow::default(),
        };
        let now = SystemTime::now();
        // The inbox order is the attention rank, not the state.
        assert!(
            row(Attention::Waiting, None).attention.rank()
                < row(Attention::Working, None).attention.rank()
        );
        assert!(
            row(Attention::Error, None).attention.rank()
                < row(Attention::CompletedUnseen, None).attention.rank()
        );
        assert!(
            row(Attention::None, None).attention.rank()
                > row(Attention::Unknown, None).attention.rank()
        );
        // No state timestamp means infinite age: sorted last of its rank.
        assert_eq!(age_of(&row(Attention::Waiting, None), now), Duration::MAX);
        assert_eq!(
            age_of(&row(Attention::Waiting, Some(1)), now),
            now.duration_since(UNIX_EPOCH + Duration::from_secs(1))
                .unwrap()
        );
    }

    /// The row enums are the JSON contract: every variant serializes to the
    /// snake_case label the field documented as a string before.
    #[test]
    fn label_enums_keep_their_wire_spellings() {
        fn wire(v: &impl Serialize) -> serde_json::Value {
            serde_json::to_value(v).unwrap()
        }
        // The display spelling is the wire spelling.
        for (kind, want) in [
            (WorkKind::Branch, "branch"),
            (WorkKind::Detached, "detached"),
            (WorkKind::Worktree, "worktree"),
            (WorkKind::ProjectSpace, "project_space"),
        ] {
            assert_eq!(kind.as_str(), want);
            assert_eq!(wire(&kind), serde_json::json!(want));
        }
        for (state, want) in [
            (ConversationState::Busy, "busy"),
            (ConversationState::Idle, "idle"),
            (ConversationState::Waiting, "waiting"),
            (ConversationState::Unknown, "unknown"),
        ] {
            assert_eq!(state.as_str(), want);
            assert_eq!(wire(&state), serde_json::json!(want));
        }
        assert_eq!(Provider::Claude.as_str(), "claude");
        for upstream in [
            Upstream::Tracked,
            Upstream::NeverPushed,
            Upstream::RemoteGone,
            Upstream::NotApplicable,
            Upstream::Unknown,
        ] {
            assert_eq!(wire(&upstream), serde_json::json!(upstream.as_str()));
        }
        for landed in [Landed::Ancestor, Landed::Content, Landed::No] {
            assert_eq!(wire(&landed), serde_json::json!(landed.as_str()));
        }
        for item in [
            WorkItem::Unknown,
            WorkItem::NotExisting,
            WorkItem::Open,
            WorkItem::Closed,
        ] {
            assert_eq!(wire(&item), serde_json::json!(item.as_str()));
        }
        for pipeline in [
            Pipeline::Busy,
            Pipeline::Succeeded,
            Pipeline::Failed,
            Pipeline::Unknown,
        ] {
            assert_eq!(wire(&pipeline), serde_json::json!(pipeline.as_str()));
        }
        for (value, want) in [
            (wire(&Upstream::Tracked), "tracked"),
            (wire(&Upstream::NeverPushed), "never_pushed"),
            (wire(&Upstream::RemoteGone), "remote_gone"),
            (wire(&Upstream::NotApplicable), "not_applicable"),
            (wire(&Upstream::Unknown), "unknown"),
            (wire(&Landed::Ancestor), "ancestor"),
            (wire(&Landed::Content), "content"),
            (wire(&Landed::No), "no"),
            (wire(&AttachmentLiveness::Instance), "instance"),
            (wire(&AttachmentLiveness::PidOnly), "pid_only"),
            (wire(&AttachmentLiveness::Dead), "dead"),
            (wire(&AttachmentLiveness::Unverifiable), "unverifiable"),
            (wire(&ConversationState::Busy), "busy"),
            (wire(&ConversationState::Idle), "idle"),
            (wire(&ConversationState::Waiting), "waiting"),
            (wire(&ConversationState::Unknown), "unknown"),
            (wire(&Provider::Claude), "claude"),
            (wire(&PaneSource::Published), "published"),
            (wire(&PaneSource::Ancestry), "ancestry"),
            (wire(&PaneSource::Tty), "tty"),
        ] {
            assert_eq!(value, serde_json::json!(want));
        }
    }

    #[test]
    fn a_cwd_resolves_or_fails_closed() {
        // A cwd that no longer exists: no anchor, no error.
        let mut errors = Vec::new();
        assert!(resolve_cwd(Path::new("/definitely/gone"), &mut errors).is_none());
        assert!(errors.is_empty());
        // A plain directory is a project space.
        let dir = std::env::temp_dir().join(format!("asd-space-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let Some(CwdPlacement::ProjectSpace { path }) = resolve_cwd(&dir, &mut errors) else {
            panic!("a plain dir is a project space") // coverage: off - a passing test never panics
        };
        assert_eq!(path, dir.canonicalize().unwrap());
        // A `.git` file that points nowhere: a repo with no checkout to
        // anchor on resolves to the repo itself.
        let broken = std::env::temp_dir().join(format!("asd-broken-{}", std::process::id()));
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(".git"), "gitdir: /definitely/not/a/dir").unwrap();
        assert!(resolve_cwd(&broken, &mut errors).is_some());
        // An unreadable `.git` file: resolve fails, the error is retained,
        // the row gets no anchor.
        let dead_dir = std::env::temp_dir().join(format!("asd-dead-{}", std::process::id()));
        fs::create_dir_all(&dead_dir).unwrap();
        let gitfile = dead_dir.join(".git");
        fs::write(&gitfile, "gitdir: /x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gitfile, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(resolve_cwd(&dead_dir, &mut errors).is_none());
        assert!(errors.iter().any(|e| e.source == "git resolve"));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::set_permissions(&gitfile, fs::Permissions::from_mode(0o644));
        let _ = fs::remove_dir_all(&broken);
        let _ = fs::remove_dir_all(&dead_dir);
    }

    /// A conversation whose live record sits at `cwd`.
    fn live_at(cwd: &Path) -> Live {
        let mut live = live();
        live.cwd = Some(cwd.to_owned());
        live
    }

    #[test]
    fn a_shared_cwd_resolves_once_and_fails_once() {
        // Three conversations on one cwd: one resolve each pass, and a
        // failure is one retained error, not one per conversation.
        let dead_dir = std::env::temp_dir().join(format!("asd-shared-{}", std::process::id()));
        fs::create_dir_all(&dead_dir).unwrap();
        let gitfile = dead_dir.join(".git");
        fs::write(&gitfile, "gitdir: /x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gitfile, fs::Permissions::from_mode(0o000)).unwrap();
        let conversations = vec![
            conversation(Some(live_at(&dead_dir)), None),
            conversation(Some(live_at(&dead_dir)), None),
            conversation(None, None),
        ];
        let mut errors = Vec::new();
        let placements = resolve_cwds(&conversations, &mut errors);
        assert_eq!(placements.len(), 3);
        assert!(placements.iter().all(Option::is_none));
        assert_eq!(errors.len(), 1, "{errors:?}");
        let _ = fs::set_permissions(&gitfile, fs::Permissions::from_mode(0o644));
        let _ = fs::remove_dir_all(&dead_dir);
    }

    #[test]
    fn an_undated_record_keeps_its_first_read_time_until_content_or_process_changes() {
        let mut seen = HashMap::new();
        let mut live = live();
        let mut instance = ProcessInstance {
            pid: live.pid,
            pid_start: live.pid_start,
        };
        assert_eq!(first_read(&mut seen, "k", &live, instance, 1_000), 1_000);
        // The same content read again keeps its date.
        assert_eq!(first_read(&mut seen, "k", &live, instance, 2_000), 1_000);
        // A new reading - another status or wait reason - is dated afresh.
        live.status_raw = Some("waiting".to_owned());
        assert_eq!(first_read(&mut seen, "k", &live, instance, 3_000), 3_000);
        live.waiting_for = Some("permission prompt".to_owned());
        assert_eq!(first_read(&mut seen, "k", &live, instance, 4_000), 4_000);
        assert_eq!(first_read(&mut seen, "k", &live, instance, 5_000), 4_000);
        // A resumed conversation under another process is a new reading even
        // when its timestamp-less state has identical content. Otherwise an old
        // wait acknowledgement or not-busy mark could apply to the new process.
        instance.pid += 1;
        assert_eq!(first_read(&mut seen, "k", &live, instance, 6_000), 6_000);
        instance.pid_start = ProcessStart::At(7_000);
        assert_eq!(first_read(&mut seen, "k", &live, instance, 7_000), 7_000);
        // Conversations are dated independently.
        assert_eq!(
            first_read(&mut seen, "other", &live, instance, 8_000),
            8_000
        );
    }

    #[test]
    fn observed_process_identity_uses_the_provider_then_the_os_fallback() {
        let row = |start| ProcessRow {
            pid: 42,
            ppid: 1,
            start,
            exe: Some("claude".to_owned()),
            tty: None,
            state: 'S',
        };
        // The provider's own start date wins over the OS snapshot's row.
        let resolved = attachment(Liveness::Instance, Placement::Superseded, false);
        let table = ProcessTable::from_rows(vec![row(ProcessStart::At(999))]);
        assert_eq!(
            observed_instance(&resolved, Some(&table)),
            ProcessInstance {
                pid: 42,
                pid_start: ProcessStart::At(1_800_000_000),
            }
        );
        // Provider silent: the already-collected process row dates the
        // instance without another system call.
        let mut resolved = attachment(
            Liveness::PidOnly("no start".to_owned()),
            Placement::Superseded,
            false,
        );
        resolved.attachment.process.pid_start = ProcessStart::Unavailable;
        assert_eq!(
            observed_instance(&resolved, Some(&table)).pid_start,
            ProcessStart::At(999)
        );
        // Both silent - no row, or a row without a date - and the pid-only
        // limitation stands.
        let undated = ProcessTable::from_rows(vec![row(ProcessStart::Unavailable)]);
        assert_eq!(
            observed_instance(&resolved, Some(&undated)).pid_start,
            ProcessStart::Unavailable
        );
        assert_eq!(
            observed_instance(&resolved, None).pid_start,
            ProcessStart::Unavailable
        );
    }

    #[test]
    fn undated_state_is_retained_only_for_live_conversations() {
        let undated = |first_ms| Undated {
            pid: 1,
            pid_start: ProcessStart::Unavailable,
            status: Some("busy".to_owned()),
            waiting_for: None,
            first_ms,
        };
        let mut seen = HashMap::new();
        seen.insert("claude\0live".to_owned(), undated(5_000));
        seen.insert("claude\0dead".to_owned(), undated(6_000));
        // A conversation whose process was observed dead loses its
        // timestamp-less date; a live one keeps it.
        let live_keys: HashSet<String> = ["claude\0live".to_owned()].into_iter().collect();
        retain_live_undated(&mut seen, &live_keys);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen["claude\0live"].first_ms, 5_000);
    }

    #[test]
    fn a_start_that_moves_a_second_between_polls_is_the_same_instance() {
        // Start readings quantize to the second from different sources, so
        // one process can read a second apart without being a replacement.
        let mut undated = HashMap::new();
        let mut live = live();
        live.status_raw = Some("waiting".to_owned());
        let at = |start| ProcessInstance {
            pid: 7,
            pid_start: ProcessStart::At(start),
        };
        let first = first_read(&mut undated, "k", &live, at(1_000), 10_000);
        assert_eq!(
            first_read(&mut undated, "k", &live, at(1_001), 20_000),
            first
        );
        assert_eq!(first_read(&mut undated, "k", &live, at(999), 30_000), first);
        assert_eq!(
            first_read(&mut undated, "k", &live, at(1_003), 40_000),
            40_000
        );
    }

    #[test]
    fn a_replacement_process_reopens_an_undated_wait_and_busy_mark() {
        // A resumed process publishing the same timestamp-less record must
        // not inherit the previous process's first-read date: that date is
        // what authored seen-state and marks are judged against.
        let mut undated = HashMap::new();
        let mut live = live();
        live.status = Some(PublishedStatus::Waiting);
        live.status_raw = Some("waiting".to_owned());
        live.waiting_for = Some("permission prompt".to_owned());
        let old_instance = ProcessInstance {
            pid: 7,
            pid_start: ProcessStart::At(1_000),
        };
        let new_instance = ProcessInstance {
            pid: 9,
            pid_start: ProcessStart::At(2_000),
        };
        // The old process's wait was read and acknowledged; the same
        // record reopens under the replacement.
        let old_first = first_read(&mut undated, "k", &live, old_instance, 10_000);
        let new_first = first_read(&mut undated, "k", &live, new_instance, 20_000);
        let mut idle = attention::WeakIdle::default();
        let derived = attention::derive(attention::Inputs {
            fold: None,
            seen: store::Seen {
                seq: 0,
                wait_ms: Some(old_first),
            },
            mark: None,
            published: Some(attention::Published {
                status: live.status,
                waiting_for: live.waiting_for.clone(),
                observed_ms: new_first,
                since_ms: Some(new_first),
            }),
            live: Some((new_instance.pid, Some(2_000))),
            now_ms: 20_000,
            ack_ok: true,
            idle: &mut idle,
        });
        assert_eq!(derived.attention, Attention::Waiting);
        assert_eq!(derived.wait_ms, Some(new_first));
        // The same resume under a `busy` record: the mark written against
        // the old process's date cannot suppress the replacement's Busy.
        live.status = Some(PublishedStatus::Busy);
        live.status_raw = Some("busy".to_owned());
        live.waiting_for = None;
        let old_first = first_read(&mut undated, "b", &live, old_instance, 10_000);
        let mark = store::Mark {
            since_ms: old_first,
            seq: 0,
            at_ms: old_first + 1,
        };
        let new_first = first_read(&mut undated, "b", &live, new_instance, 30_000);
        let mut idle = attention::WeakIdle::default();
        let derived = attention::derive(attention::Inputs {
            fold: None,
            seen: store::Seen::default(),
            mark: Some(&mark),
            published: Some(attention::Published {
                status: live.status,
                waiting_for: live.waiting_for.clone(),
                observed_ms: new_first,
                since_ms: Some(new_first),
            }),
            live: Some((new_instance.pid, Some(2_000))),
            now_ms: 30_000,
            ack_ok: true,
            idle: &mut idle,
        });
        assert_eq!(derived.exec, Exec::Busy);
        assert_eq!(derived.attention, Attention::Working);
        assert!(!derived.marked);
    }

    #[test]
    fn own_pane_matches_socket_and_id_together() {
        let pref = |socket: &str| PaneRef {
            socket: PathBuf::from(socket),
            pane: PaneId::parse("%1").unwrap(),
        };
        // Two servers can each carry a `%1`: only the matching socket's is
        // the dashboard's own - a watched `%1` elsewhere acknowledges.
        assert!(is_own_pane(Some(&pref("/sock/a")), &pref("/sock/a")));
        assert!(!is_own_pane(Some(&pref("/sock/a")), &pref("/sock/b")));
        assert!(!is_own_pane(None, &pref("/sock/a")));
        assert!(!is_own_pane(
            Some(&pref("/sock/a")),
            &PaneRef {
                socket: PathBuf::from("/sock/a"),
                pane: PaneId::parse("%2").unwrap(),
            }
        ));
    }

    #[test]
    fn own_pane_matches_one_server_under_two_spellings() {
        // `$TMUX` carries tmux's resolved socket path while discovery may
        // keep a symlinked spelling of the same server: still the
        // dashboard's own pane.
        let root = std::env::temp_dir().join(format!("agent-sessions-own-{}", std::process::id()));
        let real = root.join("real");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("default"), "").unwrap();
        let link = root.join("link");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let pref = |socket: PathBuf| PaneRef {
            socket,
            pane: PaneId::parse("%1").unwrap(),
        };
        let own = pref(real.join("default"));
        assert!(is_own_pane(Some(&own), &pref(link.join("default"))));
        assert!(!is_own_pane(Some(&own), &pref(link.join("other"))));
        fs::remove_dir_all(&root).unwrap();
    }

    /// A minimal pane for fabricated inventories.
    fn tmux_pane(socket: &str, id: &str, pid: u32) -> tmux::Pane {
        tmux::Pane {
            socket: PathBuf::from(socket),
            id: PaneId::parse(id).unwrap(),
            window: tmux::WindowId::parse("@1").unwrap(),
            session: tmux::SessionId::parse("$1").unwrap(),
            session_name: "s".to_owned(),
            pid,
            command: "claude".to_owned(),
            cwd: None,
            tty: None,
            active: true,
            last: false,
            window_active: true,
            window_activity: None,
            session_attached: 1,
            wt_adminid: None,
            wt_handle: None,
        }
    }

    /// A process table row: `pid` under `ppid`.
    fn prow(pid: u32, ppid: u32) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            start: ProcessStart::At(1_700_000_000),
            exe: Some("claude".to_owned()),
            tty: None,
            state: 'S',
        }
    }

    /// A fabricated conversation row: live, attached to `pid`.
    fn live_row(session: &str, pid: u32) -> ConversationRow {
        let mut row = conversation_row(
            &conversation(Some(live()), None),
            None,
            derived(Exec::Busy, Attention::None),
            None,
            None,
            None,
            EvidenceIn::default(),
        );
        row.session_id = session.to_owned();
        row.short_id = session.chars().take(8).collect();
        row.attachment = Some(AttachmentRow {
            pid,
            pid_start: Some(1_700_000_000),
            liveness: AttachmentLiveness::Instance,
            liveness_detail: None,
            pane: None,
            pane_source: Some(PaneSource::Published),
            placement_detail: None,
            source: EvidenceSource::Published,
            observed_at: 1_800_000_000,
        });
        row
    }

    /// A runtime over fabricated processes and panes.
    fn runtime(rows: Vec<ProcessRow>, panes: Vec<tmux::Pane>) -> Runtime {
        Runtime {
            processes: Some(ProcessTable::from_rows(rows)),
            panes: tmux::PaneInventory {
                panes,
                servers: Vec::new(),
                warnings: Vec::new(),
                stale_sockets: 0,
            },
            observed_at: SystemTime::now(),
        }
    }

    #[test]
    fn relations_follow_proven_strengths_and_never_guess() {
        // b's pid is a's parent - observed ancestry. A pane a sits in
        // whose root process descends from b relates nothing: real pane
        // roots descend from the tmux server, so such a chain proves no
        // creation. A shared ref name with different incarnation ids
        // proves nothing either.
        let mut a = live_row("aaaaaaaa-1", 101);
        let mut b = live_row("bbbbbbbb-1", 100);
        a.touches = vec![open_touch("i1")];
        b.touches = vec![open_touch("i2")];
        let runtime = runtime(
            vec![prow(101, 100), prow(100, 1), prow(50, 100)],
            vec![tmux_pane("/sock/a", "%7", 50)],
        );
        let mut convs = vec![a.clone(), b.clone()];
        relate(&mut convs, &runtime);
        let strengths: Vec<RelationStrength> =
            convs[0].related.iter().map(|r| r.strength).collect();
        assert_eq!(strengths, vec![RelationStrength::ProcessAncestry]);
        assert_eq!(convs[0].related[0].session_id, "bbbbbbbb-1");
        assert_eq!(convs[0].related[0].label, "ancestor");
        // The parent lists its child: ancestry relates both ways. Sharing
        // a ref name across distinct incarnations adds nothing more.
        let parent: Vec<(&str, &str)> = convs[1]
            .related
            .iter()
            .map(|r| (r.session_id.as_str(), r.label.as_str()))
            .collect();
        assert_eq!(parent, vec![("aaaaaaaa-1", "descendant")]);
        // A shared exact incarnation is.
        b.touches = vec![open_touch("i1")];
        let mut convs = vec![a.clone(), b.clone()];
        relate(&mut convs, &runtime);
        let strengths: Vec<RelationStrength> =
            convs[0].related.iter().map(|r| r.strength).collect();
        assert!(
            strengths.contains(&RelationStrength::SameIncarnation),
            "{strengths:?}"
        );
        // With the parent first, its descendant arrives after its touch
        // relation; the list still sorts strongest group first.
        let mut convs = vec![b.clone(), a.clone()];
        relate(&mut convs, &runtime);
        let strengths: Vec<RelationStrength> =
            convs[0].related.iter().map(|r| r.strength).collect();
        assert_eq!(
            strengths,
            vec![
                RelationStrength::ProcessAncestry,
                RelationStrength::SameIncarnation
            ]
        );
        // With no process table the first two strengths drop out; the
        // touch relation still stands. A dead or attachment-less
        // conversation relates through nothing live.
        let runtime = Runtime {
            processes: None,
            panes: tmux::PaneInventory::default(),
            observed_at: SystemTime::now(),
        };
        let mut convs = vec![a, b];
        relate(&mut convs, &runtime);
        let strengths: Vec<RelationStrength> =
            convs[0].related.iter().map(|r| r.strength).collect();
        assert_eq!(strengths, vec![RelationStrength::SameIncarnation]);
    }

    #[test]
    fn gone_rows_name_every_live_reference_and_drop_the_unreferenced() {
        // A closed incarnation whose proven workspace a pane's stored
        // edge and a live agent session still name: every reference kind.
        let root =
            std::env::temp_dir().join(format!("agent-sessions-gone-{}-a", std::process::id()));
        let missing = format!("{}/missing", root.display());
        // A second, still-existing workspace: the branch is gone but the
        // directory is not, so the row says `branch deleted`.
        let still = root.join("still");
        fs::create_dir_all(&still).unwrap();
        let still = format!("{}", still.display());
        let mut work = store::Work::default();
        let mut record = branch_record("i1", 1_000, Some(3_000));
        record.inputs.worktree_path = Some(missing.clone());
        record.inputs.admin_id = Some("adm1".to_owned());
        work.branches.insert("i1".to_owned(), record);
        let mut kept = branch_record("i2", 1_000, Some(3_000));
        kept.ref_name = "kept".to_owned();
        kept.inputs.worktree_path = Some(still.clone());
        kept.inputs.admin_id = Some("adm2".to_owned());
        work.branches.insert("i2".to_owned(), kept);
        // A branch-only close records nothing a reference could name.
        let mut bare = branch_record("i0", 1_000, Some(3_000));
        bare.ref_name = "bare".to_owned();
        work.branches.insert("i0".to_owned(), bare);
        // Two live panes bound by their stored worktree edges - the
        // summary says `2 panes` - plus one that binds nowhere.
        let mut pane = tmux_pane("/sock/a", "%9", 50);
        pane.wt_adminid = Some("adm1".to_owned());
        let mut pane2 = tmux_pane("/sock/a", "%10", 51);
        pane2.wt_adminid = Some("adm1".to_owned());
        let mut pane3 = tmux_pane("/sock/a", "%11", 52);
        pane3.wt_adminid = Some("adm2".to_owned());
        let stray = tmux_pane("/sock/a", "%12", 53);
        // A live agent session anchored inside the gone path, one inside
        // it without an attachment, and one somewhere else entirely.
        let mut c = live_row("cccccccc-1", 99);
        c.cwd = Some(PathBuf::from(&missing));
        let mut no_attach = conversation_row(
            &conversation(Some(live()), None),
            None,
            derived(Exec::Busy, Attention::None),
            None,
            None,
            None,
            EvidenceIn::default(),
        );
        no_attach.cwd = Some(PathBuf::from(&missing));
        let mut outside = live_row("dddddddd-1", 88);
        outside.cwd = Some(PathBuf::from("/elsewhere"));
        let model = Model {
            conversations: vec![c, no_attach, outside],
            ..Default::default()
        };
        let live_rt = runtime(
            vec![prow(99, 50), prow(88, 1)],
            vec![pane, pane2, pane3, stray],
        );
        let live_paths = HashSet::new();
        let rows = gone_rows(&model, &work, &live_rt, &live_paths);
        assert_eq!(rows.len(), 2, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row.gone.as_deref(), Some("branch and worktree gone"));
        let kinds: Vec<ReferenceKind> = row.references.iter().map(|r| r.kind).collect();
        for kind in [
            ReferenceKind::Pane,
            ReferenceKind::Window,
            ReferenceKind::TmuxSession,
            ReferenceKind::Process,
            ReferenceKind::AgentSession,
        ] {
            assert!(kinds.contains(&kind), "{kind:?} missing: {kinds:?}");
        }
        assert_eq!(row.section, WorkSection::CleanupReview);
        assert!(row.summary.contains("2 panes"), "{}", row.summary);
        assert!(row.summary.contains("1 agent session"), "{}", row.summary);
        // The still-present workspace reads `branch deleted`.
        let row = rows.iter().find(|r| r.name == "kept").unwrap();
        assert_eq!(row.gone.as_deref(), Some("branch deleted"));
        // Nothing referencing it: the record stays retained in the store
        // but lists nowhere.
        let quiet = runtime(vec![], vec![]);
        let model = Model::default();
        assert!(gone_rows(&model, &work, &quiet, &live_paths).is_empty());
        // The same workspace live under a new anchor: the closed record
        // does not resurrect as gone.
        let live_paths: HashSet<String> = [missing.clone()].into_iter().collect();
        assert!(gone_rows(&model, &work, &quiet, &live_paths).is_empty());
        // A vanished project space follows the same rule.
        let space =
            std::env::temp_dir().join(format!("agent-sessions-gone-{}-b", std::process::id()));
        let space = format!("{}", space.display());
        let mut paths = store::Work::default();
        paths.paths.insert(
            space.clone(),
            store::PathRecord {
                repo: None,
                parked: false,
                activity_at: Some(2_000),
                inputs: store::LifecycleInputs::default(),
                updates: Vec::new(),
                session_activity: Default::default(),
            },
        );
        let detached = format!("{}/detached", root.display());
        paths.paths.insert(
            detached.clone(),
            store::PathRecord {
                repo: Some("/r/.git".to_owned()),
                parked: false,
                activity_at: Some(2_000),
                inputs: store::LifecycleInputs {
                    worktree: Some(true),
                    ..Default::default()
                },
                updates: Vec::new(),
                session_activity: Default::default(),
            },
        );
        let mut c = live_row("cccccccc-2", 77);
        c.cwd = Some(PathBuf::from(&space));
        let mut c2 = live_row("eeeeeeee-2", 76);
        c2.cwd = Some(PathBuf::from(&detached));
        let model = Model {
            conversations: vec![c, c2],
            ..Default::default()
        };
        let rows = gone_rows(&model, &paths, &quiet, &live_paths);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, WorkKind::Detached);
        assert_eq!(rows[0].gone.as_deref(), Some("worktree gone"));
        // The detached row lists under the repo its record carries; a
        // legacy record without one falls back to its own path.
        assert_eq!(rows[0].repo, "/r/.git");
        assert_eq!(rows[0].repo_name, "r");
        assert_eq!(rows[1].gone.as_deref(), Some("project folder gone"));
        assert_eq!(rows[1].kind, WorkKind::ProjectSpace);
        assert_eq!(rows[1].repo, space);
    }

    #[test]
    fn one_vanished_path_is_one_gone_row_whatever_closed_there() {
        // A same-name branch recreated at the same worktree path, a pooled
        // directory another branch reused, and a detached checkout there
        // too: once the path is gone, one pane still in it is one dangling
        // workspace - one row, attributed to the newest closed record.
        let path = format!(
            "{}/agent-sessions-gone-{}-dup",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let mut work = store::Work::default();
        for (id, name, ended) in [
            ("i1", "feat", 3_000),
            ("i2", "feat", 9_000),
            ("i3", "other", 6_000),
        ] {
            let mut r = branch_record(id, 1_000, Some(ended));
            r.ref_name = name.to_owned();
            r.inputs.worktree_path = Some(path.clone());
            r.inputs.admin_id = Some("adm".to_owned());
            work.branches.insert(id.to_owned(), r);
        }
        work.paths.insert(
            path.clone(),
            store::PathRecord {
                repo: None,
                parked: false,
                activity_at: Some(2_000),
                inputs: store::LifecycleInputs {
                    worktree: Some(true),
                    ..Default::default()
                },
                updates: Vec::new(),
                session_activity: Default::default(),
            },
        );
        let mut pane = tmux_pane("/sock/a", "%9", 50);
        pane.wt_adminid = Some("adm".to_owned());
        let rt = runtime(vec![], vec![pane]);
        // A live agent session inside the path references the detached
        // record too.
        let mut agent = live_row("cccccccc-3", 99);
        agent.cwd = Some(PathBuf::from(&path));
        let model = Model {
            conversations: vec![agent],
            ..Default::default()
        };
        let rows = gone_rows(&model, &work, &rt, &HashSet::new());
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].identity.as_deref(), Some("i2"));
        assert_eq!(
            rows[0]
                .references
                .iter()
                .filter(|r| r.kind == ReferenceKind::Pane)
                .count(),
            1
        );
    }

    #[test]
    fn a_commit_names_the_one_conversation_whose_touch_covered_it() {
        let commit = |at: u64| CommitRow {
            sha: format!("{at:040}"),
            subject: "s".to_owned(),
            at,
            conversation: None,
        };
        // a touched i1 over [1000, 2000), b from 1500 on, c touched only
        // another incarnation.
        let mut a = live_row("aaaaaaaa-1", 1);
        a.touches = vec![TouchRow {
            valid_until: Some(2_000),
            ..open_touch("i1")
        }];
        let mut b = live_row("bbbbbbbb-1", 2);
        b.touches = vec![TouchRow {
            valid_from: 1_500,
            ..open_touch("i1")
        }];
        let mut c = live_row("cccccccc-1", 3);
        c.touches = vec![open_touch("i2")];
        let convs = vec![a, b, c];
        let mut commits = Some(vec![
            commit(1_200),
            commit(1_700),
            commit(2_000),
            commit(900),
        ]);
        attribute_commits(&mut commits, "i1", &convs);
        let named: Vec<Option<&str>> = commits
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.conversation.as_deref())
            .collect();
        // Only a covers 1200; a and b both cover 1700 - no guess; a's
        // interval ends before 2000, leaving b; nothing covers 900.
        assert_eq!(named, vec![Some("aaaaaaaa"), None, Some("bbbbbbbb"), None]);
        // An unknown list stays unknown.
        let mut unknown = None;
        attribute_commits(&mut unknown, "i1", &convs);
        assert!(unknown.is_none());
    }

    #[test]
    fn a_gone_row_keeps_its_own_placement_in_classification() {
        let mut row = space_row("/r", Path::new("/r"));
        row.gone = Some("project folder gone".to_owned());
        row.summary = "project folder gone · 1 process".to_owned();
        let summary = row.summary.clone();
        classify_work(&mut row, &[], Duration::from_secs(1), 2_000_000_000);
        assert_eq!(row.summary, summary);
        assert_eq!(row.section, WorkSection::FollowUp);
        row.section = WorkSection::CleanupReview;
        classify_work(&mut row, &[], Duration::from_secs(1), 2_000_000_000);
        assert_eq!(row.section, WorkSection::CleanupReview);
    }

    #[test]
    fn inside_path_matches_canonicalized_descent() {
        let root =
            std::env::temp_dir().join(format!("agent-sessions-inside-{}", std::process::id()));
        let real = root.join("real");
        fs::create_dir_all(real.join("sub")).unwrap();
        let link = root.join("link");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A symlinked spelling of the same directory still binds.
        assert!(inside_path(&link.join("sub"), &real));
        assert!(inside_path(&real.join("sub"), &link));
        assert!(inside_path(&real, &real));
        assert!(!inside_path(&real, &real.join("sub")));
        fs::remove_dir_all(&root).unwrap();
        // A path that no longer exists compares by literal spelling.
        let gone = root.join("gone");
        assert!(inside_path(&gone.join("x"), &gone));
        assert!(!inside_path(&gone, &gone.join("x")));
    }
}
