//! The shared worktree state vector: the eleven derived facts that both the
//! section classification and the cleanup verdicts read.
//!
//! Runtime fields (windows, live processes, agent sessions) are injected by
//! the caller; everything else is read from Git, read-only, in this module.
//! Upstream and base evidence fail closed: a remote or branch name is never
//! guessed, so an unproven base leaves `landed` and `commits_ahead_of_base`
//! `Unknown` rather than compared against `origin/main` by convention.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::evidence::Evidence;
use crate::forge::{self, ForgeStatus, Pipeline, WorkItem};
use crate::git::{self, Head, RemoteHead, RemoteListing, Repo, Track, UpstreamConfig};

/// What a Work row is anchored on. Branch incarnations and detached
/// worktrees are the Git anchors; non-Git paths are project spaces and never
/// reach the vector.
#[derive(Debug, Clone)]
pub enum Anchor {
    /// A checkout on disk.
    Worktree {
        path: PathBuf,
        admin_id: Option<String>,
        head: Head,
        locked: bool,
        /// The repository's own checkout; `git worktree remove` refuses it.
        main: bool,
        prunable: Option<String>,
    },
    /// A local branch with no worktree anywhere.
    Branch { name: String },
}

impl Anchor {
    /// The branch this anchor sits on, if any.
    pub fn branch(&self) -> Option<&str> {
        match self {
            Anchor::Branch { name } => Some(name),
            Anchor::Worktree {
                head: Head::Branch(name) | Head::Unborn(name),
                ..
            } => Some(name),
            Anchor::Worktree { .. } => None,
        }
    }
}

/// The worktree/branch pairs of a repository: every non-bare worktree plus
/// every local branch not checked out in one.
pub fn anchors(repo: &Repo) -> Result<Vec<Anchor>, git::Error> {
    let mut anchors = Vec::new();
    let mut checked_out = Vec::new();
    for wt in repo.worktrees()? {
        if wt.bare {
            continue;
        }
        if let Head::Branch(name) | Head::Unborn(name) = &wt.head {
            checked_out.push(name.clone());
        }
        anchors.push(Anchor::Worktree {
            path: wt.path,
            admin_id: wt.admin_id,
            head: wt.head,
            locked: wt.locked,
            main: wt.main,
            prunable: wt.prunable,
        });
    }
    let names = repo.local_branches()?; // coverage: off - needs refs broken where worktree list succeeded
    for name in names {
        if !checked_out.contains(&name) {
            anchors.push(Anchor::Branch { name });
        }
    }
    Ok(anchors)
}

/// tmux windows attached to the anchor: total and how many are orphaned.
/// Supplied by the runtime inventory; the Git substrate cannot see them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WindowCount {
    pub total: usize,
    pub orphaned: usize,
}

/// Facts the Git substrate cannot see, injected per collection so tests and
/// collectors share one code path.
#[derive(Debug, Default, Clone, Copy)]
pub struct RuntimeFacts {
    pub windows: WindowCount,
    /// Live processes rooted at or below the worktree.
    pub live_pids: usize,
    /// Agent session records whose `(pid, pid_start)` is live.
    pub live_agent_sessions: usize,
    /// Agent sessions ever recorded for the path, live or not.
    pub past_agent_sessions: usize,
}

/// `upstream_state`: whether the branch's configured upstream still exists
/// on the remote. `remote_gone` is only claimed when `ls-remote` proves it -
/// a remote that cannot be reached is `Unknown`, not gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamState {
    /// No `branch.<name>.remote`/`.merge` configured.
    NeverPushed,
    /// Configured, and the remote still advertises the merge ref.
    Tracked { remote: String, merge_ref: String },
    /// Configured, and `ls-remote` proves the remote no longer has it.
    RemoteGone { remote: String, merge_ref: String },
    /// Configured partially or the remote could not be asked.
    Unknown(String),
    /// Detached HEAD: there is no branch to track.
    NotApplicable,
}

/// Whether HEAD's content already lives on the proven base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// Proven not landed (including the no-delta case: nothing to land).
    No,
    /// HEAD is an ancestor of the base ref.
    AncestorMerged,
    /// Every path HEAD changed relative to the merge base is identical on
    /// the base - the squash- or rebase-landed shape `git cherry` cannot see.
    ContentMerged,
}

/// A proven base: the remote's symbolic HEAD branch, corroborated by
/// non-conflicting local and `ls-remote` evidence, with the local
/// remote-tracking ref the comparisons actually run against.
#[derive(Debug, Clone)]
pub struct Base {
    pub remote: String,
    pub branch: String,
    /// `refs/remotes/<remote>/<branch>`.
    pub local_ref: String,
}

impl Base {
    /// `origin/main` style label for reasons and views.
    pub fn label(&self) -> String {
        format!("{}/{}", self.remote, self.branch)
    }
}

/// The eleven shared fields, plus the provenance (`base`, upstream detail) a
/// verdict or view needs to explain them.
#[derive(Debug)]
pub struct WorkState {
    pub repo: Repo,
    pub anchor: Anchor,
    /// URL of the upstream remote, when the branch has one.
    pub remote_url: Option<String>,
    pub base: Evidence<Base>,
    /// The forge work-item overlay: `Unknown(PENDING)` until the forge
    /// stage lands an answer, like every remote-owned field.
    pub forge: ForgeStatus,
    pub broken: Option<String>,
    pub vector: StateVector,
}

#[derive(Debug)]
pub struct StateVector {
    /// The checkout path, or `None` for a branch with no workspace.
    pub worktree: Option<PathBuf>,
    pub windows: WindowCount,
    pub live_pids: usize,
    pub live_agent_sessions: usize,
    pub past_agent_sessions: usize,
    /// Tracked and untracked changes; `Known(false)` for a branch-only row.
    pub dirty: Evidence<bool>,
    pub working_tree: Evidence<Vec<u8>>,
    pub commits_ahead_of_base: Evidence<u64>,
    /// Commits on the proven base the row's tip lacks - the behind half of
    /// the ahead/behind pair.
    pub commits_behind_of_base: Evidence<u64>,
    /// The newest commits on the tip that the proven base lacks, at most
    /// [`COMMIT_LIST_LIMIT`]; `commits_ahead_of_base` says how many exist.
    pub commits_not_on_base: Evidence<Vec<git::LogCommit>>,
    pub upstream_state: UpstreamState,
    /// Commits not reachable from the configured upstream. For a
    /// never-pushed branch every commit past the base is unpushed by
    /// definition; for a detached HEAD it is the commits no ref reaches at
    /// all - the ones removal actually loses.
    pub unpushed_commits: Evidence<u64>,
    pub landed: Evidence<Landed>,
    /// Newest real work the anchor's reflogs record: the worktree HEAD
    /// log's and the branch log's last work entries, at their own
    /// timestamps.
    pub reflog_activity: Option<SystemTime>,
    /// The one reflog entry `reflog_activity` timestamps - its old/new
    /// sha and raw message travel with the moment, never from another
    /// line.
    pub reflog_entry: Option<git::ReflogWork>,
    /// The tip's committerdate, when it provably dates work on this
    /// incarnation (it postdates the ref's creation, or no creation is
    /// proven).
    pub commit_activity: Option<SystemTime>,
    /// Newest of `reflog_activity` and `commit_activity` - the aggregate
    /// readers that do not need the source distinction keep using.
    pub last_git_activity: Option<SystemTime>,
}

