//! The fixture-repo verdict table: a scratch repository holding every Git
//! shape cleanup has to classify, asserted field by field and verdict by
//! verdict - exactly, including reason lists.

mod support;

use std::collections::BTreeMap;
use std::path::Path;

use agent_sessions::evidence::Evidence;
use agent_sessions::forge::{ForgeStatus, Pipeline, WorkItem};
use agent_sessions::git::{self, Head, Resolved};
use agent_sessions::vector::{self, Anchor, Landed, RuntimeFacts, UpstreamState, WorkState};
use agent_sessions::verdict::{self, Verdict};
use support::fixture::{FixtureRepo, Landing};
use support::tempdir::TempDir;

/// The forge asked successfully and found no work item.
fn no_item() -> ForgeStatus {
    ForgeStatus {
        item: WorkItem::NotExisting,
        pipeline: Pipeline::Unknown,
        label: None,
        url: None,
        reason: None,
    }
}

fn open_item() -> ForgeStatus {
    ForgeStatus {
        item: WorkItem::Open,
        pipeline: Pipeline::Busy,
        label: Some("PR #191".to_owned()),
        url: Some("https://github.com/o/r/pull/191".to_owned()),
        reason: None,
    }
}

fn quiet() -> RuntimeFacts {
    RuntimeFacts::default()
}

/// The scenario set every row assertion runs against.
fn standard() -> FixtureRepo {
    let f = FixtureRepo::new("origin");

    // Ancestor-merged, pushed, clean worktree.
    f.branch_with_commits("landed", 2, true);
    f.land("landed", Landing::Merge);
    f.add_worktree("landed", Some("landed"));

    // Squash-merged: content landed, commit identity did not.
    f.branch_with_commits("squashed", 2, true);
    f.land("squashed", Landing::Squash);
    f.add_worktree("squashed", Some("squashed"));

    // Cherry-picked: content landed commit by commit.
    f.branch_with_commits("picked", 2, true);
    f.land("picked", Landing::CherryPick);
    f.add_worktree("picked", Some("picked"));

    // Pushed but never merged.
    f.branch_with_commits("wip", 2, true);
    f.add_worktree("wip", Some("wip"));

    // Pushed, then one more local-only commit.
    f.branch_with_commits("unpushed", 1, true);
    f.add_worktree("unpushed", Some("unpushed"));
    f.commit(
        &f.dir.join("wt-unpushed"),
        "unpushed.txt",
        "unpushed",
        "not yet pushed",
    );

    // Pushed, dirty worktree (tracked edit plus an untracked file).
    f.branch_with_commits("dirty", 1, true);
    let dirty = f.add_worktree("dirty", Some("dirty"));
    f.commit(&dirty, "dirty-seed.txt", "seed", "a committed base");
    f.git(&dirty, &["push", "origin", "dirty"]);
    std::fs::write(dirty.join("dirty-seed.txt"), "edited").expect("write");
    std::fs::write(dirty.join("untracked.txt"), "new").expect("write");

    // Squash-landed and then deleted on the remote: `remote_gone`. The
    // branch is deleted on the bare remote itself, so the local
    // remote-tracking ref survives - exactly what a third party's
    // delete-branch-on-merge leaves behind.
    f.branch_with_commits("gone", 1, true);
    f.land("gone", Landing::Squash);
    f.git(&f.remote, &["branch", "-D", "gone"]);
    f.add_worktree("gone", Some("gone"));

    // Local-only work: no upstream configured at all.
    f.branch_with_commits("local", 1, false);
    f.add_worktree("local", Some("local"));

    // A branch that tracks a different remote ref than its own name.
    f.branch_with_commits("release", 1, true);
    f.git(
        f.main.as_path(),
        &["branch", "tracks-release", "origin/release"],
    );
    f.git(
        f.main.as_path(),
        &[
            "branch",
            "--set-upstream-to=origin/release",
            "tracks-release",
        ],
    );
    f.add_worktree("tracks-release", Some("tracks-release"));

    // Detached HEAD with a commit reachable from no ref.
    let detached = f.add_worktree("detached", None);
    f.commit(&detached, "detached.txt", "detached", "unique commit");

    // Detached HEAD at the base tip: nothing unique, nothing lost.
    f.add_worktree("detached-clean", None);

    // Detached HEAD at another branch's tip: the commits are kept by that
    // branch, so nothing is unreachable even though they are ahead of base.
    f.branch_with_commits("kept", 1, true);
    let shared = f.dir.join("wt-detached-shared");
    f.git(
        f.main.as_path(),
        &[
            "worktree",
            "add",
            "--detach",
            shared.to_str().unwrap(),
            "kept",
        ],
    );

    // Landed but the worktree is locked: the user marked it hands-off.
    f.branch_with_commits("locked", 1, true);
    f.land("locked", Landing::Merge);
    let locked = f.add_worktree("locked", Some("locked"));
    f.git(&f.main, &["worktree", "lock", locked.to_str().unwrap()]);

    // Unborn HEAD: an orphan checkout (`switch --orphan` leaves an empty
    // index and worktree) with no commit yet.
    let unborn = f.add_worktree("unborn", None);
    f.git(&unborn, &["switch", "--orphan", "unborn"]);

    // A worktree whose directory was deleted underneath it: the porcelain
    // record remains until pruned.
    f.branch_with_commits("lost", 1, true);
    let lost = f.add_worktree("lost", Some("lost"));
    std::fs::remove_dir_all(&lost).expect("remove the worktree directory");

    // Upstream configured, but the local tracking ref is gone: `@{u}` fails.
    f.branch_with_commits("stale", 1, true);
    f.add_worktree("stale", Some("stale"));
    f.git(&f.main, &["update-ref", "-d", "refs/remotes/origin/stale"]);

    // Half an upstream: `branch.partial.remote` without a merge ref.
    f.branch_with_commits("partial", 1, false);
    f.add_worktree("partial", Some("partial"));
    f.git(&f.main, &["config", "branch.partial.remote", "origin"]);

    // A history that shares no ancestor with main.
    f.orphan_branch("unrelated", true);
    f.add_worktree("unrelated", Some("unrelated"));

    // Commits that cancel out: ahead of base, but a zero delta.
    f.branch_with_files(
        "reverted",
        &[("flip.txt", Some("x")), ("flip.txt", None)],
        true,
    );
    f.add_worktree("reverted", Some("reverted"));

    // A pathspec-magic filename: `:(glob)zzz-no-match` is glob magic
    // matching nothing, so a naive diff sees no delta where the file
    // differs - an empty match would fake "content merged".
    f.branch_with_files("magicpath", &[(":(glob)zzz-no-match", Some("m"))], true);
    f.add_worktree("magicpath", Some("magicpath"));

    // Branch-only rows: pushed and unmerged, and landed.
    f.branch_with_commits("shelved", 1, true);
    f.branch_with_commits("shipped", 1, true);
    f.land("shipped", Landing::Merge);

    f
}

