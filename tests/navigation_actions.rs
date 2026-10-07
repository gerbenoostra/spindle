//! Enter and `o` end to end: a fixture `~/.claude`, a disposable agent
//! process, a throwaway `tmux -L` server and a fixture git repository.
//! Jump and open act against exactly that world - real tmux selections
//! on the disposable server, PATH stubs standing in for the resume
//! executable and the platform opener - never the user's tmux server, a
//! live agent or the network.

mod support;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use agent_sessions::action::{ActionOutcome, ActionRequest, ResumePlan, act, work_key};
use agent_sessions::attention::Attention;
use agent_sessions::forge::{Pipeline, WorkItem};
use agent_sessions::runtime::{Provider, Runtime};
use agent_sessions::snapshot::{
    Collector, ConversationRow, ConversationState, PaneRow, Snapshot, Upstream, WorkKind, WorkRow,
    WorkSection,
};
use agent_sessions::store::{self, Seen, Store};
use agent_sessions::tmux::{PaneId, PaneTarget, WindowId};
use agent_sessions::tui;
use support::fixture::FixtureRepo;
use support::tempdir::TempDir;
use support::tmux::TmuxServer;
use support::tmux_or_skip;

const LIVE_ID: &str = "8f423bbb-1111-2222-3333-444444444444";
const STOPPED_ID: &str = "02aa0bbb-1111-2222-3333-444444444444";

/// `exec_resume` changes the process's real cwd, which every sibling
/// test in this binary shares: the whole file serializes so no spawn
/// inherits a changed - or already deleted - directory.
static CWD_LOCK: Mutex<()> = Mutex::new(());

fn locked() -> MutexGuard<'static, ()> {
    CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The platform's opener name - the one `o` resolves on this OS.
#[cfg(target_os = "macos")]
const OPENER: &str = "open";
/// The platform's opener name - the one `o` resolves on this OS.
#[cfg(not(target_os = "macos"))]
const OPENER: &str = "xdg-open";

/// The pane command of a `claude` process that is really `bash` - a
/// symlink, not a copy, because macOS kills a relocated copy of a signed
/// system binary, while `comm` still reports the invoked name.
fn fake_agent(dir: &TempDir) -> String {
    let exe = dir.join("claude");
    if !exe.exists() {
        std::os::unix::fs::symlink(support::on_path("bash"), &exe).expect("bash links");
    }
    format!("exec {} -c 'sleep 300; exit'", exe.display())
}

/// `<root>/sessions/<pid>.json`, with the `tmux` handle a Claude record
/// publishes - `procStart` is the real process's start so the pair
/// validates as that instance.
fn session_file(home: &TempDir, pid: u32, id: &str, worktree: &Path, handle: &str) {
    let sessions = home.join(".claude/sessions");
    fs::create_dir_all(&sessions).expect("mkdir");
    fs::write(
        sessions.join(format!("{pid}.json")),
        format!(
            "{{\"pid\":{pid},\"sessionId\":\"{id}\",\"status\":\"waiting\",\"updatedAt\":1788621019906,\"statusUpdatedAt\":1788621019906,\"waitingFor\":\"permission prompt\",\"cwd\":\"{}\",\"tmux\":\"{handle}\",\"procStart\":\"{}\"}}",
            worktree.display(),
            support::proc_start(pid)
        ),
    )
    .expect("session file writes");
}

