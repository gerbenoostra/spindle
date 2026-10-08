//! The Work lifecycle end to end: a fixture repo whose branches cover every
//! section, collected through the real pipeline, rendered at both terminal
//! widths and toggled through the real store.
//!
//! Everything is disposable: scratch git repositories, a temp `$HOME`, stub
//! `gh` on a private search path - no live tmux, no real state, no network.
//! A branch's old age is fabricated the way Git records it: the committer
//! clock carries `GIT_COMMITTER_DATE`, which dates both the commits and
//! their reflog entries.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, UNIX_EPOCH};

use agent_sessions::attention::Attention;
use agent_sessions::config::{Config, Loaded};
use agent_sessions::forge::{Forge, Pipeline, WorkItem};
use agent_sessions::runtime::Runtime;
use agent_sessions::snapshot::{Collector, Snapshot, WorkKind, WorkSection, to_json};
use agent_sessions::store::{
    ActivitySource, LifecycleInputs, NormEvent, Record, Store, WorkIdentity,
};
use agent_sessions::tui::{App, Key};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::fixture::{self, FixtureRepo, Landing};
use support::tempdir::TempDir;
use support::tmux::TmuxServer;
use support::tmux_or_skip;

const NEEDS_ID: &str = "11000000-1111-2222-3333-444444444444";
const RESUME_ID: &str = "22000000-1111-2222-3333-444444444444";
const ACTIVE_ID: &str = "33000000-1111-2222-3333-444444444444";
const SPACE_ID: &str = "44000000-1111-2222-3333-444444444444";
const OTHER_ID: &str = "55000000-1111-2222-3333-444444444444";

/// A world: temp `$HOME` holding `.claude` and `state/agent-sessions`, one
/// or two fixture repos, and the live fake agent where a scenario needs one.
struct World {
    home: TempDir,
    a: FixtureRepo,
    b: FixtureRepo,
    agent: Option<Child>,
}

fn claude(home: &TempDir) -> PathBuf {
    home.join(".claude")
}

fn state(home: &TempDir) -> PathBuf {
    home.join("state/agent-sessions")
}

fn store(home: &TempDir) -> Store {
    Store::open(state(home))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn collect(world: &World) -> Snapshot {
    collect_with(world, None, None)
}

/// As `collect`, over a runtime that sees the given tmux sockets.
fn collect_on(world: &World, sockets: &[std::path::PathBuf]) -> Snapshot {
    let runtime = Runtime::observe_over(sockets);
    Collector::new(claude(&world.home))
        .with_store(state(&world.home))
        .collect(&runtime, None)
}

fn collect_with(world: &World, config: Option<Loaded>, forge: Option<Forge>) -> Snapshot {
    let runtime = Runtime::observe_over(&[]);
    let mut collector = Collector::new(claude(&world.home)).with_store(state(&world.home));
    if let Some(loaded) = config {
        collector = collector.with_config(loaded);
    }
    if let Some(forge) = forge {
        collector = collector.with_forge(forge);
    }
    collector.collect(&runtime, None)
}

/// One transcript conversation rooted at `cwd` - a dead conversation: no
/// session record, so it is resumable but not live.
fn transcript(home: &TempDir, id: &str, cwd: &Path) {
    let projects = home.join(".claude/projects/t");
    fs::create_dir_all(&projects).expect("mkdir");
    fs::write(
        projects.join(format!("{id}.jsonl")),
        support::claude_turn(id, cwd, "the task"),
    )
    .expect("transcript writes");
}

/// A transcript whose one record carries no timestamp: a conversation
/// that proves no time of activity at all.
fn undated_transcript(home: &TempDir, id: &str, cwd: &Path) {
    let projects = home.join(".claude/projects/t");
    fs::create_dir_all(&projects).expect("mkdir");
    fs::write(
        projects.join(format!("{id}.jsonl")),
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"the task\"}}}}\n",
            cwd.display()
        ),
    )
    .expect("transcript writes");
}

/// One journal event for `session` in the world's store.
fn event(home: &TempDir, session: &str, native: &str, event: NormEvent, reason: &str) {
    let mut r = Record::new("claude", session, native);
    r.event = Some(event);
    r.reason = Some(reason.to_owned());
    store(home).append(r).expect("append");
}

/// A branch that reads `age` old: every commit carries the backdated
/// committer clock, and the reflog entries take it too - `reflog_times`
/// reads the entries' own dates.
fn old_pushed_branch(repo: &FixtureRepo, branch: &str, age: Duration) {
    let epoch = now() - age.as_secs();
    let date = format!("@{epoch} +0000");
    let git = |dir: &Path, args: &[&str]| {
        let out = fixture::command(Some(dir), args)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_AUTHOR_DATE", &date)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} in {dir:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let scratch = repo.dir.join(format!("scratch-{branch}"));
    git(
        &repo.main,
        &["worktree", "add", "-b", branch, scratch.to_str().unwrap()],
    );
    fs::write(scratch.join("old.txt"), "old").expect("write");
    git(&scratch, &["add", "old.txt"]);
    git(&scratch, &["commit", "-m", "old"]);
    // `--force` because a recreated branch pushes over the old tip.
    git(
        &scratch,
        &["push", "-u", "--force", &repo.remote_name, branch],
    );
    git(
        &repo.main,
        &["worktree", "remove", "--force", scratch.to_str().unwrap()],
    );
}

/// The repo `a` work row named `name`.
fn work<'a>(snapshot: &'a Snapshot, name: &str) -> &'a agent_sessions::snapshot::WorkRow {
    snapshot
        .work
        .iter()
        .find(|w| w.name == name)
        .unwrap_or_else(|| {
            panic!(
                "no work row {name}; have {:?}",
                snapshot.work.iter().map(|w| &w.name).collect::<Vec<_>>()
            )
        })
}

