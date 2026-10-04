//! Branch incarnations, touches and history end to end: a deleted and
//! recreated ref is a new incarnation (`#N`), the ref's own reflog
//! separates what polling could not see and fails closed on what it
//! cannot order, touch intervals scope conversations to exactly one
//! incarnation, and `h` reveals the excluded history - all on disposable
//! fixtures: scratch git repositories, temp `$HOME`s, no live state.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use agent_sessions::runtime::Runtime;
use agent_sessions::snapshot::{Collector, Snapshot, WorkRow, to_json};
use agent_sessions::store::{
    BranchRecord, Confidence, ContinuityEvidence, DatedTouch, LifecycleInputs, ObservedRef,
    RefCreationEvidence, Store, TouchPlacement, TouchProvenance,
};
use agent_sessions::tui::{App, Key};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::fixture::{self, FixtureRepo};
use support::tempdir::TempDir;

const CONV: &str = "8f423bbb-1111-2222-3333-444444444444";
const OTHER: &str = "02aa0bbb-1111-2222-3333-444444444444";
const THIRD: &str = "33cc0bbb-1111-2222-3333-444444444444";
/// The anchor conversation: it roots the fixture repo in scope - a repo
/// only collects when a conversation resolves into it.
const ANCHOR: &str = "aa000000-1111-2222-3333-444444444444";

/// A world: temp `$HOME` holding `.claude` and `state/agent-sessions`,
/// plus the fixture repo under test.
struct World {
    home: TempDir,
    repo: FixtureRepo,
}

fn world() -> World {
    let world = World {
        home: TempDir::new("incarnations"),
        repo: FixtureRepo::new("origin"),
    };
    transcript(&world.home, ANCHOR, &world.repo.main);
    world
}

fn claude(home: &TempDir) -> PathBuf {
    home.join(".claude")
}

fn store_dir(home: &TempDir) -> PathBuf {
    home.join("state/agent-sessions")
}

fn store(home: &TempDir) -> Store {
    Store::open(store_dir(home))
}

/// The repo's canonical identity: the common dir, symlinks resolved.
fn repo_id(repo: &FixtureRepo) -> String {
    std::fs::canonicalize(repo.main.join(".git"))
        .unwrap()
        .display()
        .to_string()
}

/// The ref's branch-reflog file in the clone's common dir.
fn reflog(repo: &FixtureRepo, name: &str) -> PathBuf {
    repo.main.join(format!(".git/logs/refs/heads/{name}"))
}

/// `git` in the repo's main checkout.
fn git(repo: &FixtureRepo, args: &[&str]) {
    repo.git(repo.main.as_path(), args);
}

/// `git` with a pinned committer clock - the reflog's epoch is that
/// clock, so branch creation dates are deterministic.
fn git_dated(repo: &FixtureRepo, date: &str, args: &[&str]) {
    let out = fixture::command(Some(repo.main.as_path()), args)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn collect(world: &World) -> Snapshot {
    let runtime = Runtime::observe_over(&[]);
    Collector::new(claude(&world.home))
        .with_store(store_dir(&world.home))
        .collect(&runtime, None)
}

/// One transcript conversation rooted at `cwd`.
fn transcript(home: &TempDir, id: &str, cwd: &Path) {
    let projects = home.join(".claude/projects/t");
    fs::create_dir_all(&projects).expect("mkdir");
    fs::write(
        projects.join(format!("{id}.jsonl")),
        support::claude_turn(id, cwd, "the task"),
    )
    .expect("transcript writes");
}

/// Append one more record to `id`'s transcript: the conversation kept
/// working, from `cwd` in a session whose project is `cwd` too.
fn turn(home: &TempDir, id: &str, cwd: &Path) {
    turn_from(home, id, cwd, cwd);
}

/// The same, run from `cwd` while the session's project is `project`.
fn turn_from(home: &TempDir, id: &str, cwd: &Path, project: &Path) {
    use std::io::Write;
    let path = home.join(format!(".claude/projects/t/{id}.jsonl"));
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("the transcript");
    f.write_all(support::claude_record(id, cwd, project, "more").as_bytes())
        .expect("the turn appends");
}

/// A live Claude conversation at `cwd`: a running process, its session
/// file and its transcript. The process dies with the guard.
struct Live(std::process::Child);

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn live(home: &TempDir, id: &str, cwd: &Path) -> Live {
    transcript(home, id, cwd);
    Live(support::live_claude(home, id, "idle", cwd))
}

/// The conversation's touches of one provenance, in row order.
fn touches_by(
    c: &agent_sessions::snapshot::ConversationRow,
    provenance: TouchProvenance,
) -> Vec<&agent_sessions::snapshot::TouchRow> {
    c.touches
        .iter()
        .filter(|t| t.provenance == provenance)
        .collect()
}

/// The work row named `name`.
fn work<'a>(snapshot: &'a Snapshot, name: &str) -> &'a WorkRow {
    snapshot
        .work
        .iter()
        .find(|w| w.name == name)
        .unwrap_or_else(|| panic!("no work row {name}"))
}

/// The conversation row by session id.
fn conversation<'a>(
    snapshot: &'a Snapshot,
    id: &str,
) -> &'a agent_sessions::snapshot::ConversationRow {
    snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == id)
        .unwrap_or_else(|| panic!("no conversation {id}"))
}

/// The persisted record for `(repo, name)`'s active incarnation.
fn record(world: &World, name: &str) -> Option<BranchRecord> {
    store(&world.home)
        .load()
        .work
        .branch(&repo_id(&world.repo), name)
        .cloned()
}