/// How long one remote's `ls-remote` answer stands before it is asked
/// again: the remote-evidence row of the freshness table.
pub const REMOTE_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Remote evidence reused across anchors and passes until its deadline.
/// Each `ls-remote` is a network round-trip; a repository with twenty
/// branches would otherwise ask the same remote twenty times for identical
/// facts, and a dashboard refreshing every few seconds would ask it every
/// refresh. Entries are keyed by repository *and* remote name, so one
/// shared cache stays correct across a multi-repo pass: two repos whose
/// remotes share a name advertise different facts. Fresh per call if
/// `collect` is used, shared across a batch with [`collect_cached`].
pub struct RemoteCache {
    deadline: Duration,
    listings: HashMap<(PathBuf, String), (SystemTime, RemoteListing)>,
}

impl Default for RemoteCache {
    fn default() -> Self {
        RemoteCache::with_deadline(REMOTE_DEADLINE)
    }
}

impl RemoteCache {
    /// A cache whose answers expire `deadline` after they were asked.
    pub fn with_deadline(deadline: Duration) -> RemoteCache {
        RemoteCache {
            deadline,
            listings: HashMap::new(),
        }
    }

    /// Whether the stored answer for `remote` still stands - the staged
    /// collector asks a remote again only once this turns false.
    pub fn fresh(&self, repo: &Repo, remote: &str) -> bool {
        let key = (repo.common_dir().to_owned(), remote.to_owned());
        self.listings
            .get(&key)
            .is_some_and(|(asked, _)| still_fresh(*asked, SystemTime::now(), self.deadline))
    }

    /// Store a freshly asked listing: the remote pool's answer enters under
    /// the same deadline a lazy ask would have set.
    pub fn seed(&mut self, repo: &Repo, remote: &str, listing: RemoteListing) {
        let key = (repo.common_dir().to_owned(), remote.to_owned());
        self.listings.insert(key, (SystemTime::now(), listing));
    }

    /// The stored listing, without asking. `None` only when the remote was
    /// never probed - callers that enumerate their asks first never miss.
    pub fn peek(&self, repo: &Repo, remote: &str) -> Option<&RemoteListing> {
        let key = (repo.common_dir().to_owned(), remote.to_owned());
        self.listings.get(&key).map(|(_, listing)| listing)
    }

    /// The remote's listing, asked again once the stored one is older
    /// than the deadline. A failed ask is stored too: `Unknown` until the
    /// next ask, not a retry storm against an unreachable host.
    fn listing(&mut self, repo: &Repo, remote: &str) -> &RemoteListing {
        let key = (repo.common_dir().to_owned(), remote.to_owned());
        let now = SystemTime::now();
        let fresh = self
            .listings
            .get(&key)
            .is_some_and(|(asked, _)| still_fresh(*asked, now, self.deadline));
        if !fresh {
            self.listings
                .insert(key.clone(), (now, repo.remote_listing(remote)));
        }
        &self.listings[&key].1
    }
}

/// Whether an answer asked at `asked` still stands at `now`. Wall-clock
/// time, not `Instant`: on macOS a monotonic clock stops while the machine
/// sleeps, and a dashboard left open overnight must not wake up trusting a
/// pre-sleep answer. A clock that went backwards proves nothing, so the
/// answer is treated as expired.
fn still_fresh(asked: SystemTime, now: SystemTime, deadline: Duration) -> bool {
    now.duration_since(asked)
        .is_ok_and(|elapsed| elapsed < deadline)
}

/// The reason remote-owned fields carry while their stage has not landed:
/// an honest `?`, not a guess at the last value.
pub const PENDING: &str = "collection pending";

/// The remote-independent half of an anchor's state, kept between the
/// local and remote stages so the remote stage recomputes nothing.
struct AnchorLocal {
    /// `branch.<name>.remote`/`.merge` - from the `for-each-ref` batch when
    /// available, the `config` probes otherwise. `None` on a detached head.
    config: Option<Result<UpstreamConfig, git::Error>>,
    /// The upstream remote's URL or a forge-parseable remote name.
    remote_url: Option<String>,
    /// The anchor tip's ref spec (`refs/heads/<name>` or a detached sha);
    /// `None` for an unborn HEAD.
    head: Option<String>,
    /// Batched `upstream:track`; `None` keeps the `@{u}` probe in apply.
    track: Option<Track>,
    /// A detached anchor's unreachable-commit count - already collected,
    /// since it is a local `rev-list`, not remote evidence.
    unreachable: Option<Evidence<u64>>,
    working_tree: Evidence<Vec<u8>>,
    dirty: Evidence<bool>,
    reflog_activity: Option<SystemTime>,
    reflog_entry: Option<git::ReflogWork>,
    commit_activity: Option<SystemTime>,
    /// The branch tip's OID, from the batch or the old-git fallback.
    head_oid: Option<String>,
    /// The newest null-old creation the branch's own reflog proves.
    creation: Option<git::RefCreation>,
    /// The short name a `Branch: renamed` reflog line moved this ref from.
    renamed_from: Option<String>,
}

/// One anchor with its local facts and current vector state.
pub struct AnchorWork {
    /// Local facts live; remote-owned fields stay `Unknown(PENDING)` until
    /// [`apply_remote`].
    pub state: WorkState,
    local: AnchorLocal,
}

impl AnchorWork {
    /// The remote name stage 3 must ask for this anchor: its configured
    /// upstream remote, or the repository's lone remote when the anchor has
    /// none - the same remote [`resolve_base`] would consult.
    fn ask(&self, remotes: &Result<Vec<String>, git::Error>) -> Option<String> {
        match configured_remote(&self.local.config) {
            Some(remote) => Some(remote.to_owned()),
            None => match remotes {
                Ok(remotes) if remotes.len() == 1 => Some(remotes[0].clone()),
                _ => None,
            },
        }
    }

    /// The tip OID the pass proved for the anchor's branch, if any.
    pub fn ref_head(&self) -> Option<&str> {
        self.local.head_oid.as_deref()
    }

    /// The newest null-old creation the branch's reflog proves.
    pub fn ref_creation(&self) -> Option<&git::RefCreation> {
        self.local.creation.as_ref()
    }

    /// The short name a `Branch: renamed` reflog line moved this ref
    /// from - the exact evidence that preserves identity across names.
    pub fn renamed_from(&self) -> Option<&str> {
        self.local.renamed_from.as_deref()
    }

    /// Patch the remote-owned fields from a finished [`apply_remote`].
    pub fn apply(&mut self, applied: RemoteApplied) {
        self.state.base = applied.base;
        self.state.vector.upstream_state = applied.upstream_state;
        self.state.vector.commits_ahead_of_base = applied.commits_ahead;
        self.state.vector.commits_behind_of_base = applied.commits_behind;
        self.state.vector.commits_not_on_base = applied.commits_listed;
        self.state.vector.unpushed_commits = applied.unpushed;
        self.state.vector.landed = applied.landed;
    }
}

/// The remote-owned fields of one anchor, computed by [`apply_remote`].
#[derive(Debug)]
pub struct RemoteApplied {
    pub upstream_state: UpstreamState,
    pub base: Evidence<Base>,
    pub commits_ahead: Evidence<u64>,
    pub commits_behind: Evidence<u64>,
    pub commits_listed: Evidence<Vec<git::LogCommit>>,
    pub unpushed: Evidence<u64>,
    pub landed: Evidence<Landed>,
}

/// One repository's local collection: every anchor and the repo-level
/// evidence the remote stage re-uses.
pub struct RepoLocal {
    /// `git remote`, kept as the result: base resolution fails closed on
    /// the error exactly like the per-anchor path.
    remotes: Result<Vec<String>, git::Error>,
    /// Remote name -> `refs/remotes/<r>/HEAD` target, covering every remote
    /// the anchors can ask for. A missing key is "no such symref".
    local_heads: HashMap<String, Result<Option<String>, String>>,
    /// The anchors with their local facts and pending states.
    pub anchors: Vec<AnchorWork>,
    /// The distinct remote names [`apply_remote`] will consult.
    pub asks: Vec<String>,
}