/// The whole world: repo `a` covering every section and repo `b` quiet.
fn world() -> World {
    let home = TempDir::new("work-sections");
    let a = FixtureRepo::new("origin");
    let b = FixtureRepo::new("origin");

    // `Needs you`: a landed branch checked out in a worktree, with an
    // unacknowledged wait on its conversation - attention outranks the
    // safe-to-clean evidence it also carries.
    a.branch_with_commits("feat-needs", 1, true);
    a.land("feat-needs", Landing::Merge);
    let needs = a.add_worktree("needs", Some("feat-needs"));
    transcript(&home, NEEDS_ID, &needs);
    event(
        &home,
        NEEDS_ID,
        "Notification",
        NormEvent::Awaiting,
        "permission prompt",
    );

    // `Active`: a live `busy` agent bound to the worktree.
    a.branch_with_commits("feat-active", 1, true);
    let active_wt = a.add_worktree("active", Some("feat-active"));
    let agent = support::live_claude(&home, ACTIVE_ID, "busy", &active_wt);
    transcript(&home, ACTIVE_ID, &active_wt);

    // `Follow up`, four ways: dirty, unpushed, resumable idle, and - the
    // project space - idle with no git at all.
    a.branch_with_commits("feat-dirty", 2, false);
    let dirty = a.add_worktree("dirty", Some("feat-dirty"));
    fs::write(dirty.join("dirty.txt"), "uncommitted").expect("write");
    a.branch_with_commits("feat-unpushed", 2, false);
    a.branch_with_commits("feat-resume", 1, true);
    let resume = a.add_worktree("resume", Some("feat-resume"));
    transcript(&home, RESUME_ID, &resume);
    let notes = home.join("notes");
    fs::create_dir_all(&notes).expect("mkdir");
    undated_transcript(&home, SPACE_ID, &notes);

    // `Ready to clean`: merged, clean, nothing live. `Cleanup review`:
    // squash-landed, so only `-D` could delete it. And a detached
    // worktree: its row is keyed by path, not a branch incarnation.
    a.branch_with_commits("feat-merged", 1, true);
    a.land("feat-merged", Landing::Merge);
    a.add_worktree("merged", Some("feat-merged"));
    a.branch_with_commits("feat-squashed", 1, true);
    a.land("feat-squashed", Landing::Squash);
    a.add_worktree("detached", None);

    // Repo `b`: a lone transcript on its main checkout, backdated so `a`
    // wins the activity ordering, plus the quiet branch `Forgotten`
    // claims - it must live where no process runs: a branch row counts
    // every live session in its repository.
    transcript(&home, OTHER_ID, &b.main);
    old_pushed_branch(&b, "feat-old", Duration::from_secs(30 * 86400));
    // `feat-stale` was created now over an old commit: the creation line
    // is bookkeeping and the tip's committerdate predates it, so the ref
    // proves no work at all - unknown activity, `?`. `feat-mystery` has
    // no reflog left: the committerdate fallback still dates it.
    let tree = b.git(&b.main, &["rev-parse", "HEAD^{tree}"]);
    let epoch = now() - 20 * 86400;
    let stale = fixture::command(
        Some(&b.main),
        &["commit-tree", tree.trim(), "-p", "main", "-m", "stale"],
    )
    .env("GIT_COMMITTER_DATE", format!("@{epoch} +0000"))
    .env("GIT_AUTHOR_DATE", format!("@{epoch} +0000"))
    .output()
    .expect("commit-tree runs");
    assert!(
        stale.status.success(),
        "commit-tree failed: {}",
        String::from_utf8_lossy(&stale.stderr)
    );
    let stale = String::from_utf8_lossy(&stale.stdout).trim().to_owned();
    b.git(&b.main, &["branch", "feat-stale", &stale]);
    b.git(&b.main, &["push", "-u", "origin", "feat-stale"]);
    b.git(&b.main, &["branch", "feat-mystery", "main"]);
    fs::remove_file(b.main.join(".git/logs/refs/heads/feat-mystery"))
        .expect("the fabricated reflog deletes");
    let t = home.join(format!(".claude/projects/t/{OTHER_ID}.jsonl"));
    fs::File::options()
        .write(true)
        .open(&t)
        .expect("the transcript")
        .set_modified(UNIX_EPOCH + Duration::from_secs(now() - 2 * 86400))
        .expect("mtime sets");
    // `a` must win the activity ordering deterministically: the fake
    // agent's process start is not observable on every platform, so one
    // of `a`'s transcript turns carries a timestamp past every Git time.
    let t = home.join(format!(".claude/projects/t/{NEEDS_ID}.jsonl"));
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&t)
        .expect("the transcript");
    use std::io::Write;
    writeln!(
        f,
        "{{\"type\":\"user\",\"sessionId\":\"{NEEDS_ID}\",\"cwd\":\"{}\",\"timestamp\":\"2030-01-01T00:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"newer\"}}}}",
        needs.display()
    )
    .expect("the turn appends");

    World {
        home,
        a,
        b,
        agent: Some(agent),
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(agent) = &mut self.agent {
            let _ = agent.kill();
            let _ = agent.wait();
        }
    }
}

/// `app` rendered into `w`x`h` cells, as text.
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

#[test]
fn every_section_classifies_and_survives_serialization() {
    let world = world();
    let snapshot = collect(&world);

    // Every produced row has exactly one section, and the fixture's rows
    // land where their evidence puts them - with the canonical summary.
    let needs = work(&snapshot, "feat-needs");
    assert_eq!(needs.section, WorkSection::NeedsYou);
    assert_eq!(needs.attention, Attention::Waiting);
    assert_eq!(needs.summary, "waiting: permission prompt · merged");
    let active = work(&snapshot, "feat-active");
    assert_eq!(active.section, WorkSection::Active);
    assert_eq!(active.attention, Attention::Working);
    assert_eq!(active.summary, "working · ↑1");
    let dirty = work(&snapshot, "feat-dirty");
    assert_eq!(dirty.section, WorkSection::FollowUp);
    assert_eq!(dirty.summary, "dirty · no remote ↑2");
    let unpushed = work(&snapshot, "feat-unpushed");
    assert_eq!(unpushed.section, WorkSection::FollowUp);
    assert_eq!(unpushed.summary, "unpushed 2 · no wt no remote ↑2");
    let resume = work(&snapshot, "feat-resume");
    assert_eq!(resume.section, WorkSection::FollowUp);
    assert_eq!(resume.summary, "resumable idle · ↑1");
    let notes = work(&snapshot, "notes");
    assert_eq!(notes.kind, WorkKind::ProjectSpace);
    assert_eq!(notes.section, WorkSection::FollowUp);
    assert_eq!(notes.summary, "resumable idle · no git");
    let old = work(&snapshot, "feat-old");
    assert_eq!(old.section, WorkSection::Forgotten);
    assert_eq!(old.summary, "idle 30d · no wt ↑1");
    // Its authored age survives: thirty days old, not re-dated.
    let age = now() - old.last_activity.expect("the branch has activity");
    assert!(age > 14 * 86400, "{age}");
    let merged = work(&snapshot, "feat-merged");
    assert_eq!(merged.section, WorkSection::ReadyToClean);
    assert_eq!(merged.summary, "wt + branch · merged");
    let squashed = work(&snapshot, "feat-squashed");
    assert_eq!(squashed.section, WorkSection::CleanupReview);
    assert_eq!(
        squashed.summary,
        "review: requires `git branch -D` · no wt ↑1 merged"
    );
    // `feat-stale` was created now over an old commit: creation and a
    // borrowed old committerdate are not work, so the row keeps `?` for
    // activity and `Forgotten` cannot claim it.
    let stale = work(&snapshot, "feat-stale");
    assert_eq!(stale.section, WorkSection::CleanupReview, "{stale:?}");
    assert_eq!(stale.last_activity, None, "{stale:?}");
    // No reflog at all: the tip's committerdate stays the fallback date.
    let mystery = work(&snapshot, "feat-mystery");
    assert_eq!(mystery.section, WorkSection::ReadyToClean, "{mystery:?}");
    assert!(mystery.last_activity.is_some(), "{mystery:?}");

    // Blocked rows name their blocker and are never cleanup candidates;
    // unknown forge state stays informational.
    assert_eq!(old.forge, WorkItem::Unknown);
    assert_eq!(unpushed.forge, WorkItem::Unknown);
    let dirty_row = work(&snapshot, "feat-dirty");
    let removal = dirty_row
        .worktree_removal
        .as_ref()
        .expect("removal verdict");
    assert_eq!(
        removal.verdict,
        agent_sessions::verdict::Verdict::Blocked,
        "{removal:?}"
    );

    // The repo rollups: `a` carries two clean rows and the rest open; `b`
    // has none. Attention rolls up to the worst row's.
    let a = snapshot
        .repos
        .iter()
        .find(|r| r.id == format!("{}", world.a.main.join(".git").display()))
        .expect("repo a");
    assert_eq!(a.attention, Attention::Waiting);
    assert!(
        a.open >= 5 && a.clean >= 2,
        "{} open · {} clean",
        a.open,
        a.clean
    );
    let b = snapshot
        .repos
        .iter()
        .find(|r| r.id == format!("{}", world.b.main.join(".git").display()))
        .expect("repo b");
    assert_eq!(b.attention, Attention::None);
    // `a` is newest on the strength of the live agent; `b` is older.
    assert_eq!(snapshot.repos[0].id, a.id);

    // The serialized snapshot is the contract: snake_case sections, the
    // additive fields present on every row.
    let json = to_json(&snapshot).expect("the snapshot serializes");
    let doc: serde_json::Value = serde_json::from_str(&json).expect("JSON");
    let rows = doc["work"].as_array().expect("work rows");
    let sections: Vec<&str> = rows
        .iter()
        .map(|r| r["section"].as_str().unwrap())
        .collect();
    for expected in [
        "needs_you",
        "active",
        "follow_up",
        "forgotten",
        "ready_to_clean",
        "cleanup_review",
    ] {
        assert!(sections.contains(&expected), "{sections:?}");
    }
    let needs_json = rows
        .iter()
        .find(|r| r["name"] == "feat-needs")
        .expect("feat-needs");
    assert!(needs_json["identity"].is_string());
    assert_eq!(needs_json["parked"], false);
    assert!(needs_json["branch_deletion"].is_object());
}