/// `app` rendered at `w`x`h`, as text.
fn render(app: &App, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|f| app.render(f)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let line: String = (0..w).map(|x| buffer[(x, y)].symbol()).collect();
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn press(app: &mut App, keys: &[Key]) {
    for &key in keys {
        app.key(key);
    }
}

/// Press `j` on [2] until the [3] title names `needle` - row order is
/// section-derived, so the test walks rather than assuming positions.
fn until_conv_title(app: &mut App, needle: &str) -> String {
    for _ in 0..12 {
        let text = render(app, 200, 30);
        if text.contains(needle) {
            return text;
        }
        app.key(Key::Char('j'));
    }
    render(app, 200, 30)
}

/// The same, walking back with `k`.
fn until_conv_title_back(app: &mut App, needle: &str) -> String {
    for _ in 0..12 {
        let text = render(app, 200, 30);
        if text.contains(needle) {
            return text;
        }
        app.key(Key::Char('k'));
    }
    render(app, 200, 30)
}

/// A ref observation for a direct `sync_repo` drive: `name` with the
/// lifecycle evidence spelled out.
fn obs(name: &str, head: Option<&str>, creation: Option<RefCreationEvidence>) -> ObservedRef {
    ObservedRef {
        name: name.to_owned(),
        head: head.map(str::to_owned),
        creation,
        renamed_from: None,
        rewritten: false,
        inputs: LifecycleInputs::default(),
    }
}

/// The same, carrying the `Branch: renamed` source.
fn obs_renamed(
    name: &str,
    head: Option<&str>,
    creation: Option<RefCreationEvidence>,
    renamed_from: &str,
) -> ObservedRef {
    let mut o = obs(name, head, creation);
    o.renamed_from = Some(renamed_from.to_owned());
    o
}

/// A placement for a direct `sync_touches` drive.
fn placement(conversation: &str, branch: &str, head: &str) -> TouchPlacement {
    TouchPlacement {
        conversation: conversation.to_owned(),
        branch: branch.to_owned(),
        head: head.to_owned(),
        provenance: TouchProvenance::Cwd,
        confidence: Confidence::Exact,
    }
}

#[test]
fn a_delete_and_recreate_opens_a_new_incarnation_and_keeps_the_old() {
    let world = world();
    git(&world.repo, &["branch", "feat"]);
    let first = collect(&world);
    let old_id = work(&first, "feat").identity.clone().unwrap();
    assert_eq!(work(&first, "feat").incarnation.as_ref().unwrap().number, 1);

    // Observed gone: the record closes at its pass.
    git(&world.repo, &["branch", "-D", "feat"]);
    let gone = collect(&world);
    assert!(!gone.work.iter().any(|w| w.name == "feat"));

    // Observed back: a new id under the same name - never a merge with
    // the closed one - and the closed one is excluded history.
    git(&world.repo, &["branch", "feat"]);
    let back = collect(&world);
    let feat = work(&back, "feat");
    assert_ne!(feat.identity.as_deref(), Some(old_id.as_str()));
    let inc = feat.incarnation.as_ref().expect("the active incarnation");
    assert_eq!(inc.number, 2);
    assert!(!inc.excluded);
    assert_eq!(feat.same_name_history.len(), 1);
    let history = &feat.same_name_history[0];
    assert_eq!(history.id, old_id);
    assert_eq!(history.number, 1);
    assert!(history.excluded);
    assert!(history.ended_at.is_some());
    // The persisted records tell the same story.
    let work = store(&world.home).load().work;
    assert!(work.branches[&old_id].ended_at.is_some());
    assert_eq!(
        work.branch(&repo_id(&world.repo), "feat").unwrap().id,
        feat.identity.clone().unwrap()
    );
}

#[test]
fn a_missed_delete_recreate_splits_on_changed_reflog_creation() {
    let world = world();
    // The first incarnation is created at a pinned past date so a
    // recreate now is provably a different, newer creation.
    git_dated(&world.repo, "@1500000000 +0000", &["branch", "feat"]);
    collect(&world);
    let old = record(&world, "feat").unwrap();
    assert_eq!(
        old.creation_evidence.as_ref().map(|c| c.at_ms),
        Some(1_500_000_000_000)
    );
    // Between polls: delete and recreate - the ref never left the set.
    git(&world.repo, &["branch", "-D", "feat"]);
    git(&world.repo, &["branch", "feat"]);
    let back = collect(&world);
    let feat = work(&back, "feat");
    let new = record(&world, "feat").unwrap();
    assert_ne!(new.id, old.id, "a newer creation is a new incarnation");
    assert_eq!(new.id, feat.identity.clone().unwrap());
    let closed = &store(&world.home).load().work.branches[&old.id];
    assert!(
        closed.ended_at.is_some(),
        "the old record closed at the pass"
    );
    assert_eq!(feat.same_name_history[0].id, old.id);
}

#[test]
fn a_creation_it_cannot_order_fails_closed_and_serializes_ambiguous() {
    let world = world();
    git(&world.repo, &["branch", "feat"]);
    collect(&world);
    let old = record(&world, "feat").unwrap();
    let stored = old.creation_evidence.clone().unwrap();
    // Rewrite the null-old entry to an *older* epoch: a creation that
    // differs but cannot be ordered after the stored one - the boundary
    // is detectable, the side is not.
    let log = reflog(&world.repo, "feat");
    let text = fs::read_to_string(&log).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    lines[0] = lines[0].replacen(
        &format!(" {}", stored.at_ms / 1000),
        &format!(" {}", stored.at_ms / 1000 - 1000),
        1,
    );
    fs::write(&log, lines.join("\n") + "\n").unwrap();
    collect(&world);
    let new = record(&world, "feat").unwrap();
    assert_ne!(new.id, old.id, "an unprovable boundary still separates");
    assert_eq!(new.continuity_evidence, ContinuityEvidence::Ambiguous);
    assert!(
        store(&world.home).load().work.branches[&old.id]
            .ended_at
            .is_some()
    );
    // A missing reflog is no boundary at all: with nothing to compare,
    // ordinary observation continues the same record.
    fs::remove_file(&log).unwrap();
    collect(&world);
    let still = record(&world, "feat").unwrap();
    assert_eq!(still.id, new.id, "absent evidence continues, never splits");
}

#[test]
fn a_force_pushed_tip_is_continuous_not_a_new_incarnation() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, false);
    // The first pass is a first observation; the next confirms it.
    collect(&world);
    collect(&world);
    let before = record(&world, "feat").unwrap();
    assert_eq!(
        before.continuity_evidence,
        ContinuityEvidence::SameReflogCreation
    );
    // Rewrite the tip onto a commit that does not descend from it - the
    // local shape a rebase or reset leaves for a force-push to publish -
    // without touching the creation line.
    world.repo.commit(&world.repo.main, "new.txt", "x", "new");
    let tip = world.repo.git(&world.repo.main, &["rev-parse", "main"]);
    git(&world.repo, &["update-ref", "refs/heads/feat", tip.trim()]);
    collect(&world);
    let after = record(&world, "feat").unwrap();
    assert_eq!(after.id, before.id);
    assert_eq!(after.continuity_evidence, ContinuityEvidence::ForcePush);
    // A later fast-forward keeps the proven rewrite: the label says the
    // identity held across one.
    let wt = world.repo.add_worktree("feat", Some("feat"));
    world.repo.commit(&wt, "more.txt", "y", "more");
    collect(&world);
    let later = record(&world, "feat").unwrap();
    assert_eq!(later.id, before.id);
    assert_eq!(later.continuity_evidence, ContinuityEvidence::ForcePush);
}

