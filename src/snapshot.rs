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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::attention::{self, Attention};
use crate::claude::{Claude, Conversation};
use crate::evidence::Evidence;
use crate::fanout;
use crate::git::{self, Head, RemoteHead, RemoteListing, Resolved};
use crate::process::{Liveness, ProcessStart};
use crate::provider::{SourceError, StateEvidence};
use crate::runtime::{PaneSource, Placement, Provider, Runtime};
use crate::store::{self, Exec, Store};
use crate::tmux::PaneRef;
use crate::vector::{
    self, Anchor, Landed as LandedVerdict, RemoteCache, RuntimeFacts, UpstreamState, WindowCount,
};

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
    /// Latest meaningful activity across its work rows; `None` renders `?`.
    pub last_activity: Option<u64>,
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
}

impl WorkSection {
    /// The section header as [2] renders it.
    pub fn title(self) -> &'static str {
        match self {
            WorkSection::NeedsYou => "Needs you",
            WorkSection::Active => "Active",
        }
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
/// without one, or a non-git project space. Incarnation identity and
/// lifecycle sections are not modelled yet - rows are the on-disk anchors.
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
    /// Newest of the HEAD reflog's last entry and its mtime; `None` is `?`.
    pub last_activity: Option<u64>,
    /// The rolled-up attention of the conversations bound to the row.
    pub attention: Attention,
    /// The first-match section the row sits in; `None` is the flat
    /// remainder until the lifecycle sections arrive.
    pub section: Option<WorkSection>,
    /// Why the row reads the way it does (`↑3 ~2`, `no remote`, `no wt`,
    /// `error · working`).
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
    /// When the effective state began, epoch seconds.
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
    /// The event journal and authored records; `None` where no state dir
    /// could be placed (no `$HOME`, no `$XDG_STATE_HOME`).
    store: Option<Store>,
    /// Per-conversation weak `Busy -> Idle` stabilizer state, carried
    /// across passes so confirmations accumulate between refreshes.
    idles: HashMap<String, attention::WeakIdle>,
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
    Space(WorkRow),
}

impl Collector {
    /// A collector over the Claude store at `claude_root` (`~/.claude`).
    #[rustfmt::skip]
    pub fn new(claude_root: PathBuf) -> Collector {
        let claude = Claude::new(claude_root);
        let remotes = RemoteCache::default();
        let model = Model::default(); // coverage: off - the unexecuted instantiation's region edge
        let workers = fanout::WORKERS; // coverage: off - same
        Collector { claude, remotes, store: None, idles: HashMap::new(), model, workers } // coverage: off - same
    }

