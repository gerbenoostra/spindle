//! Attention end to end: `hook` pings land in the journal, the reduction
//! and arbitration turn them into effective state and attention, the
//! snapshot carries it into `list --json`, and the focus observation and
//! `space` both acknowledge through the store - all against disposable
//! fixtures: a temp `$HOME`, a `-L` tmux server, a symlinked `bash` as the
//! agent process.

mod support;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use agent_sessions::attention::Attention;
use agent_sessions::runtime::Runtime;
use agent_sessions::snapshot::{Collector, ConversationState, WorkSection};
use agent_sessions::store::Store;
use support::fixture::FixtureRepo;
use support::tempdir::TempDir;
use support::tmux::TmuxServer;
use support::{BIN, stderr_of, tmux_or_skip};

const LIVE_ID: &str = "8f423bbb-1111-2222-3333-444444444444";

/// Everything a hook run needs: the fixture's own HOME, CLAUDE_CONFIG_DIR,
/// XDG_STATE_HOME and tmux socket root - never the user's.
struct Env<'a> {
    home: &'a Path,
    socket_root: &'a Path,
}

/// `agent-sessions <args>` under `env`, `stdin` on the pipe.
fn run(env: &Env<'_>, args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(BIN)
        .args(args)
        .env("HOME", env.home)
        .env("CLAUDE_CONFIG_DIR", env.home.join(".claude"))
        .env("XDG_STATE_HOME", env.home.join("state"))
        .env("TMUX_TMPDIR", env.socket_root)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    child
        .stdin
        .as_mut()
        .expect("stdin is piped")
        .write_all(stdin.as_bytes())
        .expect("the payload writes");
    child.wait_with_output().expect("the binary finishes")
}

/// The store dir `env` routes to.
fn store_dir(env: &Env<'_>) -> PathBuf {
    env.home.join("state/agent-sessions")
}

/// A collect over the world: the Claude fixture store plus the attention
/// store.
fn collect(env: &Env<'_>, socket: &Path) -> agent_sessions::snapshot::Snapshot {
    collect_as(env, socket, None)
}

/// The same collect, claiming `own` is the dashboard's own pane.
fn collect_as(
    env: &Env<'_>,
    socket: &Path,
    own: Option<&agent_sessions::tmux::PaneRef>,
) -> agent_sessions::snapshot::Snapshot {
    let runtime = Runtime::observe_over(std::slice::from_ref(&socket.to_path_buf()));
    let mut collector = Collector::new(env.home.join(".claude")).with_store(store_dir(env));
    collector.collect(&runtime, own)
}

/// The pane command of a `claude` process that is really `bash`: a symlink,
/// not a copy - macOS kills a relocated copy of a signed system binary,
/// while `comm` still reports the invoked name.
fn fake_agent(dir: &TempDir) -> String {
    let exe = dir.join("claude");
    std::os::unix::fs::symlink(support::on_path("bash"), &exe).expect("bash links");
    format!("exec {} -c 'sleep 300; exit'", exe.display())
}

/// `<root>/sessions/<pid>.json` as Claude writes it, `procStart` as a UTC
/// ctime of the real process's start.
fn session_file(home: &TempDir, pid: u32, id: &str, status: &str, worktree: &Path) {
    let sessions = home.join(".claude/sessions");
    fs::create_dir_all(&sessions).expect("mkdir");
    let start = proc_start(pid);
    fs::write(
        sessions.join(format!("{pid}.json")),
        format!(
            "{{\"pid\":{pid},\"sessionId\":\"{id}\",\"status\":\"{status}\",\"updatedAt\":1788621019906,\"statusUpdatedAt\":1788621019906,\"waitingFor\":\"permission prompt\",\"cwd\":\"{}\",\"procStart\":\"{start}\"}}",
            worktree.display()
        ),
    )
    .expect("session file writes");
}

/// The transcript the live session merges with.
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