/// All anchors of the fixture, keyed by a name the table can assert against.
/// One `RemoteCache` for the batch, as the collectors will share one pass.
fn collect_all(f: &FixtureRepo) -> BTreeMap<String, WorkState> {
    let repo = git::Repo::discover(f.main.as_path())
        .expect("discover")
        .expect("the clone is a repo");
    let mut cache = vector::RemoteCache::default();
    vector::anchors(&repo)
        .expect("anchors")
        .into_iter()
        .map(|anchor| {
            let key = match &anchor {
                Anchor::Branch { name } => format!("branch:{name}"),
                Anchor::Worktree { path, head, .. } => match head {
                    Head::Branch(name) | Head::Unborn(name) => {
                        format!("wt:{name}@{}", path.file_name().unwrap().to_string_lossy())
                    }
                    Head::Detached(_) => {
                        format!(
                            "wt:detached@{}",
                            path.file_name().unwrap().to_string_lossy()
                        )
                    }
                },
            };
            (
                key,
                vector::collect_cached(&repo, &mut cache, &anchor, quiet()),
            )
        })
        .collect()
}

fn verdicts(
    state: &WorkState,
    forge: &ForgeStatus,
) -> (Verdict, Vec<String>, Verdict, Vec<String>) {
    let (removal, deletion) = verdict::cleanup(state, forge);
    (
        removal.verdict,
        removal.reasons,
        deletion.verdict,
        deletion.reasons,
    )
}

/// Assert the worktree-removal and branch-deletion verdicts of one row.
fn expect(
    states: &BTreeMap<String, WorkState>,
    key: &str,
    removal: (Verdict, &[&str]),
    deletion: (Verdict, &[&str]),
) {
    let state = states.get(key).unwrap_or_else(|| panic!("no anchor {key}"));
    let (rv, rr, dv, dr) = verdicts(state, &no_item());
    let rr: Vec<&str> = rr.iter().map(String::as_str).collect();
    let dr: Vec<&str> = dr.iter().map(String::as_str).collect();
    assert_eq!(
        (rv, rr),
        (removal.0, removal.1.to_vec()),
        "{key}: worktree removal"
    );
    assert_eq!(
        (dv, dr),
        (deletion.0, deletion.1.to_vec()),
        "{key}: branch deletion"
    );
}