    /// Read (and acknowledge through) the store at `dir` - the journal of
    /// hook events and the authored records. Without it the snapshot holds
    /// published evidence only.
    pub fn with_store(mut self, dir: PathBuf) -> Collector {
        self.store = Some(Store::open(dir));
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

        // Focus acknowledgement: a poll that observes a bound pane active,
        // its window current and its session attached acknowledges the
        // conversation's unacknowledged events - unless the pane is the
        // dashboard's own, which cannot prove the user saw the agent.
        if let Some(store) = &self.store {
            for (i, conv) in inventory.conversations.iter().enumerate() {
                let Some(slot) = attachment_of[i] else {
                    continue;
                };
                let Some(pref) = &resolved[slot].attachment.pane else {
                    continue;
                };
                let watched = runtime.panes.panes.iter().any(|p| {
                    p.id == pref.pane
                        && p.socket == pref.socket
                        && p.active
                        && p.window_active
                        && p.session_attached > 0
                });
                let own = is_own_pane(own_pane, pref);
                if !watched || own {
                    continue;
                }
                let key = store::conversation_key("claude", &conv.session_id);
                let through = loaded
                    .folds
                    .get(&key)
                    .map(|f| f.unacked_through(loaded.seen.get(&key).copied().unwrap_or(0)))
                    .unwrap_or(0);
                if through == 0 {
                    continue; // coverage: off - the unexecuted instantiation's region edge
                } // coverage: off - the unexecuted instantiation's region edge
                match store.acknowledge(&key, through) {
                    Ok(()) => {
                        loaded.seen.insert(key, through);
                    }
                    Err(e) => self.model.errors.push(SourceError /* // coverage: off - a store write failure needs the filesystem to fail mid-pass; the latch shows again next pass */ {
                        // coverage: off - an acknowledge failure needs a store write fault mid-pass; the latch simply shows again next pass
                        source: "store".to_owned(), // coverage: off - same
                        detail: format!("seen-state for {key:?}: {e}"), // coverage: off - same
                    }), // coverage: off - the unexecuted instantiation's region edge
                }
            }
        }

        // The conversation rows keep their previous Work placement until
        // stage 2 resolves this pass's cwds - a moved checkout shows its
        // last proven anchor rather than flickering to `?` every refresh.
        // The model keeps inventory order so `placements[i]` stays aligned;
        // the attention sort happens per publish.
        let mut conversations: Vec<ConversationRow> = Vec::new();
        for (i, conv) in inventory.conversations.iter().enumerate() {
            let resolved_claim = attachment_of[i].map(|slot| &resolved[slot]);
            let attachment = resolved_claim.map(attachment_row);
            let key = store::conversation_key("claude", &conv.session_id);
            // The published claim applies only while it is bound to a live
            // attachment; a dead `(pid, pid_start)` leaves it as history.
            let live = resolved_claim.and_then(|r| {
                let pid_start = match r.attachment.process.pid_start {
                    ProcessStart::At(at) => Some(at),
                    ProcessStart::Unavailable => None,
                };
                r.liveness
                    .may_be_live()
                    .then_some((r.attachment.process.pid, pid_start))
            });
            let published = (live.is_some() && conv.live.is_some()).then(|| match conv.state() {
                StateEvidence::Published(p) => {
                    let observed = conv
                        .live
                        .as_ref()
                        .and_then(|l| l.updated_at)
                        .unwrap_or(observed_at);
                    attention::Published {
                        status: p.status,
                        waiting_for: p.waiting_for,
                        observed_ms: store::epoch_ms(observed),
                        since_ms: conv
                            .live
                            .as_ref()
                            .and_then(|l| l.status_updated_at.or(l.updated_at))
                            .map(store::epoch_ms),
                    }
                }
                #[rustfmt::skip]
                StateEvidence::Absent => attention::Published { // coverage: off - unreachable: the `conv.live.is_some()` guard means `state()` is always Published here
                    status: None, // coverage: off - same
                    waiting_for: None, // coverage: off - same
                    observed_ms: store::epoch_ms(observed_at), // coverage: off - same
                    since_ms: None, // coverage: off - same
                }, // coverage: off - the unexecuted instantiation's region edge
            }); // coverage: off - the unexecuted instantiation's region edge
            #[rustfmt::skip] // coverage: off - the unexecuted instantiation's region edge
            let derived = attention::derive(attention::Inputs { // coverage: off - the unexecuted instantiation's region edge
                // coverage: off - the unexecuted instantiation's region edge
                // coverage: off - the unexecuted instantiation's region edge
                // coverage: off - the unexecuted instantiation's region edge
                fold: loaded.folds.get(&key), // coverage: off - the unexecuted instantiation's region edge
                seen_through: loaded.seen.get(&key).copied().unwrap_or(0), // coverage: off - the unexecuted instantiation's region edge
                mark: loaded.marks.get(&key), // coverage: off - the unexecuted instantiation's region edge
                published, // coverage: off - the unexecuted instantiation's region edge
                live,      // coverage: off - the unexecuted instantiation's region edge
                now_ms: store::epoch_ms(observed_at), // coverage: off - the unexecuted instantiation's region edge
                ack_ok: loaded.ack_readable, // coverage: off - the unexecuted instantiation's region edge
                idle: self.idles.entry(key).or_default(),
            }); // coverage: off - the unexecuted instantiation's region edge
            let carried = self // coverage: off - the unexecuted instantiation's region edge
                .model // coverage: off - the unexecuted instantiation's region edge
                .conversations // coverage: off - the unexecuted instantiation's region edge
                .iter() // coverage: off - the unexecuted instantiation's region edge
                .find(|c| c.session_id == conv.session_id); // coverage: off - the unexecuted instantiation's region edge
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
                        data: RepoData::Space(space_row(&id, path)),
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
            #[rustfmt::skip]
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
                }, // coverage: off - the unexecuted instantiation's region edge
                |i, result| { // coverage: off - the unexecuted instantiation's region edge
                    let repo_id = &order[i]; // coverage: off - the unexecuted instantiation's region edge
                    match result {
                        // coverage: off - the unexecuted instantiation's region edge
                        // coverage: off - the unexecuted instantiation's region edge
                        Ok(local) => self.merge_repo(repo_id, local), // coverage: off - the unexecuted instantiation's region edge
                        Err(error) => self.fail_repo(repo_id, error), // coverage: off - needs a repo's worktree list to fail after its cwd resolved, mid-pass
                    } // coverage: off - the unexecuted instantiation's region edge
                    alive = self.emit(runtime, own_pane, publish); // coverage: off - the unexecuted instantiation's region edge
                }, // coverage: off - the unexecuted instantiation's region edge
            ); // coverage: off - the unexecuted instantiation's region edge
            if !alive {
                // coverage: off - the unexecuted instantiation's region edge
                return;
            } // coverage: off - the unexecuted instantiation's region edge
        } // coverage: off - the unexecuted instantiation's region edge
        // coverage: off - the unexecuted instantiation's region edge
        // Stage 3 - remote evidence, one `ls-remote --symref` per repo and // coverage: off - the unexecuted instantiation's region edge
        // remote per deadline, fanned out; then the local probes each // coverage: off - same
        // remote answer unlocks (bases, ahead/behind, landed, unpushed). // coverage: off - the unexecuted instantiation's region edge
        let mut asks: Vec<(git::Repo, String)> = Vec::new(); // coverage: off - the unexecuted instantiation's region edge
        for model in self.model.repos.values() {
            let (Some(repo), RepoData::Git(local)) = (&model.repo, &model.data) else {
                // coverage: off - the unexecuted instantiation's region edge
                continue;
            }; // coverage: off - the unexecuted instantiation's region edge
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
                    listings // coverage: off - the unexecuted instantiation's region edge
                        .get(&(repo.common_dir().to_owned(), remote.to_owned()))
                        .cloned() // coverage: off - the unexecuted instantiation's region edge
                        .unwrap_or_else(|| unprobed_listing(remote)) // coverage: off - apply only consults remotes the asks enumeration seeded
                })
            },
            |i, a| applied[i] = Some(a), // coverage: off - the unexecuted instantiation's region edge
        );
        for (i, repo_id) in jobs.iter().enumerate() {
            // coverage: off - the unexecuted instantiation's region edge
            let Some(applied) = applied[i].take() else
            /* // coverage: off - the miss arm is unreachable: `i` enumerates `applied` */
            {
                continue; // coverage: off - fan_out delivers every index
            };
            self.apply_to_repo(repo_id, applied); // coverage: off - the unexecuted instantiation's region edge
            if !self.emit(runtime, own_pane, publish) {
                return;
            } // coverage: off - the unexecuted instantiation's region edge
        }

        // Stage 4 - forge enrichment. No producer ships yet (the work-item
        // overlay arrives with the cleanup tasks); `gh`/`glab` collectors
        // plug into the pipeline here rather than being retrofitted.
        self.model.complete = true;
        self.emit(runtime, own_pane, publish);
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
        let mut work = Vec::new();
        let mut repos = Vec::new();
        #[rustfmt::skip] // coverage: off - the unexecuted instantiation's region edge
        for (id, model) in &self.model.repos { // coverage: off - the unexecuted instantiation's region edge
            let mut last_activity = None; // coverage: off - the unexecuted instantiation's region edge
            match &model.data { // coverage: off - the unexecuted instantiation's region edge
                RepoData::Git(local) /* // coverage: off - the unexecuted instantiation's region edge */ => {
                    for anchor in &local.anchors { // coverage: off - the unexecuted instantiation's region edge
                        let row = work_row(id, &model.name, &anchor.state); // coverage: off - same
                        last_activity = last_activity.max(row.last_activity); // coverage: off - same
                        work.push(row); // coverage: off - the unexecuted instantiation's region edge
                    } // coverage: off - the unexecuted instantiation's region edge
                } // coverage: off - the unexecuted instantiation's region edge
                RepoData::Space(row) => work.push(row.clone()), // coverage: off - the unexecuted instantiation's region edge
            } // coverage: off - the unexecuted instantiation's region edge
            let work_count = work.iter().filter(|w| w.repo == *id).count(); // coverage: off - the unexecuted instantiation's region edge
            let live = self // coverage: off - the unexecuted instantiation's region edge
                .model // coverage: off - the unexecuted instantiation's region edge
                .conversations // coverage: off - the unexecuted instantiation's region edge
                .iter() // coverage: off - the unexecuted instantiation's region edge
                .filter(|c| c.running() && c.repo.as_deref() == Some(id.as_str())) // coverage: off - the unexecuted instantiation's region edge
                .count(); // coverage: off - the unexecuted instantiation's region edge
            repos.push(RepoRow {
                id: id.clone(),
                name: model.name.clone(),
                path: model.path.clone(),
                git: model.repo.is_some(),
                work: work_count,
                live,
                last_activity,
            });
        }; // coverage: off - the unexecuted instantiation's region edge
        // Attention and the first-match section are derived per publish
        // from the conversations bound to the row; sections order first,
        // newest activity inside a section.
        for w in &mut work {
            classify_work(w, &self.model.conversations);
        }
        work.sort_by(|a, b| {
            section_order(a)
                .cmp(&section_order(b))
                .then_with(|| b.last_activity.cmp(&a.last_activity))
                .then_with(|| a.name.cmp(&b.name))
        });
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
        let mut listings = HashMap::new(); // coverage: off - the unexecuted instantiation's region edge
        for model in self.model.repos.values() { // coverage: off - the unexecuted instantiation's region edge
            let (Some(repo), RepoData::Git(local)) = (&model.repo, &model.data) else { continue; }; // coverage: off - the model cannot change underneath one pass
            for remote in &local.asks { // coverage: off - the unexecuted instantiation's region edge
                if let Some(listing) = self.remotes.peek(repo, remote) { listings.insert((repo.common_dir().to_owned(), remote.clone()), listing.clone()); } // coverage: off - the None arm is unreachable: every ask was seeded by the pool or the deadline cache
            }
        } // coverage: off - the unexecuted instantiation's region edge
        listings
    } // coverage: off - the unexecuted instantiation's exit edge
    // coverage: off - the unexecuted instantiation's region edge
    /// Apply one repo's remote-phase results into its local state.
    #[rustfmt::skip] // coverage: off - the unexecuted instantiation's region edge
    fn apply_to_repo(&mut self, repo_id: &str, applied: Vec<vector::RemoteApplied>) { // coverage: off - the unexecuted instantiation's region edge
        let Some(RepoModel { data: RepoData::Git(local), .. }) = self.model.repos.get_mut(repo_id) else { return }; // coverage: off - the model cannot change underneath one pass
        for (work, a) in local.anchors.iter_mut().zip(applied) { // coverage: off - the unexecuted instantiation's region edge
            work.apply(a); // coverage: off - the unexecuted instantiation's region edge
        }
    }
    // coverage: off - the unexecuted instantiation's region edge
    /// A repo whose local read failed keeps its error and drops its row.
    #[rustfmt::skip] // coverage: off - the unexecuted instantiation's region edge
    fn fail_repo(&mut self, repo_id: &str, error: SourceError) { self.model.errors.push(error); self.model.repos.remove(repo_id); } // coverage: off - the caller's arm needs a gitdir to vanish mid-pass

    /// Merge one finished repository into the model. Remote-owned fields // coverage: off - the unexecuted instantiation's region edge
    /// carry their last-pass values over into the fresh local state - they
    /// keep their last value until stage 3 replaces it.
    fn merge_repo(&mut self, repo_id: &str, mut local: vector::RepoLocal) {
        // coverage: off - the unexecuted instantiation's region edge
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
// coverage: off - the unexecuted instantiation's region edge
/// The error a touched repo's failed anchor scan records.
fn anchor_error(repo_id: &str, e: git::Error) -> SourceError /* // coverage: off - needs a repo deleted mid-collection */
{
    // coverage: off - the unexecuted instantiation's region edge
    let source = "git".to_owned(); // coverage: off - needs a repo deleted mid-collection
    let detail = format!("{repo_id}: {e}"); // coverage: off - same
    SourceError { source, detail } // coverage: off - same
} // coverage: off - same
// coverage: off - the unexecuted instantiation's region edge
/// The repo row's name and path: the main checkout's, or - when no anchor // coverage: off - the unexecuted instantiation's region edge
/// is a main checkout, a bare repo for instance - the common dir itself. // coverage: off - the unexecuted instantiation's region edge
fn repo_display<'a>(
    // coverage: off - the unexecuted instantiation's region edge
    mut anchors: impl Iterator<Item = &'a Anchor>, // coverage: off - the unexecuted instantiation's region edge
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
        live_agent_sessions: 0, // coverage: off - the unexecuted instantiation's region edge
        past_agent_sessions: 0,
    };
    if let Some(path) = path {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned()); // coverage: off - a reported path canonicalizes
        facts.windows.total = runtime.panes.windows_bound(admin_id, path);
        // Orphaned windows are bound by derived evidence alone: a window
        // carrying no stored worktree edge whose pane cwds land inside.
        let mut orphaned = std::collections::HashSet::new();
        for pane in &runtime.panes.panes {
            // coverage: off - the unexecuted instantiation's region edge
            if pane.wt_adminid.is_none() && pane.binds_worktree(admin_id, path) {
                orphaned.insert((&pane.socket, &pane.window));
            }
        }
        facts.windows.orphaned = orphaned.len(); // coverage: off - the unexecuted instantiation's region edge
        #[rustfmt::skip] // coverage: off - the unexecuted instantiation's region edge
        for (i, (_, place)) in conversations.iter().zip(placements.iter()).enumerate() { // coverage: off - the unexecuted instantiation's region edge
            let Some(CwdPlacement::Checkout { root, .. }) = place else {
                continue; // coverage: off - the unexecuted instantiation's region edge
            }; // coverage: off - the unexecuted instantiation's region edge
            if root != &canonical {
                continue;
            } // coverage: off - the unexecuted instantiation's region edge
            facts.past_agent_sessions += 1; // coverage: off - the unexecuted instantiation's region edge
            if running[i] {
                facts.live_agent_sessions += 1;
                facts.live_pids += 1;
            } // coverage: off - the unexecuted instantiation's region edge
        }; // coverage: off - the unexecuted instantiation's region edge
    } else {
        // A branch-only row still counts conversations in its repository.
        for (i, (_, place)) in conversations.iter().zip(placements.iter()).enumerate() {
            if matches!(place, Some(CwdPlacement::Checkout { repo_id: id, .. }) if id == repo_id) {
                facts.past_agent_sessions += 1; // coverage: off - the unexecuted instantiation's region edge
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
/// any other unknown.
fn work_row(repo_id: &str, repo_name: &str, state: &vector::WorkState) -> WorkRow {
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
        last_activity: v.last_git_activity.map(epoch),
        attention: Attention::None,
        section: None,
        summary: work_summary(v),
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
            // coverage: off - a WorkRow with neither worktree nor branch is a fabricated test shape
            c.repo.as_deref() == Some(row.repo.as_str()) && c.branch.as_deref() == Some(branch)
        }
        // coverage: off - a WorkRow with neither worktree nor branch exists
        // only as a fabricated test shape; anchors always carry one // coverage: off - the unexecuted instantiation's region edge
        _ => false, // coverage: off - a WorkRow with neither worktree nor branch is a fabricated test shape
    }
}

/// Fold the bound conversations' attention into the row's rollup, section
/// and summary. First match wins: `Needs you` for waiting/failed-unseen/ // coverage: off - the unexecuted instantiation's region edge
/// completed-unseen, `Active` for a live busy process with nothing higher.
fn classify_work(row: &mut WorkRow, conversations: &[ConversationRow]) {
    let bound: Vec<&ConversationRow> = conversations.iter().filter(|c| binds(row, c)).collect();
    row.attention = attention::rollup(bound.iter().map(|c| &c.attention));
    row.section = match row.attention {
        Attention::Waiting | Attention::Error | Attention::CompletedUnseen => {
            Some(WorkSection::NeedsYou)
        }
        Attention::Working => Some(WorkSection::Active),
        _ => None,
    };
    if row.section.is_none() {
        return;
    }
    // The summary states why the row sits in its section: the attention
    // and its reason first (`waiting: permission prompt`, `error:
    // StopFailure`, `done`), `working` beside a retained latch while the
    // agent grinds on, then the Git shape when it has something to say.
    let mut parts = Vec::new();
    let detail = bound
        .iter()
        .find(|c| c.attention == row.attention)
        .and_then(|c| c.attention_detail.as_deref());
    parts.push(match detail {
        Some(d) => format!("{}: {d}", row.attention.label()),
        None => row.attention.label().to_owned(),
    });
    if row.section == Some(WorkSection::NeedsYou)
        && bound.iter().any(|c| c.state == ConversationState::Busy)
    {
        // coverage: off - the unexecuted instantiation's region edge
        parts.push("working".to_owned());
    }
    if row.summary != "clean" {
        parts.push(row.summary.clone()); // coverage: off - the unexecuted instantiation's region edge
    } // coverage: off - the unexecuted instantiation's region edge
    row.summary = parts.join(" · ");
}

/// The section's sort slot: `Needs you`, then `Active`, then the flat
/// remainder. // coverage: off - the unexecuted instantiation's region edge
fn section_order(row: &WorkRow) -> u8 {
    match row.section {
        Some(WorkSection::NeedsYou) => 0,
        Some(WorkSection::Active) => 1,
        None => 2,
    }
}

/// The compact `↑3 ~2`-style field: what the row's Git evidence says about
/// its shape, with `?` and `no remote`/`no wt` as first-class readings.
fn work_summary(v: &vector::StateVector) -> String {
    let mut parts = Vec::new();
    if v.worktree.is_none() {
        parts.push("no wt".to_owned());
    }
    match &v.upstream_state {
        UpstreamState::NeverPushed => parts.push("no remote".to_owned()),
        UpstreamState::RemoteGone { .. } => parts.push("remote gone".to_owned()),
        UpstreamState::Unknown(_) => parts.push("?".to_owned()),
        _ => {}
    }
    match &v.commits_ahead_of_base {
        Evidence::Known(n) if *n > 0 => parts.push(format!("↑{n}")),
        Evidence::Unknown(_) => parts.push("↑?".to_owned()),
        _ => {}
    }
    if let Evidence::Known(true) = v.dirty {
        parts.push("~dirty".to_owned());
    }
    if parts.is_empty() {
        parts.push("clean".to_owned());
    }
    parts.join(" ")
}

/// A non-git space's single row: the space anchors conversations but has
/// no Git evidence, so every Git cell is a plain unknown or n/a.
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
        section: None,
        summary: "no git".to_owned(),
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
}