impl RepoLocal {
    /// The configured remote names, when `git remote` answered; `None` on
    /// the read's own error.
    pub fn remote_names(&self) -> Option<&[String]> {
        self.remotes.as_ref().ok().map(Vec::as_slice)
    }
}

/// Stage 2 for one repository: worktrees, branches and every fact local
/// Git proves, from one `for-each-ref` when the platform's git batches
/// (>= 2.41) and the per-branch probes otherwise. `runtime_of` supplies the
/// agent-visible facts per anchor; errors fail the repository, not its
/// neighbours.
pub fn collect_local_repo(
    repo: &Repo,
    runtime_of: impl Fn(&Anchor) -> RuntimeFacts,
) -> Result<RepoLocal, git::Error> {
    // The batch is best-effort: an older git rejects the atom format and
    // every batched fact falls back to its per-branch probe - except the
    // tip OIDs, which the fallback reads in one `for-each-ref`.
    let facts = repo.ref_facts().ok();
    let tips = facts
        .is_none()
        .then(|| repo.branch_tips().unwrap_or_default()); // coverage: off - needs a git too old for the atoms
    let mut anchors = Vec::new();
    let mut checked_out = std::collections::HashSet::new();
    for wt in repo.worktrees()? {
        if wt.bare {
            continue;
        }
        if let Head::Branch(name) | Head::Unborn(name) = &wt.head {
            checked_out.insert(name.clone());
        }
        anchors.push(Anchor::Worktree {
            path: wt.path,
            admin_id: wt.admin_id,
            head: wt.head,
            locked: wt.locked,
            main: wt.main,
            prunable: wt.prunable,
        });
    }
    match facts.as_ref() {
        // `%(worktreepath)` is the batch's checked-out evidence: a branch
        // checked out nowhere is a branch-only anchor. Probed equivalent
        // to the porcelain HEAD fields, including prunable worktrees.
        Some(facts) => {
            // Refname order, as `for-each-ref` printed them: the work-row
            // order would otherwise shuffle whenever two rows tie.
            let mut names: Vec<&String> = facts.branches.keys().collect();
            names.sort();
            for name in names {
                let fact = &facts.branches[name];
                if fact.worktree.is_none() && !checked_out.contains(name) {
                    anchors.push(Anchor::Branch { name: name.clone() });
                }
            }
        }
        None => unchecked_branch_anchors(repo, &checked_out, &mut anchors)?, // coverage: off - the `?` arm needs a git too old for the atoms
    }

    let works: Vec<AnchorWork> = anchors
        .into_iter()
        .map(|anchor| {
            let runtime = runtime_of(&anchor);
            anchor_work(repo, anchor, facts.as_ref(), tips.as_ref(), runtime)
        })
        .collect();
    let remotes = repo.remotes();
    let asks = remote_asks(&works, &remotes);
    let local_heads = local_heads(repo, &asks, facts.as_ref());
    Ok(RepoLocal {
        remotes,
        local_heads,
        anchors: works,
        asks,
    })
}

/// The branch-only fallback when the batch read failed (`%(worktreepath)`
/// answers "checked out nowhere" and `for-each-ref` predates the atoms).
#[rustfmt::skip]
fn unchecked_branch_anchors(repo: &Repo, checked_out: &std::collections::HashSet<String>, anchors: &mut Vec<Anchor>) -> Result<(), git::Error> { // coverage: off - needs a git too old for the atoms
    for name in repo.local_branches()? { if !checked_out.contains(&name) { anchors.push(Anchor::Branch { name }); } } // coverage: off - same
    Ok(()) // coverage: off - same
} // coverage: off - same

/// The remotes [`apply_remote`] will consult for these anchors: every
/// configured upstream remote, plus the lone remote of a single-remote
/// repository for anchors without one - the set `resolve_base` chooses
/// from, asked once per repo per deadline.
fn remote_asks(works: &[AnchorWork], remotes: &Result<Vec<String>, git::Error>) -> Vec<String> {
    let mut asks = std::collections::BTreeSet::new();
    for work in works {
        if let Some(remote) = work.ask(remotes) {
            asks.insert(remote);
        }
    }
    asks.into_iter().collect()
}

/// `refs/remotes/<r>/HEAD` targets for the asked remotes: the batch's
/// `%(symref)` atoms when present, one `symbolic-ref` per remote otherwise.
fn local_heads(
    repo: &Repo,
    asks: &[String],
    facts: Option<&git::RefFacts>,
) -> HashMap<String, Result<Option<String>, String>> {
    asks.iter()
        .map(|remote| {
            let head = match facts {
                Some(facts) => Ok(facts.remote_heads.get(remote).cloned()),
                None => repo.local_remote_head(remote).map_err(|e| e.to_string()), // coverage: off - needs a git too old for the atoms
            };
            (remote.clone(), head)
        })
        .collect()
}

/// The remote-independent reads for one anchor: upstream config, remote
/// URL, tip spec, workspace facts, and - where the batch supplied it -
/// `upstream:track` and the tip's committerdate.
fn anchor_work(
    repo: &Repo,
    anchor: Anchor,
    facts: Option<&git::RefFacts>,
    tips: Option<&HashMap<String, String>>,
    runtime: RuntimeFacts,
) -> AnchorWork {
    let branch = anchor.branch();
    let fact = branch.and_then(|b| facts.and_then(|f| f.branches.get(b)));
    let config = branch.map(|b| upstream_config(repo, b, fact));
    // The branch's own reflog is the incarnation evidence: the newest
    // null-old entry proves this incarnation's creation, and a
    // `Branch: renamed` line proves continuity through a rename.
    let lifecycle = branch.map(|name| repo.ref_lifecycle(name));
    let head_oid = branch.and_then(|name| {
        fact.and_then(|f| f.head.clone())
            .or_else(|| tips.and_then(|m| m.get(name).cloned()))
    });
    let working_tree = match &anchor {
        Anchor::Worktree {
            path,
            admin_id,
            prunable: Some(_),
            ..
        } => match admin_id {
            Some(id) => repo.admin_status(id, path),
            None => Evidence::Unknown(
                "git status: prunable worktree has no resolvable admin dir".to_owned(),
            ),
        },
        Anchor::Worktree { path, .. } => repo.status(path),
        Anchor::Branch { .. } => Evidence::Unknown("no worktree".to_owned()),
    };
    let (reflog_entry, commit_activity) = git_activity(repo, &anchor, fact);
    let reflog_activity = reflog_entry.as_ref().map(|w| w.at);
    let local = AnchorLocal {
        remote_url: remote_url(repo, &config),
        config,
        head: head_spec(&anchor),
        track: fact.and_then(|f| f.track),
        unreachable: match &anchor {
            // A detached HEAD has no upstream; what removal loses is what
            // no ref reaches - a local `rev-list`, so it is collected now.
            Anchor::Worktree {
                head: Head::Detached(sha),
                ..
            } => Some(match repo.unreachable_commits(sha) {
                Ok(count) => Evidence::Known(count),
                Err(e) => Evidence::Unknown(format!("unreachable count: {e}")),
            }),
            _ => None,
        },
        dirty: match &anchor {
            Anchor::Worktree { .. } => working_tree.clone().map(|bytes| !bytes.is_empty()),
            Anchor::Branch { .. } => Evidence::Known(false),
        },
        working_tree,
        reflog_activity,
        reflog_entry,
        commit_activity,
        head_oid,
        creation: lifecycle.as_ref().and_then(|l| l.creation.clone()),
        renamed_from: lifecycle.and_then(|l| l.renamed_from),
    };
    let state = WorkState {
        repo: repo.clone(),
        anchor: anchor.clone(),
        remote_url: local.remote_url.clone(),
        base: Evidence::Unknown(PENDING.to_owned()),
        forge: ForgeStatus {
            item: WorkItem::Unknown,
            pipeline: Pipeline::Unknown,
            label: None,
            url: None,
            reason: Some(PENDING.to_owned()),
            occurred_at_ms: None,
        },
        broken: match &anchor {
            Anchor::Worktree { prunable, .. } => prunable.clone(),
            Anchor::Branch { .. } => None,
        },
        vector: StateVector {
            worktree: match &anchor {
                Anchor::Worktree { path, .. } => Some(path.clone()),
                Anchor::Branch { .. } => None,
            },
            windows: runtime.windows,
            live_pids: runtime.live_pids,
            live_agent_sessions: runtime.live_agent_sessions,
            past_agent_sessions: runtime.past_agent_sessions,
            dirty: local.dirty.clone(),
            working_tree: local.working_tree.clone(),
            commits_ahead_of_base: Evidence::Unknown(PENDING.to_owned()),
            commits_behind_of_base: Evidence::Unknown(PENDING.to_owned()),
            commits_not_on_base: Evidence::Unknown(PENDING.to_owned()),
            upstream_state: UpstreamState::Unknown(PENDING.to_owned()),
            unpushed_commits: local
                .unreachable
                .clone()
                .unwrap_or_else(|| Evidence::Unknown(PENDING.to_owned())),
            landed: Evidence::Unknown(PENDING.to_owned()),
            reflog_activity: local.reflog_activity,
            reflog_entry: local.reflog_entry.clone(),
            commit_activity: local.commit_activity,
            last_git_activity: [local.reflog_activity, local.commit_activity]
                .into_iter()
                .flatten()
                .max(),
        },
    };
    AnchorWork { state, local } // coverage: off - the unexecuted instantiation's region edge
}