#[test]
fn ordinary_commits_fast_forward_under_the_same_creation() {
    let world = world();
    git(&world.repo, &["branch", "feat"]);
    collect(&world);
    let before = record(&world, "feat").unwrap();
    // Plain work on the branch moves the tip forward: the same reflog
    // creation, never a force-push.
    let wt = world.repo.add_worktree("feat", Some("feat"));
    world.repo.commit(&wt, "work.txt", "w", "plain work");
    let snapshot = collect(&world);
    let after = record(&world, "feat").unwrap();
    assert_eq!(after.id, before.id);
    assert_eq!(
        after.continuity_evidence,
        ContinuityEvidence::SameReflogCreation
    );
    assert_eq!(
        after.head.as_deref(),
        Some(world.repo.git(&wt, &["rev-parse", "feat"]).trim())
    );
    assert_eq!(
        work(&snapshot, "feat")
            .incarnation
            .as_ref()
            .unwrap()
            .continuity,
        ContinuityEvidence::SameReflogCreation
    );
}

#[test]
fn a_quiet_pass_rewrites_nothing_yet_reports_its_observation() {
    let world = world();
    git(&world.repo, &["branch", "feat"]);
    let mut collector = Collector::new(claude(&world.home)).with_store(store_dir(&world.home));
    // The second pass settles the first observation's label; the third
    // has nothing left to write.
    collector.collect(&Runtime::observe_over(&[]), None);
    collector.collect(&Runtime::observe_over(&[]), None);
    let file = store_dir(&world.home).join("work.json");
    let bytes = fs::read(&file).unwrap();
    let stored = record(&world, "feat").unwrap().last_observed_at;
    // Cross a whole second so the reported observation must move; each
    // pass observes its own runtime instant.
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let snapshot = collector.collect(&Runtime::observe_over(&[]), None);
    assert!(fs::read(&file).unwrap() == bytes, "a quiet pass wrote");
    let reported = work(&snapshot, "feat")
        .incarnation
        .as_ref()
        .unwrap()
        .last_observed_at;
    assert!(reported > stored / 1000, "{reported} vs {stored}");
}

#[test]
fn a_proven_rename_moves_the_record_an_unproven_one_separates() {
    let world = world();
    git(&world.repo, &["branch", "feat"]);
    let first = collect(&world);
    let before = record(&world, "feat").unwrap();

    // `git branch -m` moves the reflog - creation line and all - and
    // leaves the `Branch: renamed` line: identity survives.
    git(&world.repo, &["branch", "-m", "feat", "feat-renamed"]);
    collect(&world);
    let moved = record(&world, "feat-renamed").expect("the renamed record");
    assert_eq!(moved.id, before.id);
    assert_eq!(moved.continuity_evidence, ContinuityEvidence::ProvenRename);
    assert_eq!(moved.first_observed_at, before.first_observed_at);
    assert!(record(&world, "feat").is_none());
    assert_eq!(work(&first, "feat").name, "feat");

    // The same move without the line - a delete plus a create - is two
    // incarnations, never a preserved identity.
    git(&world.repo, &["branch", "-D", "feat-renamed"]);
    git(&world.repo, &["branch", "feat-spawned"]);
    collect(&world);
    let spawned = record(&world, "feat-spawned").unwrap();
    assert_ne!(spawned.id, before.id);
    let work = store(&world.home).load().work;
    let renamed = &work.branches[&before.id];
    assert!(renamed.ended_at.is_some(), "the renamed record closed too");
    assert_eq!(renamed.ref_name, "feat-renamed");
}

#[test]
fn a_stale_rename_line_never_steals_a_recreated_name() {
    let world = world();
    git(&world.repo, &["branch", "old"]);
    collect(&world);
    let old_id = record(&world, "old").unwrap().id;

    // `git branch -m` moves the record, and `new`'s reflog keeps the
    // `Branch: renamed` line forever.
    git(&world.repo, &["branch", "-m", "old", "new"]);
    collect(&world);
    let moved = record(&world, "new").unwrap();
    assert_eq!(moved.id, old_id);

    // Recreate `old` beside `new`: the next pass observes BOTH names,
    // and `new`'s stale rename evidence must not move `old`'s fresh
    // record onto `new`'s taken destination. Two incarnations, both
    // live, with `new`'s id untouched.
    git(&world.repo, &["branch", "old"]);
    collect(&world);
    let old_now = record(&world, "old").expect("old's own incarnation");
    let new_still = record(&world, "new").expect("new's incarnation");
    assert_ne!(old_now.id, old_id, "recreated old is a new incarnation");
    assert_eq!(new_still.id, old_id, "new keeps the moved record");
    assert_ne!(old_now.id, new_still.id);
    // The persisted lookups point the right way too.
    let work = store(&world.home).load().work;
    let repo = repo_id(&world.repo);
    assert_eq!(work.branch(&repo, "old").unwrap().id, old_now.id);
    assert_eq!(work.branch(&repo, "new").unwrap().id, old_id);
    // One pass on, `old` was already active when the stale line showed:
    // the observed old ref still never moves under `new`.
    collect(&world);
    let work = store(&world.home).load().work;
    assert_eq!(work.branch(&repo, "old").unwrap().id, old_now.id);
    assert_eq!(work.branch(&repo, "new").unwrap().id, old_id);
}