#[test]
fn the_lists_render_at_55_and_200_columns() {
    let world = world();
    let mut app = App::new(collect(&world)).with_store(store(&world.home));
    // Tall enough that the whole Work list fits: all + four section
    // headers + the rows + the collapsed cleanup line.
    for width in [55u16, 200] {
        let text = render(&app, width, 44);
        for line in text.lines() {
            assert_eq!(line.chars().count(), width as usize, "{line}");
        }
        assert!(text.contains("Needs you"), "{text}");
        assert!(text.contains("Active"), "{text}");
        assert!(text.contains("Follow up"), "{text}");
        assert!(text.contains("Forgotten"), "{text}");
        // Under `all`, cleanup collapses into exactly one line.
        assert!(text.contains("Cleanup 3 safe"), "{text}");
        assert!(!text.contains("Ready to clean"), "{text}");
        assert!(!text.contains("Cleanup review"), "{text}");
        assert!(!text.contains("main/feat-merged"), "{text}");
        // Global work labels carry the repo name - clipped tail and all.
        assert!(text.contains("main/feat-need"), "{text}");
    }
    // The wide frame keeps the canonical summaries, the whole collapsed
    // cleanup line and the full repo-prefixed label.
    let text = render(&app, 200, 44);
    assert!(text.contains("main/feat-needs"), "{text}");
    assert!(text.contains("Cleanup 3 safe · 2 review"), "{text}");
    assert!(text.contains("resumable idle · no git"), "{text}");
    // Repo rows carry the rolled-up counts and the attention glyph.
    assert!(text.contains("open · 3 clean"), "{text}");
    // A row with no proven work renders `?` for its age - the notes
    // space's transcript carries no timestamp.
    let notes = text
        .lines()
        .find(|l| l.contains("notes") && l.contains("resumable idle"))
        .expect("the notes row renders");
    assert!(notes.contains("no git ?│"), "{notes}");
    // The Work list's footer advertises `p`.
    app.key(Key::Char('2'));
    let text = render(&app, 200, 44);
    assert!(text.contains("p park"), "{text}");

    // Under repo scope the cleanup rows expand under their own headers
    // and labels lose the repo prefix - and `feat-stale`, created over an
    // old commit with no work since, renders its unknown age as `?`.
    app.key(Key::Char('1'));
    app.key(Key::Char('j'));
    app.key(Key::Char('j'));
    app.key(Key::Char('2'));
    let text = render(&app, 200, 44);
    assert!(text.contains("Ready to clean"), "{text}");
    assert!(text.contains("Cleanup review"), "{text}");
    assert!(!text.contains("Cleanup 3 safe"), "{text}");
    let stale = text
        .lines()
        .find(|l| l.contains("feat-stale"))
        .expect("feat-stale renders");
    assert!(stale.contains("?│"), "{stale}");
    // Repo `a` still expands its own cleanup rows unprefixed.
    app.key(Key::Char('1'));
    app.key(Key::Char('k'));
    app.key(Key::Char('2'));
    let text = render(&app, 200, 44);
    assert!(text.contains("● feat-active"), "{text}");
}

#[test]
fn parked_suppresses_forgotten_and_nothing_else() {
    let world = world();
    let snapshot = collect(&world);
    let old = work(&snapshot, "feat-old");
    assert_eq!(old.section, WorkSection::Forgotten);
    let identity = old.identity.clone().expect("an authored identity");

    // Park it through the store: the next collect leaves `Forgotten` -
    // the squash-landed-like review verdicts claim it - and marks the row
    // `parked`. Attention would still win, but this row has none.
    store(&world.home)
        .toggle_parked(&WorkIdentity::Branch(identity.clone()))
        .expect("parks");
    // A changed working tree on a still-live record is work: dirty
    // `feat-resume`'s worktree so the next pass lands a WorkingTree
    // activity at the file's own mtime, which `last_activity` folds in.
    fs::write(world.a.dir.join("wt-resume/dirty.txt"), "x").unwrap();
    let snapshot = collect(&world);
    let old = work(&snapshot, "feat-old");
    assert_eq!(old.section, WorkSection::CleanupReview);
    assert!(old.parked);
    assert!(old.summary.ends_with("· parked"), "{}", old.summary);
    let resume = work(&snapshot, "feat-resume");
    assert!(
        resume.summary.starts_with("dirty") && resume.last_activity.is_some(),
        "{}",
        resume.summary
    );

    // A fresh collector reads the same authored state: parking survives
    // a restart.
    let runtime = Runtime::observe_over(&[]);
    let mut fresh = Collector::new(claude(&world.home)).with_store(state(&world.home));
    let restarted = fresh.collect(&runtime, None);
    assert!(work(&restarted, "feat-old").parked);

    // Unparking puts it straight back.
    store(&world.home)
        .toggle_parked(&WorkIdentity::Branch(identity.clone()))
        .expect("unparks");
    let snapshot = collect(&world);
    let old = work(&snapshot, "feat-old");
    assert_eq!(old.section, WorkSection::Forgotten);
    assert!(!old.parked);
    assert!(!old.summary.contains("parked"), "{}", old.summary);

    // A path record transitions the same way: reseed `notes` with a
    // proven fingerprint, then a different one, and the store lands the
    // change as an observation - detection, never activity.
    let notes_path = world.home.join("notes").canonicalize().unwrap();
    for dirty in [false, true] {
        let inputs = LifecycleInputs {
            dirty: Some(dirty),
            ..LifecycleInputs::default()
        };
        store(&world.home)
            .sync_path(
                &notes_path.display().to_string(),
                &notes_path.display().to_string(),
                &inputs,
                &[],
                now() * 1000,
            )
            .expect("seeds");
    }
    let snapshot = collect(&world);
    let notes = work(&snapshot, "notes");
    assert!(!notes.observations.is_empty(), "{}", notes.summary);
    store(&world.home)
        .toggle_parked(&WorkIdentity::Path(notes_path.display().to_string()))
        .expect("parks the space");
    let snapshot = collect(&world);
    let notes = work(&snapshot, "notes");
    assert!(notes.parked);
    assert!(notes.summary.ends_with("· parked"), "{}", notes.summary);

    // Parked suppresses only `Forgotten`: a waiting row keeps `Needs you`
    // and gains the marker.
    let needs_id = work(&snapshot, "feat-needs")
        .identity
        .clone()
        .expect("an authored identity");
    store(&world.home)
        .toggle_parked(&WorkIdentity::Branch(needs_id))
        .expect("parks");
    let snapshot = collect(&world);
    let needs = work(&snapshot, "feat-needs");
    assert_eq!(needs.section, WorkSection::NeedsYou);
    assert!(needs.parked);
    assert!(needs.summary.ends_with("· parked"), "{}", needs.summary);

    // Delete and recreate the branch: a new incarnation, parked cleared.
    // The delete needs its own collect - the sync closes the record only
    // for a pass that actually observed the ref's absence.
    world.b.git(&world.b.main, &["branch", "-D", "feat-old"]);
    let snapshot = collect(&world);
    assert!(
        !snapshot.work.iter().any(|w| w.name == "feat-old"),
        "the deleted branch leaves no row"
    );
    old_pushed_branch(&world.b, "feat-old", Duration::from_secs(30 * 86400));
    let snapshot = collect(&world);
    let old = work(&snapshot, "feat-old");
    assert_eq!(old.section, WorkSection::Forgotten);
    assert!(!old.parked, "recreation must not inherit the parked flag");
    assert_ne!(old.identity.as_deref(), Some(identity.as_str()));
}