#[test]
fn the_fixture_table() {
    let f = standard();
    let states = collect_all(&f);

    // The main checkout itself is a row, but `git worktree remove` always
    // refuses it, so its removal can never be safe.
    expect(
        &states,
        "wt:main@main",
        (Verdict::Blocked, &["main worktree"]),
        (
            Verdict::Blocked,
            &[&format!(
                "checked out in {}, and its removal is blocked",
                f.main.display()
            )],
        ),
    );

    expect(
        &states,
        "wt:landed@wt-landed",
        (
            Verdict::Safe,
            &["clean", "nothing live", "landed on origin/main (ancestor)"],
        ),
        (
            Verdict::SafeAfterWorktreeRemoval,
            &[
                "merged into origin/main",
                "nothing unpushed",
                &format!(
                    "checked out in {}; deleted after the worktree is removed",
                    f.dir.join("wt-landed").display()
                ),
            ],
        ),
    );

    for squash_like in ["squashed", "picked"] {
        expect(
            &states,
            &format!("wt:{squash_like}@wt-{squash_like}"),
            (
                Verdict::Safe,
                &[
                    "clean",
                    "nothing live",
                    "landed on origin/main (content match)",
                ],
            ),
            (
                Verdict::Review,
                &[
                    "requires `git branch -D`",
                    "landed on origin/main by content, not ancestry",
                    &format!(
                        "checked out in {}; deleted after the worktree is removed",
                        f.dir.join(format!("wt-{squash_like}")).display()
                    ),
                ],
            ),
        );
    }

    expect(
        &states,
        "wt:wip@wt-wip",
        (
            Verdict::Review,
            &[
                "2 commits ahead of origin/main and not landed",
                "removal keeps branch wip and its commits",
            ],
        ),
        (
            Verdict::Review,
            &[
                "requires `git branch -D`",
                "not landed on origin/main",
                &format!(
                    "checked out in {}; deleted after the worktree is removed",
                    f.dir.join("wt-wip").display()
                ),
            ],
        ),
    );

    expect(
        &states,
        "wt:dirty@wt-dirty",
        (Verdict::Blocked, &["uncommitted changes"]),
        (
            Verdict::Blocked,
            &[&format!(
                "checked out in {}, and its removal is blocked",
                f.dir.join("wt-dirty").display()
            )],
        ),
    );

    expect(
        &states,
        "wt:unpushed@wt-unpushed",
        (Verdict::Blocked, &["1 unpushed commit"]),
        (
            Verdict::Blocked,
            &[
                "1 unpushed commit",
                &format!(
                    "checked out in {}, and its removal is blocked",
                    f.dir.join("wt-unpushed").display()
                ),
            ],
        ),
    );

    expect(
        &states,
        "wt:gone@wt-gone",
        (
            Verdict::Safe,
            &[
                "clean",
                "nothing live",
                "upstream gone; probably landed",
                "landed on origin/main (content match)",
            ],
        ),
        (
            Verdict::Review,
            &[
                "requires `git branch -D`",
                "landed on origin/main by content, not ancestry",
                &format!(
                    "checked out in {}; deleted after the worktree is removed",
                    f.dir.join("wt-gone").display()
                ),
            ],
        ),
    );

    expect(
        &states,
        "wt:local@wt-local",
        (Verdict::Blocked, &["1 unpushed commit"]),
        (
            Verdict::Blocked,
            &[
                "1 unpushed commit",
                &format!(
                    "checked out in {}, and its removal is blocked",
                    f.dir.join("wt-local").display()
                ),
            ],
        ),
    );

    expect(
        &states,
        "wt:tracks-release@wt-tracks-release",
        (
            Verdict::Review,
            &[
                "1 commit ahead of origin/main and not landed",
                "removal keeps branch tracks-release and its commits",
            ],
        ),
        (
            Verdict::Review,
            &[
                "requires `git branch -D`",
                "not landed on origin/main",
                &format!(
                    "checked out in {}; deleted after the worktree is removed",
                    f.dir.join("wt-tracks-release").display()
                ),
            ],
        ),
    );

    // `release` itself is a pushed-but-unmerged branch-only row.
    expect(
        &states,
        "branch:release",
        (Verdict::NotApplicable, &["no worktree"]),
        (
            Verdict::Review,
            &["requires `git branch -D`", "not landed on origin/main"],
        ),
    );

    expect(
        &states,
        "branch:shelved",
        (Verdict::NotApplicable, &["no worktree"]),
        (
            Verdict::Review,
            &["requires `git branch -D`", "not landed on origin/main"],
        ),
    );

    // A landed branch with no worktree deletes outright.
    expect(
        &states,
        "branch:shipped",
        (Verdict::NotApplicable, &["no worktree"]),
        (
            Verdict::Safe,
            &["merged into origin/main", "nothing unpushed"],
        ),
    );

    // Detached unique commits fail closed: removal is unrecoverable.
    expect(
        &states,
        "wt:detached@wt-detached",
        (
            Verdict::Blocked,
            &["detached HEAD: 1 unique commit reachable from no ref"],
        ),
        (Verdict::NotApplicable, &["no branch"]),
    );

    // Detached at the base tip: no unique commits, so nothing is lost.
    expect(
        &states,
        "wt:detached@wt-detached-clean",
        (
            Verdict::Safe,
            &["clean", "nothing live", "landed on origin/main (ancestor)"],
        ),
        (Verdict::NotApplicable, &["no branch"]),
    );

    // Detached at a branch tip: `kept` reaches the commits, so removal
    // reviews the not-landed facts rather than blocking on phantom loss.
    expect(
        &states,
        "wt:detached@wt-detached-shared",
        (
            Verdict::Review,
            &["1 commit ahead of origin/main and not landed"],
        ),
        (Verdict::NotApplicable, &["no branch"]),
    );

    expect(
        &states,
        "wt:locked@wt-locked",
        (Verdict::Blocked, &["worktree is locked"]),
        (
            Verdict::Blocked,
            &[&format!(
                "checked out in {}, and its removal is blocked",
                f.dir.join("wt-locked").display()
            )],
        ),
    );

    expect(
        &states,
        "wt:unborn@wt-unborn",
        (
            Verdict::Blocked,
            &[
                "cannot prove nothing is unpushed (unborn HEAD)",
                "cannot prove commits relative to base (unborn HEAD)",
            ],
        ),
        (
            Verdict::Blocked,
            &[
                "cannot prove nothing is unpushed (unborn HEAD)",
                "cannot prove landing (unborn HEAD)",
                &format!(
                    "checked out in {}, and its removal is blocked",
                    f.dir.join("wt-unborn").display()
                ),
            ],
        ),
    );

    // The git error detail varies with the path; assert the stable parts.
    {
        let state = &states["wt:lost@wt-lost"];
        let (removal, deletion) = verdict::cleanup(state, &no_item());
        assert_eq!(removal.verdict, Verdict::Blocked);
        assert!(
            removal
                .reasons
                .iter()
                .any(|r| r.starts_with("cannot prove the worktree is clean")),
            "{:?}",
            removal.reasons
        );
        assert_eq!(deletion.verdict, Verdict::Blocked);
        assert!(
            deletion
                .reasons
                .iter()
                .any(|r| r.contains("its removal is blocked")),
            "{:?}",
            deletion.reasons
        );
    }

    // A stale `@{u}` and a half-configured upstream both fail closed.
    for key in ["wt:stale@wt-stale", "wt:partial@wt-partial"] {
        let state = &states[key];
        let (removal, deletion) = verdict::cleanup(state, &no_item());
        assert_eq!(removal.verdict, Verdict::Blocked, "{key}");
        assert!(
            removal
                .reasons
                .iter()
                .any(|r| r.starts_with("cannot prove nothing is unpushed")),
            "{key}: {:?}",
            removal.reasons
        );
        assert_eq!(deletion.verdict, Verdict::Blocked, "{key}");
    }
    assert!(matches!(
        states["wt:partial@wt-partial"].vector.upstream_state,
        UpstreamState::Unknown(_)
    ));

    // No shared ancestor: provably not landed, review-able but never safe.
    expect(
        &states,
        "wt:unrelated@wt-unrelated",
        (
            Verdict::Review,
            &[
                "1 commit ahead of origin/main and not landed",
                "removal keeps branch unrelated and its commits",
            ],
        ),
        (
            Verdict::Review,
            &[
                "requires `git branch -D`",
                "not landed on origin/main",
                &format!(
                    "checked out in {}; deleted after the worktree is removed",
                    f.dir.join("wt-unrelated").display()
                ),
            ],
        ),
    );

    // A pathspec-magic-named file that differs: not landed, not "content
    // match" - the filename must reach diff as data, not as a pattern.
    expect(
        &states,
        "wt:magicpath@wt-magicpath",
        (
            Verdict::Review,
            &[
                "1 commit ahead of origin/main and not landed",
                "removal keeps branch magicpath and its commits",
            ],
        ),
        (
            Verdict::Review,
            &[
                "requires `git branch -D`",
                "not landed on origin/main",
                &format!(
                    "checked out in {}; deleted after the worktree is removed",
                    f.dir.join("wt-magicpath").display()
                ),
            ],
        ),
    );

    // Two commits with a zero net delta: not landed.
    expect(
        &states,
        "wt:reverted@wt-reverted",
        (
            Verdict::Review,
            &[
                "2 commits ahead of origin/main and not landed",
                "removal keeps branch reverted and its commits",
            ],
        ),
        (
            Verdict::Review,
            &[
                "requires `git branch -D`",
                "not landed on origin/main",
                &format!(
                    "checked out in {}; deleted after the worktree is removed",
                    f.dir.join("wt-reverted").display()
                ),
            ],
        ),
    );

    // The upstream-tracking state vector itself.
    let v = &states["wt:gone@wt-gone"].vector;
    assert!(matches!(v.upstream_state, UpstreamState::RemoteGone { .. }));
    assert_eq!(v.landed, Evidence::Known(Landed::ContentMerged));
    let v = &states["wt:local@wt-local"].vector;
    assert_eq!(v.upstream_state, UpstreamState::NeverPushed);
    let v = &states["wt:tracks-release@wt-tracks-release"].vector;
    assert!(matches!(
        &v.upstream_state,
        UpstreamState::Tracked { merge_ref, .. } if merge_ref == "refs/heads/release"
    ));
    assert_eq!(v.commits_ahead_of_base, Evidence::Known(1));
    assert_eq!(v.unpushed_commits, Evidence::Known(0));
    assert_eq!(
        states["wt:dirty@wt-dirty"].vector.dirty,
        Evidence::Known(true)
    );
    assert_eq!(
        states["branch:shelved"].vector.dirty,
        Evidence::Known(false)
    );
    assert!(states["wt:wip@wt-wip"].vector.last_git_activity.is_some());
}