/// `<root>/projects/<slug>/<id>.jsonl` - a transcript-only conversation.
fn transcript(home: &TempDir, slug: &str, id: &str, cwd: &Path) {
    let projects = home.join(format!(".claude/projects/{slug}"));
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

/// The store dir a world's collect and actions share.
fn store_dir(home: &TempDir) -> PathBuf {
    home.join("state/agent-sessions")
}

/// One collect over a world: the Claude fixture, the attention store,
/// the disposable server's whole inventory.
fn snapshot(home: &TempDir, socket: &Path) -> Snapshot {
    let mut collector = Collector::new(home.join(".claude")).with_store(store_dir(home));
    let runtime = Runtime::observe_over(std::slice::from_ref(&socket.to_path_buf()));
    collector.collect(&runtime, None)
}

/// The world: one fixture repo with a `feat-login` worktree and a
/// branch-only `feat-no-wt`, a fake Claude waiting in a tmux pane (the
/// handle published, like a real record does), and a transcript-only
/// stopped conversation.
struct World {
    tmux: TmuxServer,
    home: TempDir,
    _repo: FixtureRepo,
    worktree: PathBuf,
    /// The agent pane's `%id`.
    pane: String,
    /// The agent pane's `@id` window.
    window: String,
}

fn world() -> World {
    let tmux = TmuxServer::new();
    let repo = FixtureRepo::new("origin");
    repo.branch_with_commits("feat-login", 1, false);
    let worktree = repo.add_worktree("login", Some("feat-login"));
    repo.branch_with_commits("feat-no-wt", 1, false);

    let home = TempDir::new("nav-actions");
    let agent = fake_agent(&home);
    tmux.tmux(&[
        "new-session",
        "-d",
        "-s",
        "agents",
        "-x",
        "100",
        "-y",
        "24",
        "-c",
        worktree.to_str().unwrap(),
        &agent,
    ]);
    let (pid, handle) = tmux.pane_running("agents", "claude");
    // `agents:@W.%P` - the pieces the jump must select exactly.
    let (window, pane) = handle
        .split_once(':')
        .and_then(|(_, ids)| ids.split_once('.'))
        .map(|(w, p)| (w.to_owned(), p.to_owned()))
        .expect("the handle is session:@window.%pane");
    // Demote the agent pane: a second pane takes its window's active
    // slot, a second window takes the session's current slot - the jump
    // has to earn both back.
    tmux.tmux(&["split-window", "-t", &window, "-c", "/tmp"]);
    tmux.tmux(&["new-window", "-t", "agents", "-c", "/tmp"]);

    session_file(&home, pid, LIVE_ID, &worktree, &handle);
    transcript(&home, "-r-login", LIVE_ID, &worktree);
    transcript(&home, "-r-login", STOPPED_ID, &worktree);

    World {
        tmux,
        home,
        _repo: repo,
        worktree,
        pane,
        window,
    }
}

/// Sessions, windows and panes on the server - a count per kind, so an
/// action's footprint is asserted exactly.
fn topology(tmux: &TmuxServer) -> [usize; 3] {
    let count = |args: &[&str]| tmux.tmux(args).lines().count();
    [
        count(&["list-sessions", "-F", "#{session_id}"]),
        count(&["list-windows", "-a", "-F", "#{window_id}"]),
        count(&["list-panes", "-a", "-F", "#{pane_id}"]),
    ]
}

/// `session`'s current window id.
fn current_window(tmux: &TmuxServer, session: &str) -> String {
    tmux.tmux(&[
        "list-windows",
        "-t",
        session,
        "-F",
        "#{window_id} #{window_active}",
    ])
    .lines()
    .find(|line| line.ends_with(" 1"))
    .map(|line| line.split(' ').next().unwrap().to_owned())
    .expect("a session always has a current window")
}

/// Whether `pane` is the active pane of `window`.
fn pane_active(tmux: &TmuxServer, window: &str, pane: &str) -> bool {
    tmux.tmux(&[
        "list-panes",
        "-t",
        window,
        "-F",
        "#{pane_id} #{pane_active}",
    ])
    .lines()
    .any(|line| line == format!("{pane} 1"))
}

/// A PATH stub: writes its argv, one element per line, and its cwd into
/// the stub directory - the exact record a resume or opener would leave.
fn stub(dir: &TempDir, name: &str, extra: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\npwd > \"{}\"\n{extra}",
            dir.join("argv").display(),
            dir.join("cwd").display(),
        ),
    )
    .expect("the stub writes");
    let mut permissions = fs::metadata(&path).expect("the stub exists").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("the stub is executable");
    path
}

/// A `ConversationRow` carrying only what the resume tests name: the
/// provider's resume argv verbatim, nothing else proven.
fn conversation(session_id: &str) -> ConversationRow {
    ConversationRow {
        provider: Provider::Claude,
        session_id: session_id.to_owned(),
        short_id: session_id.chars().take(8).collect(),
        title: None,
        state: ConversationState::Idle,
        state_raw: None,
        waiting_for: None,
        state_since: None,
        state_since_ms: None,
        attention: Attention::None,
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
        resume_argv: vec![
            "claude".to_owned(),
            "--resume".to_owned(),
            session_id.to_owned(),
        ],
        latest_prompt: None,
        latest_reply: None,
        repo: None,
        worktree: None,
        branch: None,
        touches: Vec::new(),
        current_incarnation: None,
        started_at: None,
        related: Vec::new(),
        evidence: Default::default(),
    }
}

/// A `WorkRow` carrying only what the forge tests name.
fn work_row(name: &str, forge: WorkItem, url: Option<&str>) -> WorkRow {
    WorkRow {
        repo: "/repos/a/.git".to_owned(),
        repo_name: "a".to_owned(),
        kind: WorkKind::Branch,
        name: name.to_owned(),
        worktree: None,
        branch: Some(name.to_owned()),
        dirty: None,
        broken: None,
        activities: Vec::new(),
        observations: Vec::new(),
        commits_ahead: None,
        unpushed: None,
        upstream: Upstream::Unknown,
        upstream_detail: None,
        landed: None,
        base: None,
        windows: 0,
        live_pids: 0,
        live_sessions: 0,
        past_sessions: 0,
        last_activity: None,
        attention: Attention::None,
        identity: Some(format!("i-{name}")),
        incarnation: None,
        same_name_history: Vec::new(),
        parked: false,
        forge,
        pipeline: Pipeline::Unknown,
        forge_label: None,
        forge_url: url.map(str::to_owned),
        commits_behind: None,
        commits: None,
        panes: Vec::new(),
        gone: None,
        references: Vec::new(),
        worktree_removal: None,
        branch_deletion: None,
        section: WorkSection::FollowUp,
        summary: String::new(),
    }
}

/// A complete snapshot holding exactly the rows given.
fn snapshot_of(work: Vec<WorkRow>, conversations: Vec<ConversationRow>) -> Snapshot {
    let mut snapshot = Snapshot::empty();
    snapshot.complete = true;
    snapshot.work = work;
    snapshot.conversations = conversations;
    snapshot
}

/// `request` resolved and acted on through the library API.
fn run(
    request: &ActionRequest,
    snapshot: &Snapshot,
    store: Option<&Store>,
    path: Option<&OsStr>,
) -> ActionOutcome {
    act(request, snapshot, store, path)
}