#[test]
fn p_parks_the_selected_work_row_and_reclassifies_in_place() {
    let world = world();
    let app_snapshot = collect(&world);
    let mut app = App::new(app_snapshot).with_store(store(&world.home));

    // `p` outside the Work list, and on `all`, is inert: no writes.
    app.key(Key::Char('p'));
    let before = fs::read_to_string(state(&world.home).join("work.json")).unwrap();
    app.key(Key::Char('1'));
    app.key(Key::Char('p'));
    app.key(Key::Char('2'));
    app.key(Key::Char('p'));
    let after = fs::read_to_string(state(&world.home).join("work.json")).unwrap();
    assert_eq!(before, after, "inert keypresses write nothing");

    // The Work list under `all`: section order puts the forgotten row
    // last - past Needs you, Active and the Follow-up rows.
    let rows = app
        .snapshot
        .work
        .iter()
        .filter(|w| {
            !matches!(
                w.section,
                WorkSection::ReadyToClean | WorkSection::CleanupReview
            )
        })
        .count();
    for _ in 0..rows {
        app.key(Key::Char('j'));
    }
    app.key(Key::Char('p'));
    let old = work(&app.snapshot, "feat-old");
    assert!(old.parked);
    assert_eq!(old.section, WorkSection::CleanupReview);
    let doc: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(state(&world.home).join("work.json")).unwrap())
            .unwrap();
    let branches = doc["data"]["branches"].as_object().unwrap();
    assert!(
        branches
            .values()
            .any(|r| r["ref_name"] == "feat-old" && r["parked"] == true),
        "{doc}"
    );

    // Repo scope lists the row again; `p` unparks it and it returns to
    // `Forgotten` without a refresh. `feat-old` lives in repo `b` - the
    // second row on [1].
    app.key(Key::Char('1'));
    app.key(Key::Char('j'));
    app.key(Key::Char('j'));
    app.key(Key::Char('2'));
    let visible = app
        .snapshot
        .work
        .iter()
        .filter(|w| w.repo == work(&app.snapshot, "feat-old").repo)
        .position(|w| w.name == "feat-old")
        .expect("scoped row")
        + 1;
    for _ in 0..visible {
        app.key(Key::Char('j'));
    }
    app.key(Key::Char('p'));
    let old = work(&app.snapshot, "feat-old");
    assert!(!old.parked);
    assert_eq!(old.section, WorkSection::Forgotten);

    // A path-keyed row parks by its canonical path identity: the `notes`
    // project space is its own repo, cursor `j` to it on [1].
    let notes_repo = app
        .snapshot
        .repos
        .iter()
        .position(|r| r.name == "notes")
        .expect("the space row");
    app.key(Key::Char('1'));
    for _ in 0..notes_repo {
        app.key(Key::Char('j'));
    }
    app.key(Key::Char('2'));
    app.key(Key::Char('j'));
    app.key(Key::Char('p'));
    let notes = work(&app.snapshot, "notes");
    assert!(notes.parked);
    assert!(notes.summary.ends_with("· parked"), "{}", notes.summary);

    // A refused write surfaces as a notice instead of a silent miss:
    // corrupt `work.json`, keep the in-memory identities, press `p`.
    fs::write(state(&world.home).join("work.json"), "not json").unwrap();
    app.key(Key::Char('p'));
    let text = render(&app, 200, 44);
    assert!(text.contains("park: not saved:"), "{text}");

    // And with the file malformed the next collect reports the store
    // error and produces branch rows without an identity, which makes `p`
    // inert on them.
    let mut app = App::new(collect(&world)).with_store(store(&world.home));
    assert!(
        app.snapshot.errors.iter().any(|e| e.source == "work.json"),
        "{:?}",
        app.snapshot.errors
    );
    assert!(
        app.snapshot
            .work
            .iter()
            .all(|w| w.branch.is_some() != w.identity.is_some()),
        "branch rows identify only through a record"
    );
    app.key(Key::Char('2'));
    let rows = app
        .snapshot
        .work
        .iter()
        .take_while(|w| {
            !matches!(
                w.section,
                WorkSection::ReadyToClean | WorkSection::CleanupReview
            )
        })
        .count();
    for _ in 0..rows {
        app.key(Key::Char('j'));
    }
    assert_eq!(
        app.snapshot.work[rows - 1].name,
        "feat-old",
        "{:?}",
        app.snapshot.work[rows - 1]
    );
    app.key(Key::Char('p'));
    assert!(app.snapshot.work.iter().all(|w| !w.parked));
}

#[test]
fn a_configured_threshold_moves_the_forgotten_line() {
    let world = world();
    // Two days old: `Forgotten` needs strictly beyond `forgotten_after`,
    // so the default 14d keeps the branch out of it - the review verdict
    // claims the row - while a configured `1d` catches it. Repo `b` holds
    // no live process, so the row is allowed to go quiet.
    let day = Duration::from_secs(86400);
    old_pushed_branch(&world.b, "feat-recent", 2 * day);
    let snapshot = collect(&world);
    assert_eq!(
        work(&snapshot, "feat-recent").section,
        WorkSection::CleanupReview
    );
    let loaded = Loaded {
        config: Config {
            forgotten_after: day,
        },
        warnings: vec!["a trial warning".to_owned()],
    };
    let snapshot = collect_with(&world, Some(loaded), None);
    assert!(
        snapshot.errors.iter().any(|e| e.source == "config"),
        "{:?}",
        snapshot.errors
    );
    assert_eq!(
        work(&snapshot, "feat-recent").section,
        WorkSection::Forgotten
    );
    // The thirty-day-old branch is `Forgotten` under both.
    assert_eq!(work(&snapshot, "feat-old").section, WorkSection::Forgotten);
}

#[test]
fn forge_state_drives_checks_reasons_and_stays_informational() {
    let home = TempDir::new("work-forge");
    let a = FixtureRepo::new("origin");
    // Four pushed branches: failing checks, pending, merged-PR and green -
    // plus a plain transcript row on `main` and a remote-less repo whose
    // rows can only answer `no upstream remote`.
    a.branch_with_commits("feat-fail", 1, true);
    a.branch_with_commits("feat-pend", 1, true);
    a.branch_with_commits("feat-shipped", 1, true);
    a.land("feat-shipped", Landing::Merge);
    a.branch_with_commits("feat-green", 1, true);
    a.branch_with_commits("feat-clear", 1, true);
    // Landed on main, but its worktree carries uncommitted changes:
    // landed plus blocked.
    a.branch_with_commits("feat-blocked", 1, true);
    a.land("feat-blocked", Landing::Merge);
    let blocked = a.add_worktree("blocked", Some("feat-blocked"));
    fs::write(blocked.join("dirty.txt"), "x").expect("dirty writes");
    transcript(&home, OTHER_ID, &a.main);
    // Only now point `origin` at a forge URL: every earlier push ran
    // against the real (local) remote, and `ls-remote` fails cleanly on
    // the unroutable host.
    a.git(
        &a.main,
        &["remote", "set-url", "origin", "https://github.invalid/o/r"],
    );
    // A repo with no remote at all still lands a row.
    let solo = TempDir::new("solo-repo");
    let solo_main = solo.join("w");
    fixture::command(Some(solo.path()), &["init", "-b", "main", "w"])
        .output()
        .expect("init");
    fs::write(solo_main.join("f"), "f").unwrap();
    let c = fixture::command(Some(&solo_main), &["add", "f"])
        .output()
        .expect("add");
    assert!(c.status.success());
    let c = fixture::command(Some(&solo_main), &["commit", "-m", "one"])
        .output()
        .expect("commit");
    assert!(c.status.success());
    transcript(&home, SPACE_ID, &solo_main);

    // The stub answers per branch: failing and pending PRs open, a green
    // one too, the shipped one's PR merged, everything else absent.
    let stubs = TempDir::new("forge-stubs");
    let body = format!(
        "#!/bin/sh\ncase \"$*\" in\n  *feat-fail*) printf '%s' '{}' ;;\n  *feat-pend*) printf '%s' '{}' ;;\n  *feat-green*) printf '%s' '{}' ;;
  *feat-clear*) printf '%s' '{}' ;;\n  *feat-blocked*) printf '%s' '{}' ;;\n  *feat-shipped*--state\\ open*|*feat-shipped*open*) printf '[]' ;;\n  *feat-shipped*) printf '%s' '{}' ;;\n  *) printf '[]' ;;\nesac\nexit 0\n",
        r#"[{"number":41,"state":"OPEN","url":"https://github.invalid/o/r/pull/41","statusCheckRollup":[{"status":"COMPLETED","conclusion":"FAILURE"}]}]"#,
        r#"[{"number":42,"state":"OPEN","url":"https://github.invalid/o/r/pull/42","statusCheckRollup":[{"status":"IN_PROGRESS"}]}]"#,
        r#"[{"number":45,"state":"OPEN","url":"https://github.invalid/o/r/pull/45","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]}]"#,
        r#"[{"number":46,"state":"OPEN","url":"https://github.invalid/o/r/pull/46","statusCheckRollup":[]}]"#,
        r#"[{"number":47,"state":"OPEN","url":"https://github.invalid/o/r/pull/47","statusCheckRollup":[]}]"#,
        r#"[{"number":43,"state":"MERGED","url":"https://github.invalid/o/r/pull/43","statusCheckRollup":[]}]"#,
    );
    let gh = stubs.join("gh");
    fs::write(&gh, body).expect("stub writes");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&gh).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gh, perms).unwrap();

    let world = World {
        home,
        a,
        b: FixtureRepo::new("origin"),
        agent: None,
    };
    let runtime = Runtime::observe_over(&[]);
    let mut collector = Collector::new(claude(&world.home))
        .with_store(state(&world.home))
        .with_forge(Forge::with_path(stubs.path().as_os_str().to_owned()));
    let snapshot = collector.collect(&runtime, None);

    let fail = work(&snapshot, "feat-fail");
    assert_eq!(fail.section, WorkSection::FollowUp);
    assert!(
        fail.summary.starts_with("checks failed"),
        "{}",
        fail.summary
    );
    assert_eq!(fail.forge, WorkItem::Open);
    assert_eq!(fail.forge_label.as_deref(), Some("PR #41"));
    assert_eq!(
        fail.forge_url.as_deref(),
        Some("https://github.invalid/o/r/pull/41")
    );
    let pend = work(&snapshot, "feat-pend");
    assert_eq!(pend.section, WorkSection::FollowUp);
    assert!(
        pend.summary.starts_with("checks pending"),
        "{}",
        pend.summary
    );
    let green = work(&snapshot, "feat-green");
    assert_eq!(green.pipeline, Pipeline::Succeeded);
    assert!(green.summary.contains("checks ok"), "{}", green.summary);
    // An open item with no rollup data answers `unknown` - honestly, and
    // triggering nothing.
    let clear = work(&snapshot, "feat-clear");
    assert_eq!(clear.forge, WorkItem::Open);
    assert_eq!(clear.pipeline, Pipeline::Unknown);
    assert!(!clear.summary.contains("checks"), "{}", clear.summary);
    // Landed but blocked by the still-open-looking evidence? No - merged
    // means the item is closed; the row's verdict stays informational.
    let shipped = work(&snapshot, "feat-shipped");
    assert_eq!(shipped.forge, WorkItem::Closed);
    assert!(shipped.forge_label.is_some());
    // Landed with a live PR open and uncommitted work in its worktree:
    // blocked, follow-up, `merged · blocked` - never a cleanup candidate.
    let blocked = work(&snapshot, "feat-blocked");
    assert_eq!(blocked.section, WorkSection::FollowUp, "{blocked:?}");
    assert_eq!(blocked.forge, WorkItem::Open);
    assert_eq!(blocked.pipeline, Pipeline::Unknown);
    assert!(
        blocked.summary.starts_with("merged · blocked"),
        "{}",
        blocked.summary
    );
    let removal = blocked.worktree_removal.as_ref().expect("a removal");
    assert_eq!(
        removal.verdict,
        agent_sessions::verdict::Verdict::Blocked,
        "{removal:?}"
    );
    // `main`'s transcript row is the space row for the repo; pushed rows
    // whose PR queries answered nothing get NotExisting, others Unknown.
    let main = work(&snapshot, "main");
    assert_eq!(main.section, WorkSection::FollowUp, "{}", main.summary);
    let solo_row = snapshot
        .work
        .iter()
        .find(|w| w.repo.contains("solo-repo"))
        .expect("the remote-less repo's row");
    assert_eq!(solo_row.forge, WorkItem::Unknown);

    // A second pass on the same collector carries remote-owned evidence -
    // the forge answer included - while stage 4 is still fresh.
    let snapshot = collector.collect(&runtime, None);
    let fail = work(&snapshot, "feat-fail");
    assert_eq!(fail.forge, WorkItem::Open);
    assert_eq!(fail.pipeline, Pipeline::Failed);
    assert_eq!(fail.forge_label.as_deref(), Some("PR #41"));
    let green = work(&snapshot, "feat-green");
    assert_eq!(green.pipeline, Pipeline::Succeeded);
}