#[test]
fn injected_runtime_facts_change_the_verdict() {
    let f = standard();
    let repo = git::Repo::discover(f.main.as_path())
        .expect("discover")
        .expect("repo");
    let anchor = vector::anchors(&repo)
        .expect("anchors")
        .into_iter()
        .find(|a| a.branch() == Some("landed"))
        .expect("landed anchor");

    // A live process or agent session blocks removal.
    for runtime in [
        RuntimeFacts {
            live_pids: 1,
            ..quiet()
        },
        RuntimeFacts {
            live_agent_sessions: 2,
            ..quiet()
        },
    ] {
        let state = vector::collect(&repo, &anchor, runtime);
        let (removal, _) = verdict::cleanup(&state, &no_item());
        assert_eq!(removal.verdict, Verdict::Blocked, "{runtime:?}");
    }

    // Orphaned windows are references, not blockers.
    let state = vector::collect(
        &repo,
        &anchor,
        RuntimeFacts {
            windows: vector::WindowCount {
                total: 1,
                orphaned: 1,
            },
            ..quiet()
        },
    );
    let (removal, _) = verdict::cleanup(&state, &no_item());
    assert_eq!(removal.verdict, Verdict::Safe);
}

#[test]
fn an_open_work_item_blocks_and_unknown_does_not() {
    let f = standard();
    let states = collect_all(&f);
    let state = &states["wt:landed@wt-landed"];

    let (removal, deletion) = verdict::cleanup(state, &open_item());
    assert_eq!(removal.verdict, Verdict::Blocked);
    assert_eq!(removal.reasons, ["open PR #191"]);
    assert_eq!(deletion.verdict, Verdict::Blocked);
    assert!(deletion.reasons.contains(&"open PR #191".to_owned()));

    // Unknown forge state is informational; it never blocks by itself.
    let unknown = ForgeStatus {
        item: WorkItem::Unknown,
        pipeline: Pipeline::Unknown,
        label: None,
        url: None,
        reason: Some("gh is not on PATH".to_owned()),
    };
    let (removal, _) = verdict::cleanup(state, &unknown);
    assert_eq!(removal.verdict, Verdict::Safe);
}