/// `plan` spawned as a child - not exec'd - so the test sees exactly
/// what the stub recorded.
fn enact(plan: &ResumePlan) -> std::process::Output {
    Command::new(&plan.executable)
        .args(&plan.argv)
        .current_dir(&plan.cwd)
        .output()
        .expect("the resume stub spawns")
}

/// Restore the process cwd on drop: `exec_resume` changes it for real,
/// and a threaded test binary shares it.
struct RestoreCwd(PathBuf);

impl Drop for RestoreCwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[test]
fn enter_selects_the_live_pane_and_records_seen_state() {
    let _locked = locked();
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    let snap = snapshot(&world.home, &world.tmux.socket);
    let store = Store::open(store_dir(&world.home));
    let before = topology(&world.tmux);
    // The agent pane is neither active nor current before the jump.
    assert_ne!(current_window(&world.tmux, "agents"), world.window);
    assert!(!pane_active(&world.tmux, &world.window, &world.pane));

    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: LIVE_ID.to_owned(),
        },
        &snap,
        Some(&store),
        None,
    );

    match &outcome {
        ActionOutcome::Done(Some(message)) => {
            assert!(message.contains(&world.pane), "{message}")
        }
        other => panic!("a live conversation jumps: {other:?}"),
    }
    assert_eq!(current_window(&world.tmux, "agents"), world.window);
    assert!(pane_active(&world.tmux, &world.window, &world.pane));
    assert_eq!(topology(&world.tmux), before, "a jump creates nothing");

    // The deliberate jump acknowledged the waiting conversation: the
    // recorded wait lands in seen-state, through the store's own write.
    let seen = Store::open(store_dir(&world.home)).load().seen;
    let key = store::conversation_key("claude", LIVE_ID);
    assert!(
        seen.get(&key).is_some_and(|s| s.wait_ms.is_some()),
        "{seen:?}"
    );

    // An acknowledgement the store refuses still lands the jump - the
    // caveat is reported, not hidden.
    let not_a_dir = world.home.join("a-file");
    fs::write(&not_a_dir, "x").unwrap();
    let bad = Store::open(not_a_dir);
    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: LIVE_ID.to_owned(),
        },
        &snap,
        Some(&bad),
        None,
    );
    match &outcome {
        ActionOutcome::Done(Some(message)) => {
            assert!(message.contains("not saved"), "{message}")
        }
        other => panic!("a refused ack still reports the jump: {other:?}"),
    }

    // A request the fresh evidence can no longer match reports and
    // changes nothing.
    let gone = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: "11111111-2222-3333-4444-555555555555".to_owned(),
        },
        &snap,
        Some(&store),
        None,
    );
    assert!(matches!(gone, ActionOutcome::Failed(_)), "{gone:?}");
}

#[test]
fn a_live_conversation_without_a_pane_reports_and_preserves() {
    let _locked = locked();
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    // A second live record on a process that owns no pane: spawned
    // outside tmux, so nothing binds it.
    let mut agent = support::live_claude(&world.home, STOPPED_ID, "waiting", &world.worktree);
    let snap = snapshot(&world.home, &world.tmux.socket);
    let store = Store::open(store_dir(&world.home));

    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: STOPPED_ID.to_owned(),
        },
        &snap,
        Some(&store),
        None,
    );
    match &outcome {
        ActionOutcome::Failed(message) => assert!(message.contains("pane"), "{message}"),
        other => panic!("live but unbound reports, never jumps: {other:?}"),
    }
    let seen = Store::open(store_dir(&world.home)).load().seen;
    assert!(seen.is_empty(), "nothing was acknowledged: {seen:?}");
    let _ = agent.kill();
    let _ = agent.wait();
}

#[test]
fn a_failed_select_preserves_seen_state() {
    let _locked = locked();
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    let snap = snapshot(&world.home, &world.tmux.socket);
    let store = Store::open(store_dir(&world.home));
    // The snapshot still believes the pane bound; the server is gone
    // before the request resolves, and the select fails.
    let killed = world.tmux.try_tmux(&["kill-server"]);
    assert!(killed.status.success());

    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: LIVE_ID.to_owned(),
        },
        &snap,
        Some(&store),
        None,
    );
    match &outcome {
        ActionOutcome::Failed(message) => assert!(message.contains("tmux"), "{message}"),
        other => panic!("a dead server cannot select: {other:?}"),
    }
    let seen = Store::open(store_dir(&world.home)).load().seen;
    assert!(seen.is_empty(), "a failed select acknowledges nothing");

    // The work-row select fails the same way.
    let row = snap
        .work
        .iter()
        .find(|w| w.name == "feat-login")
        .expect("the worktree row");
    let outcome = run(
        &ActionRequest::EnterWork { key: work_key(row) },
        &snap,
        None,
        None,
    );
    assert!(matches!(outcome, ActionOutcome::Failed(_)), "{outcome:?}");
}