#[test]
fn a_symlinked_checkout_lands_on_the_same_repo_and_incarnation() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    // One conversation through the real path, one through a symlink of it:
    // both resolve to the same canonical repo and incarnation.
    let link = world.home.join("linked");
    std::os::unix::fs::symlink(&wt, &link).expect("symlink");
    transcript(&world.home, CONV, &wt);
    transcript(&world.home, OTHER, &link);
    let snapshot = collect(&world);
    let real = conversation(&snapshot, CONV);
    let linked = conversation(&snapshot, OTHER);
    assert_eq!(real.repo, Some(repo_id(&world.repo)));
    assert_eq!(linked.repo, real.repo);
    assert_eq!(
        linked.touches[0].incarnation_id,
        real.touches[0].incarnation_id
    );
}

#[test]
fn the_json_carries_ids_numbers_intervals_and_evidence() {
    let world = world();
    git(&world.repo, &["branch", "feat"]);
    collect(&world);
    // Recreated between passes at the same tip: only the reflog creation
    // stamp tells the two apart, so it must fall in a later second than
    // the first creation - left to the clock, both often share one.
    git(&world.repo, &["branch", "-D", "feat"]);
    let later = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 5;
    git_dated(&world.repo, &format!("@{later} +0000"), &["branch", "feat"]);
    let snapshot = collect(&world);
    let json: serde_json::Value = serde_json::from_str(&to_json(&snapshot).unwrap()).unwrap();
    let feat = json["work"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "feat")
        .expect("the feat row");
    assert_eq!(feat["incarnation"]["number"], 2);
    assert_eq!(feat["incarnation"]["excluded"], false);
    let history = &feat["same_name_history"][0];
    assert_eq!(history["number"], 1);
    assert_eq!(history["excluded"], true);
    assert!(history["ended_at"].is_u64());
    // The persisted touch serializes its full shape: incarnation id,
    // head, interval, provenance, confidence.
    let file: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(store_dir(&world.home).join("work.json")).unwrap(),
    )
    .unwrap();
    let branch = &file["data"]["branches"][feat["identity"].as_str().unwrap()];
    assert_eq!(branch["ref_name"], "feat");
    assert!(branch["first_observed_at"].is_u64());
    assert!(branch["last_observed_at"].is_u64());
    assert!(branch["creation_evidence"]["head"].is_string());
    assert!(branch["creation_evidence"]["at_ms"].is_u64());
    assert!(branch["continuity_evidence"].is_string());
}

#[test]
fn the_store_keeps_ninety_days_or_touched_history_whichever_is_longer() {
    let dir = TempDir::new("incarnation-retention");
    let store = Store::open(dir.join("store"));
    let repo = "/repo/.git";
    let day = 86_400_000_u64;
    store
        .sync_repo(
            repo,
            &[obs("kept", None, None), obs("dropped", None, None)],
            1_000,
        )
        .unwrap();
    let work = store.load().work;
    let kept = work.branch(repo, "kept").unwrap().id.clone();
    let dropped = work.branch(repo, "dropped").unwrap().id.clone();
    // `kept` holds a conversation's placement; `dropped` has none.
    store
        .sync_touches(&[placement("claude:s1", &kept, "aaa")], &[], 1_500)
        .unwrap();
    // Both close together at 2_000 - the touch interval closes with its
    // incarnation, not at some later absence.
    store.sync_repo(repo, &[], 2_000).unwrap();
    let work = store.load().work;
    assert_eq!(work.touches[0].valid_until, Some(2_000));
    // Ninety-one days on: the untouched record prunes, the touched one
    // stays - a conversation still names it.
    store
        .sync_repo(repo, &[obs("main", None, None)], 2_000 + 91 * day)
        .unwrap();
    let work = store.load().work;
    assert!(work.branches.contains_key(&kept));
    assert!(!work.branches.contains_key(&dropped));
}