/// Backdate a transcript file: its conversation's last turn reads `days`
/// old.
fn age_transcript(home: &TempDir, id: &str, days: u64) {
    // The records' own stamps age with the file: a month-old conversation
    // wrote month-old records.
    let at = UNIX_EPOCH + Duration::from_secs(now() - days * 86400);
    let path = home.join(format!(".claude/projects/t/{id}.jsonl"));
    let text = fs::read_to_string(&path).expect("the transcript");
    let aged: String = text
        .lines()
        .map(|line| {
            let Some(start) = line.find("\"timestamp\":\"") else {
                return format!("{line}\n");
            };
            let value = start + "\"timestamp\":\"".len();
            let end = value + line[value..].find('"').expect("a closed stamp");
            format!("{}{}{}\n", &line[..value], support::iso(at), &line[end..])
        })
        .collect();
    fs::write(&path, aged).expect("the transcript rewrites");
    fs::File::options()
        .write(true)
        .open(&path)
        .expect("the transcript")
        .set_modified(at)
        .expect("mtime sets");
}

#[test]
fn a_resumable_conversation_does_not_hold_finished_or_quiet_work() {
    let home = TempDir::new("work-resumable");
    let a = FixtureRepo::new("origin");
    // Landed and clean, with a dead conversation in its worktree: the
    // work is finished, so cleanup claims it, not `resumable idle`.
    a.branch_with_commits("feat-merged", 1, true);
    a.land("feat-merged", Landing::Merge);
    let merged = a.add_worktree("merged", Some("feat-merged"));
    transcript(&home, NEEDS_ID, &merged);
    age_transcript(&home, NEEDS_ID, 40);
    // Unlanded, a month quiet, its worktree added today over the old
    // commits and holding a month-old dead conversation: `Forgotten`.
    // The add itself is no activity.
    old_pushed_branch(&a, "feat-old", Duration::from_secs(30 * 86400));
    let old = a.add_worktree("old", Some("feat-old"));
    transcript(&home, RESUME_ID, &old);
    age_transcript(&home, RESUME_ID, 30);
    // A project space is never forgotten or cleaned, so its old
    // resumable conversation still asks for a pick-up.
    let notes = home.join("notes");
    fs::create_dir_all(&notes).expect("mkdir");
    transcript(&home, SPACE_ID, &notes);
    age_transcript(&home, SPACE_ID, 40);
    let world = World {
        home,
        a,
        b: FixtureRepo::new("origin"),
        agent: None,
    };
    let snapshot = collect(&world);
    let merged = work(&snapshot, "feat-merged");
    assert_eq!(merged.section, WorkSection::ReadyToClean, "{merged:?}");
    let old = work(&snapshot, "feat-old");
    assert_eq!(old.section, WorkSection::Forgotten, "{old:?}");
    assert!(old.summary.starts_with("idle 30d"), "{}", old.summary);
    let notes = work(&snapshot, "notes");
    assert_eq!(notes.section, WorkSection::FollowUp, "{notes:?}");
    assert_eq!(notes.summary, "resumable idle · no git");
    // The conversations stay listed: only the row's section moved.
    assert!(
        snapshot
            .conversations
            .iter()
            .any(|c| c.session_id == RESUME_ID)
    );
}

#[test]
fn an_unreachable_remote_is_not_activity() {
    let home = TempDir::new("work-offline");
    let a = FixtureRepo::new("origin");
    old_pushed_branch(&a, "feat-quiet", Duration::from_secs(30 * 86400));
    // A conversation on the main checkout brings the repo into scope.
    transcript(&home, OTHER_ID, &a.main);
    age_transcript(&home, OTHER_ID, 30);
    let world = World {
        home,
        a,
        b: FixtureRepo::new("origin"),
        agent: None,
    };
    let quiet = |snapshot: &Snapshot| {
        let row = work(snapshot, "feat-quiet");
        assert_eq!(row.section, WorkSection::Forgotten, "{row:?}");
        assert!(row.summary.starts_with("idle 30d"), "{}", row.summary);
    };
    quiet(&collect(&world));
    // Offline: the remote cannot be asked, so upstream and delivery read
    // unknown. An unproven reading is no transition - the row stays
    // `Forgotten` at its old age - and neither is coming back online.
    let url = world.a.git(&world.a.main, &["remote", "get-url", "origin"]);
    world.a.git(
        &world.a.main,
        &["remote", "set-url", "origin", "/nonexistent/remote.git"],
    );
    let offline = collect(&world);
    assert_eq!(
        work(&offline, "feat-quiet").upstream,
        agent_sessions::snapshot::Upstream::Unknown
    );
    quiet(&offline);
    world
        .a
        .git(&world.a.main, &["remote", "set-url", "origin", url.trim()]);
    quiet(&collect(&world));
}

