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

use crate::claude::{Claude, Conversation};
use crate::evidence::Evidence;
use crate::git::{self, Head, Resolved};
use crate::process::{Liveness, ProcessStart};
use crate::provider::{PublishedStatus, SourceError, StateEvidence};
use crate::runtime::{PaneSource, Placement, Provider, Runtime};
use crate::tmux::{PaneId, PaneRef};
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

/// `ConversationRow.state`: the provider's published execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationState {
    Busy,
    Idle,
    Waiting,
    /// No published state, or one the mapping does not know.
    Unknown,
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
    /// Why the row reads the way it does (`↑3 ~2`, `no remote`, `no wt`).
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
    /// The provider's published reading, not yet arbitrated against hooks
    /// (there are none yet).
    pub state: ConversationState,
    /// The provider's raw status string, kept for the evidence view.
    pub state_raw: Option<String>,
    /// The provider's own wait reason, verbatim.
    pub waiting_for: Option<String>,
    /// When the effective state began, epoch seconds.
    pub state_since: Option<u64>,
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

/// The collector: owns the plugins (and so their incremental indexes) and the
/// caches reused across passes until their freshness deadline. A collect is reads only - everything writes-averse
/// in the boundary stays averse here.
pub struct Collector {
    claude: Claude,
    remotes: RemoteCache,
}

impl Collector {
    /// A collector over the Claude store at `claude_root` (`~/.claude`).
    pub fn new(claude_root: PathBuf) -> Collector {
        Collector {
            claude: Claude::new(claude_root),
            remotes: RemoteCache::default(),
        }
    }