#[test]
fn sync_repo_separates_what_it_cannot_prove_and_keeps_what_it_can() {
    let dir = TempDir::new("incarnation-store");
    let store = Store::open(dir.join("store"));
    let repo = "/repo/.git";
    let creation = |head: &str, at_ms: u64| {
        Some(RefCreationEvidence {
            head: head.to_owned(),
            at_ms,
        })
    };

    // A creation that differs but is not provably newer - same stamp,
    // different head - is a boundary without a proven side: separated,
    // labelled `ambiguous`.
    store
        .sync_repo(
            repo,
            &[obs("feat", Some("a"), creation("a", 5_000))],
            10_000,
        )
        .unwrap();
    let first = store.load().work.branch(repo, "feat").unwrap().id.clone();
    store
        .sync_repo(
            repo,
            &[obs("feat", Some("b"), creation("b", 5_000))],
            11_000,
        )
        .unwrap();
    let work = store.load().work;
    let current = work.branch(repo, "feat").unwrap();
    assert_ne!(current.id, first);
    assert_eq!(current.continuity_evidence, ContinuityEvidence::Ambiguous);
    assert_eq!(work.branches[&first].ended_at, Some(11_000));

    // A record opened before creation evidence existed splits too when
    // the observed creation provably postdates its first sighting.
    store
        .sync_repo(repo, &[obs("early", None, None)], 1_000)
        .unwrap();
    let early = store.load().work.branch(repo, "early").unwrap().id.clone();
    store
        .sync_repo(repo, &[obs("early", None, creation("c", 2_000))], 3_000)
        .unwrap();
    let work = store.load().work;
    let current = work.branch(repo, "early").unwrap();
    assert_ne!(current.id, early);
    assert_eq!(work.branches[&early].ended_at, Some(3_000));

    // Missing evidence where none is owed: a record with creation
    // evidence survives a pass that brings neither creation nor tip.
    store
        .sync_repo(
            repo,
            &[obs("quiet", Some("h"), creation("h", 6_000))],
            20_000,
        )
        .unwrap();
    let quiet = store.load().work.branch(repo, "quiet").unwrap().id.clone();
    store
        .sync_repo(repo, &[obs("quiet", None, None)], 21_000)
        .unwrap();
    assert_eq!(store.load().work.branch(repo, "quiet").unwrap().id, quiet);

    // A rename line naming a ref with no active record moves nothing:
    // the new name opens its own first-observed record.
    store
        .sync_repo(
            repo,
            &[obs_renamed(
                "fresh",
                Some("f"),
                creation("f", 9_000),
                "ghost",
            )],
            30_000,
        )
        .unwrap();
    assert_eq!(
        store
            .load()
            .work
            .branch(repo, "fresh")
            .unwrap()
            .continuity_evidence,
        ContinuityEvidence::FirstObservation
    );

    // A renamed record that never carried creation evidence adopts the
    // moved log's - and keeps its id and label through the move.
    store
        .sync_repo(repo, &[obs("old-name", None, None)], 31_000)
        .unwrap();
    let old = store
        .load()
        .work
        .branch(repo, "old-name")
        .unwrap()
        .id
        .clone();
    store
        .sync_repo(
            repo,
            &[obs_renamed(
                "new-name",
                Some("n"),
                creation("n", 900),
                "old-name",
            )],
            32_000,
        )
        .unwrap();
    let work = store.load().work;
    let moved = work.branch(repo, "new-name").unwrap();
    assert_eq!(moved.id, old);
    assert_eq!(moved.head.as_deref(), Some("n"));
    assert_eq!(moved.continuity_evidence, ContinuityEvidence::ProvenRename);
    assert_eq!(moved.creation_evidence.as_ref().map(|c| c.at_ms), Some(900));
    assert!(work.branch(repo, "old-name").is_none());

    // A rename whose tip this pass did not prove keeps the last proven
    // head: missing evidence erases nothing.
    store
        .sync_repo(
            repo,
            &[obs_renamed("third-name", None, None, "new-name")],
            33_000,
        )
        .unwrap();
    let work = store.load().work;
    let moved = work.branch(repo, "third-name").unwrap();
    assert_eq!(moved.id, old);
    assert_eq!(moved.head.as_deref(), Some("n"));

    // A stale rename claiming a taken destination moves nothing either:
    // `dest` keeps its own record, `gone-src` simply closes.
    store
        .sync_repo(
            repo,
            &[obs("gone-src", None, None), obs("dest", None, None)],
            42_000,
        )
        .unwrap();
    let dest_id = store.load().work.branch(repo, "dest").unwrap().id.clone();
    store
        .sync_repo(repo, &[obs_renamed("dest", None, None, "gone-src")], 43_000)
        .unwrap();
    let work = store.load().work;
    assert_eq!(work.branch(repo, "dest").unwrap().id, dest_id);
    assert!(work.branch(repo, "gone-src").is_none());

    // A retained record adopts its first proven creation value too -
    // observed at a time consistent with the record's sighting, so no
    // boundary is detected.
    store
        .sync_repo(repo, &[obs("late", None, None)], 40_000)
        .unwrap();
    let late = store.load().work.branch(repo, "late").unwrap().id.clone();
    store
        .sync_repo(repo, &[obs("late", None, creation("l", 39_000))], 41_000)
        .unwrap();
    let work = store.load().work;
    let late_record = work.branch(repo, "late").unwrap();
    assert_eq!(late_record.id, late);
    assert_eq!(
        late_record.creation_evidence.as_ref().map(|c| c.at_ms),
        Some(39_000)
    );
    assert_eq!(late_record.last_observed_at, 41_000);
}

#[test]
fn sync_touches_is_append_only_and_idempotent() {
    let dir = TempDir::new("incarnation-touches");
    let store = Store::open(dir.join("store"));
    let repo = "/repo/.git";
    store
        .sync_repo(repo, &[obs("feat", Some("a"), None)], 1_000)
        .unwrap();
    let id = store.load().work.branch(repo, "feat").unwrap().id.clone();

    // First placement opens; the identical repeat writes nothing.
    store
        .sync_touches(&[placement("claude:s1", &id, "a")], &[], 1_500)
        .unwrap();
    let bytes = fs::read(dir.join("store/work.json")).unwrap();
    store
        .sync_touches(&[placement("claude:s1", &id, "a")], &[], 1_600)
        .unwrap();
    assert_eq!(fs::read(dir.join("store/work.json")).unwrap(), bytes);

    // A moved head on the same incarnation corrects: close, append -
    // never rewrite the earlier interval.
    store
        .sync_touches(&[placement("claude:s1", &id, "b")], &[], 2_000)
        .unwrap();
    let work = store.load().work;
    assert_eq!(work.touches.len(), 2);
    assert_eq!(work.touches[0].valid_until, Some(2_000));
    assert_eq!(work.touches[1].head.as_deref(), Some("b"));
    assert_eq!(work.touches[1].valid_from, 2_000);

    // Absence closes nothing: a pass without the conversation's
    // placement leaves the interval open.
    store.sync_touches(&[], &[], 3_000).unwrap();
    assert_eq!(store.load().work.touches[1].valid_until, None);

    // A placement on a closed incarnation lands nowhere.
    store.sync_repo(repo, &[], 4_000).unwrap();
    store
        .sync_touches(&[placement("claude:s1", &id, "b")], &[], 5_000)
        .unwrap();
    let work = store.load().work;
    assert_eq!(work.touches.len(), 2);
    assert_eq!(work.touches[1].valid_until, Some(4_000));

    // Dated provider intervals: one naming no stored record lands
    // nowhere; one on the closed incarnation is history, its end bounded
    // by the incarnation's; replaying it changes nothing.
    let dated = |branch: &str, from: u64, until: Option<u64>| DatedTouch {
        conversation: "claude:s2".to_owned(),
        branch: branch.to_owned(),
        valid_from: from,
        valid_until: until,
    };
    store
        .sync_touches(
            &[],
            &[
                dated("i-missing", 1_000, None),
                dated(&id, 1_200, Some(9_000)),
            ],
            6_000,
        )
        .unwrap();
    let work = store.load().work;
    assert_eq!(work.touches.len(), 3);
    let history = &work.touches[2];
    assert_eq!(history.branch, id);
    assert_eq!(history.provenance, TouchProvenance::ProviderBranch);
    assert_eq!(history.head, None);
    assert_eq!(history.valid_until, Some(4_000));
    let bytes = fs::read(dir.join("store/work.json")).unwrap();
    store
        .sync_touches(&[], &[dated(&id, 1_200, Some(9_000))], 7_000)
        .unwrap();
    assert!(fs::read(dir.join("store/work.json")).unwrap() == bytes);
}