/// The live process's start as Claude's `procStart` ctime (UTC).
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
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - secs;
    // UTC ctime via `date -u -r`: the platforms both know it.
    let out = Command::new("date")
        .args(["-u", "-r", &epoch.to_string(), "+%a %b %e %H:%M:%S %Y"])
        .output()
        .expect("date runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The world: one repo, one worktree, one fake agent in its own tmux
/// session, a live session record and its transcript.
struct World {
    tmux: TmuxServer,
    home: TempDir,
    _repo: FixtureRepo,
    worktree: PathBuf,
    pid: u32,
}

fn world() -> World {
    let tmux = TmuxServer::new();
    let repo = FixtureRepo::new("origin");
    repo.branch_with_commits("feat-login", 1, false);
    let worktree = repo.add_worktree("login", Some("feat-login"));
    let home = TempDir::new("attention");
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
    let (pid, _handle) = tmux.pane_running("agents", "claude");
    session_file(&home, pid, LIVE_ID, "busy", &worktree);
    transcript(&home, "-r-login", LIVE_ID, &worktree);
    World {
        tmux,
        home,
        _repo: repo,
        worktree,
        pid,
    }
}

fn env(world: &World) -> Env<'_> {
    Env {
        home: world.home.path(),
        socket_root: world.tmux.socket_root(),
    }
}

#[test]
fn hook_writes_one_record_and_stays_silent() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    let env = env(&world);
    let payload = format!(
        "{{\"session_id\":\"{LIVE_ID}\",\"cwd\":\"{}\",\"tool_name\":\"Bash\"}}",
        world.worktree.display()
    );
    let out = run(&env, &["hook", "claude", "PreToolUse"], &payload);
    // The contract: exactly one journal record, nothing on stdout, zero.
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert!(out.stdout.is_empty(), "a hook writes nothing to stdout");
    let journal = fs::read(store_dir(&env).join("journal.log")).expect("the journal exists");
    assert!(journal.len() > 4);
    let loaded = Store::open(store_dir(&env)).load();
    assert_eq!(loaded.folds.len(), 1);
    let fold = &loaded.folds[&agent_sessions::store::conversation_key("claude", LIVE_ID)];
    assert_eq!(
        fold.last_event.as_ref().map(|e| e.kind),
        Some(agent_sessions::store::NormEvent::Activity)
    );
    // The hook resolved the process instance from Claude's own file.
    assert_eq!(
        fold.last_event.as_ref().and_then(|e| e.pid),
        Some(world.pid)
    );
}

#[test]
fn hook_failures_and_ping_events_still_exit_zero() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    let env = env(&world);
    // An unmapped event writes a diagnostic ping, not a failure.
    let out = run(&env, &["hook", "claude", "SubagentStillRunning"], "{}");
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    let loaded = Store::open(store_dir(&env)).load();
    assert!(
        loaded.folds.is_empty(),
        "a session-less ping folds to nothing"
    );
    assert!(loaded.max_seq >= 1);
    // An unwritable store dir: read-only HOME + state dir under it.
    let locked = world.home.join("locked");
    fs::create_dir_all(&locked).unwrap();
    let mut perms = fs::metadata(&locked).unwrap().permissions();
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o444);
    }
    fs::set_permissions(&locked, perms).unwrap();
    let env = Env {
        home: &locked,
        socket_root: env.socket_root,
    };
    let out = run(&env, &["hook", "claude", "Stop"], "{}");
    assert_eq!(
        out.status.code(),
        Some(0),
        "a write failure still exits zero"
    );
    assert!(out.stdout.is_empty());
    assert!(
        stderr_of(&out).contains("hook"),
        "debug detail went to stderr"
    );
    // And a non-object payload is still one record, not a crash.
    let env = Env {
        home: world.home.path(),
        socket_root: world.tmux.socket_root(),
    };
    let out = run(&env, &["hook", "claude", "Stop"], "not json");
    assert_eq!(out.status.code(), Some(0));
    let loaded = Store::open(store_dir(&env)).load();
    assert!(loaded.max_seq >= 2);
}

/// With no HOME and no XDG_STATE_HOME there is nowhere to write: the
/// record drops, the exit stays zero.
#[test]
fn a_hook_without_any_state_dir_still_exits_zero() {
    let out = Command::new(BIN)
        .args(["hook", "claude", "Stop"])
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TMUX_TMPDIR")
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    assert!(out.stdout.is_empty());
}