#[test]
fn resolution_follows_symlinks_to_one_repo_identity() {
    let f = standard();
    let link = f.dir.join("link-to-main");
    std::os::unix::fs::symlink(f.main.as_path(), &link).expect("symlink");

    let direct = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let via_link = git::Repo::discover(&link).unwrap().unwrap();
    assert_eq!(direct, via_link, "one canonical repo id through symlinks");

    let inside = f.dir.join("wt-landed");
    let Resolved::Checkout(checkout) = git::resolve(&inside).expect("resolve") else {
        panic!("a worktree resolves to a checkout");
    };
    assert_eq!(checkout.repo, direct);
    assert_eq!(checkout.head, Head::Branch("landed".to_owned()));
    assert_eq!(checkout.admin_id.as_deref(), Some("wt-landed"));

    // A non-Git path is a project space, and `.git` itself is repo-only.
    let scratch = TempDir::new("project-space");
    assert!(matches!(
        git::resolve(scratch.path()).unwrap(),
        Resolved::ProjectSpace(_)
    ));
    assert!(matches!(
        git::resolve(&f.main.join(".git")).unwrap(),
        Resolved::RepoOnly(_)
    ));
}

#[test]
fn missing_and_conflicting_remote_head_leave_the_base_unproven() {
    // A remote that advertises no symbolic HEAD at all: HEAD detached on the
    // bare repository means `ls-remote --symref` answers with a sha and no
    // `ref:` line.
    let f = FixtureRepo::new("origin");
    f.branch_with_commits("feat", 1, true);
    f.add_worktree("feat", Some("feat"));
    let sha = f.git(&f.remote, &["rev-parse", "main"]);
    f.git(&f.remote, &["update-ref", "--no-deref", "HEAD", sha.trim()]);
    // A detached worktree with a unique commit in the same repo: with no
    // proven base, its commits cannot be proven reachable from a ref.
    let detached = f.add_worktree("detached", None);
    f.commit(&detached, "d.txt", "x", "unique commit");

    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("feat"))
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());
    assert!(state.base.reason().unwrap().contains("advertises no HEAD"));
    assert!(
        state
            .vector
            .landed
            .reason()
            .unwrap()
            .contains("no proven base"),
        "{:?}",
        state.vector.landed
    );
    let (removal, _) = verdict::cleanup(&state, &no_item());
    assert_eq!(removal.verdict, Verdict::Blocked);
    assert!(
        removal
            .reasons
            .iter()
            .any(|r| r.contains("cannot prove commits relative to base"))
    );

    // The detached worktree: unique commits need no base to be counted -
    // the unreachable check is ref-local, so the verdict still blocks but
    // now names the true count.
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| {
            matches!(
                a,
                Anchor::Worktree {
                    head: Head::Detached(_),
                    ..
                }
            )
        })
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());
    let (removal, _) = verdict::cleanup(&state, &no_item());
    assert_eq!(removal.verdict, Verdict::Blocked);
    assert!(
        removal
            .reasons
            .iter()
            .any(|r| r.contains("unique commit reachable from no ref")),
        "{:?}",
        removal.reasons
    );

    // A remote whose HEAD moved after the local symref was recorded.
    let f = FixtureRepo::new("origin");
    f.branch_with_commits("feat", 1, true);
    f.add_worktree("feat", Some("feat"));
    f.git(f.main.as_path(), &["branch", "trunk"]);
    f.git(f.main.as_path(), &["push", "origin", "trunk"]);
    f.git(&f.remote, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);

    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("feat"))
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());
    let reason = state
        .base
        .reason()
        .expect("conflict leaves the base unproven");
    assert!(reason.contains("conflicting remote HEAD"), "{reason}");
    assert!(state.vector.commits_ahead_of_base.reason().is_some());
}