/// The anchor's source-distinguished Git activity, `(reflog, commit)`:
/// real work the worktree HEAD and branch reflogs record, and the tip's
/// committerdate when it provably dates this incarnation. The branch log
/// matters for a worktree added over commits made elsewhere, whose HEAD
/// log holds only the add; the committerdate matters when a branch moved
/// without a reflog write; a probe path keeps reflog only.
fn git_activity(
    repo: &Repo,
    anchor: &Anchor,
    fact: Option<&git::BranchFact>,
) -> (Option<git::ReflogWork>, Option<SystemTime>) {
    // The HEAD reflog's real work only: the add's creation line and a
    // `checkout:` line are lifecycle events, not activity.
    let head = match anchor {
        Anchor::Worktree { admin_id, main, .. } => worktree_head_log(*main, admin_id.as_deref())
            .and_then(|log| repo.reflog_times(&log).newest_work),
        Anchor::Branch { .. } => None,
    };
    let (mut branch, mut committed) = (None, None);
    if let Some(name) = anchor.branch() {
        let times = repo.reflog_times(&PathBuf::from(format!("logs/refs/heads/{name}")));
        branch = times.newest_work;
        committed = fact
            .and_then(|f| f.committer_date)
            .map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
        // A tip commit counts only when it strictly postdates the ref's
        // creation: `git branch feat old-sha` borrows an old commit's
        // date without doing work. When the log proves no creation, the
        // committer date is the fallback it always was.
        committed = committed.filter(|t| times.created_at.is_none_or(|c| *t > c));
    }
    // HEAD and branch entries compete as whole tuples: newest wins, and a
    // same-time tie resolves on `(old, new, message)` so the selected
    // moment and metadata always name the same line.
    ([head, branch].into_iter().flatten().max(), committed)
} // coverage: off - the unexecuted instantiation's exit edge

/// `branch.<name>.remote`/`.merge`: the batch's `upstream:remotename` and
/// `upstream:remoteref` pair when they resolve - exactly the `Full` case -
/// and the config probes otherwise, which is how a partial pair or an
/// unresolvable remote name keeps its distinct fail-closed reading.
fn upstream_config(
    repo: &Repo,
    branch: &str,
    fact: Option<&git::BranchFact>,
) -> Result<UpstreamConfig, git::Error> {
    match fact.and_then(|f| f.upstream.clone()) {
        Some((remote, merge)) => Ok(UpstreamConfig::Full { remote, merge }),
        None => repo.upstream_config(branch),
    }
} // coverage: off - the unexecuted instantiation's exit edge

/// The configured remote names itself even when it cannot be reached, so it
/// still feeds base resolution and forge routing. The URL is the remote's
/// configured `remote.<name>.url`, or the name itself when it parses as a
/// forge remote (`branch.<name>.remote` may carry the URL directly).
fn remote_url(repo: &Repo, config: &Option<Result<UpstreamConfig, git::Error>>) -> Option<String> {
    let remote = configured_remote(config)?;
    repo.remote_url(remote)
        .ok()
        .flatten()
        .or_else(|| forge::parse_remote(remote).map(|_| remote.to_owned()))
}

/// The remote a `Full` upstream names; any other shape has none.
fn configured_remote(config: &Option<Result<UpstreamConfig, git::Error>>) -> Option<&str> {
    match config {
        Some(Ok(UpstreamConfig::Full { remote, .. })) => Some(remote.as_str()),
        _ => None,
    }
}

/// A ref spec for the anchor's tip that resolves from the common dir:
/// `HEAD` alone would name the main worktree's HEAD.
fn head_spec(anchor: &Anchor) -> Option<String> {
    match anchor {
        Anchor::Branch { name } => Some(format!("refs/heads/{name}")),
        Anchor::Worktree { head, .. } => match head {
            Head::Branch(name) => Some(format!("refs/heads/{name}")),
            Head::Detached(sha) => Some(sha.clone()),
            Head::Unborn(_) => None,
        },
    }
}

