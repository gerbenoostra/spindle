//! The shared worktree state vector: the eleven derived facts that both the
//! section classification and the cleanup verdicts read.
//!
//! Runtime fields (windows, live processes, agent sessions) are injected by
//! the caller; everything else is read from Git, read-only, in this module.
//! Upstream and base evidence fail closed: a remote or branch name is never
//! guessed, so an unproven base leaves `landed` and `commits_ahead_of_base`
//! `Unknown` rather than compared against `origin/main` by convention.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::evidence::Evidence;
use crate::git::{self, Head, RemoteHead, Repo, UpstreamConfig};

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
    pub commits_ahead_of_base: Evidence<u64>,
    pub upstream_state: UpstreamState,
    /// Commits not reachable from the configured upstream. For a
    /// never-pushed branch or detached HEAD every commit past the base is
    /// unpushed by definition.
    pub unpushed_commits: Evidence<u64>,
    pub landed: Evidence<Landed>,
    /// Newest of the worktree HEAD (or branch) reflog's last entry and its
    /// mtime.
    pub last_git_activity: Option<SystemTime>,
}

/// Collect the vector for one anchor. Reads only; all runtime fields come
/// from `runtime`.
pub fn collect(repo: &Repo, anchor: &Anchor, runtime: RuntimeFacts) -> WorkState {
    let config = anchor.branch().map(|branch| repo.upstream_config(branch));
    let upstream = match &config {
        None => UpstreamState::NotApplicable,
        Some(Ok(config)) => upstream_state(repo, config),
        Some(Err(e)) => UpstreamState::Unknown(format!("upstream config: {e}")),
    };
    // The configured remote names itself even when it cannot be reached, so
    // it still feeds base resolution and forge routing.
    let configured_remote = match config {
        Some(Ok(UpstreamConfig::Full { remote, .. })) => Some(remote),
        _ => None,
    };
    let remote_url = configured_remote
        .as_deref()
        .and_then(|remote| repo.remote_url(remote).ok().flatten());
    let base = resolve_base(repo, configured_remote.as_deref());

    // A ref spec for the anchor's tip that resolves from the common dir:
    // `HEAD` alone would name the main worktree's HEAD.
    let head = match anchor {
        Anchor::Branch { name } => Some(format!("refs/heads/{name}")),
        Anchor::Worktree { head, .. } => match head {
            Head::Branch(name) => Some(format!("refs/heads/{name}")),
            Head::Detached(sha) => Some(sha.clone()),
            Head::Unborn(_) => None,
        },
    };

    let (landed, commits_ahead, unpushed) = match &head {
        None => (
            Evidence::Unknown("unborn HEAD".to_owned()),
            Evidence::Unknown("unborn HEAD".to_owned()),
            Evidence::Unknown("unborn HEAD".to_owned()),
        ),
        Some(head) => {
            let commits = commits_ahead(repo, head, &base);
            let landed = landed(repo, head, &base);
            let unpushed = unpushed_commits(repo, anchor.branch(), &upstream, &commits);
            (landed, commits, unpushed)
        }
    };

    let (dirty, last_git_activity) = match anchor {
        Anchor::Worktree { path, admin_id, .. } => (
            repo.dirty(path),
            repo.reflog_activity(&worktree_head_log(admin_id.as_deref())),
        ),
        Anchor::Branch { name } => (
            Evidence::Known(false),
            repo.reflog_activity(&PathBuf::from(format!("logs/refs/heads/{name}"))),
        ),
    };

    WorkState {
        repo: repo.clone(),
        anchor: anchor.clone(),
        remote_url,
        base,
        vector: StateVector {
            worktree: match anchor {
                Anchor::Worktree { path, .. } => Some(path.clone()),
                Anchor::Branch { .. } => None,
            },
            windows: runtime.windows,
            live_pids: runtime.live_pids,
            live_agent_sessions: runtime.live_agent_sessions,
            past_agent_sessions: runtime.past_agent_sessions,
            dirty,
            commits_ahead_of_base: commits_ahead,
            upstream_state: upstream,
            unpushed_commits: unpushed,
            landed,
            last_git_activity,
        },
    }
}

/// `$GIT_COMMON_DIR/worktrees/<id>/logs/HEAD` for a linked worktree,
/// `$GIT_COMMON_DIR/logs/HEAD` for the main one: the reflog is per worktree.
fn worktree_head_log(admin_id: Option<&str>) -> PathBuf {
    match admin_id {
        Some(id) => PathBuf::from(format!("worktrees/{id}/logs/HEAD")),
        None => PathBuf::from("logs/HEAD"),
    }
}