/// The other two providers' argv words parse and their payloads can carry
/// the process instance themselves - no provider store to resolve.
#[test]
fn vibe_and_devin_hooks_write_their_rows() {
    let home = TempDir::new("attention-vd");
    let env = Env {
        home: home.path(),
        socket_root: home.path(),
    };
    for (provider, event) in [("vibe", "post_agent"), ("devin", "SessionStart")] {
        let out = run(
            &env,
            &["hook", provider, event],
            "{\"session_id\":\"s-vd\",\"pid\":7,\"pid_start\":50}",
        );
        assert_eq!(out.status.code(), Some(0), "{provider} {event}");
    }
    let loaded = Store::open(store_dir(&env)).load();
    for (provider, want) in [
        ("vibe", agent_sessions::store::NormEvent::End),
        ("devin", agent_sessions::store::NormEvent::Start),
    ] {
        let fold = loaded
            .folds
            .get(&agent_sessions::store::conversation_key(provider, "s-vd"))
            .unwrap_or_else(|| panic!("{provider}'s record folded"));
        assert_eq!(fold.last_event.as_ref().map(|e| e.kind), Some(want));
        assert_eq!(fold.last_event.as_ref().and_then(|e| e.pid), Some(7));
    }
}

/// A store dir but no Claude root: `resolve_process` drops the provider
/// lookup and still records.
#[test]
fn a_claude_hook_without_a_store_dir_still_writes() {
    let home = TempDir::new("attention-noroot");
    // XDG_STATE_HOME set, but no HOME and no CLAUDE_CONFIG_DIR: the
    // provider's own files cannot even be located.
    let out = Command::new(BIN)
        .args(["hook", "claude", "Stop"])
        .env("XDG_STATE_HOME", home.join("state"))
        .env_remove("HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("TMUX_TMPDIR")
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let loaded = Store::open(home.join("state/agent-sessions")).load();
    assert_eq!(loaded.max_seq, 1);
}

#[test]
fn an_unknown_provider_is_a_usage_error() {
    let home = TempDir::new("attention-usage");
    let env = Env {
        home: home.path(),
        socket_root: home.path(),
    };
    let out = run(&env, &["hook", "notanagent", "Stop"], "{}");
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("unknown provider"));
    assert!(out.stdout.is_empty());
}