/// Stage 3 for one repository: the remote-owned fields of every anchor.
/// `listing_of(repo, remote)` answers the remote's advertised listing - the
/// collector's pre-fetched map in the staged path, a lazy [`RemoteCache`]
/// in the direct one - and `ahead-behind` runs once per distinct proven
/// base rather than per branch.
pub fn apply_remote(
    repo: &Repo,
    local: &RepoLocal,
    mut listing_of: impl FnMut(&Repo, &str) -> RemoteListing,
) -> Vec<RemoteApplied> {
    // First the upstream state and the base, so the ahead-behind batches
    // group anchors by their proven base ref.
    let resolved: Vec<(UpstreamState, Evidence<Base>)> = local
        .anchors
        .iter()
        .map(|work| {
            let upstream = upstream_state_of(&work.local.config, |remote| {
                remote_refs(&listing_of(repo, remote))
            });
            let base = resolve_base(
                repo,
                configured_remote(&work.local.config),
                &local.remotes,
                &local.local_heads,
                |remote| listing_of(repo, remote).head.clone(),
            );
            (upstream, base)
        })
        .collect();

    // One `%(ahead-behind:<ref>)` per distinct proven base ref: the ahead
    // count is `rev-list --count <ref>..<branch>` and `ahead == 0` is the
    // ancestor test `landed` opens with. A failed or missing batch entry
    // keeps the per-branch probes.
    let mut batches: HashMap<String, HashMap<String, (u64, u64)>> = HashMap::new();
    for (_, base) in &resolved {
        if let Evidence::Known(base) = base {
            batches
                .entry(base.local_ref.clone())
                .or_insert_with(|| repo.ahead_behind(&base.local_ref).unwrap_or_default());
        }
    }

    local
        .anchors
        .iter()
        .zip(resolved)
        .map(|(work, (upstream, base))| {
            let (commits_ahead, commits_behind, commits_listed, landed, unpushed) = match &work.local.head {
                None /* // coverage: off - the unborn arm's second region is an unexecuted-instantiation edge */ => (
                    Evidence::Unknown("unborn HEAD".to_owned()),
                    Evidence::Unknown("unborn HEAD".to_owned()),
                    Evidence::Unknown("unborn HEAD".to_owned()),
                    Evidence::Unknown("unborn HEAD".to_owned()),
                    Evidence::Unknown("unborn HEAD".to_owned()),
                ),
                Some(head) => {
                    let counts = base.known().and_then(|b| {
                        head.strip_prefix("refs/heads/").and_then(|name| {
                            batches.get(&b.local_ref)?.get(name).copied() // coverage: off - every proven base was batched above
                        })
                    });
                    let commits = commits_ahead(repo, head, &base, counts.map(|(a, _)| a));
                    let behind = commits_behind(repo, head, &base, counts.map(|(_, b)| b));
                    let listed = commits_listed(repo, head, &base, &commits);
                    let landed = landed(repo, head, &base, counts.map(|(a, _)| a == 0));
                    let unpushed = match &work.local.unreachable {
                        // A detached HEAD has no upstream; the unreachable
                        // count collected in stage 2 is what removal loses.
                        Some(unreachable) => unreachable.clone(),
                        None => unpushed_commits(
                            repo,
                            work.state.anchor.branch(),
                            work.local.track,
                            &upstream,
                            &commits,
                        ),
                    };
                    (commits, behind, listed, landed, unpushed)
                }
            };
            RemoteApplied {
                upstream_state: upstream,
                base,
                commits_ahead,
                commits_behind,
                commits_listed,
                unpushed,
                landed,
            }
        })
        .collect()
}

/// `$GIT_COMMON_DIR/logs/HEAD` for the main worktree,
/// `$GIT_COMMON_DIR/worktrees/<id>/logs/HEAD` for a linked one: the reflog
/// is per worktree. A linked worktree whose admin id cannot be resolved
/// gets `None` - its log is somewhere unknowable, and reading the main
/// worktree's reflog instead would misattribute activity.
fn worktree_head_log(main: bool, admin_id: Option<&str>) -> Option<PathBuf> {
    if main {
        return Some(PathBuf::from("logs/HEAD"));
    }
    admin_id.map(|id| PathBuf::from(format!("worktrees/{id}/logs/HEAD")))
}

/// A read-only `ls-remote` decides whether the remote still advertises the
/// configured merge ref. An unreachable remote is `Unknown`, not `remote_gone`:
/// gone is only claimed when the remote answered and did not have the ref.
fn upstream_state_of(
    config: &Option<Result<UpstreamConfig, git::Error>>,
    mut refs_of: impl FnMut(&str) -> Evidence<std::sync::Arc<Vec<String>>>,
) -> UpstreamState {
    let Some(config) = config else {
        return UpstreamState::NotApplicable;
    };
    match config {
        Err(e) => UpstreamState::Unknown(format!("upstream config: {e}")),
        Ok(UpstreamConfig::None) => UpstreamState::NeverPushed,
        Ok(UpstreamConfig::Partial) => {
            UpstreamState::Unknown("incomplete upstream config".to_owned())
        }
        Ok(UpstreamConfig::Full { remote, merge }) => {
            match refs_of(remote).map(|refs| refs.iter().any(|r| r == merge)) {
                Evidence::Known(true) => UpstreamState::Tracked {
                    remote: remote.clone(),
                    merge_ref: merge.clone(),
                },
                Evidence::Known(false) => UpstreamState::RemoteGone {
                    remote: remote.clone(),
                    merge_ref: merge.clone(),
                },
                Evidence::Unknown(reason) => UpstreamState::Unknown(reason),
            }
        }
    }
}

/// The remote's advertised ref listing: `refs` is an `Arc` share, so
/// repeated membership checks are refcount bumps, not copies.
fn remote_refs(listing: &RemoteListing) -> Evidence<std::sync::Arc<Vec<String>>> {
    listing.refs.clone()
}

/// The base branch: the symbolic HEAD of the upstream remote, or of the
/// repository's only remote when no upstream is configured (a single remote
/// is a derivation, not a guess; zero or several remotes prove nothing).
/// `ls-remote --symref` is authoritative; a local `refs/remotes/<r>/HEAD`
/// symref may corroborate it or stand in when the remote is unreachable, but
/// the two disagreeing is a conflict, and a conflict is `Unknown`.
fn resolve_base(
    repo: &Repo,
    upstream_remote: Option<&str>,
    remotes: &Result<Vec<String>, git::Error>,
    local_heads: &HashMap<String, Result<Option<String>, String>>,
    mut head_of: impl FnMut(&str) -> RemoteHead,
) -> Evidence<Base> {
    let remote = match upstream_remote {
        Some(remote) => remote.to_owned(),
        None => match remotes {
            Ok(remotes) if remotes.len() == 1 => remotes[0].clone(),
            Ok(remotes) => {
                return Evidence::Unknown(format!(
                    "no upstream remote and {} remotes configured",
                    remotes.len()
                ));
            }
            Err(e) => return Evidence::Unknown(format!("remote list: {e}")),
        },
    };

    // A `.` remote is the repository itself and has no remote-tracking
    // symref to consult. An unprobed remote name reads as no symref, which
    // is what `symbolic-ref` reports for it too.
    let local = if remote == "." {
        None
    } else {
        match local_heads.get(&remote) {
            Some(Err(e)) => return Evidence::Unknown(format!("local remote HEAD: {e}")),
            entry => entry.and_then(|r| r.clone().ok().flatten()),
        }
    };
    let branch = match head_of(&remote) {
        RemoteHead::Advertised(advertised) => match &local {
            Some(local) if *local != advertised => {
                return Evidence::Unknown(format!(
                    "conflicting remote HEAD: local refs/remotes/{remote}/HEAD is \
                     {local}, the remote advertises {advertised}"
                ));
            }
            _ => advertised,
        },
        RemoteHead::NotAdvertised => {
            return Evidence::Unknown(format!("remote {remote} advertises no HEAD"));
        }
        // The remote could not be asked; the local symref is the recorded
        // evidence and stands alone rather than conflicting.
        RemoteHead::Unreachable(_) => match local {
            Some(local) => local,
            None => {
                return Evidence::Unknown(format!(
                    "remote {remote} unreachable and no local remote HEAD"
                ));
            }
        },
    };

    // A `.` remote is the repository itself, so its "tracking ref" is the
    // advertised branch under refs/heads, not a remote-tracking ref.
    let local_ref = if remote == "." {
        format!("refs/heads/{branch}")
    } else {
        format!("refs/remotes/{remote}/{branch}")
    };
    if repo.has_ref(&local_ref) {
        Evidence::Known(Base {
            remote,
            branch,
            local_ref,
        })
    } else {
        Evidence::Unknown(format!("base ref {local_ref} is not fetched locally"))
    }
}

/// `rev-list --count <base>..<head>`, or the batch's ahead count when the
/// branch was in one - same number, no spawn.
fn commits_ahead(
    repo: &Repo,
    head: &str,
    base: &Evidence<Base>,
    ahead: Option<u64>,
) -> Evidence<u64> {
    match base {
        Evidence::Unknown(reason) => Evidence::Unknown(format!("no proven base ({reason})")),
        Evidence::Known(base) => match ahead {
            Some(count) => Evidence::Known(count),
            None => match repo.rev_list_count(&base.local_ref, head) {
                Ok(count) => Evidence::Known(count),
                Err(e) => Evidence::Unknown(format!("rev-list: {e}")),
            },
        },
    }
}