#[test]
fn enter_selects_the_work_window_deterministically() {
    let _locked = locked();
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    // A second window bound to the same worktree: two action targets,
    // and the sorted lowest one - the agent's - is what gets selected.
    world.tmux.tmux(&[
        "new-window",
        "-t",
        "agents",
        "-c",
        world.worktree.to_str().unwrap(),
    ]);
    let snap = snapshot(&world.home, &world.tmux.socket);
    let row = snap
        .work
        .iter()
        .find(|w| w.name == "feat-login")
        .expect("the worktree row");
    assert!(row.panes.len() >= 2, "two windows bind: {:?}", row.panes);
    let key = work_key(row);
    let before = topology(&world.tmux);
    assert_ne!(current_window(&world.tmux, "agents"), world.window);

    let outcome = run(&ActionRequest::EnterWork { key }, &snap, None, None);

    match &outcome {
        ActionOutcome::Done(Some(message)) => {
            assert!(message.contains(&world.window), "{message}")
        }
        other => panic!("the work window selects: {other:?}"),
    }
    assert_eq!(current_window(&world.tmux, "agents"), world.window);
    assert_eq!(topology(&world.tmux), before, "enter creates nothing");

    // A branch-only row has no bound topology: it reports, selects
    // nothing.
    let bare = snap
        .work
        .iter()
        .find(|w| w.name == "feat-no-wt")
        .expect("the branch-only row");
    let outcome = run(
        &ActionRequest::EnterWork {
            key: work_key(bare),
        },
        &snap,
        None,
        None,
    );
    match &outcome {
        ActionOutcome::Failed(message) => assert!(message.contains("window"), "{message}"),
        other => panic!("missing topology reports unavailable: {other:?}"),
    }
    // And a key nothing names reports gone.
    let outcome = run(
        &ActionRequest::EnterWork {
            key: "no\0such\0row".to_owned(),
        },
        &snap,
        None,
        None,
    );
    assert!(matches!(outcome, ActionOutcome::Failed(_)), "{outcome:?}");
}

#[test]
fn same_name_work_rows_select_their_own_window() {
    let _locked = locked();
    if !tmux_or_skip() {
        return;
    }
    // Two detached checkouts of one commit read identically by name -
    // `detached @<sha>` twice - so the workspace distinguishes them; a
    // recreated branch likewise shares its name with the gone record a
    // reference still names. Each row's bound pane lives on a dead
    // socket of its own: the select fails either way, but the error
    // names whose target it tried.
    let detached = |workspace: &str, socket: &str| WorkRow {
        worktree: Some(PathBuf::from(workspace)),
        kind: WorkKind::Detached,
        panes: vec![PaneRow {
            handle: "agents:@1.%1".to_owned(),
            command: "claude".to_owned(),
            target: Some(PaneTarget {
                socket: PathBuf::from(socket),
                window: WindowId::parse("@1").unwrap(),
                pane: PaneId::parse("%1").unwrap(),
            }),
        }],
        ..work_row("detached @abc1234", WorkItem::Unknown, None)
    };
    let snap = snapshot_of(
        vec![detached("/ws/a", "/sock/a"), detached("/ws/b", "/sock/b")],
        Vec::new(),
    );
    assert_ne!(
        work_key(&snap.work[0]),
        work_key(&snap.work[1]),
        "the selection key pins one row"
    );

    let outcome = run(
        &ActionRequest::EnterWork {
            key: work_key(&snap.work[1]),
        },
        &snap,
        None,
        None,
    );
    match &outcome {
        ActionOutcome::Failed(message) => {
            assert!(
                message.contains("/sock/b"),
                "the second row's target: {message}"
            )
        }
        other => panic!("a dead socket reports its own target: {other:?}"),
    }
}