/// The full walk the task's verification asks for: busy -> waiting ->
/// completed-unseen, through JSON and the store, then killed-pid reaping.
#[test]
fn attention_flows_end_to_end() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    let env = env(&world);
    let hook = |event: &str| {
        let payload = format!(
            "{{\"session_id\":\"{LIVE_ID}\",\"cwd\":\"{}\"}}",
            world.worktree.display()
        );
        let out = run(&env, &["hook", "claude", event], &payload);
        assert_eq!(out.status.code(), Some(0), "{event}");
    };

    // Busy: a start plus the published `busy` - Active, `working`, `●`.
    hook("UserPromptSubmit");
    let snapshot = collect(&env, &world.tmux.socket);
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == LIVE_ID)
        .expect("the live conversation");
    assert_eq!(conv.state, ConversationState::Busy);
    assert_eq!(conv.attention, Attention::Working);
    let work = snapshot
        .work
        .iter()
        .find(|w| w.name == "feat-login")
        .expect("the work row");
    assert_eq!(work.section, Some(WorkSection::Active));
    assert_eq!(work.attention, Attention::Working);

    // Waiting: a permission prompt latches `waiting`, the row moves to
    // `Needs you`.
    hook("PermissionRequest");
    let snapshot = collect(&env, &world.tmux.socket);
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == LIVE_ID)
        .unwrap();
    assert_eq!(conv.state, ConversationState::Waiting);
    assert_eq!(conv.attention, Attention::Waiting);
    assert_eq!(conv.waiting_for.as_deref(), Some("permission prompt"));
    let work = snapshot
        .work
        .iter()
        .find(|w| w.name == "feat-login")
        .unwrap();
    assert_eq!(work.section, Some(WorkSection::NeedsYou));
    assert!(work.summary.contains("waiting"), "{}", work.summary);

    // `list --json` carries the same reading - the contract surface.
    let out = run(&env, &["list", "--json"], "");
    assert_eq!(out.status.code(), Some(0));
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the snapshot is JSON");
    let conv_json = json["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["session_id"] == LIVE_ID)
        .expect("the conversation in JSON");
    assert_eq!(conv_json["attention"], "waiting");
    assert_eq!(conv_json["state"], "waiting");
    assert_eq!(conv_json["attention_seq"], 2);
    let work_json = json["work"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "feat-login")
        .expect("the work row in JSON");
    assert_eq!(work_json["section"], "needs_you");
    assert_eq!(work_json["attention"], "waiting");

    // Clean end while still unacknowledged: `done` shows even after the
    // state goes idle - and it survives the process dying.
    hook("Stop");
    let snapshot = collect(&env, &world.tmux.socket);
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == LIVE_ID)
        .unwrap();
    assert_eq!(conv.state, ConversationState::Idle);
    // Precedence: the retained waiting latch still outranks the done.
    // Both are unacknowledged events on the same conversation.
    assert_eq!(conv.attention, Attention::CompletedUnseen);
    assert_eq!(conv.attention_seq, Some(3));

    // The pane watched - active, current, attached - acknowledges the
    // latch on the next collect, and only then. The client process is
    // held: its piped stdin staying open is what keeps it attached.
    let mut client = world.tmux.attach_client("agents");
    std::thread::sleep(std::time::Duration::from_millis(200));
    // When the dashboard itself is the watched pane it acknowledges
    // nothing - it cannot look at itself.
    let pane_id = world
        .tmux
        .tmux(&["display-message", "-t", "agents", "-p", "#{pane_id}"]);
    let own = agent_sessions::tmux::PaneRef {
        socket: world.tmux.socket.clone(),
        pane: agent_sessions::tmux::PaneId::parse(pane_id.trim()).expect("the pane id parses"),
    };
    let snapshot = collect_as(&env, &world.tmux.socket, Some(&own));
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == LIVE_ID)
        .unwrap();
    assert_eq!(
        conv.attention,
        Attention::CompletedUnseen,
        "own pane never acknowledges"
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    let snapshot = collect(&env, &world.tmux.socket);
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == LIVE_ID)
        .unwrap();
    assert_eq!(conv.attention, Attention::None, "focus acknowledged");
    assert_eq!(conv.attention_seq, None);
    // A watched conversation whose latches are already acknowledged has
    // nothing left to write - the ack pass skips it.
    let snapshot = collect(&env, &world.tmux.socket);
    assert_eq!(
        snapshot
            .conversations
            .iter()
            .find(|c| c.session_id == LIVE_ID)
            .unwrap()
            .attention,
        Attention::None
    );
    let seen = Store::open(store_dir(&env)).load().seen;
    assert_eq!(
        seen.get(&agent_sessions::store::conversation_key("claude", LIVE_ID)),
        Some(&3)
    );

    // Kill the agent: within one refresh the process claim is dead, the
    // latch is acknowledged already - nothing ghost-busy, nothing lost.
    Command::new("kill")
        .arg(world.pid.to_string())
        .status()
        .expect("kill runs");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let snapshot = collect(&env, &world.tmux.socket);
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == LIVE_ID)
        .unwrap();
    assert!(!conv.running());
    assert_eq!(conv.state, ConversationState::Unknown);
    assert_eq!(conv.attention, Attention::None);

    // And a late error after death is a latch again - durable, unseen.
    hook("StopFailure");
    let snapshot = collect(&env, &world.tmux.socket);
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == LIVE_ID)
        .unwrap();
    assert_eq!(conv.attention, Attention::Error);
    let work = snapshot
        .work
        .iter()
        .find(|w| w.name == "feat-login")
        .unwrap();
    assert_eq!(work.section, Some(WorkSection::NeedsYou));
    let _ = client.kill();
    let _ = client.wait();
}