#[test]
fn an_unreachable_remote_is_unknown_not_gone() {
    let f = FixtureRepo::new("origin");
    f.branch_with_commits("feat", 1, true);
    f.add_worktree("feat", Some("feat"));
    std::fs::remove_dir_all(&f.remote).expect("the remote goes away");

    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("feat"))
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());

    // Unreachable is not `remote_gone`, and the local remote-HEAD symref
    // still proves the base on its own.
    assert!(matches!(
        state.vector.upstream_state,
        UpstreamState::Unknown(_)
    ));
    assert!(state.base.is_known());
    let (removal, _) = verdict::cleanup(&state, &no_item());
    assert_eq!(removal.verdict, Verdict::Review);
}

#[test]
fn a_remote_named_other_than_origin_resolves() {
    let f = FixtureRepo::new("upstream");
    f.branch_with_commits("feat", 1, true);
    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("feat"))
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());
    assert!(matches!(
        &state.vector.upstream_state,
        UpstreamState::Tracked { remote, .. } if remote == "upstream"
    ));
    assert_eq!(
        state.base.known().map(|b| b.label()).as_deref(),
        Some("upstream/main")
    );
}

#[test]
fn remote_evidence_is_memoized_within_a_collection_pass() {
    // Two branches on one remote: collecting the second asks nothing the
    // first already answered. Removing the remote between the two collects
    // proves it - a fresh ls-remote would be Unreachable, the cache still
    // answers Tracked and a proven base.
    let f = FixtureRepo::new("origin");
    f.branch_with_commits("one", 1, true);
    f.branch_with_commits("two", 1, true);
    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchors = vector::anchors(&repo).unwrap();
    let mut cache = vector::RemoteCache::default();

    let one = anchors.iter().find(|a| a.branch() == Some("one")).unwrap();
    let state = vector::collect_cached(&repo, &mut cache, one, quiet());
    assert!(matches!(
        state.vector.upstream_state,
        UpstreamState::Tracked { .. }
    ));

    std::fs::remove_dir_all(&f.remote).expect("the remote goes away");
    let two = anchors.iter().find(|a| a.branch() == Some("two")).unwrap();
    let state = vector::collect_cached(&repo, &mut cache, two, quiet());
    assert!(
        matches!(state.vector.upstream_state, UpstreamState::Tracked { .. }),
        "{:?}",
        state.vector.upstream_state
    );
    assert!(state.base.is_known(), "{:?}", state.base);
}

