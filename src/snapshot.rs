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
use crate::runtime::{PaneSource, Placement, Provider, Runtime};
use crate::store::{self, Exec, Store};
use crate::tmux::PaneRef;
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
}

impl RepoRow {
    /// Recompute the rollup from the repo's rows in `work` - attention,
    /// the `N open · M clean` counts and the latest activity. The emit
    /// path and a `p` reclassification share it, so both read the same way.
    pub fn roll_up(&mut self, work: &[WorkRow]) {
        let rows: Vec<&WorkRow> = work.iter().filter(|w| w.repo == self.id).collect();
        self.work = rows.len();
        self.attention = attention::rollup(rows.iter().map(|w| &w.attention));
        self.open = rows.iter().filter(|w| w.section.open()).count();
        self.clean = rows.len() - self.open;
        self.last_activity = rows.iter().filter_map(|w| w.last_activity).max();
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
    /// Newest of the HEAD reflog's last entry and its mtime, folded with
    /// the authored record's transition time and bound conversations'
    /// activity; `None` is `?`.
    pub last_activity: Option<u64>,
    /// The rolled-up attention of the conversations bound to the row.
    pub attention: Attention,
    /// The row's work identity: the active branch incarnation's id for
    /// branch rows, the canonical path for a detached worktree or a
    /// project space. `None` for a branch that has no record yet.
    pub identity: Option<String>,
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
    /// fingerprints - loaded at stage 1 and refreshed by the pass's sync.
    work: store::Work,
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
        let mut loaded = self.store.as_ref().map(Store::load).unwrap_or_default();
        self.model.errors.append(&mut loaded.errors);
        self.model.errors.extend(self.warnings.iter().cloned());
        self.model.work = std::mem::take(&mut loaded.work);
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
            let attachment = resolved_claim.map(attachment_row);
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
                conv, attachment, derived, repo, worktree, branch,
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
        let placements = resolve_cwds(&inventory.conversations, &mut self.model.errors);
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
    /// incarnations), one path sync per detached anchor and project space.
    /// Then `model.work` re-reads so the published snapshot's identities
    /// and parked flags are this pass's, not the stage-1 load's.
    fn sync_work(&mut self, store: &Store, observed_ms: u64) {
        for (repo_id, model) in &self.model.repos {
            match &model.data {
                RepoData::Git(local) => {
                    let mut seen = HashSet::new();
                    let refs: Vec<store::ObservedRef> = local
                        .anchors
                        .iter()
                        .filter_map(|w| {
                            let name = w.state.anchor.branch()?;
                            seen.insert(name.to_owned()).then(|| store::ObservedRef {
                                name: name.to_owned(),
                                inputs: lifecycle_inputs(&w.state),
                            })
                        })
                        .collect();
                    if let Err(e) = store.sync_repo(repo_id, &refs, observed_ms) {
                        self.model.errors.push(work_state_error(repo_id, e));
                    }
                    for w in &local.anchors {
                        if let Anchor::Worktree {
                            path,
                            head: Head::Detached(_),
                            ..
                        } = &w.state.anchor
                            && let Err(e) = store.sync_path(
                                &path.display().to_string(),
                                &lifecycle_inputs(&w.state),
                                observed_ms,
                            )
                        {
                            self.model.errors.push(work_state_error(repo_id, e));
                        }
                    }
                }
                RepoData::Space(row) => {
                    if let Err(e) =
                        store.sync_path(&row.repo, &store::LifecycleInputs::default(), observed_ms)
                    {
                        self.model.errors.push(work_state_error(repo_id, e));
                    }
                }
            }
        }
        let (work, mut errors) = store.work();
        self.model.work = work;
        self.model.errors.append(&mut errors);
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
        &self,
        runtime: &Runtime,
        own_pane: Option<&PaneRef>,
        publish: &mut dyn FnMut(Snapshot) -> bool,
    ) -> bool {
        let now = epoch(runtime.observed_at);
        let mut work = Vec::new();
        for (id, model) in &self.model.repos {
            match &model.data {
                RepoData::Git(local) => {
                    for anchor in &local.anchors {
                        work.push(work_row(id, &model.name, &anchor.state, &self.model.work));
                    }
                }
                RepoData::Space(row) => {
                    let mut row = (**row).clone();
                    apply_path_record(&mut row, &self.model.work);
                    work.push(row);
                }
            }
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
    let upstream = match &v.upstream_state {
        UpstreamState::NeverPushed => Upstream::NeverPushed,
        UpstreamState::Tracked { .. } => Upstream::Tracked,
        UpstreamState::RemoteGone { .. } => Upstream::RemoteGone,
        UpstreamState::Unknown(_) => Upstream::Unknown,
        UpstreamState::NotApplicable => Upstream::NotApplicable,
    };
    let upstream_detail = match &v.upstream_state {
        UpstreamState::Tracked { remote, merge_ref }
        | UpstreamState::RemoteGone { remote, merge_ref } => Some(format!("{remote}/{merge_ref}")),
        UpstreamState::Unknown(reason) => Some(reason.clone()),
        _ => None,
    };
    // Work identity: an active incarnation's id for a branch row, the
    // canonical path for a detached one. A branch whose record the sync
    // has not written yet carries no identity rather than a guess.
    let (identity, parked, authored_ms) = match &branch {
        Some(name) => match authored.branch(repo_id, name) {
            Some(r) => (Some(r.id.clone()), r.parked, r.activity_at),
            None => (None, false, None),
        },
        None => {
            let path = v.worktree.as_ref().map(|p| p.display().to_string());
            match path.and_then(|p| authored.path(&p).map(|r| (p, r))) {
                Some((p, r)) => (Some(p), r.parked, r.activity_at),
                None => (
                    v.worktree.as_ref().map(|p| p.display().to_string()),
                    false,
                    None,
                ),
            }
        }
    };
    let (removal, deletion) = verdict::cleanup(state, &state.forge);
    WorkRow {
        repo: repo_id.to_owned(),
        repo_name: repo_name.to_owned(),
        kind,
        name,
        worktree: v.worktree.clone(),
        branch,
        dirty: v.dirty.known().copied(),
        commits_ahead: v.commits_ahead_of_base.known().copied(),
        unpushed: v.unpushed_commits.known().copied(),
        upstream,
        upstream_detail,
        landed: v.landed.known().map(|l| match l {
            LandedVerdict::AncestorMerged => Landed::Ancestor,
            LandedVerdict::ContentMerged => Landed::Content,
            LandedVerdict::No => Landed::No,
        }),
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
        parked,
        forge: state.forge.item,
        pipeline: state.forge.pipeline,
        forge_label: state.forge.label.clone(),
        forge_url: state.forge.url.clone(),
        worktree_removal: Some(removal),
        branch_deletion: Some(deletion),
        section: WorkSection::FollowUp,
        summary: String::new(),
    }
}

/// The lifecycle fingerprint one anchor's state produces for the authored
/// record: every field a `work.json` record compares next pass to date a
/// transition at its observation.
fn lifecycle_inputs(state: &vector::WorkState) -> store::LifecycleInputs {
    let v = &state.vector;
    store::LifecycleInputs {
        dirty: v.dirty.known().copied(),
        worktree: v.worktree.is_some(),
        worktree_path: v.worktree.as_ref().map(|p| p.display().to_string()),
        ahead: v.commits_ahead_of_base.known().copied(),
        unpushed: v.unpushed_commits.known().copied(),
        upstream: Some(match &v.upstream_state {
            UpstreamState::NeverPushed => "never_pushed".to_owned(),
            UpstreamState::Tracked { remote, merge_ref } => {
                format!("tracked {remote}/{merge_ref}")
            }
            UpstreamState::RemoteGone { remote, merge_ref } => {
                format!("remote_gone {remote}/{merge_ref}")
            }
            UpstreamState::Unknown(reason) => format!("unknown {reason}"),
            UpstreamState::NotApplicable => "not_applicable".to_owned(),
        }),
        landed: v.landed.known().map(|l| {
            match l {
                LandedVerdict::AncestorMerged => "ancestor",
                LandedVerdict::ContentMerged => "content",
                LandedVerdict::No => "no",
            }
            .to_owned()
        }),
        forge: Some(
            match state.forge.item {
                WorkItem::Unknown => "unknown",
                WorkItem::NotExisting => "not_existing",
                WorkItem::Open => "open",
                WorkItem::Closed => "closed",
            }
            .to_owned(),
        ),
        pipeline: Some(
            match state.forge.pipeline {
                Pipeline::Busy => "busy",
                Pipeline::Succeeded => "succeeded",
                Pipeline::Failed => "failed",
                Pipeline::Unknown => "unknown",
            }
            .to_owned(),
        ),
    }
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

/// Refresh a path-keyed row's authored fields - a project space's row is
/// stored in the model, so parked and transition dates apply per publish.
fn apply_path_record(row: &mut WorkRow, authored: &store::Work) {
    let Some(identity) = &row.identity else {
        return; // coverage: off - a space row's identity is its path, always present
    };
    if let Some(record) = authored.path(identity) {
        row.parked = record.parked;
        row.last_activity = row
            .last_activity
            .into_iter()
            .chain(record.activity_at.map(|ms| ms / 1000))
            .max();
    }
}

/// Whether the conversation is bound to the work row: its checkout path for
/// a row with one, its branch for a branch-only row, its repo identity for
/// a project space. A detached row binds only by path.
pub fn binds(row: &WorkRow, c: &ConversationRow) -> bool {
    match (row.kind, row.worktree.as_deref(), row.branch.as_deref()) {
        (WorkKind::ProjectSpace, _, _) => c.repo.as_deref() == Some(row.repo.as_str()),
        (_, Some(root), _) => {
            c.repo.as_deref() == Some(row.repo.as_str()) && c.worktree.as_deref() == Some(root)
        }
        (_, None, Some(branch)) => {
            c.repo.as_deref() == Some(row.repo.as_str()) && c.branch.as_deref() == Some(branch)
        }
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
    if let Some(reason) = follow_up(row, bound) {
        return (WorkSection::FollowUp, reason);
    }
    // Forgotten: unfinished, nothing running, quiet strictly beyond the
    // configured threshold - and parked suppresses only this.
    let finished = matches!(row.landed, Some(Landed::Ancestor | Landed::Content));
    let quiet_beyond = row
        .last_activity
        .is_some_and(|a| now.saturating_sub(a) > forgotten_after.as_secs());
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
/// tree, unpushed commits, a resumable idle conversation, or a blocked
/// cleanup verdict. Unknown forge state triggers nothing by itself.
fn follow_up(row: &WorkRow, bound: &[&ConversationRow]) -> Option<String> {
    if row.forge == WorkItem::Open {
        match row.pipeline {
            Pipeline::Failed => return Some("checks failed".to_owned()),
            Pipeline::Busy => return Some("checks pending".to_owned()),
            Pipeline::Succeeded | Pipeline::Unknown => {}
        }
    }
    if row.dirty == Some(true) {
        return Some("dirty".to_owned());
    }
    if let Some(n) = row.unpushed.filter(|n| *n > 0) {
        return Some(format!("unpushed {n}"));
    }
    if bound
        .iter()
        .any(|c| !c.running() && !c.resume_argv.is_empty())
    {
        return Some("resumable idle".to_owned());
    }
    let blocked = [row.worktree_removal.as_ref(), row.branch_deletion.as_ref()]
        .into_iter()
        .flatten()
        .find(|a| a.verdict == Verdict::Blocked);
    if let Some(blocked) = blocked {
        // Landed work whose cleanup is blocked names both facts; anything
        // else names its first concrete blocker.
        if matches!(row.landed, Some(Landed::Ancestor | Landed::Content)) {
            return Some("merged · blocked".to_owned());
        }
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
        parked: false,
        forge: WorkItem::Unknown,
        pipeline: Pipeline::Unknown,
        forge_label: None,
        forge_url: None,
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
fn attachment_row(r: &crate::runtime::ResolvedAttachment) -> AttachmentRow {
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
        pane: r.attachment.pane.as_ref().map(display_pane),
        pane_source,
        placement_detail,
    }
}

/// A resolved attachment's pane as `session:window.pane` - the handle shape
/// a provider would publish - when the pane record is findable.
fn display_pane(pane: &PaneRef) -> String {
    pane.to_string()
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

/// Whether two readings of one pid's start are the same instance. An
/// OS-derived start is recomputed from `etime` each poll and can move by a
/// second across a boundary, so it compares with `ProcessStart`'s tolerance
/// rather than exactly; two undated readings stay pid-only and equal.
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

fn conversation_row(
    conv: &Conversation,
    attachment: Option<AttachmentRow>,
    derived: attention::Derived,
    repo: Option<String>,
    worktree: Option<PathBuf>,
    branch: Option<String>,
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
        journal_seq: (derived.journal_seq > 0).then_some(derived.journal_seq),
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
        }
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
            let row = attachment_row(&attachment(liveness, placement, true));
            assert_eq!(row.liveness, want_state);
            assert_eq!(row.pane_source, want_source);
        }
        // A bound pane renders its handle; a dead one renders the reason.
        let dead = attachment_row(&attachment(
            Liveness::Dead("gone".to_owned()),
            Placement::Dead("gone".to_owned()),
            true,
        ));
        assert_eq!(dead.placement_detail.as_deref(), Some("gone"));
        assert!(dead.pane.is_some(), "the pane ref stays for evidence");
        let bare = attachment_row(&attachment(
            Liveness::Instance,
            Placement::Bound(PaneSource::Ancestry),
            false,
        ));
        assert!(bare.pane.is_none());
        assert_eq!(bare.pid_start, Some(1_800_000_000));
        let undated = attachment_row(&attachment(
            Liveness::Instance,
            Placement::Bound(PaneSource::Ancestry),
            false,
        ));
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
            );
            c.state = state;
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
        // A conversation on another branch does not bind.
        let mut other = conv(Attention::Waiting, ConversationState::Waiting);
        other.branch = Some("elsewhere".to_owned());
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
        // `etime` quantizes, so the OS-derived start of one process can read
        // a second apart on consecutive polls without being a replacement.
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
}