/// `enter`/deliberate-jump seen-state is T9's; cursor movement must never
/// acknowledge. The store only moves on `space`.
#[test]
fn cursor_movement_writes_no_seen_state() {
    let dir = TempDir::new("attention-space");
    let store = Store::open(dir.path().join("agent-sessions"));
    // A snapshot with an unacknowledged conversation.
    let mut snapshot = agent_sessions::snapshot::Snapshot::empty();
    snapshot.complete = true;
    snapshot
        .conversations
        .push(agent_sessions::snapshot::ConversationRow {
            provider: agent_sessions::runtime::Provider::Claude,
            session_id: "s1".to_owned(),
            short_id: "s1".to_owned(),
            title: None,
            state: agent_sessions::snapshot::ConversationState::Waiting,
            state_raw: None,
            waiting_for: Some("permission prompt".to_owned()),
            state_since: None,
            state_since_ms: None,
            attention: Attention::Waiting,
            attention_detail: Some("permission prompt".to_owned()),
            attention_seq: Some(1),
            journal_seq: Some(1),
            last_activity: None,
            live: true,
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
        });
    let mut app = agent_sessions::tui::App::new(snapshot).with_store(store);
    for key in [
        agent_sessions::tui::Key::Char('j'),
        agent_sessions::tui::Key::Char('k'),
        agent_sessions::tui::Key::Tab,
        agent_sessions::tui::Key::Char('1'),
        agent_sessions::tui::Key::Char('3'),
    ] {
        app.key(key);
    }
    let loaded = Store::open(dir.path().join("agent-sessions")).load();
    assert!(loaded.seen.is_empty(), "navigation acknowledged nothing");
    // `space` acknowledges through the latch's sequence.
    app.key(agent_sessions::tui::Key::Char('j'));
    app.key(agent_sessions::tui::Key::Char(' '));
    let loaded = Store::open(dir.path().join("agent-sessions")).load();
    assert_eq!(
        loaded
            .seen
            .get(&agent_sessions::store::conversation_key("claude", "s1")),
        Some(&1)
    );
}

/// The checked-in parity table: the projection's summary words and glyphs
/// are what the contract says, not what the code happened to emit.
#[test]
fn the_parity_table_is_what_the_code_emits() {
    let table = include_str!("fixtures/attention-parity.md");
    let rows = support::markdown::table_after(table, "# Attention parity");
    // Drop the header and separator rows.
    let rows: Vec<Vec<String>> = rows.into_iter().skip(2).collect();
    let glyphs: std::collections::HashMap<&str, &str> = [
        ("waiting", "!"),
        ("error", "✗"),
        ("done", "✓"),
        ("working", "●"),
        ("unknown", "?"),
        ("none", ""),
    ]
    .into_iter()
    .collect();
    for row in rows {
        let (condition, _detailed, summary, glyph) =
            (&row[0], &row[1], row[2].as_str(), row[3].as_str());
        let expected = glyphs
            .get(summary)
            .copied()
            .unwrap_or_else(|| panic!("`{summary}` is not a known attention word: {row:?}"));
        let glyph = match glyph {
            "blank" => "",
            other => other,
        };
        assert_eq!(glyph, expected, "{condition}: summary `{summary}`");
    }
    // The orderings are stated in prose; the code enforces them.
    assert!(table.contains("waiting > error > done > working"));
    assert!(table.contains("error > done > waiting > working"));
    assert!(Attention::Waiting.rank() < Attention::Error.rank());
    assert!(Attention::Error.rank() < Attention::CompletedUnseen.rank());
    assert!(Attention::CompletedUnseen.rank() < Attention::Working.rank());
    assert!(Attention::Error.precedence() < Attention::CompletedUnseen.precedence());
    assert!(Attention::CompletedUnseen.precedence() < Attention::Waiting.precedence());
    assert!(Attention::Waiting.precedence() < Attention::Working.precedence());
    // And the glyph map matches the table's third column.
    assert_eq!(Attention::Waiting.glyph(), "!");
    assert_eq!(Attention::Error.glyph(), "✗");
    assert_eq!(Attention::CompletedUnseen.glyph(), "✓");
    assert_eq!(Attention::Working.glyph(), "●");
    assert_eq!(Attention::Unknown.glyph(), "?");
    assert_eq!(Attention::None.glyph(), "");
}