#[test]
fn a_park_during_a_pass_holds_in_every_later_stage() {
    let world = world();
    let runtime = Runtime::observe_over(&[]);
    let mut collector = Collector::new(claude(&world.home))
        .with_store(state(&world.home))
        .with_workers(1);
    // The first pass writes the incarnation records `p` names.
    let first = collector.collect(&runtime, None);
    let identity = work(&first, "feat-old")
        .identity
        .clone()
        .expect("an authored identity");
    // `p` lands while the next pass is mid-flight, right after its first
    // stage published: every later stage must already read it parked.
    let store = store(&world.home);
    let mut published = 0usize;
    let mut stale = Vec::new();
    collector.collect_staged(&runtime, None, &mut |snapshot| {
        published += 1;
        if published == 1 {
            store
                .toggle_parked(&WorkIdentity::Branch(identity.clone()))
                .expect("parks");
        } else if let Some(row) = snapshot.work.iter().find(|w| w.name == "feat-old")
            && !row.parked
        {
            stale.push((published, row.section));
        }
        true
    });
    assert!(published > 2, "{published}");
    assert!(stale.is_empty(), "stages that dropped the park: {stale:?}");
}

/// The snapshot's conversation row named `id`.
fn conv<'a>(snapshot: &'a Snapshot, id: &str) -> &'a agent_sessions::snapshot::ConversationRow {
    snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == id)
        .unwrap_or_else(|| panic!("no conversation {id}"))
}

#[test]
fn conversation_activity_is_transcript_and_hook_producer_time_only() {
    let world = world();
    // Pin the live conversation's transcript to an old message time: the
    // session file's `updatedAt`/`statusUpdatedAt` - publication time -
    // can no longer date it, and neither can the pass that first saw it.
    let active_wt = world.a.dir.join("wt-active");
    fs::write(
        world
            .home
            .join(format!(".claude/projects/t/{ACTIVE_ID}.jsonl")),
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{ACTIVE_ID}\",\"cwd\":\"{}\",\"timestamp\":\"2020-01-01T00:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"old\"}}}}\n",
            active_wt.display()
        ),
    )
    .expect("the transcript writes");
    let snapshot = collect(&world);
    let active = conv(&snapshot, ACTIVE_ID);
    assert_eq!(
        active.last_activity,
        Some(1_577_836_800),
        "the transcript's own time, not the file's publication: {active:?}"
    );
    // A hook record's producer timestamp counts at its own time; the
    // journal's commit time - much newer than the pinned `pts` - never
    // does.
    let mut r = Record::new("claude", ACTIVE_ID, "PostToolUse");
    r.event = Some(NormEvent::Activity);
    r.pts = Some(1_600_000_000_000);
    store(&world.home).append(r).expect("append");
    let snapshot = collect(&world);
    let active = conv(&snapshot, ACTIVE_ID);
    assert_eq!(active.last_activity, Some(1_600_000_000));
    // First detection alone proves no time at all: the undated
    // transcript's conversation keeps `?`.
    let space = conv(&snapshot, SPACE_ID);
    assert_eq!(space.last_activity, None);
}

#[test]
fn a_recent_observation_never_passes_for_work_activity() {
    let world = world();
    let before = collect(&world);
    let old = work(&before, "feat-old");
    let activity = old.last_activity.expect("source-backed activity");
    // Flip a proven lifecycle input on the record directly: the store
    // lands the transition as an observation at this pass, not work.
    let repo_id = world.b.main.join(".git").display().to_string();
    let b_rollup = before
        .repos
        .iter()
        .find(|r| r.id == repo_id)
        .expect("repo b")
        .last_activity;
    let refs: Vec<agent_sessions::store::ObservedRef> =
        ["main", "feat-old", "feat-stale", "feat-mystery"]
            .iter()
            .map(|name| agent_sessions::store::ObservedRef {
                name: (*name).to_owned(),
                head: None,
                creation: None,
                renamed_from: None,
                rewritten: false,
                commit: None,
                activities: Vec::new(),
                inputs: LifecycleInputs {
                    dirty: Some(*name == "feat-old"),
                    ..LifecycleInputs::default()
                },
            })
            .collect();
    store(&world.home)
        .sync_repo(&repo_id, &refs, now() * 1000)
        .expect("the observation lands");
    let after = collect(&world);
    let old = work(&after, "feat-old");
    assert!(
        !old.observations.is_empty(),
        "the transition recorded its detection: {old:?}"
    );
    // Recency, section and repo order all read activity alone: the
    // observation changes none of them.
    assert_eq!(old.last_activity, Some(activity), "{old:?}");
    assert_eq!(old.section, WorkSection::Forgotten, "{old:?}");
    assert_eq!(
        after
            .repos
            .iter()
            .find(|r| r.id == repo_id)
            .expect("repo b")
            .last_activity,
        b_rollup,
        "the observation cannot reach the repo rollup either"
    );
    // The JSON exposes both histories at their own timestamp fields.
    let json = to_json(&after).expect("serializes");
    assert!(json.contains("\"activities\":"), "{json}");
    assert!(json.contains("\"occurred_at_ms\":"), "{json}");
    assert!(json.contains("\"observations\":"), "{json}");
    assert!(json.contains("\"observed_at_ms\":"), "{json}");
}

/// `git reflog expire` rewrites the log even when nothing expires: the
/// entry bytes stay identical but the file's mtime jumps to now.
/// `reflog_times` reads only the entries' own dates, so maintenance
/// cannot manufacture activity - the row's `last_activity` must not
/// move. The mtime pinned old is the regression handle: under the
/// removed mtime folding, before would read the pin and after would
/// read now. (Regression: mtime folding let a no-op expire refresh a
/// 30-day-old branch to "just now".)
#[test]
fn reflog_maintenance_is_not_activity() {
    let repo = FixtureRepo::new("origin");
    let home = TempDir::new("work-reflog-maint");
    let branch = "feat-old";
    let age = Duration::from_secs(30 * 24 * 3600);
    old_pushed_branch(&repo, branch, age);
    transcript(&home, RESUME_ID, &repo.main);
    let mut collector = Collector::new(claude(&home)).with_store(state(&home));
    let log = repo.main.join(format!(".git/logs/refs/heads/{branch}"));
    // Pin the last write old, matching the entries: a reader that folds
    // mtime into `worked_at` sees the same old instant a clean log shows.
    fs::File::options()
        .write(true)
        .open(&log)
        .expect("the reflog opens")
        .set_modified(UNIX_EPOCH + Duration::from_secs(now() - age.as_secs()))
        .expect("mtime pins");

    let before = collector.collect(&Runtime::observe_over(&[]), None);
    let bytes_before = fs::read(&log).expect("the reflog reads");
    let activity_before = work(&before, branch).last_activity;
    assert!(
        activity_before.is_some_and(|a| now() - a >= 29 * 24 * 3600),
        "the old branch reads old before maintenance: {activity_before:?}"
    );

    repo.git(
        &repo.main,
        &[
            "reflog",
            "expire",
            "--expire=never",
            "--expire-unreachable=never",
            "--all",
        ],
    );

    // Sanity: the rewrite really happened - the entries are byte-identical
    // while the file's mtime moved. Both halves of the artifact.
    let bytes_after = fs::read(&log).expect("the reflog reads");
    assert_eq!(bytes_before, bytes_after, "expire rewrote the entries");
    let mtime_after = fs::metadata(&log)
        .and_then(|m| m.modified())
        .expect("mtime reads");
    assert!(
        mtime_after > UNIX_EPOCH + Duration::from_secs(now() - 60),
        "expire rewrote the file: {mtime_after:?}"
    );

    let after = collector.collect(&Runtime::observe_over(&[]), None);
    assert_eq!(
        work(&after, branch).last_activity,
        activity_before,
        "unchanged entries changed last_activity"
    );
}