#[test]
fn a_stopped_conversation_produces_the_exact_resume() {
    let _locked = locked();
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    let stub_dir = TempDir::new("nav-path");
    let exe = stub(&stub_dir, "claude", "");
    let snap = snapshot(&world.home, &world.tmux.socket);
    let store = Store::open(store_dir(&world.home));

    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: STOPPED_ID.to_owned(),
        },
        &snap,
        Some(&store),
        Some(stub_dir.path().as_os_str()),
    );
    let ActionOutcome::Resume(plan) = outcome else {
        panic!("a stopped resumable conversation resumes: {outcome:?}")
    };
    assert_eq!(plan.executable, exe.as_os_str());
    assert_eq!(
        plan.argv,
        [OsString::from("--resume"), OsString::from(STOPPED_ID)],
        "argv[0] separates; the rest is the provider's own argv"
    );
    assert_eq!(plan.cwd, world.worktree);

    // Enacted, the plan lands the stub in the work root with the
    // provider's argv verbatim.
    let out = enact(&plan);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(stub_dir.join("argv")).expect("argv recorded"),
        format!("--resume\n{STOPPED_ID}\n")
    );
    assert_eq!(
        fs::read_to_string(stub_dir.join("cwd"))
            .expect("cwd recorded")
            .trim(),
        world.worktree.display().to_string()
    );

    // Every resume precondition reports rather than launching: a PATH
    // without the executable, a missing root, a gone root, and a
    // provider with no resume at all.
    let empty = TempDir::new("nav-empty");
    let missing = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: STOPPED_ID.to_owned(),
        },
        &snap,
        Some(&store),
        Some(empty.path().as_os_str()),
    );
    match &missing {
        ActionOutcome::Failed(m) => assert!(m.contains("not found"), "{m}"),
        other => panic!("a missing executable reports: {other:?}"),
    }
    let mut unknown_root = conversation(STOPPED_ID);
    unknown_root.worktree = None;
    let mut gone_root = conversation(STOPPED_ID);
    gone_root.worktree = Some(PathBuf::from("/definitely/gone/root"));
    let mut incapable = conversation(STOPPED_ID);
    incapable.resume_argv = Vec::new();
    for (row, expect) in [
        (&unknown_root, "work root"),
        (&gone_root, "gone"),
        (&incapable, "no resume"),
    ] {
        let outcome = run(
            &ActionRequest::EnterConversation {
                provider: Provider::Claude,
                session_id: STOPPED_ID.to_owned(),
            },
            &snapshot_of(Vec::new(), vec![row.clone()]),
            Some(&store),
            Some(stub_dir.path().as_os_str()),
        );
        match &outcome {
            ActionOutcome::Failed(m) => assert!(m.contains(expect), "{m}"),
            other => panic!("{expect} reports: {other:?}"),
        }
    }
    let seen = Store::open(store_dir(&world.home)).load().seen;
    assert!(seen.is_empty(), "refused resumes record nothing: {seen:?}");

    // An argv0 carrying `/` is the explicit path: the stub itself
    // qualifies; a missing one refuses.
    let mut explicit = conversation(STOPPED_ID);
    explicit.resume_argv = vec![
        exe.to_string_lossy().to_string(),
        "--resume".to_owned(),
        STOPPED_ID.to_owned(),
    ];
    explicit.worktree = Some(world.worktree.clone());
    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: STOPPED_ID.to_owned(),
        },
        &snapshot_of(Vec::new(), vec![explicit]),
        None,
        None,
    );
    match &outcome {
        ActionOutcome::Resume(plan) => assert_eq!(plan.executable, exe.as_os_str()),
        other => panic!("an explicit path resolves: {other:?}"),
    }
    let mut absent = conversation(STOPPED_ID);
    absent.resume_argv = vec![
        "/definitely/gone/claude".to_owned(),
        "--resume".to_owned(),
        STOPPED_ID.to_owned(),
    ];
    absent.worktree = Some(world.worktree.clone());
    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: STOPPED_ID.to_owned(),
        },
        &snapshot_of(Vec::new(), vec![absent]),
        None,
        None,
    );
    assert!(matches!(outcome, ActionOutcome::Failed(_)), "{outcome:?}");

    // A `None` path resolves against the inherited PATH: a name that is
    // not there reports not found rather than guessing.
    let mut unfound = conversation(STOPPED_ID);
    unfound.resume_argv = vec![
        "definitely-not-an-executable".to_owned(),
        "--resume".to_owned(),
        STOPPED_ID.to_owned(),
    ];
    unfound.worktree = Some(world.worktree.clone());
    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: STOPPED_ID.to_owned(),
        },
        &snapshot_of(Vec::new(), vec![unfound]),
        None,
        None,
    );
    match &outcome {
        ActionOutcome::Failed(m) => assert!(m.contains("not found"), "{m}"),
        other => panic!("a PATH miss reports: {other:?}"),
    }
}

#[test]
fn a_shell_metachar_session_id_stays_one_argument() {
    let _locked = locked();
    let stub_dir = TempDir::new("nav-metas");
    stub(&stub_dir, "claude", "");
    let root = TempDir::new("nav-metawt");
    let id = "id;$(touch nope) with spaces";
    let mut row = conversation(id);
    row.worktree = Some(root.path().to_path_buf());
    let snap = snapshot_of(Vec::new(), vec![row]);

    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: id.to_owned(),
        },
        &snap,
        None,
        Some(stub_dir.path().as_os_str()),
    );
    let ActionOutcome::Resume(plan) = outcome else {
        panic!("the id resumes: {outcome:?}")
    };
    assert_eq!(
        plan.argv,
        [OsString::from("--resume"), OsString::from(id)],
        "the whole id is one argv element"
    );
    let out = enact(&plan);
    assert!(out.status.success());
    assert_eq!(
        fs::read_to_string(stub_dir.join("argv")).expect("argv recorded"),
        format!("--resume\n{id}\n"),
        "spaces and metacharacters survive as one argument"
    );
    assert!(
        !root.join("nope").exists() && !stub_dir.join("nope").exists(),
        "no shell ever saw the id"
    );
}

#[test]
fn a_fresh_live_replacement_jumps_instead_of_resuming() {
    let _locked = locked();
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    // The stopped conversation's id is claimed live on a new pane before
    // the request resolves - the fresh evidence jumps, never resumes.
    let agent = fake_agent(&world.home);
    world.tmux.tmux(&[
        "new-session",
        "-d",
        "-s",
        "rebound",
        "-x",
        "100",
        "-y",
        "24",
        "-c",
        world.worktree.to_str().unwrap(),
        &agent,
    ]);
    let (pid, handle) = world.tmux.pane_running("rebound", "claude");
    session_file(&world.home, pid, STOPPED_ID, &world.worktree, &handle);
    let snap = snapshot(&world.home, &world.tmux.socket);
    let store = Store::open(store_dir(&world.home));

    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: STOPPED_ID.to_owned(),
        },
        &snap,
        Some(&store),
        None,
    );
    match &outcome {
        ActionOutcome::Done(Some(message)) => assert!(message.contains('%'), "{message}"),
        ActionOutcome::Resume(_) => panic!("a live replacement jumps, never resumes"),
        other => panic!("a live replacement jumps: {other:?}"),
    }
}