#[test]
fn a_shared_remote_cache_scopes_evidence_per_repo() {
    // Two repos whose remotes share the name `origin` but advertise
    // different facts: a cache keyed by remote name alone would hand the
    // second repo the first repo's HEAD and ref listing.
    let a = FixtureRepo::new("origin");
    a.branch_with_commits("feat", 1, true);
    a.branch_with_commits("only-on-a", 1, true);

    let b = FixtureRepo::new("origin");
    b.branch_with_commits("feat", 1, true);
    // Repoint B's remote HEAD at trunk, fetched and recorded locally so the
    // evidence is non-conflicting.
    b.git(&b.remote, &["branch", "trunk", "main"]);
    b.git(&b.remote, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
    b.git(b.main.as_path(), &["fetch", "origin"]);
    b.git(b.main.as_path(), &["remote", "set-head", "origin", "-a"]);
    // A branch tracking a ref only A's remote advertises: RemoteGone on B,
    // Tracked only if B were handed A's ref listing.
    b.git(b.main.as_path(), &["branch", "bfeat"]);
    b.git(
        b.main.as_path(),
        &["config", "branch.bfeat.remote", "origin"],
    );
    b.git(
        b.main.as_path(),
        &["config", "branch.bfeat.merge", "refs/heads/only-on-a"],
    );

    let mut cache = vector::RemoteCache::default();
    let repo_a = git::Repo::discover(a.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo_a)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("feat"))
        .unwrap();
    let state = vector::collect_cached(&repo_a, &mut cache, &anchor, quiet());
    assert_eq!(
        state.base.known().map(|b| b.label()).as_deref(),
        Some("origin/main")
    );

    let repo_b = git::Repo::discover(b.main.as_path()).unwrap().unwrap();
    let anchors = vector::anchors(&repo_b).unwrap();
    let feat = anchors.iter().find(|a| a.branch() == Some("feat")).unwrap();
    let state = vector::collect_cached(&repo_b, &mut cache, feat, quiet());
    assert_eq!(
        state.base.known().map(|b| b.label()).as_deref(),
        Some("origin/trunk"),
        "{:?}",
        state.base
    );
    let bfeat = anchors
        .iter()
        .find(|a| a.branch() == Some("bfeat"))
        .unwrap();
    let state = vector::collect_cached(&repo_b, &mut cache, bfeat, quiet());
    assert!(
        matches!(
            state.vector.upstream_state,
            UpstreamState::RemoteGone { .. }
        ),
        "{:?}",
        state.vector.upstream_state
    );
}

#[test]
fn a_self_referential_remote_resolves_against_the_repo_not_the_cwd() {
    // `branch.<name>.remote = .` means "this repository": the merge ref is a
    // local branch, and `ls-remote .` only answers when it runs from inside
    // the repo - from the caller's cwd it would fail or read another repo.
    let f = FixtureRepo::new("origin");
    f.branch_with_commits("stacked", 1, false);
    f.add_worktree("stacked", Some("stacked"));
    f.git(&f.main, &["config", "branch.stacked.remote", "."]);
    f.git(
        &f.main,
        &["config", "branch.stacked.merge", "refs/heads/main"],
    );

    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("stacked"))
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());
    assert!(
        matches!(state.vector.upstream_state, UpstreamState::Tracked { .. }),
        "{:?}",
        state.vector.upstream_state
    );
    assert!(state.base.is_known(), "{:?}", state.base);
}

#[test]
fn a_worktree_path_with_a_newline_resolves_in_full() {
    // `worktree list --porcelain` emits paths raw: a newline in the
    // directory name splits the record's `worktree` line in two, which is
    // why the listing is read with -z. Asserted through the anchor itself:
    // a truncated path would either not match here or, worse, point at a
    // sibling directory and attribute its state.
    let f = FixtureRepo::new("origin");
    let weird = f.dir.join("wt-with\nnewline");
    f.git(
        f.main.as_path(),
        &["worktree", "add", weird.to_str().unwrap(), "--detach"],
    );
    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let weird = weird.canonicalize().expect("the worktree exists");
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| matches!(a, Anchor::Worktree { path, .. } if path == &weird))
        .expect("the newline worktree resolves by its full path");
    let state = vector::collect(&repo, &anchor, quiet());
    assert_eq!(state.vector.dirty, Evidence::Known(false));
}

#[test]
fn a_url_valued_remote_still_feeds_forge_routing() {
    // `branch.<name>.remote` may be the URL itself rather than a named
    // remote: there is no `remote.<name>.url` to look up, but the URL is
    // right there for the forge to route on. Port 1 on localhost refuses
    // the ls-remote instantly, so nothing network-bound runs.
    let f = FixtureRepo::new("origin");
    f.branch_with_commits("feat", 1, true);
    f.add_worktree("feat", Some("feat"));
    f.git(
        f.main.as_path(),
        &[
            "config",
            "branch.feat.remote",
            "https://127.0.0.1:1/o/r.git",
        ],
    );
    f.git(
        f.main.as_path(),
        &["config", "branch.feat.merge", "refs/heads/feat"],
    );
    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("feat"))
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());
    assert_eq!(
        state.remote_url.as_deref(),
        Some("https://127.0.0.1:1/o/r.git")
    );
    // The remote itself is unreachable: upstream evidence stays Unknown.
    assert!(matches!(
        state.vector.upstream_state,
        UpstreamState::Unknown(_)
    ));
}