/// A dated transcript on a non-git project space backfills real
/// source-backed work on first collection: the row's `last_activity`
/// and a `Conversation` activity agree at the record's own time, the
/// observations baseline stays empty, a fresh collector over the same
/// store replays nothing, and the detail pane renders the event's
/// source - never `activity: ?`.
#[test]
fn a_dated_project_space_turn_backfills_source_activity() {
    let home = TempDir::new("work-space-dated");
    let space = home.join("notes");
    fs::create_dir_all(&space).expect("mkdir");
    let projects = home.join(".claude/projects/t");
    fs::create_dir_all(&projects).expect("mkdir");
    fs::write(
        projects.join(format!("{SPACE_ID}.jsonl")),
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{SPACE_ID}\",\"cwd\":\"{}\",\"timestamp\":\"2026-09-01T00:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"the task\"}}}}\n",
            space.display()
        ),
    )
    .expect("the dated transcript writes");
    let world = World {
        home,
        a: FixtureRepo::new("origin"),
        b: FixtureRepo::new("origin"),
        agent: None,
    };
    // The first complete snapshot already carries the transcript's own
    // time as source-backed activity: the pass backfills before the row
    // renders.
    let snapshot = collect(&world);
    assert!(snapshot.complete, "{:?}", snapshot.errors);
    let row = work(&snapshot, "notes");
    assert_eq!(row.kind, WorkKind::ProjectSpace);
    assert_eq!(row.last_activity, Some(1_788_220_800), "{row:?}");
    assert!(
        row.activities.iter().any(|e| {
            e.source == ActivitySource::Conversation && e.occurred_at_ms == 1_788_220_800_000
        }),
        "the transcript's own time is source-backed activity: {:?}",
        row.activities
    );
    assert!(
        row.observations.is_empty(),
        "first collection stays a silent baseline: {:?}",
        row.observations
    );
    let activities = row.activities.clone();

    // Every later collection - the same pass again or a brand-new
    // collector over the same store - agrees and replays nothing.
    let again = collect(&world);
    assert_eq!(work(&again, "notes").activities, activities);
    let mut fresh = Collector::new(claude(&world.home)).with_store(state(&world.home));
    let third = fresh.collect(&Runtime::observe_over(&[]), None);
    let third_row = work(&third, "notes");
    assert_eq!(
        third_row.activities, activities,
        "a replayed update lands nothing twice"
    );

    // The detail pane names the event's source at its own time.
    let mut app = App::new(third).with_store(store(&world.home));
    app.key(Key::Char('2'));
    // The list's first entry is `all`; the work rows follow it.
    let index = 1 + app
        .snapshot
        .work
        .iter()
        .position(|w| w.name == "notes")
        .expect("the notes row");
    for _ in 0..index {
        app.key(Key::Char('j'));
    }
    let text = render(&app, 200, 44);
    assert!(text.contains("notes"), "{text}");
    assert!(!text.contains("activity: ?"), "{text}");
    assert!(text.contains("conversation:"), "{text}");
    assert!(text.contains("the task"), "{text}");
}

/// Turn `text` appended to conversation `id`'s transcript at `cwd`.
fn append_turn(home: &TempDir, id: &str, cwd: &Path, text: &str) {
    let transcript = home.join(format!(".claude/projects/t/{id}.jsonl"));
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("the transcript");
    use std::io::Write;
    f.write_all(support::claude_turn(id, cwd, text).as_bytes())
        .expect("the turn appends");
}

/// The one summary row `name`'s work row carries.
fn only_summary<'a>(
    snapshot: &'a Snapshot,
    name: &str,
) -> &'a agent_sessions::snapshot::ConversationSummary {
    let row = work(snapshot, name);
    assert_eq!(
        row.conversation_summaries.len(),
        1,
        "{name}: {:?}",
        row.conversation_summaries
    );
    &row.conversation_summaries[0]
}

#[test]
fn conversation_summaries_compact_turns_through_the_pipeline() {
    // A live agent on one branch's worktree: repeated transcript turns
    // compact into one summary at the newest source time; moving the
    // conversation leaves its captured context behind on the old row.
    let home = TempDir::new("conversation-summaries");
    let a = FixtureRepo::new("origin");
    a.branch_with_commits("feat-one", 1, true);
    let wt_one = a.add_worktree("one", Some("feat-one"));
    a.branch_with_commits("feat-two", 1, true);
    let wt_two = a.add_worktree("two", Some("feat-two"));
    let id = "66000000-1111-2222-3333-444444444444";
    transcript(&home, id, &wt_one);
    let mut agent = support::live_claude(&home, id, "busy", &wt_one);
    let world = World {
        home,
        a,
        b: FixtureRepo::new("origin"),
        agent: None,
    };

    let first = collect(&world);
    let summary = only_summary(&first, "feat-one");
    assert_eq!(
        summary.key,
        agent_sessions::store::conversation_key("claude", id)
    );
    assert_eq!(
        summary
            .context
            .as_ref()
            .and_then(|c| c.prompt_excerpt.as_deref()),
        Some("the task")
    );
    let first_at = summary.occurred_at_ms;

    // A repeated turn is still one row, at the newer source time, with
    // the newer prompt captured - the per-turn reasons stay raw history
    // underneath, never displayed per turn.
    // `last_activity` carries second precision: a turn must land a
    // whole second later to count as newer.
    std::thread::sleep(Duration::from_millis(1_100));
    append_turn(&world.home, id, &wt_one, "the follow-up");
    let second = collect(&world);
    let summary = only_summary(&second, "feat-one");
    assert!(summary.occurred_at_ms > first_at, "{summary:?}");
    assert_eq!(
        summary
            .context
            .as_ref()
            .and_then(|c| c.prompt_excerpt.as_deref()),
        Some("the follow-up")
    );
    let row = work(&second, "feat-one");
    assert!(
        row.activities
            .iter()
            .filter(|e| e.source == ActivitySource::Conversation)
            .count()
            >= 2,
        "raw history retained: {:?}",
        row.activities
    );

    // The conversation moves to the other branch's worktree: its newest
    // context lands on that record alone; the old record keeps the
    // excerpt it captured - a prompt never leaks across rows.
    agent.kill().expect("kill");
    let _ = agent.wait();
    let mut agent = support::live_claude(&world.home, id, "busy", &wt_two);
    // `last_activity` carries second precision: a turn must land a
    // whole second later to count as newer.
    std::thread::sleep(Duration::from_millis(1_100));
    append_turn(&world.home, id, &wt_two, "moved task");
    let third = collect(&world);
    assert_eq!(
        only_summary(&third, "feat-two")
            .context
            .as_ref()
            .and_then(|c| c.prompt_excerpt.as_deref()),
        Some("moved task")
    );
    assert_eq!(
        only_summary(&third, "feat-one")
            .context
            .as_ref()
            .and_then(|c| c.prompt_excerpt.as_deref()),
        Some("the follow-up"),
        "the old row retains its own capture"
    );

    // Provider-absent: the transcript gone, the agent dead - the record's
    // persisted summary still speaks, no current provider state reads in.
    // A second quiet conversation keeps the repo itself in scope: a repo
    // exists to the snapshot only while one resolves into it.
    agent.kill().expect("kill");
    let _ = agent.wait();
    let keeper_id = "77000000-1111-2222-3333-444444444444";
    transcript(&world.home, keeper_id, &world.a.main);
    let mut keeper = support::live_claude(&world.home, keeper_id, "idle", &world.a.main);
    fs::remove_file(world.home.join(format!(".claude/projects/t/{id}.jsonl")))
        .expect("the transcript deletes");
    fs::remove_file(
        world
            .home
            .join(format!(".claude/sessions/{}.json", agent.id())),
    )
    .expect("the dead session file clears");
    let fourth = collect(&world);
    keeper.kill().expect("kill");
    let _ = keeper.wait();
    let summary = only_summary(&fourth, "feat-two");
    assert_eq!(
        summary
            .context
            .as_ref()
            .and_then(|c| c.prompt_excerpt.as_deref()),
        Some("moved task"),
        "persisted fallback without the provider"
    );
    // The detail view renders the compacted row: one `last activity`
    // line, no raw turn reasons.
    let mut app = App::new(fourth);
    app.key(Key::Char('2'));
    let mut seen = String::new();
    for _ in 0..12 {
        let text = render(&app, 200, 40);
        seen.push_str(&text);
        if text.contains("Work - feat-two") {
            assert!(text.contains("last activity"), "{text}");
            assert!(text.contains("prompt: moved task"), "{text}");
            assert!(!text.contains("moved task moved task"), "{text}");
            break;
        }
        app.key(Key::Char('j'));
    }
    assert!(seen.contains("Work - feat-two"), "feat-two focused: {seen}");
}