#[test]
fn exec_resume_chdirs_acks_then_execs() {
    let _locked = locked();
    let dir = TempDir::new("nav-exec");
    let store_dir = dir.join("state");
    let store = Store::open(store_dir.clone());
    let key = store::conversation_key("claude", STOPPED_ID);
    let launch = std::env::current_dir().expect("a cwd");
    let _restore = RestoreCwd(launch.clone());
    let exe = stub(&dir, "claude", "");
    let argv = vec![OsString::from("--resume"), OsString::from(STOPPED_ID)];
    let cwd_is = |dir: &Path| assert_eq!(std::env::current_dir().expect("a cwd"), dir);

    // A missing work root fails before anything moves - nothing
    // recorded, cwd untouched.
    let plan = ResumePlan {
        executable: exe.clone().into_os_string(),
        argv: argv.clone(),
        cwd: dir.join("missing"),
        acknowledgement: Some((key.clone(), 3, Some(7))),
    };
    assert!(
        exec_resume_err(&plan, Some(&store)).contains("cannot enter"),
        "a missing root refuses"
    );
    cwd_is(&launch);
    assert!(Store::open(store_dir.clone()).load().seen.is_empty());

    // An executable that is missing or not runnable fails preflight:
    // before the acknowledgement, and with the dashboard's cwd restored.
    let no_exec = dir.join("claude-not-executable");
    fs::write(&no_exec, "#!/bin/sh\nexit 0\n").expect("a regular file");
    for executable in [
        dir.join("missing-exe").into_os_string(),
        no_exec.clone().into_os_string(),
    ] {
        let plan = ResumePlan {
            executable,
            argv: argv.clone(),
            cwd: dir.path().to_path_buf(),
            acknowledgement: Some((key.clone(), 3, Some(7))),
        };
        let err = exec_resume_err(&plan, Some(&store));
        assert!(err.contains("not executable"), "{err}");
        cwd_is(&launch);
        assert!(
            Store::open(store_dir.clone()).load().seen.is_empty(),
            "a preflight failure preserves seen-state"
        );
    }

    // The one refusal preflight cannot rule out is execve's own - here
    // E2BIG, an argv the kernel cannot carry (ENOEXEC would shell
    // fallback and replace this test process instead of erroring). By
    // then the acknowledgement has landed; the failure still reports
    // and restores the cwd - it cannot un-write the ack.
    let plan = ResumePlan {
        executable: exe.clone().into_os_string(),
        argv: vec![OsString::from("x".repeat(2 * 1024 * 1024))],
        cwd: dir.path().to_path_buf(),
        acknowledgement: Some((key.clone(), 3, Some(7))),
    };
    let err = exec_resume_err(&plan, Some(&store));
    assert!(err.contains("claude"), "{err}");
    cwd_is(&launch);
    let seen = Store::open(store_dir.clone()).load().seen;
    assert_eq!(
        seen.get(&key).copied(),
        Some(Seen {
            seq: 3,
            wait_ms: Some(7)
        }),
        "the authored ack lands once preflight passes: {seen:?}"
    );

    // A store that refuses the ack reports it instead of exec'ing - and
    // still puts the cwd back.
    let not_a_dir = dir.join("a-file");
    fs::write(&not_a_dir, "x").unwrap();
    let bad = Store::open(not_a_dir);
    let err = exec_resume_err(&plan, Some(&bad));
    assert!(err.contains("seen-state"), "{err}");
    cwd_is(&launch);

    // A plan carrying no acknowledgement - or no store - reaches exec
    // with nothing to write and reports the same refusal.
    let quiet = ResumePlan {
        acknowledgement: None,
        ..plan.clone()
    };
    let err = exec_resume_err(&quiet, Some(&store));
    assert!(err.contains("claude"), "{err}");
    cwd_is(&launch);
    let err = exec_resume_err(&plan, None);
    assert!(err.contains("claude"), "{err}");
    cwd_is(&launch);

    // A deleted cwd fails before anything moves - and resolving with no
    // cwd to anchor on leaves a relative candidate unresolved.
    let gone = TempDir::new("nav-cwd");
    let gone_path = gone.path().to_path_buf();
    std::env::set_current_dir(&gone_path).expect("the dir exists");
    drop(gone);
    let err = exec_resume_err(&plan, Some(&store));
    assert!(err.contains("current directory"), "{err}");
    let mut row = conversation(STOPPED_ID);
    row.resume_argv = vec![
        "./claude".to_owned(),
        "--resume".to_owned(),
        STOPPED_ID.to_owned(),
    ];
    row.worktree = Some(dir.path().to_path_buf());
    let outcome = run(
        &ActionRequest::EnterConversation {
            provider: Provider::Claude,
            session_id: STOPPED_ID.to_owned(),
        },
        &snapshot_of(Vec::new(), vec![row]),
        None,
        None,
    );
    assert!(matches!(outcome, ActionOutcome::Failed(_)), "{outcome:?}");
    std::env::set_current_dir(&launch).expect("the launch dir still exists");
    cwd_is(&launch);
}

/// `exec_resume`'s error string; the success side never returns.
fn exec_resume_err(plan: &ResumePlan, store: Option<&Store>) -> String {
    agent_sessions::action::exec_resume(plan, store).expect_err("exec only fails here")
}