#[test]
fn fixture_commands_are_isolated_from_the_ambient_git_environment() {
    // An ambient GIT_DIR or GIT_WORK_TREE overrides `-C` discovery: without
    // an explicit removal the fixture's mutations would land in whatever
    // live repository the environment names.
    let dir = TempDir::new("env-isolation");
    let cmd = support::fixture::command(Some(dir.path()), &["status"]);
    let envs: std::collections::HashMap<&std::ffi::OsStr, Option<&std::ffi::OsStr>> =
        cmd.get_envs().collect();
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_QUARANTINE_PATH",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        assert_eq!(envs.get(std::ffi::OsStr::new(var)), Some(&None), "{var}");
    }
}

#[test]
fn the_reads_leave_the_repository_untouched() {
    let f = standard();
    let refs_before = f.git(f.main.as_path(), &["for-each-ref"]);

    let states = collect_all(&f);
    for state in states.values() {
        verdict::cleanup(state, &no_item());
    }

    assert_eq!(f.git(f.main.as_path(), &["for-each-ref"]), refs_before);
    // No lock file may be left behind: GIT_OPTIONAL_LOCKS=0 is the contract.
    let locks: Vec<_> = walk(&f.main.join(".git"))
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "lock"))
        .collect();
    assert!(locks.is_empty(), "lock files left behind: {locks:?}");
    // And the worktrees' dirty state is read, not produced.
    assert!(
        f.git(f.dir.join("wt-dirty").as_path(), &["status", "--porcelain"])
            .contains("untracked.txt")
    );
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[test]
fn a_branch_deleted_mid_read_fails_closed() {
    // The anchor names a branch that no longer exists: every count is
    // unknown, and both actions fail closed rather than trusting a guess.
    let f = FixtureRepo::new("origin");
    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = Anchor::Branch {
        name: "ghost".to_owned(),
    };
    let state = vector::collect(&repo, &anchor, quiet());
    assert!(state.vector.commits_ahead_of_base.reason().is_some());
    assert!(state.vector.landed.reason().is_some());
    assert!(state.vector.unpushed_commits.reason().is_some());
    let (removal, deletion) = verdict::cleanup(&state, &no_item());
    assert_eq!(removal.verdict, Verdict::NotApplicable);
    assert_eq!(deletion.verdict, Verdict::Blocked);
    assert_eq!(
        deletion.reasons.len(),
        2,
        "unpushed and landing are both unproven"
    );
}

#[test]
fn multiple_remotes_and_a_missing_local_base_ref_fail_closed() {
    // With two remotes and no upstream, there is no safe base to pick.
    let f = FixtureRepo::new("origin");
    f.branch_with_commits("feat", 1, false);
    f.add_worktree("feat", Some("feat"));
    f.git(&f.main, &["remote", "add", "mirror", "/nonexistent.git"]);
    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("feat"))
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());
    assert!(
        state
            .base
            .reason()
            .unwrap()
            .contains("2 remotes configured"),
        "{:?}",
        state.base
    );

    // The remote HEAD exists and is advertised, but the local tracking ref
    // it names was never fetched: still no base.
    let f = FixtureRepo::new("origin");
    f.branch_with_commits("feat", 1, true);
    f.add_worktree("feat", Some("feat"));
    f.git(&f.main, &["update-ref", "-d", "refs/remotes/origin/main"]);
    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("feat"))
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());
    assert!(
        state.base.reason().unwrap().contains("not fetched locally"),
        "{:?}",
        state.base
    );
}

#[test]
fn an_unfetched_remote_leaves_no_local_base_evidence() {
    // The remote exists in config but was never fetched: there is no local
    // remote HEAD to fall back on, and ls-remote cannot reach it.
    let f = FixtureRepo::new("origin");
    f.branch_with_commits("feat", 1, false);
    f.add_worktree("feat", Some("feat"));
    f.git(&f.main, &["remote", "add", "shadow", "/nonexistent.git"]);
    f.git(&f.main, &["config", "branch.feat.remote", "shadow"]);
    f.git(&f.main, &["config", "branch.feat.merge", "refs/heads/feat"]);
    let repo = git::Repo::discover(f.main.as_path()).unwrap().unwrap();
    let anchor = vector::anchors(&repo)
        .unwrap()
        .into_iter()
        .find(|a| a.branch() == Some("feat"))
        .unwrap();
    let state = vector::collect(&repo, &anchor, quiet());
    assert!(
        state
            .base
            .reason()
            .unwrap()
            .contains("unreachable and no local remote HEAD"),
        "{:?}",
        state.base
    );
}