/// Strip `session_context` from every record in `work.json`, keeping the
/// cursors - the file a pre-context build would have written.
fn strip_session_context(home: &TempDir) {
    let path = state(home).join("work.json");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("work.json")).expect("json");
    let data = doc["data"].as_object_mut().expect("data");
    for key in ["branches", "paths"] {
        for record in data[key].as_object_mut().expect(key).values_mut() {
            record.as_object_mut().unwrap().remove("session_context");
        }
    }
    fs::write(&path, serde_json::to_vec(&doc).unwrap()).expect("work.json writes");
}

#[test]
fn cursor_only_records_compact_when_the_provider_is_absent() {
    // A record holding cursors but no context - written before context
    // existed - still compacts to one summary row once its provider is
    // gone, without a new activity event or a moved `last_activity`.
    let home = TempDir::new("cursor-only");
    let a = FixtureRepo::new("origin");
    a.branch_with_commits("feat-only", 1, true);
    let wt = a.add_worktree("only", Some("feat-only"));
    let id = "88000000-1111-2222-3333-444444444444";
    transcript(&home, id, &wt);
    let mut agent = support::live_claude(&home, id, "busy", &wt);
    let world = World {
        home,
        a,
        b: FixtureRepo::new("origin"),
        agent: None,
    };
    let first = collect(&world);
    let before = work(&first, "feat-only");
    let before_at = before.conversation_summaries[0].occurred_at_ms;
    let before_activities = before.activities.clone();
    let before_last = before.last_activity;

    // The provider goes silent and the file predates context.
    agent.kill().expect("kill");
    let _ = agent.wait();
    fs::remove_file(world.home.join(format!(".claude/projects/t/{id}.jsonl")))
        .expect("the transcript deletes");
    fs::remove_dir_all(world.home.join(".claude/sessions")).expect("sessions clear");
    strip_session_context(&world.home);
    // A quiet conversation keeps the repo in scope.
    let keeper_id = "99000000-1111-2222-3333-444444444444";
    transcript(&world.home, keeper_id, &world.a.main);

    let second = collect(&world);
    let summary = only_summary(&second, "feat-only");
    assert_eq!(
        summary.key,
        agent_sessions::store::conversation_key("claude", id)
    );
    assert_eq!(summary.occurred_at_ms, before_at);
    assert!(
        summary.context.is_none(),
        "no context was ever captured: {summary:?}"
    );
    let after = work(&second, "feat-only");
    assert_eq!(after.activities, before_activities, "no new activity");
    assert_eq!(after.last_activity, before_last);

    // Rendered: the id alone carries the row - no title, no prompt. The
    // row sits in a cleanup section, collapsed under `all`, so scope to
    // the repo first to expand it.
    let mut app = App::new(second);
    app.key(Key::Char('1'));
    app.key(Key::Char('j'));
    app.key(Key::Char('2'));
    for _ in 0..12 {
        let text = render(&app, 200, 40);
        if text.contains("Work - feat-only") {
            let line = text
                .lines()
                .find(|l| l.contains("last activity"))
                .expect("the summary row renders: {text}");
            assert!(line.contains("88000000"), "{text}");
            assert!(!line.contains(" - "), "{text}");
            return;
        }
        app.key(Key::Char('j'));
    }
    panic!("feat-only never focused");
}

#[test]
fn gone_branch_and_detached_path_rows_keep_their_summaries() {
    if !tmux_or_skip() {
        return;
    }
    // A conversation per anchor, both captured; then both workspaces
    // vanish under panes that still sit inside them - the gone rows keep
    // the summaries their records captured, not the provider's present.
    let home = TempDir::new("gone-summaries");
    let tmux = TmuxServer::new();
    let a = FixtureRepo::new("origin");
    a.branch_with_commits("feat-gone", 1, true);
    let wt = a.add_worktree("gone", Some("feat-gone"));
    let det = a.add_worktree("det", None);
    let gid = "aa110000-1111-2222-3333-444444444444";
    let did = "bb220000-1111-2222-3333-444444444444";
    let kid = "cc330000-1111-2222-3333-444444444444";
    transcript(&home, gid, &wt);
    transcript(&home, did, &det);
    transcript(&home, kid, &a.main);
    let mut agent_g = support::live_claude(&home, gid, "busy", &wt);
    let mut agent_d = support::live_claude(&home, did, "busy", &det);
    // Panes parked inside both workspaces: what retains the gone rows.
    tmux.tmux(&[
        "new-session",
        "-d",
        "-s",
        "g",
        "-x",
        "100",
        "-y",
        "24",
        "-c",
        wt.to_str().unwrap(),
        "sleep 300",
    ]);
    tmux.tmux(&[
        "new-session",
        "-d",
        "-s",
        "d",
        "-x",
        "100",
        "-y",
        "24",
        "-c",
        det.to_str().unwrap(),
        "sleep 300",
    ]);
    let world = World {
        home,
        a,
        b: FixtureRepo::new("origin"),
        agent: None,
    };
    let sockets = [tmux.socket.clone()];
    let first = collect_on(&world, &sockets);
    let branch_row = work(&first, "feat-gone");
    let branch_summary = only_summary(&first, "feat-gone").clone();
    let det_row = first
        .work
        .iter()
        .find(|w| w.worktree.as_deref() == Some(det.as_path()))
        .expect("the detached row");
    let det_summary = det_row
        .conversation_summaries
        .iter()
        .find(|s| s.key.contains(did))
        .expect("the detached summary")
        .clone();
    let branch_last = branch_row.last_activity;
    let det_last = det_row.last_activity;

    // Both gone: worktrees removed, the branch deleted - while the panes
    // and agents still sit inside. The agents die too, so nothing about
    // the provider's present can substitute for the captured context.
    agent_g.kill().expect("kill");
    let _ = agent_g.wait();
    agent_d.kill().expect("kill");
    let _ = agent_d.wait();
    world.a.git(
        &world.a.main,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
    world.a.git(
        &world.a.main,
        &["worktree", "remove", "--force", det.to_str().unwrap()],
    );
    world.a.git(&world.a.main, &["branch", "-D", "feat-gone"]);

    let second = collect_on(&world, &sockets);
    let gone_branch = second
        .work
        .iter()
        .find(|w| w.name == "feat-gone" && w.gone.is_some())
        .expect("the gone branch row");
    let summary = gone_branch
        .conversation_summaries
        .iter()
        .find(|s| s.key == branch_summary.key)
        .expect("the retained summary");
    assert_eq!(summary.occurred_at_ms, branch_summary.occurred_at_ms);
    assert_eq!(
        summary
            .context
            .as_ref()
            .and_then(|c| c.prompt_excerpt.as_deref()),
        branch_summary
            .context
            .as_ref()
            .and_then(|c| c.prompt_excerpt.as_deref()),
        "the gone row keeps the record's own capture: {summary:?}"
    );
    assert!(summary.context.is_some());
    assert_eq!(gone_branch.last_activity, branch_last);

    let gone_det = second
        .work
        .iter()
        .find(|w| w.kind == WorkKind::Detached && w.gone.is_some())
        .expect("the gone detached row");
    let summary = gone_det
        .conversation_summaries
        .iter()
        .find(|s| s.key == det_summary.key)
        .expect("the retained detached summary");
    assert_eq!(summary.occurred_at_ms, det_summary.occurred_at_ms);
    assert_eq!(
        summary
            .context
            .as_ref()
            .and_then(|c| c.prompt_excerpt.as_deref()),
        det_summary
            .context
            .as_ref()
            .and_then(|c| c.prompt_excerpt.as_deref())
    );
    assert_eq!(gone_det.last_activity, det_last);

    // Rendered: the gone branch's row still shows the compacted summary.
    // It sits in cleanup review, collapsed under `all` - scope to the
    // repo to expand it.
    let mut app = App::new(second);
    app.key(Key::Char('1'));
    app.key(Key::Char('j'));
    app.key(Key::Char('2'));
    for _ in 0..16 {
        let text = render(&app, 200, 40);
        if text.contains("Work - feat-gone") {
            assert!(text.contains("last activity"), "{text}");
            assert!(text.contains("aa110000"), "{text}");
            return;
        }
        app.key(Key::Char('j'));
    }
    panic!("the gone feat-gone row never focused");
}