#[test]
fn relative_executables_anchor_on_the_dashboard_cwd() {
    let _locked = locked();
    // The launch cwd holds the real stubs; the Work root holds a decoy
    // with the same name that would fail if it were ever run.
    let launch_dir = TempDir::new("nav-launch");
    let work_dir = TempDir::new("nav-work");
    let launch = launch_dir.path().to_path_buf();
    let _restore = RestoreCwd(std::env::current_dir().expect("a cwd"));
    let real = stub(&launch_dir, "claude", "");
    let bin = launch_dir.join("bin");
    fs::create_dir_all(&bin).expect("bin");
    let bin_stub = bin.join("claude");
    fs::write(&bin_stub, "#!/bin/sh\nexit 9\n").expect("a second claude");
    fs::set_permissions(&bin_stub, fs::Permissions::from_mode(0o755)).unwrap();
    let decoy = work_dir.join("claude");
    fs::write(&decoy, "#!/bin/sh\nexit 9\n").expect("the decoy");
    fs::set_permissions(&decoy, fs::Permissions::from_mode(0o755)).unwrap();

    std::env::set_current_dir(&launch).expect("the launch dir exists");

    // `./claude` resolves to the file the resolver saw under the launch
    // cwd - not the decoy waiting under the Work root.
    let mut row = conversation(STOPPED_ID);
    row.resume_argv = vec![
        "./claude".to_owned(),
        "--resume".to_owned(),
        STOPPED_ID.to_owned(),
    ];
    row.worktree = Some(work_dir.path().to_path_buf());
    let snap = snapshot_of(Vec::new(), vec![row]);
    let request = ActionRequest::EnterConversation {
        provider: Provider::Claude,
        session_id: STOPPED_ID.to_owned(),
    };
    let outcome = run(&request, &snap, None, None);
    let ActionOutcome::Resume(plan) = outcome else {
        panic!("`./claude` resolves: {outcome:?}")
    };
    assert_eq!(
        plan.executable,
        real.as_os_str(),
        "anchored to the launch cwd"
    );
    let out = enact(&plan);
    assert!(out.status.success(), "the launch stub ran, not the decoy");
    assert_eq!(
        fs::read_to_string(launch.join("argv")).expect("argv recorded"),
        format!("--resume\n{STOPPED_ID}\n")
    );
    assert_eq!(
        fs::read_to_string(launch.join("cwd"))
            .expect("cwd recorded")
            .trim(),
        work_dir.path().display().to_string(),
        "the Work root is still the resumed process's cwd"
    );

    // A relative PATH entry anchors on the launch cwd the same way -
    // this resume_argv keeps the bare name so PATH search runs.
    let mut bare = conversation(STOPPED_ID);
    bare.worktree = Some(work_dir.path().to_path_buf());
    let snap = snapshot_of(Vec::new(), vec![bare]);
    let outcome = run(&request, &snap, None, Some(OsStr::new("bin")));
    let ActionOutcome::Resume(plan) = outcome else {
        panic!("`bin/claude` resolves: {outcome:?}")
    };
    assert_eq!(
        plan.executable,
        bin_stub.as_os_str(),
        "a relative PATH dir anchored to the launch cwd"
    );
    let out = enact(&plan);
    assert_eq!(
        out.status.code(),
        Some(9),
        "the bin stub ran, not the launch stub's exit 0"
    );
}

