//! The Work lifecycle end to end: a fixture repo whose branches cover every
//! section, collected through the real pipeline, rendered at both terminal
//! widths and toggled through the real store.
//!
//! Everything is disposable: scratch git repositories, a temp `$HOME`, stub
//! `gh` on a private search path - no live tmux, no real state, no network.
//! A branch's old age is fabricated the way Git records it: the committer
//! clock carries `GIT_COMMITTER_DATE`, and the branch reflog's mtime is set
//! with it, because the collector reads both.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, UNIX_EPOCH};

use agent_sessions::attention::Attention;
use agent_sessions::config::{Config, Loaded};
use agent_sessions::forge::{Forge, Pipeline, WorkItem};
use agent_sessions::runtime::Runtime;
use agent_sessions::snapshot::{Collector, Snapshot, WorkKind, WorkSection, to_json};
use agent_sessions::store::{LifecycleInputs, NormEvent, Record, Store, WorkIdentity};
use agent_sessions::tui::{App, Key};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::fixture::{self, FixtureRepo, Landing};
use support::tempdir::TempDir;

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

/// A `claude` process that is really `bash`: a symlink, not a copy - macOS
/// kills a relocated copy of a signed system binary, while `comm` still
/// reports the invoked name. Spawns `sleep` under the `claude` name.
fn live_agent(home: &TempDir, id: &str, status: &str, worktree: &Path) -> Child {
    let exe = home.join("claude");
    std::os::unix::fs::symlink(support::on_path("bash"), &exe).expect("bash links");
    let child = Command::new(&exe)
        .arg("-c")
        .arg("sleep 300; exit")
        .spawn()
        .expect("the agent spawns");
    let sessions = home.join(".claude/sessions");
    fs::create_dir_all(&sessions).expect("mkdir");
    fs::write(
        sessions.join(format!("{}.json", child.id())),
        format!(
            "{{\"pid\":{},\"sessionId\":\"{id}\",\"status\":\"{status}\",\"updatedAt\":1788621019906,\"statusUpdatedAt\":1788621019906,\"cwd\":\"{}\",\"procStart\":\"{}\"}}",
            child.id(),
            worktree.display(),
            proc_start(child.id()),
        ),
    )
    .expect("session file writes");
    child
}

/// The live process's start as Claude's `procStart` ctime (UTC) - computed
/// from `etime` so the `(pid, pid_start)` pair validates as that instance.
fn proc_start(pid: u32) -> String {
    let out = Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .expect("ps runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let secs: u64 = {
        let t = text.trim();
        let (days, rest) = match t.split_once('-') {
            Some((d, r)) => (d.parse::<u64>().unwrap(), r),
            None => (0, t),
        };
        let mut parts = rest.rsplitn(3, ':');
        let s = parts.next().unwrap().parse::<u64>().unwrap();
        let m = parts.next().map_or(0, |p| p.parse().unwrap());
        let h = parts.next().map_or(0, |p| p.parse().unwrap());
        days * 86400 + h * 3600 + m * 60 + s
    };
    let epoch = now() - secs;
    let out = Command::new("date")
        .args(["-u", "-r", &epoch.to_string(), "+%a %b %e %H:%M:%S %Y"])
        .output()
        .expect("date runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A branch that reads `age` old: every commit carries the backdated
/// committer clock (reflog entries take it too), and the branch reflog's
/// mtime is set to match - `reflog_times` folds the mtime into the newest
/// work entry when the last write was work.
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
    let log = repo.main.join(format!(".git/logs/refs/heads/{branch}"));
    fs::File::options()
        .write(true)
        .open(&log)
        .expect("the branch reflog")
        .set_modified(UNIX_EPOCH + Duration::from_secs(epoch))
        .expect("mtime sets");
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
    let agent = live_agent(&home, ACTIVE_ID, "busy", &active_wt);
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
    transcript(&home, SPACE_ID, &notes);

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
    // A changed lifecycle input on a still-live record is a transition:
    // dirty `feat-resume`'s worktree so the next pass lands an authored
    // `activity_at` the row's `last_activity` folds in.
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
    // different fingerprint and the store dates the change at the sync.
    let notes_path = world.home.join("notes").canonicalize().unwrap();
    let inputs = LifecycleInputs {
        dirty: Some(true),
        ..LifecycleInputs::default()
    };
    store(&world.home)
        .sync_path(&notes_path.display().to_string(), &inputs, now() * 1000)
        .expect("seeds");
    let snapshot = collect(&world);
    let notes = work(&snapshot, "notes");
    assert!(notes.last_activity.is_some(), "{}", notes.summary);
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
    fs::File::options()
        .write(true)
        .open(home.join(format!(".claude/projects/t/{id}.jsonl")))
        .expect("the transcript")
        .set_modified(UNIX_EPOCH + Duration::from_secs(now() - days * 86400))
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