#[test]
fn the_history_rows_render_excluded_and_scope_their_own_conversations() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    // convs ONE and THREE ride feat's first incarnation.
    transcript(&world.home, CONV, &wt);
    transcript(&world.home, THIRD, &wt);
    collect(&world);
    let inc1 = record(&world, "feat").unwrap().id;
    // Switch the worktree away and delete: feat#1 closes with its
    // touches. conv THREE's cwd leaves the repo entirely - its feat#1
    // interval stays its only touch ever.
    world.repo.git(&wt, &["checkout", "-b", "other"]);
    transcript(&world.home, THIRD, &world.home.path().join("elsewhere"));
    git(&world.repo, &["branch", "-D", "feat"]);
    // conv TWO sits on main the whole time - never on feat.
    transcript(&world.home, OTHER, &world.repo.main);
    collect(&world);
    // Recreate without a checkout: the second incarnation is a
    // branch-only row nobody touches.
    git(&world.repo, &["branch", "feat"]);
    let snapshot = collect(&world);
    let inc2 = work(&snapshot, "feat").identity.clone().unwrap();
    assert_ne!(inc1, inc2);
    let feat = work(&snapshot, "feat");
    assert_eq!(feat.incarnation.as_ref().unwrap().number, 2);
    assert_eq!(feat.same_name_history.len(), 1);

    // The exact-incarnation rule: feat#2 claims no open touch and no
    // session count - not conv ONE's closed feat#1 interval, not conv
    // THREE's feat#1-only history, not the repository's conversations a
    // branch-only row would count by location.
    assert_eq!(feat.live_sessions, 0, "{feat:?}");
    assert_eq!(feat.live_pids, 0, "{feat:?}");
    assert_eq!(feat.past_sessions, 0, "{feat:?}");
    for id in [CONV, OTHER, THIRD] {
        assert!(!agent_sessions::snapshot::binds(
            feat,
            conversation(&snapshot, id)
        ));
    }
    assert!(
        conversation(&snapshot, THIRD)
            .touches
            .iter()
            .all(|t| t.incarnation_id == inc1)
    );

    // `h` reveals the excluded incarnation under feat#2; without it the
    // history stays hidden. The recreated branch-only row lands in the
    // cleanup section - the `all` scope collapses it, so the repo scope
    // is where the row and its history list.
    let mut app = App::new(snapshot);
    let text = render(&app, 200, 30);
    assert!(!text.contains("excluded"), "{text}");
    press(&mut app, &[Key::Char('1'), Key::Char('j')]);
    app.key(Key::Char('h'));
    let text = render(&app, 200, 30);
    assert!(text.contains("feat#2"), "{text}");
    assert!(text.contains("feat#1"), "{text}");
    assert_eq!(text.matches("excluded").count(), 1, "{text}");

    // The excluded row is selectable; [3] titles it as excluded history
    // and scopes to exactly its touches - the two convs that rode feat#1.
    press(&mut app, &[Key::Char('2')]);
    let text = until_conv_title(&mut app, "feat#1 · excluded history");
    assert!(text.contains(&CONV[..8]), "{text}");
    assert!(text.contains(&THIRD[..8]), "{text}");
    assert!(!text.contains(&OTHER[..8]), "{text}");
    // The active row claims its own scope: current incarnation, nothing
    // bound - its historical namesakes stayed out.
    let text = until_conv_title_back(&mut app, "feat#2 · current incarnation");
    assert!(!text.contains(&CONV[..8]), "{text}");

    // `h` off while on the history row: the hidden id falls back to
    // `all` rather than retargeting whatever row now shares its index.
    let _ = until_conv_title(&mut app, "feat#1 · excluded history");
    app.key(Key::Char('h'));
    let text = render(&app, 200, 30);
    assert!(!text.contains("excluded"), "{text}");
    assert!(!text.contains("feat#1 · excluded history"), "{text}");
    // `h` back on restores the row; an active selection keeps its
    // identity across the same toggle.
    app.key(Key::Char('h'));
    let text = render(&app, 200, 30);
    assert!(text.contains("feat#1"), "{text}");
    let _ = until_conv_title(&mut app, "feat#2 · current incarnation");
    app.key(Key::Char('h'));
    let text = render(&app, 200, 30);
    assert!(!text.contains("excluded"), "{text}");
    assert!(text.contains("feat#2 · current incarnation"), "{text}");
    app.key(Key::Char('h'));

    // Back on the excluded row, a refresh keeps the selection on the
    // incarnation id itself - the history row's identity is its record,
    // not the name it shares with the active one.
    let _ = until_conv_title(&mut app, "feat#1 · excluded history");
    app.refresh(collect(&world));
    let text = render(&app, 200, 30);
    assert!(text.contains("feat#1 · excluded history"), "{text}");

    // `space` and `p` on a history row are inert: nothing is written.
    let mut app = app.with_store(store(&world.home));
    let before = fs::read(store_dir(&world.home).join("work.json")).unwrap();
    press(&mut app, &[Key::Char(' '), Key::Char('p')]);
    let text = render(&app, 200, 30);
    assert!(text.contains("excluded"), "{text}");
    assert_eq!(
        fs::read(store_dir(&world.home).join("work.json")).unwrap(),
        before
    );
}