    /// One pass: inventory the providers, resolve runtime evidence, then
    /// resolve every conversation's cwd into repo/worktree/branch anchors
    /// and collect the work rows' Git evidence. `runtime` is the merged
    /// process/tmux observation taken for this pass; `own_pane` is the
    /// pane the dashboard itself sits in, when known.
    pub fn collect(&mut self, runtime: &Runtime, own_pane: Option<&PaneId>) -> Snapshot {
        let observed_at = runtime.observed_at;
        let inventory = self.claude.scan();
        let mut errors = inventory.errors;

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

        // Work identity per conversation: the cwd resolves to a checkout, a
        // bare repo, a project space, or nothing still on disk. Distinct
        // cwds are few while conversations are many, so each resolves once
        // per pass - a failure is also one error, not one per conversation.
        let placements = resolve_cwds(&inventory.conversations, &mut errors);

        // Repos: every distinct repository plus every non-git project space.
        let mut repos: BTreeMap<String, RepoRow> = BTreeMap::new();
        // Anchor the repos conversations resolved into, then every anchor of
        // each touched repo so [2] shows the repo's whole work surface, not
        // just where agents sat.
        let mut touched: BTreeMap<String, ()> = BTreeMap::new();
        for place in placements.iter().flatten() {
            if let CwdPlacement::Checkout { repo_id, .. } = place {
                touched.insert(repo_id.clone(), ());
            }
        }

        let mut work: Vec<WorkRow> = Vec::new();
        for repo_id in touched.keys() {
            let repo = git::Repo {
                common_dir: PathBuf::from(repo_id),
            };
            let anchors = match vector::anchors(&repo) {
                Ok(anchors) => anchors,
                Err(e) /* // coverage: off - a repo deleted mid-collection makes anchors() fail */ => {
                    errors.push(anchor_error(repo_id, e)); // coverage: off - same
                    continue; // coverage: off - same
                }
            };
            // The main checkout names and paths the repo row.
            let (name, path) = repo_display(&anchors, &repo);
            for anchor in &anchors {
                work.push(work_row(
                    &repo,
                    &name,
                    anchor,
                    &mut self.remotes,
                    runtime_facts(
                        runtime,
                        &inventory.conversations,
                        &running,
                        &placements,
                        anchor,
                        repo_id,
                    ),
                ));
            }
            repos.insert(
                repo_id.clone(),
                RepoRow {
                    id: repo_id.clone(),
                    name,
                    path,
                    git: true,
                    work: 0,
                    live: 0,
                    last_activity: None,
                },
            );
        }

        // Non-git project spaces become rows on their own pseudo-repo.
        for place in placements.iter().flatten() {
            if let CwdPlacement::ProjectSpace { path } = place {
                let id = path.display().to_string();
                repos.entry(id.clone()).or_insert_with(|| RepoRow {
                    id: id.clone(),
                    name: display_name(path),
                    path: path.clone(),
                    git: false,
                    work: 0,
                    live: 0,
                    last_activity: None,
                });
                work.push(WorkRow {
                    repo: id.clone(),
                    repo_name: display_name(path),
                    kind: WorkKind::ProjectSpace,
                    name: display_name(path),
                    // The row's workspace is the space itself: no checkout,
                    // but the path is what its conversations anchor on.
                    worktree: Some(path.clone()),
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
                    summary: "no git".to_owned(),
                });
            }
        }

        let mut conversations: Vec<ConversationRow> = Vec::new();
        for (i, conv) in inventory.conversations.iter().enumerate() {
            let attachment = attachment_of[i].map(|slot| attachment_row(&resolved[slot]));
            let (repo, worktree, branch) = match &placements[i] {
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
            conversations.push(conversation_row(conv, attachment, repo, worktree, branch));
        }

        // Counts roll up: work rows per repo, live conversations per repo,
        // and the repo's age as the newest activity across its work.
        for row in &mut repos.values_mut() {
            row.work = work.iter().filter(|w| w.repo == row.id).count();
            row.live = conversations
                .iter()
                .filter(|c| c.running() && c.repo.as_deref() == Some(row.id.as_str()))
                .count();
            row.last_activity = work
                .iter()
                .filter(|w| w.repo == row.id)
                .filter_map(|w| w.last_activity)
                .max();
        }

        sort_rows(&mut work, &mut conversations, observed_at);

        Snapshot {
            schema_version: SCHEMA_VERSION,
            observed_at: epoch(observed_at),
            own_pane: own_pane.map(|p| p.as_str().to_owned()),
            repos: repos.into_values().collect(),
            work,
            conversations,
            errors,
            skipped: inventory
                .skipped
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            stale_sockets: runtime.panes.stale_sockets,
        }
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
fn repo_display(anchors: &[Anchor], repo: &git::Repo) -> (String, PathBuf) {
    anchors
        .iter()
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
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned()); // coverage: off - a reported path canonicalizes
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
fn work_row(
    repo: &git::Repo,
    repo_name: &str,
    anchor: &Anchor,
    remotes: &mut RemoteCache,
    facts: RuntimeFacts,
) -> WorkRow {
    let state = vector::collect_cached(repo, remotes, anchor, facts);
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
        repo: repo.common_dir().display().to_string(),
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
        summary: work_summary(v),
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

fn conversation_row(
    conv: &Conversation,
    attachment: Option<AttachmentRow>,
    repo: Option<String>,
    worktree: Option<PathBuf>,
    branch: Option<String>,
) -> ConversationRow {
    let (state, state_raw, waiting_for) = match conv.state() {
        StateEvidence::Published(p) => (
            match p.status {
                Some(PublishedStatus::Busy) => ConversationState::Busy,
                Some(PublishedStatus::Idle) => ConversationState::Idle,
                Some(PublishedStatus::Waiting) => ConversationState::Waiting,
                None => ConversationState::Unknown,
            },
            Some(p.raw),
            p.waiting_for,
        ),
        StateEvidence::Absent => (ConversationState::Unknown, None, None),
    };
    ConversationRow {
        provider: Provider::Claude,
        session_id: conv.session_id.clone(),
        short_id: conv.session_id.chars().take(8).collect(),
        title: conv.title(),
        state,
        state_raw,
        waiting_for,
        state_since: conv.state_since().map(epoch),
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

/// Ordering for the two sorted lists: work by meaningful activity, most
/// recent first, unknown last; conversations by attention rank then
/// time-in-state, so a waiting row outranks everything older.
fn sort_rows(work: &mut [WorkRow], conversations: &mut [ConversationRow], at: SystemTime) {
    work.sort_by_key(|w| std::cmp::Reverse(w.last_activity));
    conversations.sort_by(|a, b| {
        attention_rank(a)
            .cmp(&attention_rank(b))
            .then_with(|| age_of(a, at).cmp(&age_of(b, at)))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
}

/// Lower sorts first: the attention glyph's inbox order. A claim the
/// runtime proved dead ranks below everything - the published state its
/// stale file still reports is not a live signal.
fn attention_rank(c: &ConversationRow) -> u8 {
    if c.live && !c.running() {
        return 4;
    }
    match c.state {
        ConversationState::Waiting => 0,
        ConversationState::Busy => 1,
        ConversationState::Idle => 2,
        ConversationState::Unknown => 3,
    }
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
    use crate::claude::{Live, Transcript};
    use crate::process::ProcessInstance;
    use crate::runtime::{EvidenceSource, LiveAttachment, ResolvedAttachment};
    use std::fs;

    /// One conversation fabricated to order: `live` and `transcript` each
    /// optional, so every merge shape can be built.
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

    #[test]
    fn a_row_exists_for_every_conversation_shape() {
        // Live-only, transcript-only and merged each produce one row; the
        // state column is the published status, `unknown` otherwise.
        for (live, want) in [
            (live_with(Some("busy")), ConversationState::Busy),
            (live_with(Some("idle")), ConversationState::Idle),
            (live_with(Some("waiting")), ConversationState::Waiting),
            (live_with(Some("strange")), ConversationState::Unknown),
            (live_with(None), ConversationState::Unknown),
        ] {
            let row = conversation_row(&conversation(Some(live), None), None, None, None, None);
            assert_eq!(row.state, want, "{want:?}");
            assert!(row.live);
        }
        let row = conversation_row(&conversation(None, None), None, None, None, None);
        assert_eq!(row.state, ConversationState::Unknown);
        assert!(!row.live);
        assert_eq!(row.short_id, "11111111");
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
        assert_eq!(repo_display(&[], &repo).0, "app.git");
        let anchors = vec![Anchor::Branch {
            name: "keep".to_owned(),
        }];
        assert_eq!(repo_display(&anchors, &repo).0, "app.git");
        let anchors = vec![Anchor::Worktree {
            path: PathBuf::from("/repos/app"),
            admin_id: None,
            head: Head::Branch("main".to_owned()),
            locked: false,
            main: true,
        }];
        assert_eq!(
            repo_display(&anchors, &repo),
            ("app".to_owned(), PathBuf::from("/repos/app"))
        );
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
        let row = |state: ConversationState, since: Option<u64>| ConversationRow {
            provider: Provider::Claude,
            session_id: String::new(),
            short_id: String::new(),
            title: None,
            state,
            state_raw: None,
            waiting_for: None,
            state_since: since,
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
        assert_eq!(attention_rank(&row(ConversationState::Waiting, None)), 0);
        assert_eq!(attention_rank(&row(ConversationState::Busy, None)), 1);
        assert_eq!(attention_rank(&row(ConversationState::Idle, None)), 2);
        assert_eq!(attention_rank(&row(ConversationState::Unknown, None)), 3);
        // No state timestamp means infinite age: sorted last of its rank.
        assert_eq!(
            age_of(&row(ConversationState::Waiting, None), now),
            Duration::MAX
        );
        assert_eq!(
            age_of(&row(ConversationState::Waiting, Some(1)), now),
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
        for (value, want) in [
            (wire(&WorkKind::Branch), "branch"),
            (wire(&WorkKind::Detached), "detached"),
            (wire(&WorkKind::Worktree), "worktree"),
            (wire(&WorkKind::ProjectSpace), "project_space"),
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
}