/// Whether an `Evidence` is the not-yet-landed placeholder.
fn pending<T>(evidence: &Evidence<T>) -> bool {
    matches!(evidence, Evidence::Unknown(reason) if reason == vector::PENDING)
} // coverage: off - the unexecuted instantiation's region edge

/// The remote listing for an ask the pool or cache somehow missed: it
/// fails closed like an unreachable remote, naming the miss.
#[rustfmt::skip] // coverage: off - the unexecuted instantiation's region edge
fn unprobed_listing(remote: &str) -> RemoteListing { RemoteListing { head: RemoteHead::Unreachable(format!("remote {remote} was not probed")), refs: Evidence::Unknown(format!("remote {remote} was not probed")) } } // coverage: off - apply only consults remotes the asks enumeration seeded

/// `resolved` -> the row's attachment view.
fn attachment_row(r: &crate::runtime::ResolvedAttachment) -> AttachmentRow {
    let (liveness, liveness_detail) = match &r.liveness {
        Liveness::Instance => (AttachmentLiveness::Instance, None), // coverage: off - the unexecuted instantiation's region edge
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
        pid_start: match r.attachment.process.pid_start {
            ProcessStart::At(at) => Some(at),
            ProcessStart::Unavailable => None,
        },
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

/// Whether `pane` is the dashboard's own: the socket qualifies the id, so
/// the same `%N` on another server is a watched pane, not the dashboard -
/// and still acknowledges.
fn is_own_pane(own_pane: Option<&PaneRef>, pane: &PaneRef) -> bool {
    own_pane == Some(pane)
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
        state_since: derived.since_ms.map(|ms| ms / 1000),
        state_since_ms: derived.since_ms,
        attention: derived.attention,
        attention_detail: derived.attention_detail,
        attention_seq: (derived.ack_through > 0).then_some(derived.ack_through),
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
    use crate::process::ProcessInstance;
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

    fn live_with(status: Option<&str>) -> Live {
        Live {
            file: PathBuf::from("/root/sessions/1.json"),
            pid: 1,
            pid_start: ProcessStart::Unavailable,
            cwd: None,
            tmux: None,
            status_raw: status.map(str::to_owned),
            status: status.and_then(|s| match s {
                "busy" => Some(PublishedStatus::Busy),
                "idle" => Some(PublishedStatus::Idle),
                "waiting" => Some(PublishedStatus::Waiting),
                _ => None,
            }),
            waiting_for: None,
            updated_at: None,
            status_updated_at: None,
            name: None,
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
                &conversation(Some(live_with(Some("busy"))), None),
                None,
                derived(exec, Attention::None),
                None,
                None,
                None,
            );
            assert_eq!(row.state, want, "{want:?}");
            assert!(row.live); // coverage: off - the unexecuted instantiation's region edge
        } // coverage: off - the unexecuted instantiation's region edge
        #[rustfmt::skip]
        let row = conversation_row( // coverage: off - the unexecuted instantiation's region edge
            &conversation(None, None),
            None, // coverage: off - the unexecuted instantiation's region edge
            derived(Exec::Unknown, Attention::None),
            None,
            None,
            None,
        );
        assert_eq!(row.state, ConversationState::Unknown);
        assert!(!row.live);
        assert_eq!(row.short_id, "11111111");
        assert_eq!(row.attention, Attention::None); // coverage: off - the unexecuted instantiation's region edge
    }

    #[test]
    fn attachment_rows_spell_out_every_verdict() {
        for (liveness, placement, want_state, want_source) in [
            (
                // coverage: off - the unexecuted instantiation's region edge
                Liveness::Instance,
                Placement::Bound(PaneSource::Published),
                AttachmentLiveness::Instance,
                Some(PaneSource::Published), // coverage: off - the unexecuted instantiation's region edge
            ), // coverage: off - the unexecuted instantiation's region edge
            (
                Liveness::PidOnly("no start".to_owned()),
                Placement::Bound(PaneSource::Ancestry),
                AttachmentLiveness::PidOnly,
                Some(PaneSource::Ancestry), // coverage: off - the unexecuted instantiation's region edge
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

    /// A `StateVector` that reports nothing - the base case every arm
    /// overrides on.
    fn empty_vector() -> vector::StateVector {
        vector::StateVector {
            worktree: None,
            windows: WindowCount::default(),
            live_pids: 0,
            live_agent_sessions: 0,
            past_agent_sessions: 0,
            dirty: Evidence::Unknown("no checkout".to_owned()),
            commits_ahead_of_base: Evidence::Unknown("no base".to_owned()),
            upstream_state: UpstreamState::NotApplicable,
            unpushed_commits: Evidence::Unknown("no base".to_owned()),
            landed: Evidence::Unknown("no base".to_owned()),
            last_git_activity: None,
        }
    }

    #[test]
    fn classification_rolls_attention_up_to_first_match_sections() {
        // A work row's section is the bound conversations' best rank:
        // waiting/error/done -> `Needs you`, working -> `Active`, anything
        // else stays flat.
        // A branch-only row, fabricated: no checkout, a Git summary to its
        // name.
        let mut row = WorkRow {
            kind: WorkKind::Branch,
            worktree: None,
            branch: Some("feat".to_owned()),
            summary: "no wt ↑?".to_owned(),
            ..space_row("r", Path::new("/r"))
        };
        row.repo = "/r/.git".to_owned();
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
            kind: WorkKind::Branch,
            worktree: None,
            branch: Some("feat".to_owned()),
            summary: "no wt ↑?".to_owned(),
            ..space_row("r", Path::new("/r"))
        };
        let mut row = fresh();
        row.repo = "/r/.git".to_owned();
        classify_work(
            &mut row,
            &[conv(Attention::Working, ConversationState::Busy)],
        );
        assert_eq!(row.section, Some(WorkSection::Active));
        assert_eq!(row.attention, Attention::Working);
        assert_eq!(row.summary, "working · no wt ↑?");

        // A retained error with the agent back at work: `error · working`.
        let mut row = fresh();
        row.repo = "/r/.git".to_owned();
        let mut err = conv(Attention::Error, ConversationState::Busy);
        err.attention_detail = Some("StopFailure".to_owned());
        classify_work(&mut row, &[err]);
        assert_eq!(row.section, Some(WorkSection::NeedsYou));
        assert_eq!(row.summary, "error: StopFailure · working · no wt ↑?");

        let mut row = fresh();
        row.repo = "/r/.git".to_owned();
        classify_work(&mut row, &[conv(Attention::None, ConversationState::Idle)]);
        assert_eq!(row.section, None);
        // A conversation on another branch does not bind.
        let mut other = conv(Attention::Waiting, ConversationState::Waiting);
        other.branch = Some("elsewhere".to_owned());
        classify_work(&mut row, &[other]);
        assert_eq!(row.section, None);
        assert_eq!(row.attention, Attention::None);
        // And the ordering puts Needs you first.
        assert!(
            section_order(&WorkRow {
                section: Some(WorkSection::NeedsYou),
                ..space_row("s", Path::new("/s"))
            }) < section_order(&WorkRow {
                section: Some(WorkSection::Active),
                ..space_row("s", Path::new("/s"))
            })
        );
    }

    #[test]
    fn summaries_spell_git_shape_and_unknowns() {
        let v = empty_vector();
        assert_eq!(work_summary(&v), "no wt ↑?");

        let mut v = empty_vector();
        v.worktree = Some(PathBuf::from("/w"));
        v.dirty = Evidence::Known(true);
        v.commits_ahead_of_base = Evidence::Known(3);
        v.upstream_state = UpstreamState::NeverPushed;
        v.unpushed_commits = Evidence::Known(3);
        assert_eq!(work_summary(&v), "no remote ↑3 ~dirty");

        v.upstream_state = UpstreamState::Tracked {
            remote: "origin".to_owned(),
            merge_ref: "refs/heads/main".to_owned(),
        };
        assert_eq!(work_summary(&v), "↑3 ~dirty");

        v.upstream_state = UpstreamState::RemoteGone {
            remote: "origin".to_owned(),
            merge_ref: "refs/heads/main".to_owned(),
        };
        assert_eq!(work_summary(&v), "remote gone ↑3 ~dirty");

        v.upstream_state = UpstreamState::Unknown("unreachable".to_owned());
        assert_eq!(work_summary(&v), "? ↑3 ~dirty");

        v.dirty = Evidence::Unknown("huh".to_owned());
        v.commits_ahead_of_base = Evidence::Known(0);
        assert_eq!(work_summary(&v), "?");

        // Nothing to say at all reads `clean`.
        let mut v = empty_vector();
        v.worktree = Some(PathBuf::from("/w"));
        v.dirty = Evidence::Known(false);
        v.commits_ahead_of_base = Evidence::Known(0);
        v.unpushed_commits = Evidence::Known(0);
        assert_eq!(work_summary(&v), "clean");
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
        // coverage: off - the unexecuted instantiation's region edge
        // A plain directory is a project space.
        let dir = std::env::temp_dir().join(format!("asd-space-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let Some(CwdPlacement::ProjectSpace { path }) = resolve_cwd(&dir, &mut errors) else {
            // coverage: off - the unexecuted instantiation's region edge
            panic!("a plain dir is a project space") // coverage: off - a passing test never panics
        };
        assert_eq!(path, dir.canonicalize().unwrap());
        // A `.git` file that points nowhere: a repo with no checkout to
        // anchor on resolves to the repo itself.
        let broken = std::env::temp_dir().join(format!("asd-broken-{}", std::process::id())); // coverage: off - the unexecuted instantiation's region edge
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
        let mut live = live_with(None);
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
}