#[test]
fn a_live_worktree_switch_closes_one_cwd_interval_and_opens_the_next() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    let _agent = live(&world.home, CONV, &wt);
    let first = collect(&world);
    let conv = conversation(&first, CONV);
    assert!(conv.running());
    let feat_id = work(&first, "feat").identity.clone().unwrap();
    let cwd = touches_by(conv, TouchProvenance::Cwd);
    assert_eq!(cwd.len(), 1, "{:?}", conv.touches);
    assert_eq!(cwd[0].incarnation_id, feat_id);
    assert_eq!(cwd[0].valid_until, None);
    assert_eq!(cwd[0].confidence, Confidence::Exact);
    assert_eq!(
        cwd[0].head.as_deref(),
        Some(world.repo.git(&wt, &["rev-parse", "feat"]).trim())
    );
    assert_eq!(conv.current_incarnation.as_deref(), Some(feat_id.as_str()));

    // The live conversation's worktree switches branch: the observed
    // interval closes at exactly the pass that opens the next one.
    world.repo.git(&wt, &["checkout", "-b", "other"]);
    let second = collect(&world);
    let conv = conversation(&second, CONV);
    let other_id = work(&second, "other").identity.clone().unwrap();
    let cwd = touches_by(conv, TouchProvenance::Cwd);
    assert_eq!(cwd.len(), 2, "{:?}", conv.touches);
    assert_eq!(cwd[0].incarnation_id, feat_id);
    assert_eq!(cwd[1].incarnation_id, other_id);
    assert_eq!(cwd[0].valid_until, Some(cwd[1].valid_from));
    assert_eq!(cwd[1].valid_until, None);
    assert_eq!(conv.current_incarnation.as_deref(), Some(other_id.as_str()));
    // A restart reproduces the same intervals - nothing rewrote history.
    let third = collect(&world);
    assert_eq!(conversation(&third, CONV).touches, conv.touches);
}

#[test]
fn a_dead_conversation_stays_on_the_branch_it_worked_on() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    transcript(&world.home, CONV, &wt);
    let first = collect(&world);
    let feat_id = work(&first, "feat").identity.clone().unwrap();
    let conv = conversation(&first, CONV);
    assert!(!conv.running());
    // Its own records place it: provider evidence, no tip.
    assert_eq!(conv.touches.len(), 1, "{:?}", conv.touches);
    assert_eq!(conv.touches[0].provenance, TouchProvenance::ProviderBranch);
    assert_eq!(conv.touches[0].incarnation_id, feat_id);
    assert_eq!(conv.touches[0].head, None);
    assert!(agent_sessions::snapshot::binds(work(&first, "feat"), conv));

    // The worktree moves on without it: where the directory points now
    // says nothing about where the finished conversation worked.
    world.repo.git(&wt, &["checkout", "-b", "other"]);
    let second = collect(&world);
    let conv = conversation(&second, CONV);
    assert_eq!(conv.touches.len(), 1, "{:?}", conv.touches);
    assert_eq!(conv.touches[0].incarnation_id, feat_id);
    assert!(agent_sessions::snapshot::binds(work(&second, "feat"), conv));
    let other = work(&second, "other");
    assert!(!agent_sessions::snapshot::binds(other, conv));
    assert_eq!(other.past_sessions, 0, "{other:?}");
}

#[test]
fn a_recreated_name_never_inherits_the_old_incarnations_conversations() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    transcript(&world.home, CONV, &wt);
    let first = collect(&world);
    let old = work(&first, "feat").identity.clone().unwrap();
    // Delete the branch out from under the worktree, then recreate it
    // there: the same path, the same name, a new incarnation.
    world.repo.git(&wt, &["checkout", "--detach"]);
    git(&world.repo, &["branch", "-D", "feat"]);
    collect(&world);
    world.repo.git(&wt, &["checkout", "-b", "feat"]);
    let snapshot = collect(&world);
    let feat = work(&snapshot, "feat");
    assert_eq!(feat.incarnation.as_ref().unwrap().number, 2);
    let conv = conversation(&snapshot, CONV);
    // The conversation keeps exactly its closed feat#1 interval: no
    // attention, session count or binding reaches feat#2.
    assert_eq!(conv.touches.len(), 1, "{:?}", conv.touches);
    assert_eq!(conv.touches[0].incarnation_id, old);
    assert!(conv.touches[0].valid_until.is_some());
    assert!(!agent_sessions::snapshot::binds(feat, conv));
    assert_eq!(feat.past_sessions, 0, "{feat:?}");
    assert_eq!(feat.live_sessions, 0, "{feat:?}");
}

#[test]
fn a_record_older_than_every_incarnation_places_nothing() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    // A record dated before the ref provably existed names some earlier
    // `feat` nobody recorded: it fails closed instead of guessing.
    let projects = world.home.join(".claude/projects/t");
    fs::create_dir_all(&projects).expect("mkdir");
    fs::write(
        projects.join(format!("{CONV}.jsonl")),
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{CONV}\",\"cwd\":\"{}\",\"gitBranch\":\"feat\",\"timestamp\":\"2020-01-01T00:00:00.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"old\"}}}}\n",
            wt.display()
        ),
    )
    .expect("transcript writes");
    let snapshot = collect(&world);
    assert!(conversation(&snapshot, CONV).touches.is_empty());
    assert_eq!(work(&snapshot, "feat").past_sessions, 0);
}