/// How many commits past the base a row lists: the detail view's
/// recent history, not an archive - `commits_ahead_of_base` counts the
/// rest.
pub const COMMIT_LIST_LIMIT: usize = 20;

/// `log <base>..<head>`, at most [`COMMIT_LIST_LIMIT`] - skipped when the
/// ahead count already proved there is nothing to list.
fn commits_listed(
    repo: &Repo,
    head: &str,
    base: &Evidence<Base>,
    ahead: &Evidence<u64>,
) -> Evidence<Vec<git::LogCommit>> {
    match (base, ahead) {
        (Evidence::Unknown(reason), _) => Evidence::Unknown(format!("no proven base ({reason})")),
        (_, Evidence::Known(0)) => Evidence::Known(Vec::new()),
        (Evidence::Known(base), _) => {
            match repo.log_range(&base.local_ref, head, COMMIT_LIST_LIMIT) {
                Ok(commits) => Evidence::Known(commits),
                Err(e) => Evidence::Unknown(format!("log: {e}")),
            }
        }
    }
}

/// `rev-list --count <head>..<base>` - the commits the base has that the
/// tip lacks - or the batch's behind count when the branch was in one.
fn commits_behind(
    repo: &Repo,
    head: &str,
    base: &Evidence<Base>,
    behind: Option<u64>,
) -> Evidence<u64> {
    match base {
        Evidence::Unknown(reason) => Evidence::Unknown(format!("no proven base ({reason})")),
        Evidence::Known(base) => match behind {
            Some(count) => Evidence::Known(count),
            None => match repo.rev_list_count(head, &base.local_ref) {
                Ok(count) => Evidence::Known(count),
                Err(e) => Evidence::Unknown(format!("rev-list: {e}")),
            },
        },
    }
}

/// Ancestry first; when HEAD is no ancestor, the squash/rebase shape is
/// checked by requiring every path HEAD changed relative to the merge base
/// to be identical on the base. No delta at all is not landed. `ancestor`
/// carries the batch's `ahead == 0` verdict when one exists - exactly
/// `merge-base --is-ancestor` - and `None` keeps the probe.
fn landed(
    repo: &Repo,
    head: &str,
    base: &Evidence<Base>,
    ancestor: Option<bool>,
) -> Evidence<Landed> {
    let Evidence::Known(base) = base else {
        return Evidence::Unknown(format!(
            "no proven base ({})",
            base.reason().unwrap_or_default()
        ));
    };
    let ancestor = match ancestor {
        Some(ancestor) => Evidence::Known(ancestor),
        None => repo.is_ancestor(head, &base.local_ref),
    };
    match ancestor {
        Evidence::Known(true) => return Evidence::Known(Landed::AncestorMerged),
        Evidence::Unknown(reason) => return Evidence::Unknown(reason),
        Evidence::Known(false) => {}
    }
    let merge_base = match repo.merge_base(head, &base.local_ref) {
        Ok(Some(sha)) => sha,
        // Unrelated histories cannot have landed.
        Ok(None) => return Evidence::Known(Landed::No),
        Err(e) => return Evidence::Unknown(format!("merge-base: {e}")), // coverage: off - needs merge-base to fail where rev-list succeeded
    };
    let paths = match repo.changed_paths(&merge_base, head) {
        Ok(paths) => paths,
        Err(e) => return Evidence::Unknown(format!("diff --name-only: {e}")), // coverage: off - needs diff to fail where merge-base succeeded
    };
    if paths.is_empty() {
        return Evidence::Known(Landed::No);
    }
    repo.paths_match(head, &base.local_ref, &paths).map(|same| {
        if same {
            Landed::ContentMerged
        } else {
            Landed::No
        }
    })
}

/// Commits the configured upstream does not have. A branch that was never
/// pushed - or a detached HEAD - has no upstream at all, so every commit
/// past the base is unpushed by definition. The batch's `upstream:track`
/// ahead count answers it directly when present; `@{u}` resolves through
/// the refspec otherwise, which is why it handles a merge ref naming a
/// different remote branch and fails closed when the tracking ref is gone.
fn unpushed_commits(
    repo: &Repo,
    branch: Option<&str>,
    track: Option<Track>,
    upstream: &UpstreamState,
    commits_ahead: &Evidence<u64>,
) -> Evidence<u64> {
    match (upstream, branch) {
        (UpstreamState::NeverPushed | UpstreamState::NotApplicable, _) => commits_ahead.clone(),
        (_, Some(branch)) => match track {
            Some(Track::Counts { ahead, .. }) => Evidence::Known(ahead),
            _ => {
                let upstream_ref = format!("{branch}@{{u}}");
                match repo.rev_list_count(&upstream_ref, &format!("refs/heads/{branch}")) {
                    Ok(count) => Evidence::Known(count),
                    Err(e) => Evidence::Unknown(format!("unpushed count via @{{u}}: {e}")),
                }
            }
        },
        _ => Evidence::Unknown("no branch for an upstream".to_owned()), // coverage: off - a tracked upstream implies a branch
    }
}

/// Collect the vector for one anchor. Reads only; all runtime fields come
/// from `runtime`.
pub fn collect(repo: &Repo, anchor: &Anchor, runtime: RuntimeFacts) -> WorkState {
    collect_cached(repo, &mut RemoteCache::default(), anchor, runtime)
}

/// [`collect`] with a caller-owned [`RemoteCache`], so a batch over a
/// repository's anchors asks the network once per remote, not once per row.
/// This is the per-branch path the batched collection must agree with:
/// every remote answer still comes lazily through `cache`.
pub fn collect_cached(
    repo: &Repo,
    cache: &mut RemoteCache,
    anchor: &Anchor,
    runtime: RuntimeFacts,
) -> WorkState {
    let remotes = repo.remotes();
    let mut local = probe_repo_local(repo, anchor.clone(), runtime, remotes);
    let applied = apply_remote(repo, &local, |r, name| cache.listing(r, name).clone());
    let mut work = local
        .anchors
        .pop()
        .expect("probe_repo_local built one anchor"); // coverage: off - it always builds exactly one
    let applied = applied
        .into_iter()
        .next()
        .expect("apply_remote returns one entry per anchor"); // coverage: off - same
    work.apply(applied);
    work.state
}

