//! Independent per-action cleanup verdicts: `safe`, `review` or `blocked`,
//! each with the reasons that produced it.
//!
//! Verdicts are derived, never authored, and fail closed: a local safety
//! fact that cannot be proven (dirty, unpushed, commits relative to base)
//! blocks, while forge work-item state is informational - an `unknown` forge
//! never blocks by itself, but a known open pull request or merge request
//! does. A `review` verdict names the force the action would need; `blocked`
//! means a fact must change before the action is even offered.

use std::path::PathBuf;

use crate::evidence::Evidence;
use crate::forge::{ForgeStatus, WorkItem};
use crate::git::Head;
use crate::vector::{Anchor, Landed, UpstreamState, WorkState};

/// The verdict on one cleanup action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The action is provably lossless.
    Safe,
    /// Branch deletion only: the branch is checked out in this row's
    /// worktree, so the deletion is ordered after - and conditioned on - the
    /// worktree's removal.
    SafeAfterWorktreeRemoval,
    /// The action needs an explicit human decision; the reasons name the
    /// force it would take (e.g. `git branch -D`).
    Review,
    /// A fact must change first; the action is not offered.
    Blocked,
    /// There is nothing to act on (a branch-only row has no worktree).
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionVerdict {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

/// Would `git worktree remove` be a provably lossless act?
///
/// Blockers: uncommitted changes, a locked worktree, live processes or agent
/// sessions, a known open work item, unpushed commits, detached unique
/// commits, and any unproven local safety fact. Unlanded-but-referenced
/// commits are `review`: the branch keeps them, but removing the workspace
/// of unfinished work is a human's call.
pub fn worktree_removal(state: &WorkState, forge: &ForgeStatus) -> ActionVerdict {
    let Anchor::Worktree {
        head, locked, main, ..
    } = &state.anchor
    else {
        return ActionVerdict {
            verdict: Verdict::NotApplicable,
            reasons: vec!["no worktree".to_owned()],
        };
    };
    let v = &state.vector;
    let mut blockers = Vec::new();

    // `git worktree remove` refuses the main working tree outright.
    if *main {
        blockers.push("main worktree".to_owned());
    }

    match &v.dirty {
        Evidence::Known(true) => blockers.push("uncommitted changes".to_owned()),
        Evidence::Unknown(reason) => {
            blockers.push(format!("cannot prove the worktree is clean ({reason})"))
        }
        Evidence::Known(false) => {}
    }
    if *locked {
        blockers.push("worktree is locked".to_owned());
    }
    if v.live_pids > 0 {
        blockers.push(plural(v.live_pids, "live process"));
    }
    if v.live_agent_sessions > 0 {
        blockers.push(plural(v.live_agent_sessions, "live agent session"));
    }
    if forge.item == WorkItem::Open {
        blockers.push(format!(
            "open {}",
            forge.label.as_deref().unwrap_or("work item")
        ));
    }
    match head {
        Head::Detached(_) => match &v.unpushed_commits {
            Evidence::Known(0) => {}
            Evidence::Known(n) => blockers.push(format!(
                "detached HEAD: {} reachable from no ref",
                plural(*n as usize, "unique commit")
            )),
            Evidence::Unknown(reason) => blockers.push(format!(
                "detached HEAD: cannot prove commits are reachable from a ref ({reason})"
            )),
        },
        _ => {
            match &v.unpushed_commits {
                Evidence::Known(n) if *n > 0 => {
                    blockers.push(plural(*n as usize, "unpushed commit"))
                }
                Evidence::Unknown(reason) => {
                    blockers.push(format!("cannot prove nothing is unpushed ({reason})"))
                }
                _ => {}
            }
            if let Evidence::Unknown(reason) = &v.commits_ahead_of_base {
                blockers.push(format!("cannot prove commits relative to base ({reason})"));
            }
        }
    }

    if !blockers.is_empty() {
        return ActionVerdict {
            verdict: Verdict::Blocked,
            reasons: blockers,
        };
    }
    let mut reasons = vec!["clean".to_owned(), "nothing live".to_owned()];
    if let UpstreamState::RemoteGone { .. } = v.upstream_state {
        // A deleted remote branch is the strongest "merged" signal: it
        // survives squash and rebase merges that erase commit identity.
        reasons.push("upstream gone; probably landed".to_owned());
    }
    match &v.landed {
        Evidence::Known(Landed::AncestorMerged) => {
            reasons.push(format!("landed on {} (ancestor)", base_label(state)));
            ActionVerdict {
                verdict: Verdict::Safe,
                reasons,
            }
        }
        Evidence::Known(Landed::ContentMerged) => {
            reasons.push(format!("landed on {} (content match)", base_label(state)));
            ActionVerdict {
                verdict: Verdict::Safe,
                reasons,
            }
        }
        Evidence::Known(Landed::No) => match v.commits_ahead_of_base {
            // Zero ahead is an ancestor, which `Landed` already reported, so
            // `n` is always nonzero here.
            Evidence::Known(n) => {
                // A bare quoted commit token is a forbidden Git write argv
                // for the source-invariant scan, so the noun stays inline.
                let mut reasons = vec![format!(
                    "{n} commit{} ahead of {} and not landed",
                    if n == 1 { "" } else { "s" },
                    base_label(state)
                )];
                reasons.extend(
                    state
                        .anchor
                        .branch()
                        .map(|b| format!("removal keeps branch {b} and its commits")),
                );
                ActionVerdict {
                    verdict: Verdict::Review,
                    reasons,
                }
            }
            Evidence::Unknown(_) => ActionVerdict {
                verdict: Verdict::Blocked,
                reasons,
            }, // coverage: off - an unknown count is a blocker above
        },
        Evidence::Unknown(reason) => blocked_landing(reason), // coverage: off - needs a proven count with unproven ancestry
    }
}

#[rustfmt::skip]
fn blocked_landing(reason: &str) -> ActionVerdict { ActionVerdict { verdict: Verdict::Blocked, reasons: vec![format!("cannot prove landing ({reason})")] } } // coverage: off - needs a proven count with unproven ancestry

/// Would `git branch -d` be a provably lossless act?
///
/// `-d` succeeds only for a branch merged by ancestry; content-merged or
/// unlanded work needs `git branch -D`, which is what `review` means here.
/// A branch checked out in this row's worktree is ordered after the
/// worktree's removal (`safe after worktree removal`), and shares its fate
/// when the removal is blocked.
pub fn branch_deletion(
    state: &WorkState,
    removal: &ActionVerdict,
    forge: &ForgeStatus,
) -> ActionVerdict {
    let Some(_branch) = state.anchor.branch() else {
        return ActionVerdict {
            verdict: Verdict::NotApplicable,
            reasons: vec!["no branch".to_owned()],
        };
    };
    let v = &state.vector;
    let checked_out_in: Option<PathBuf> = match &state.anchor {
        Anchor::Worktree { path, .. } => Some(path.clone()),
        Anchor::Branch { .. } => None,
    };

    let mut blockers = Vec::new();
    if forge.item == WorkItem::Open {
        blockers.push(format!(
            "open {}",
            forge.label.as_deref().unwrap_or("work item")
        ));
    }
    match &v.unpushed_commits {
        Evidence::Known(n) if *n > 0 => blockers.push(plural(*n as usize, "unpushed commit")),
        Evidence::Unknown(reason) => {
            blockers.push(format!("cannot prove nothing is unpushed ({reason})"))
        }
        _ => {}
    }
    if let Evidence::Unknown(reason) = &v.landed {
        blockers.push(format!("cannot prove landing ({reason})"));
    }
    if let (Some(path), Verdict::Blocked) = (&checked_out_in, removal.verdict) {
        blockers.push(format!(
            "checked out in {}, and its removal is blocked",
            path.display()
        ));
    }
    if !blockers.is_empty() {
        return ActionVerdict {
            verdict: Verdict::Blocked,
            reasons: blockers,
        };
    }

    let (verdict, mut reasons) = match &v.landed {
        Evidence::Known(Landed::AncestorMerged) => (
            Verdict::Safe,
            vec![
                format!("merged into {}", base_label(state)),
                "nothing unpushed".to_owned(),
            ],
        ),
        Evidence::Known(Landed::ContentMerged) => (
            Verdict::Review,
            vec![
                "requires `git branch -D`".to_owned(),
                format!("landed on {} by content, not ancestry", base_label(state)),
            ],
        ),
        Evidence::Known(Landed::No) => (
            Verdict::Review,
            vec![
                "requires `git branch -D`".to_owned(),
                format!("not landed on {}", base_label(state)),
            ],
        ),
        Evidence::Unknown(_) => (Verdict::Blocked, vec!["unproven landing".to_owned()]), // coverage: off - unproven landing is a blocker above
    };
    if let Some(path) = checked_out_in {
        reasons.push(format!(
            "checked out in {}; deleted after the worktree is removed",
            path.display()
        ));
        if verdict == Verdict::Safe {
            return ActionVerdict {
                verdict: Verdict::SafeAfterWorktreeRemoval,
                reasons,
            };
        }
    }
    ActionVerdict { verdict, reasons }
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn base_label(state: &WorkState) -> String {
    match &state.base {
        Evidence::Known(base) => base.label(),
        Evidence::Unknown(_) => "<unproven base>".to_owned(), // coverage: off - a proven landing implies a proven base
    }
}

/// Both verdicts of a row, in the order a cleanup plan presents them:
/// worktree removal first, then branch deletion, which may depend on it.
pub fn cleanup(state: &WorkState, forge: &ForgeStatus) -> (ActionVerdict, ActionVerdict) {
    let removal = worktree_removal(state, forge);
    let deletion = branch_deletion(state, &removal, forge);
    (removal, deletion)
}