#[test]
fn a_conversation_that_followed_a_switch_records_both_from_its_own_records() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    transcript(&world.home, CONV, &wt);
    collect(&world);
    // The conversation switched the branch and kept working: its next
    // record names the new branch, and its trail closes the first
    // interval at exactly that record.
    world.repo.git(&wt, &["checkout", "-b", "other"]);
    turn(&world.home, CONV, &wt);
    let snapshot = collect(&world);
    let conv = conversation(&snapshot, CONV);
    let names: Vec<&str> = conv.touches.iter().map(|t| t.ref_name.as_str()).collect();
    assert_eq!(names, ["feat", "other"], "{:?}", conv.touches);
    assert_eq!(
        conv.touches[0].valid_until,
        Some(conv.touches[1].valid_from)
    );
    assert_eq!(conv.touches[1].valid_until, None);
    assert!(
        conv.touches
            .iter()
            .all(|t| t.provenance == TouchProvenance::ProviderBranch)
    );
    assert_eq!(conv.current_incarnation, work(&snapshot, "other").identity);
    // A restart reproduces the same intervals.
    let again = collect(&world);
    assert_eq!(conversation(&again, CONV).touches, conv.touches);
}

#[test]
fn a_record_from_another_repository_stays_on_the_projects_branch() {
    let world = world();
    let other_repo = FixtureRepo::new("origin");
    // A conversation of its own brings the other repository into scope.
    transcript(&world.home, OTHER, &other_repo.main);
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    transcript(&world.home, CONV, &wt);
    collect(&world);
    let feat_id = record(&world, "feat").unwrap().id;
    // The conversation runs a command inside the other repository: its
    // record still names the project's branch, so the evidence says
    // nothing about the other repository - no touch lands there and the
    // feat interval holds.
    turn_from(&world.home, CONV, &other_repo.main, &wt);
    let snapshot = collect(&world);
    let conv = conversation(&snapshot, CONV);
    assert_eq!(conv.touches.len(), 1, "{:?}", conv.touches);
    assert_eq!(conv.touches[0].incarnation_id, feat_id);
    assert_eq!(conv.touches[0].valid_until, None);
    assert!(conv.touches.iter().all(|t| t.repo == repo_id(&world.repo)));
}

#[test]
fn the_global_scope_lists_each_conversation_once_with_its_touch_path() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    transcript(&world.home, CONV, &wt);
    collect(&world);
    // A switch the conversation worked through gives it a
    // two-incarnation path: feat#1 -> other#1.
    world.repo.git(&wt, &["checkout", "-b", "other"]);
    turn(&world.home, CONV, &wt);
    // A second conversation sits in the main checkout - one touch.
    transcript(&world.home, OTHER, &world.repo.main);
    // A third has no resolved placement at all.
    transcript(&world.home, THIRD, &world.home.path().join("elsewhere"));
    let snapshot = collect(&world);
    let mut app = App::new(snapshot);
    let text = render(&app, 200, 30);
    assert!(text.contains("feat#1 → other#1"), "{text}");
    // One row per conversation, no matter how many incarnations it rode.
    assert_eq!(text.matches(&CONV[..8]).count(), 1, "{text}");
    assert_eq!(text.matches(&OTHER[..8]).count(), 1, "{text}");
    // The single-touch conversation carries its full context instead.
    assert!(text.contains("main#1"), "{text}");
    // Selecting one incarnation drops the context line entirely.
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    let text = render(&app, 200, 30);
    assert!(!text.contains("→"), "{text}");
    // Back on `all` the lines return - at 55 columns the rows fit and
    // nothing clips or overflows; at 200 they already did above.
    press(&mut app, &[Key::Char('k')]);
    let text = render(&app, 55, 30);
    assert!(text.lines().all(|l| l.chars().count() <= 55));
    assert!(text.contains("feat#1"), "{text}");
}

#[test]
fn a_trail_through_subdirectories_and_detached_heads_places_exactly() {
    let world = world();
    world.repo.branch_with_commits("feat", 1, true);
    let wt = world.repo.add_worktree("feat", Some("feat"));
    transcript(&world.home, CONV, &wt);
    // Work continues from a subdirectory: the same branch, one interval.
    let sub = wt.join("sub");
    fs::create_dir_all(&sub).expect("mkdir");
    turn_from(&world.home, CONV, &sub, &wt);
    // Then the project detaches: `HEAD` names no branch, so the feat
    // interval ends at that record and nothing new is placed.
    world.repo.git(&wt, &["checkout", "--detach"]);
    turn(&world.home, CONV, &wt);
    let snapshot = collect(&world);
    let conv = conversation(&snapshot, CONV);
    assert_eq!(conv.touches.len(), 1, "{:?}", conv.touches);
    assert_eq!(conv.touches[0].ref_name, "feat");
    assert!(conv.touches[0].valid_until.is_some());
    assert!(!agent_sessions::snapshot::binds(
        work(&snapshot, "feat"),
        conv
    ));
}

#[test]
fn a_live_conversation_off_any_recorded_branch_opens_no_cwd_interval() {
    let world = world();
    // Live in a project space (no branch at all) and on an unborn branch
    // (a name with no ref behind it): neither has an incarnation to touch.
    let space = world.home.join("space");
    fs::create_dir_all(&space).expect("mkdir");
    let unborn = world.home.join("unborn");
    fs::create_dir_all(&unborn).expect("mkdir");
    fixture::command(Some(&unborn), &["init", "-q", "-b", "fresh"])
        .output()
        .expect("git init");
    let _a = live(&world.home, CONV, &space);
    let _b = live(&world.home, OTHER, &unborn);
    let snapshot = collect(&world);
    for id in [CONV, OTHER] {
        let conv = conversation(&snapshot, id);
        assert!(conv.running(), "{id}");
        assert!(conv.touches.is_empty(), "{id}: {:?}", conv.touches);
    }
}