/// A read-only `ls-remote` decides whether the remote still advertises the
/// configured merge ref. An unreachable remote is `Unknown`, not `remote_gone`:
/// gone is only claimed when the remote answered and did not have the ref.
fn upstream_state(repo: &Repo, config: &UpstreamConfig) -> UpstreamState {
    match config {
        UpstreamConfig::None => UpstreamState::NeverPushed,
        UpstreamConfig::Partial => UpstreamState::Unknown("incomplete upstream config".to_owned()),
        UpstreamConfig::Full { remote, merge } => match repo.remote_advertises(remote, merge) {
            Evidence::Known(true) => UpstreamState::Tracked {
                remote: remote.clone(),
                merge_ref: merge.clone(),
            },
            Evidence::Known(false) => UpstreamState::RemoteGone {
                remote: remote.clone(),
                merge_ref: merge.clone(),
            },
            Evidence::Unknown(reason) => UpstreamState::Unknown(reason),
        },
    }
}

/// The base branch: the symbolic HEAD of the upstream remote, or of the
/// repository's only remote when no upstream is configured (a single remote
/// is a derivation, not a guess; zero or several remotes prove nothing).
/// `ls-remote --symref` is authoritative; a local `refs/remotes/<r>/HEAD`
/// symref may corroborate it or stand in when the remote is unreachable, but
/// the two disagreeing is a conflict, and a conflict is `Unknown`.
fn resolve_base(repo: &Repo, upstream_remote: Option<&str>) -> Evidence<Base> {
    let remote = match upstream_remote {
        Some(remote) => remote.to_owned(),
        None => match repo.remotes() {
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
    // symref to consult.
    let local = if remote == "." {
        None
    } else {
        match repo.local_remote_head(&remote) {
            Ok(local) => local,
            Err(e) => return Evidence::Unknown(format!("local remote HEAD: {e}")), // coverage: off - `remotes()` already failed on a repo this broken
        }
    };
    let branch = match repo.remote_head(&remote) {
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

fn commits_ahead(repo: &Repo, head: &str, base: &Evidence<Base>) -> Evidence<u64> {
    match base {
        Evidence::Unknown(reason) => Evidence::Unknown(format!("no proven base ({reason})")),
        Evidence::Known(base) => match repo.rev_list_count(&base.local_ref, head) {
            Ok(count) => Evidence::Known(count),
            Err(e) => Evidence::Unknown(format!("rev-list: {e}")),
        },
    }
}

/// Ancestry first; when HEAD is no ancestor, the squash/rebase shape is
/// checked by requiring every path HEAD changed relative to the merge base
/// to be identical on the base. No delta at all is not landed.
fn landed(repo: &Repo, head: &str, base: &Evidence<Base>) -> Evidence<Landed> {
    let Evidence::Known(base) = base else {
        return Evidence::Unknown(format!(
            "no proven base ({})",
            base.reason().unwrap_or_default()
        ));
    };
    match repo.is_ancestor(head, &base.local_ref) {
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
/// past the base is unpushed by definition. `@\{u}` resolves through the
/// refspec, which is why it handles a merge ref naming a different remote
/// branch.
fn unpushed_commits(
    repo: &Repo,
    branch: Option<&str>,
    upstream: &UpstreamState,
    commits_ahead: &Evidence<u64>,
) -> Evidence<u64> {
    match (upstream, branch) {
        (UpstreamState::NeverPushed | UpstreamState::NotApplicable, _) => commits_ahead.clone(),
        (_, Some(branch)) => {
            let upstream_ref = format!("{branch}@{{u}}");
            match repo.rev_list_count(&upstream_ref, &format!("refs/heads/{branch}")) {
                Ok(count) => Evidence::Known(count),
                Err(e) => Evidence::Unknown(format!("unpushed count via @{{u}}: {e}")),
            }
        }
        _ => Evidence::Unknown("no branch for an upstream".to_owned()), // coverage: off - a tracked upstream implies a branch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_unknown(upstream: &UpstreamState) -> bool {
        matches!(upstream, UpstreamState::Unknown(_))
    }

    /// A `Repo` whose common dir is a plain file: every read errors, which is
    /// the only shape collect() cannot derive a fact from - it must collect
    /// `Unknown`s, not crash.
    fn broken_repo() -> Repo {
        let dir =
            std::env::temp_dir().join(format!("agent-sessions-broken-{}", std::process::id()));
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
    fn a_bare_repo_lists_no_checkouts() {
        // A bare repository's porcelain entry is skipped: it is storage,
        // not a workspace.
        let dir = std::env::temp_dir().join(format!("agent-sessions-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            std::process::Command::new("git")
                .args(["init", "--bare", dir.to_str().unwrap()])
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