/// The single-anchor [`RepoLocal`] of the per-branch path: config, remote
/// name and workspace facts from the probes the batch replaces, asks and
/// local HEAD symrefs probed exactly as `resolve_base` would consult them.
fn probe_repo_local(
    repo: &Repo,
    anchor: Anchor,
    runtime: RuntimeFacts,
    remotes: Result<Vec<String>, git::Error>,
) -> RepoLocal {
    let tips = repo.branch_tips().unwrap_or_default();
    let work = anchor_work(repo, anchor, None, Some(&tips), runtime);
    let asks = remote_asks(std::slice::from_ref(&work), &remotes);
    let local_heads = local_heads(repo, &asks, None);
    RepoLocal {
        remotes,
        local_heads,
        anchors: vec![work],
        asks,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_remote_answer_expires_by_wall_clock() {
        let asked = std::time::UNIX_EPOCH + super::Duration::from_secs(1_000);
        let deadline = super::REMOTE_DEADLINE;
        assert!(super::still_fresh(asked, asked, deadline));
        assert!(super::still_fresh(asked, asked + deadline / 2, deadline));
        assert!(!super::still_fresh(asked, asked + deadline, deadline));
        // Hours of sleep count: the wall clock kept moving.
        let overnight = super::Duration::from_secs(8 * 3600);
        assert!(!super::still_fresh(asked, asked + overnight, deadline));
        // A clock set backwards is no proof of freshness.
        let earlier = asked - super::Duration::from_secs(1);
        assert!(!super::still_fresh(asked, earlier, deadline));
    }

    use super::*;

    fn is_unknown(upstream: &UpstreamState) -> bool {
        matches!(upstream, UpstreamState::Unknown(_))
    }

    /// A `Repo` whose common dir is a plain file: every read errors, which is
    /// the only shape collect() cannot derive a fact from - it must collect
    /// `Unknown`s, not crash.
    fn broken_repo() -> Repo {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        // A unique dir per call: two broken-repo tests share a process, and
        // a shared path turns fixture writes into a race.
        let dir = std::env::temp_dir().join(format!(
            "agent-sessions-broken-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("not-a-repo");
        std::fs::write(&file, "x").unwrap();
        Repo { common_dir: file }
    }

    #[test]
    fn a_broken_repo_collects_unknowns_not_panics() {
        let repo = broken_repo();
        let state = collect(
            &repo,
            &Anchor::Branch {
                name: "x".to_owned(),
            },
            RuntimeFacts::default(),
        );
        assert!(is_unknown(&state.vector.upstream_state));
        assert!(!is_unknown(&UpstreamState::NeverPushed));
        assert!(!state.base.is_known());
        assert!(!state.vector.commits_ahead_of_base.is_known());
        assert!(!state.vector.landed.is_known());
        assert!(!state.vector.unpushed_commits.is_known());
        assert!(anchors(&repo).is_err());
    }

    #[test]
    fn a_prunable_anchor_with_no_admin_dir_fails_closed_on_dirty() {
        let repo = broken_repo();
        let state = collect(
            &repo,
            &Anchor::Worktree {
                path: PathBuf::from("/nonexistent"),
                admin_id: None,
                head: Head::Detached("deadbeef".to_owned()),
                locked: false,
                main: false,
                prunable: Some("gitdir file points to non-existent location".to_owned()),
            },
            RuntimeFacts::default(),
        );
        assert_eq!(
            state.broken.as_deref(),
            Some("gitdir file points to non-existent location")
        );
        assert!(matches!(
            &state.vector.dirty,
            Evidence::Unknown(r) if r.contains("no resolvable admin dir")
        ));
    }

    #[test]
    fn a_detached_anchor_on_a_broken_repo_collects_unknowns() {
        // The unreachable-commit count needs no upstream and no base, but
        // it still needs git to answer - on a broken repo it is Unknown,
        // and the verdict's detached arm blocks on that unknown.
        let repo = broken_repo();
        let state = collect(
            &repo,
            &Anchor::Worktree {
                path: PathBuf::from("/nonexistent"),
                admin_id: Some("x".to_owned()),
                head: Head::Detached("deadbeef".to_owned()),
                locked: false,
                main: false,
                prunable: None,
            },
            RuntimeFacts::default(),
        );
        assert!(!state.vector.unpushed_commits.is_known());
        let forge = crate::forge::ForgeStatus {
            item: crate::forge::WorkItem::Unknown,
            pipeline: crate::forge::Pipeline::Unknown,
            label: None,
            url: None,
            reason: None,
            occurred_at_ms: None,
        };
        let (removal, _) = crate::verdict::cleanup(&state, &forge);
        assert!(matches!(removal.verdict, crate::verdict::Verdict::Blocked)); // coverage: off - miss edge is the assert failing
        assert!(
            removal
                .reasons
                .iter()
                .any(|r| r.contains("cannot prove commits are reachable from a ref")),
            "{:?}",
            removal.reasons
        );
    }

    #[test]
    fn an_unresolvable_admin_id_reads_no_reflog() {
        assert_eq!(
            worktree_head_log(true, None),
            Some(PathBuf::from("logs/HEAD"))
        );
        assert_eq!(
            worktree_head_log(false, Some("wt1")),
            Some(PathBuf::from("worktrees/wt1/logs/HEAD"))
        );
        assert_eq!(worktree_head_log(false, None), None);
    }

    /// A `Repo` over a temporary common dir, removed on drop so a
    /// panicking test cannot leave its fixture behind.
    struct TempRepo(Repo);

    impl std::ops::Deref for TempRepo {
        type Target = Repo;

        fn deref(&self) -> &Repo {
            &self.0
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.common_dir);
        }
    }

    /// A `Repo` over a fabricated common dir whose reflog files alone
    /// exist: `logs` maps each relative log path to its contents.
    fn reflog_repo(logs: &[(&str, &str)]) -> TempRepo {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agent-sessions-reflog-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Wrapped before the fixture writes: a panic mid-write still
        // drops the dir.
        let repo = TempRepo(Repo { common_dir: dir });
        for (rel, body) in logs {
            let path = repo.common_dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        repo
    }

    #[test]
    fn a_reflog_repo_drops_its_common_dir() {
        let dir = {
            let repo = reflog_repo(&[("logs/HEAD", "aaaa bbbb A <a@b> 1 +0000\tcommit: x\n")]);
            let dir = repo.common_dir().to_owned();
            assert!(dir.exists(), "the fixture is live inside the scope");
            dir
        };
        assert!(!dir.exists(), "the drop removed {dir:?}");
    }

    /// A worktree anchor over `head`, `main` marking the repository's own
    /// checkout and `admin_id` naming the linked one's admin dir.
    fn worktree(head: Head, main: bool, admin_id: Option<&str>) -> Anchor {
        Anchor::Worktree {
            path: PathBuf::from("/wt"),
            admin_id: admin_id.map(str::to_owned),
            head,
            locked: false,
            main,
            prunable: None,
        }
    }

    #[test]
    fn reflog_activity_selects_the_newest_entry_across_head_and_branch() {
        // The branch log's newer entry wins as a whole tuple: its own
        // time, shas and message, not a mix of lines.
        let repo = reflog_repo(&[
            (
                "logs/HEAD",
                "aaaa bbbb A <a@b> 1700001000 +0200\tcommit: head work\n",
            ),
            (
                "logs/refs/heads/feat",
                "cccc dddd A <a@b> 1700002000 +0200\tcommit: branch work\n",
            ),
        ]);
        let anchor = worktree(Head::Branch("feat".to_owned()), true, None);
        let (entry, _) = git_activity(&repo, &anchor, None);
        let entry = entry.expect("the branch entry is newer");
        assert_eq!(entry.at, UNIX_EPOCH + Duration::from_secs(1_700_002_000));
        assert_eq!(
            (entry.old_sha.as_str(), entry.new_sha.as_str()),
            ("cccc", "dddd")
        );
        assert_eq!(entry.message, "commit: branch work");

        // Reversed: the HEAD log's newer entry wins the same way.
        let repo = reflog_repo(&[
            (
                "logs/HEAD",
                "aaaa bbbb A <a@b> 1700002000 +0200\tcommit: head work\n",
            ),
            (
                "logs/refs/heads/feat",
                "cccc dddd A <a@b> 1700001000 +0200\tcommit: branch work\n",
            ),
        ]);
        let (entry, _) = git_activity(&repo, &anchor, None);
        let entry = entry.expect("the head entry is newer");
        assert_eq!(entry.message, "commit: head work");
        assert_eq!(entry.new_sha, "bbbb");
    }

    #[test]
    fn reflog_activity_breaks_same_time_ties_on_the_whole_tuple() {
        let anchor = worktree(Head::Branch("feat".to_owned()), true, None);
        // Same epoch in both logs: `(old, new, message)` decides, one
        // discriminator at a time.
        for (head_shas, branch_shas, want) in [
            ("aaaa bbbb", "cccc dddd", "dddd"), // old decides
            ("cccc dddd", "aaaa bbbb", "dddd"), // the head log can win too
            ("aaaa bbbb", "aaaa cccc", "cccc"), // new decides
        ] {
            let repo = reflog_repo(&[
                (
                    "logs/HEAD",
                    &format!("{head_shas} A <a@b> 1700001000 +0200\tcommit: same\n"),
                ),
                (
                    "logs/refs/heads/feat",
                    &format!("{branch_shas} A <a@b> 1700001000 +0200\tcommit: same\n"),
                ),
            ]);
            let (entry, _) = git_activity(&repo, &anchor, None);
            assert_eq!(
                entry.expect("an entry").new_sha,
                want,
                "head {head_shas} vs branch {branch_shas}"
            );
        }
        // Equal shas: the greater message decides.
        let repo = reflog_repo(&[
            (
                "logs/HEAD",
                "aaaa bbbb A <a@b> 1700001000 +0200\tcommit: z\n",
            ),
            (
                "logs/refs/heads/feat",
                "aaaa bbbb A <a@b> 1700001000 +0200\tcommit: a\n",
            ),
        ]);
        let (entry, _) = git_activity(&repo, &anchor, None);
        assert_eq!(entry.expect("an entry").message, "commit: z");
        // Identical tuples in both logs collapse to that one entry.
        let body = "aaaa bbbb A <a@b> 1700001000 +0200\tcommit: same\n";
        let repo = reflog_repo(&[("logs/HEAD", body), ("logs/refs/heads/feat", body)]);
        let (entry, _) = git_activity(&repo, &anchor, None);
        let entry = entry.expect("an entry");
        assert_eq!(entry.new_sha, "bbbb");
        assert_eq!(entry.message, "commit: same");
    }

    #[test]
    fn reflog_activity_reads_each_anchor_shape_own_log() {
        // A branch-only anchor has no HEAD log to read: the branch's own
        // log is the whole story.
        let repo = reflog_repo(&[
            (
                "logs/HEAD",
                "aaaa bbbb A <a@b> 1700002000 +0200\tcommit: head work\n",
            ),
            (
                "logs/refs/heads/feat",
                "cccc dddd A <a@b> 1700001000 +0200\tcommit: branch work\n",
            ),
        ]);
        let anchor = Anchor::Branch {
            name: "feat".to_owned(),
        };
        let (entry, _) = git_activity(&repo, &anchor, None);
        let entry = entry.expect("the branch entry");
        assert_eq!(entry.message, "commit: branch work");
        assert_eq!(entry.at, UNIX_EPOCH + Duration::from_secs(1_700_001_000));

        // A detached worktree reads its HEAD log and no branch's.
        let anchor = worktree(Head::Detached("deadbeef".to_owned()), true, None);
        let (entry, _) = git_activity(&repo, &anchor, None);
        let entry = entry.expect("the head entry");
        assert_eq!(entry.message, "commit: head work");

        // A linked worktree reads `worktrees/<id>/logs/HEAD`: the main
        // worktree's newer entry must not be attributed to it.
        let repo = reflog_repo(&[
            (
                "logs/HEAD",
                "aaaa bbbb A <a@b> 1700002000 +0200\tcommit: main work\n",
            ),
            (
                "worktrees/wt9/logs/HEAD",
                "cccc dddd A <a@b> 1700001000 +0200\tcommit: linked work\n",
            ),
        ]);
        let anchor = worktree(Head::Detached("deadbeef".to_owned()), false, Some("wt9"));
        let (entry, _) = git_activity(&repo, &anchor, None);
        let entry = entry.expect("the linked worktree's own entry");
        assert_eq!(entry.message, "commit: linked work");
        assert_eq!(entry.at, UNIX_EPOCH + Duration::from_secs(1_700_001_000));

        // A linked worktree whose admin id never resolved reads no HEAD
        // log at all.
        let anchor = worktree(Head::Detached("deadbeef".to_owned()), false, None);
        let (entry, _) = git_activity(&repo, &anchor, None);
        assert!(entry.is_none(), "{entry:?}");
    }

    #[test]
    fn commit_activity_still_filters_to_post_creation_tip_dates() {
        let repo = reflog_repo(&[(
            "logs/refs/heads/feat",
            "0000 aaaa A <a@b> 1700001000 +0200\tbranch: Created from main\n",
        )]);
        let anchor = Anchor::Branch {
            name: "feat".to_owned(),
        };
        let fact = |date: u64| git::BranchFact {
            upstream: None,
            track: None,
            committer_date: Some(date),
            worktree: None,
            head: None,
        };
        // A tip dated after the proven creation counts; one dated at or
        // before it is the borrowed date of `git branch feat old-sha`.
        let (_, committed) = git_activity(&repo, &anchor, Some(&fact(1_700_002_000)));
        assert_eq!(
            committed,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_002_000))
        );
        let (_, committed) = git_activity(&repo, &anchor, Some(&fact(1_700_001_000)));
        assert_eq!(committed, None);
        // No reflog at all: the committerdate stays the fallback it was.
        let repo = reflog_repo(&[]);
        let (_, committed) = git_activity(&repo, &anchor, Some(&fact(1_700_001_000)));
        assert_eq!(
            committed,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_001_000))
        );
    }

    #[test]
    fn reflog_entry_reaches_the_vector_with_its_own_time() {
        // Through collect(): the selected entry and `reflog_activity`
        // agree, and `last_git_activity` stays the newest of the sources.
        let repo = reflog_repo(&[(
            "logs/refs/heads/feat",
            "aaaa bbbb A <a@b> 1700001000 +0200\tcommit: real work\n",
        )]);
        let state = collect(
            &repo,
            &Anchor::Branch {
                name: "feat".to_owned(),
            },
            RuntimeFacts::default(),
        );
        let entry = state.vector.reflog_entry.as_ref().expect("the entry");
        assert_eq!(entry.message, "commit: real work");
        assert_eq!(entry.new_sha, "bbbb");
        assert_eq!(state.vector.reflog_activity, Some(entry.at));
        assert_eq!(state.vector.last_git_activity, Some(entry.at));
    }

    #[test]
    fn a_bare_repo_lists_no_checkouts() {
        // A bare repository's porcelain entry is skipped: it is storage,
        // not a workspace.
        let dir = std::env::temp_dir().join(format!("agent-sessions-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            git::git_command(&[], &["init", "--bare", dir.to_str().unwrap()])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap()
                .status
                .success()
        );
        let repo = Repo {
            common_dir: dir.clone(),
        };
        assert!(anchors(&repo).unwrap().is_empty());
        // The staged local read agrees: no checkouts, no asks, nothing for
        // the remote stage to apply.
        let local = collect_local_repo(&repo, |_| RuntimeFacts::default()).unwrap(); // coverage: off - the panic edge is a failed assertion
        assert!(local.anchors.is_empty() && local.asks.is_empty());
        let applied = apply_remote(&repo, &local, |_, _| panic!("no anchors")); // coverage: off - proves the listing callback never runs
        assert!(applied.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_repo_fails_local_collection_cleanly() {
        // A gitdir that is a plain file: `worktree list` fails, and the
        // error propagates rather than producing invented anchors.
        let dir =
            std::env::temp_dir().join(format!("agent-sessions-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("not-a-repo");
        std::fs::write(&file, "x").unwrap();
        let repo = Repo { common_dir: file };
        let result = collect_local_repo(&repo, |_| RuntimeFacts::default()); // coverage: off - the unexecuted instantiation's region edge
        let err = match result {
            Ok(_) => panic!("a broken repo cannot collect"), // coverage: off - the panic edge is a failed assertion
            Err(e) => e,
        };
        assert!(format!("{err}").contains("worktree"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