#[test]
fn o_opens_the_recorded_url_and_reports_every_other_state() {
    let _locked = locked();
    let opener_dir = TempDir::new("nav-opener");
    stub(&opener_dir, OPENER, "");
    let work = |forge: WorkItem, url: Option<&str>| {
        snapshot_of(vec![work_row("feat/x", forge, url)], Vec::new())
    };
    // The request carries the row's recorded verdict and URL: the
    // re-resolve proves only that the row still exists.
    let request = |snapshot: &Snapshot| {
        let w = &snapshot.work[0];
        ActionRequest::OpenForge {
            key: work_key(w),
            item: w.forge,
            url: w.forge_url.clone(),
        }
    };

    // GitHub and GitLab URLs pass to the platform opener as exactly one
    // argument - the recorded URL, nothing guessed.
    for url in [
        "https://github.com/o/r/pull/191",
        "https://gitlab.com/o/r/-/merge_requests/7",
    ] {
        let snap = work(WorkItem::Open, Some(url));
        let outcome = run(
            &request(&snap),
            &snap,
            None,
            Some(opener_dir.path().as_os_str()),
        );
        match &outcome {
            ActionOutcome::Done(Some(m)) => assert!(m.contains(url), "{m}"),
            other => panic!("a known URL opens: {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(opener_dir.join("argv")).expect("argv recorded"),
            format!("{url}\n"),
            "exactly one argument"
        );
        let _ = fs::remove_file(opener_dir.join("argv"));
    }

    // Every non-open state reports its word and never invokes the
    // opener; an open item without a URL reports unavailable.
    for (item, expect) in [
        (WorkItem::NotExisting, "not existing"),
        (WorkItem::Closed, "closed"),
        (WorkItem::Unknown, "?"),
    ] {
        let snap = work(item, None);
        let outcome = run(
            &request(&snap),
            &snap,
            None,
            Some(opener_dir.path().as_os_str()),
        );
        match &outcome {
            ActionOutcome::Failed(m) => assert_eq!(m, expect, "{m}"),
            other => panic!("{expect} reports: {other:?}"),
        }
    }
    let snap = work(WorkItem::Open, None);
    let outcome = run(
        &request(&snap),
        &snap,
        None,
        Some(opener_dir.path().as_os_str()),
    );
    match &outcome {
        ActionOutcome::Failed(m) => assert!(m.contains("unavailable"), "{m}"),
        other => panic!("a missing URL reports unavailable: {other:?}"),
    }
    assert!(
        !opener_dir.join("argv").exists(),
        "no opener ever ran for a report"
    );

    // The re-resolved row carries no forge evidence - the action's
    // collect stops before that stage - so the verdict and URL are the
    // recorded ones the request carries: the row exists, the URL opens.
    let snap = work(WorkItem::Unknown, None);
    let outcome = run(
        &ActionRequest::OpenForge {
            key: work_key(&snap.work[0]),
            item: WorkItem::Open,
            url: Some("https://github.com/o/r/pull/191".to_owned()),
        },
        &snap,
        None,
        Some(opener_dir.path().as_os_str()),
    );
    match &outcome {
        ActionOutcome::Done(Some(m)) => assert!(m.contains("pull/191"), "{m}"),
        other => panic!("the recorded URL opens: {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(opener_dir.join("argv")).expect("argv recorded"),
        "https://github.com/o/r/pull/191\n",
        "the recorded URL, not the fresh row's empty field"
    );
    let _ = fs::remove_file(opener_dir.join("argv"));

    // A PATH without the opener, a nonzero opener, and an opener that
    // cannot exec each report failure rather than guessing.
    let empty = TempDir::new("nav-no-opener");
    let snap = work(WorkItem::Open, Some("https://github.com/o/r/pull/191"));
    let outcome = run(&request(&snap), &snap, None, Some(empty.path().as_os_str()));
    match &outcome {
        ActionOutcome::Failed(m) => assert!(m.contains("not found"), "{m}"),
        other => panic!("a missing opener reports: {other:?}"),
    }
    let nonzero = TempDir::new("nav-nonzero");
    stub(&nonzero, OPENER, "exit 3");
    let outcome = run(
        &request(&snap),
        &snap,
        None,
        Some(nonzero.path().as_os_str()),
    );
    match &outcome {
        ActionOutcome::Failed(_) => {}
        other => panic!("a nonzero opener fails: {other:?}"),
    }
    let garbage = TempDir::new("nav-garbage");
    let bad = garbage.join(OPENER);
    fs::write(&bad, "not an executable").unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
    let outcome = run(
        &request(&snap),
        &snap,
        None,
        Some(garbage.path().as_os_str()),
    );
    match &outcome {
        ActionOutcome::Failed(_) => {}
        other => panic!("an unexecutable opener fails: {other:?}"),
    }

    // A work key nothing names reports gone.
    let outcome = run(
        &ActionRequest::OpenForge {
            key: "no\0such\0row".to_owned(),
            item: WorkItem::Open,
            url: Some("https://github.com/o/r/pull/191".to_owned()),
        },
        &snap,
        None,
        Some(opener_dir.path().as_os_str()),
    );
    assert!(matches!(outcome, ActionOutcome::Failed(_)), "{outcome:?}");
}

#[test]
fn the_action_collect_stops_before_remote_evidence() {
    let _locked = locked();
    let repo = FixtureRepo::new("origin");
    repo.branch_with_commits("feat-login", 1, false);
    let worktree = repo.add_worktree("login", Some("feat-login"));
    let home = TempDir::new("nav-local");
    transcript(&home, "-r-login", STOPPED_ID, &worktree);
    let mut collector = Collector::new(home.join(".claude")).with_store(store_dir(&home));

    let snap = collector.collect_local(&Runtime::observe_over(&[]), None);

    // Stages 1 and 2 landed - the conversation and the work row its cwd
    // resolves to - then the pass stopped: no remote or forge ask ran,
    // so nothing carries forge evidence and the snapshot is incomplete.
    assert!(
        snap.conversations
            .iter()
            .any(|c| c.session_id == STOPPED_ID),
        "stage 1's conversation rows"
    );
    let row = snap
        .work
        .iter()
        .find(|w| w.name == "feat-login")
        .expect("stage 2's work row");
    assert_eq!(row.forge, WorkItem::Unknown);
    assert!(row.forge_url.is_none());
    assert!(!snap.complete);
}

#[test]
fn take_action_is_the_only_thing_the_app_spawns() {
    let _locked = locked();
    // The App never acts itself: a keypress only stages the request.
    let mut app = tui::App::new(snapshot_of(
        vec![work_row("feat/x", WorkItem::Open, Some("https://u"))],
        vec![conversation("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")],
    ));
    app.key(tui::Key::Char('2'));
    app.key(tui::Key::Char('j'));
    app.key(tui::Key::Char('o'));
    assert!(matches!(
        app.take_action(),
        Some(ActionRequest::OpenForge { .. })
    ));
    // The work cursor back on `all` unscopes [3]: the conversation lists.
    app.key(tui::Key::Char('k'));
    app.key(tui::Key::Char('3'));
    app.key(tui::Key::Char('j'));
    app.key(tui::Key::Enter);
    assert!(matches!(
        app.take_action(),
        Some(ActionRequest::EnterConversation { .. })
    ));
}
