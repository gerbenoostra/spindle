//! The Claude provider end to end: a fixture `~/.claude`, a real (disposable)
//! process standing in for the agent, a throwaway tmux server and a fixture
//! git repository, fused into the snapshot and printed as `list --json`.
//!
//! Nothing here touches the user's real stores, processes or servers: the
//! store is a tempdir, the agent a renamed `bash`, the tmux a `-L` server.

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;

use agent_sessions::git;
use agent_sessions::provider::SourceError;
use agent_sessions::runtime::{PaneSource, Runtime};
use agent_sessions::snapshot::{
    AttachmentLiveness, Collector, ConversationState, Landed, SCHEMA_VERSION, Snapshot, Upstream,
    WorkKind,
};
use support::fixture::{FixtureRepo, Landing};
use support::tempdir::TempDir;
use support::tmux::TmuxServer;
use support::{BIN, stderr_of, tmux_or_skip};

const LIVE_ID: &str = "8f423bbb-1111-2222-3333-444444444444";
const MERGED_ID: &str = "02aa0bbb-1111-2222-3333-444444444444";
const OLD_ID: &str = "33cc0bbb-1111-2222-3333-444444444444";

/// A transcript whose records carry no `cwd` key at all.
fn transcript_no_cwd(root: &TempDir, slug: &str, id: &str) {
    let projects = root.join(format!(".claude/projects/{slug}"));
    fs::create_dir_all(&projects).expect("mkdir");
    fs::write(
        projects.join(format!("{id}.jsonl")),
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"message\":{{\"role\":\"user\",\"content\":\"no cwd\"}}}}\n"
        ),
    )
    .expect("transcript writes");
}

/// The pane command of a `claude` process that is really `bash` - a symlink,
/// not a copy, because macOS kills a relocated copy of a signed system
/// binary, while `comm` still reports the invoked name: the basename is what
/// liveness checks. `bash` comes from `PATH`: the Linux nix sandbox has no
/// `/bin/sleep`, and its `sleep` is multicall `coreutils`, which rejects
/// the argv0 `claude`. The trailing `exit` keeps bash from exec'ing `sleep`
/// in its own place.
fn fake_agent(dir: &TempDir) -> String {
    let exe = dir.join("claude");
    std::os::unix::fs::symlink(support::on_path("bash"), &exe).expect("bash links");
    format!("exec {} -c 'sleep 300; exit'", exe.display())
}

/// `<root>/sessions/<pid>.json`.
fn session_file(root: &TempDir, pid: u32, id: &str, status: &str, extra: &str) {
    let sessions = root.join(".claude/sessions");
    fs::create_dir_all(&sessions).expect("mkdir");
    fs::write(
        sessions.join(format!("{pid}.json")),
        format!(
            "{{\"pid\":{pid},\"sessionId\":\"{id}\",\"status\":\"{status}\",\"updatedAt\":1788621019906,\"statusUpdatedAt\":1788621019906{extra}}}"
        ),
    )
    .expect("session file writes");
}

/// `procStart` as Claude writes it: a UTC ctime (`Tue Sep 22 16:18:53 2026`).
/// Read from the kernel through `ps -o lstart` so the `(pid, pid_start)`
/// pair validates as the same instance.
fn proc_start(pid: u32) -> String {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC0")
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// `<root>/projects/<slug>/<uuid>.jsonl`.
fn transcript(root: &TempDir, slug: &str, id: &str, cwd: &Path, texts: &[(&str, &str)]) {
    let projects = root.join(format!(".claude/projects/{slug}"));
    fs::create_dir_all(&projects).expect("mkdir");
    let mut lines = Vec::new();
    for (i, (role, text)) in texts.iter().enumerate() {
        lines.push(format!(
            "{{\"type\":\"{role}\",\"sessionId\":\"{id}\",\"cwd\":\"{}\",\"message\":{{\"role\":\"{role}\",\"content\":\"{text}\"}},\"timestamp\":\"2020-01-0{}T00:00:00Z\"}}",
            cwd.display(),
            i + 1,
        ));
    }
    // Records are newline-terminated, as the provider writes them - an
    // unterminated tail is a write still in flight and is not consumed.
    fs::write(
        projects.join(format!("{id}.jsonl")),
        format!("{}\n", lines.join("\n")),
    )
    .expect("transcript writes");
}

/// The store, process, server and repo of the whole scenario: one live
/// session in a tmux pane on the fixture worktree, its transcript merged on
/// the same id, one transcript-only ancient conversation, and a malformed
/// file of each kind.
struct World {
    tmux: TmuxServer,
    home: TempDir,
    _repo: FixtureRepo,
    _repo2: FixtureRepo,
    worktree: std::path::PathBuf,
    live_pid: u32,
    /// The agent pane's `%id` - what the published handle names.
    pane_id: String,
}

fn world() -> World {
    let tmux = TmuxServer::new();
    let repo = FixtureRepo::new("origin");
    repo.branch_with_commits("feat-login", 1, false);
    let worktree = repo.add_worktree("login", Some("feat-login"));
    // A branch with no worktree, so the repo has both row kinds.
    repo.branch_with_commits("feat-no-wt", 1, false);
    // The shapes every work row can take: a detached checkout, an unborn
    // HEAD, a branch whose remote ref is gone, one whose remote is unknown,
    // one merge-landed, one squash-landed, and a dirty worktree.
    let detached = repo.add_worktree("det", None);
    let unborn = repo.add_worktree("unborn", None);
    repo.git(&unborn, &["switch", "--orphan", "unborn-head"]);
    repo.branch_with_commits("feat-gone", 1, true);
    repo.git(repo.main.as_path(), &["push", "origin", ":feat-gone"]);
    repo.branch_with_commits("feat-unknown", 1, false);
    repo.git(
        repo.main.as_path(),
        &["config", "branch.feat-unknown.remote", "no-such-remote"],
    );
    repo.git(
        repo.main.as_path(),
        &[
            "config",
            "branch.feat-unknown.merge",
            "refs/heads/feat-unknown",
        ],
    );
    repo.branch_with_commits("feat-merged", 1, true);
    repo.land("feat-merged", Landing::Merge);
    repo.branch_with_commits("feat-squashed", 1, false);
    repo.land("feat-squashed", Landing::Squash);
    fs::write(worktree.join("uncommitted.txt"), "dirty").unwrap();

    // A second repo, so the rollup names more than one repository.
    let repo2 = FixtureRepo::new("origin");
    repo2.branch_with_commits("keep", 1, false);
    let keep = repo2.add_worktree("keep", Some("keep"));

    let home = TempDir::new("claude-store");
    let agent = fake_agent(&home);
    // The agent runs in its own tmux session, cwd inside the worktree.
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

    // The live record merges with its transcript of the same id. `procStart`
    // is the real process's start, so the pair validates as the instance.
    session_file(
        &home,
        pid,
        LIVE_ID,
        "waiting",
        &format!(
            ",\"waitingFor\":\"permission prompt\",\"cwd\":\"{}\",\"tmux\":\"{}\",\"name\":\"fix login\",\"procStart\":\"{}\"",
            worktree.display(),
            handle,
            proc_start(pid)
        ),
    );
    transcript(
        &home,
        "-r-login",
        LIVE_ID,
        &worktree,
        &[("user", "add the form"), ("assistant", "done")],
    );
    // A merged-id conversation that is transcript-only: live file absent.
    transcript(
        &home,
        "-r-login",
        MERGED_ID,
        &worktree,
        &[("user", "older")],
    );
    // An ancient transcript - the inventory has no age cutoff.
    transcript(&home, "-r-login", OLD_ID, &worktree, &[("user", "ancient")]);
    // A transcript with no cwd in any record: the conversation exists with
    // no placement at all.
    transcript_no_cwd(&home, "-n", "77777777-8888-9999-0000-111111111111");
    // cwds landing on the rest of resolve's arms: a detached worktree, an
    // unborn head, the second repo's checkout, and inside a bare repo
    // (RepoOnly).
    transcript(
        &home,
        "-d",
        "88888888-9999-0000-1111-222222222222",
        &detached,
        &[("user", "detached here")],
    );
    transcript(
        &home,
        "-u",
        "aaaa0000-bbbb-1111-2222-333344445555",
        &unborn,
        &[("user", "unborn head")],
    );
    transcript(
        &home,
        "-k",
        "99999999-0000-1111-2222-333333333333",
        &keep,
        &[("user", "mainless repo")],
    );
    transcript(
        &home,
        "-b",
        "00000000-1111-2222-3333-444444444444",
        &repo.remote,
        &[("user", "inside a bare repo")],
    );
    // One malformed record of each kind, isolated from the rest. The session
    // file must be pid-named to be a record at all.
    fs::write(home.join(".claude/sessions/4999.json"), "{oops").unwrap();
    fs::write(
        home.join(format!(
            ".claude/projects/-r-login/{}.jsonl",
            "44444444-5555-6666-7777-888888888888"
        )),
        "not json at all\n",
    )
    .unwrap();
    // A file the safety rules reject without parsing lands in `skipped`,
    // not in conversations or errors.
    fs::write(home.join(".claude/projects/-r-login/notes.jsonl"), "x").unwrap();

    World {
        tmux,
        home,
        _repo: repo,
        _repo2: repo2,
        worktree,
        live_pid: pid,
        pane_id: handle.rsplit('.').next().unwrap_or_default().to_owned(),
    }
}

fn collect(world: &World) -> agent_sessions::snapshot::Snapshot {
    let root = world.home.join(".claude");
    let mut collector = Collector::new(root);
    let runtime = Runtime::observe_over(std::slice::from_ref(&world.tmux.socket));
    collector.collect(&runtime, None)
}

#[test]
fn the_snapshot_holds_live_transcript_and_merged_conversations() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    let snapshot = collect(&world);

    assert_eq!(snapshot.schema_version, SCHEMA_VERSION);

    // Eight conversations: the live one, the merged transcript, the ancient
    // one, the cwd-less one, and one in each remaining cwd shape -
    // deduplicated on the session id.
    let ids: Vec<&str> = snapshot
        .conversations
        .iter()
        .map(|c| c.session_id.as_str())
        .collect();
    assert_eq!(ids.len(), 8, "deduplicated ids: {ids:?}");
    assert!(ids.contains(&LIVE_ID) && ids.contains(&MERGED_ID) && ids.contains(&OLD_ID));

    let live = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == LIVE_ID)
        .expect("the live conversation");
    assert!(live.live);
    assert_eq!(live.state, ConversationState::Waiting);
    assert_eq!(live.waiting_for.as_deref(), Some("permission prompt"));
    assert_eq!(live.title.as_deref(), Some("fix login"));
    assert_eq!(live.latest_prompt.as_deref(), Some("add the form"));
    assert_eq!(live.latest_reply.as_deref(), Some("done"));
    // Its transcript merged on the same id, and the published pane bound.
    assert!(live.transcript.is_some());
    let attachment = live.attachment.as_ref().expect("a live claim resolves");
    assert_eq!(attachment.pid, world.live_pid);
    assert_eq!(
        attachment.liveness,
        AttachmentLiveness::Instance,
        "{attachment:?}"
    );
    // The published handle bound: `socket:%id`, ending in the agent's pane.
    assert!(
        attachment
            .pane
            .as_deref()
            .is_some_and(|p| p.ends_with(&world.pane_id)),
        "{attachment:?} vs {}",
        world.pane_id
    );
    assert_eq!(attachment.pane_source, Some(PaneSource::Published));

    // Work identity came through the cwd: the worktree and its branch.
    assert_eq!(live.worktree.as_deref(), Some(world.worktree.as_path()));
    assert_eq!(live.branch.as_deref(), Some("feat-login"));

    // Transcript-only history: not live, no attachment, unknown state - and
    // no age cutoff hides the ancient record.
    let merged = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == MERGED_ID)
        .expect("the merged conversation");
    assert!(!merged.live);
    assert_eq!(merged.state, ConversationState::Unknown);
    assert!(merged.attachment.is_none());
    assert!(merged.transcript.is_some());
    let ancient = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == OLD_ID)
        .expect("no age cutoff hides it");
    assert_eq!(ancient.latest_prompt.as_deref(), Some("ancient"));

    // One malformed record of each kind was isolated into errors; the rest
    // of the store was unaffected.
    assert!(snapshot.errors.len() >= 2, "{:?}", snapshot.errors);
    assert!(snapshot.errors.iter().any(|e| e.source.contains("session")));
    assert!(
        snapshot
            .errors
            .iter()
            .any(|e| e.source.contains("transcript"))
    );

    // The repo and its work rows exist: the checkout on main, the
    // branch-only row, and the repo rolled up under [1].
    assert!(snapshot.repos.iter().any(|r| r.git));
    let names: Vec<&str> = snapshot.work.iter().map(|w| w.name.as_str()).collect();
    assert!(names.contains(&"feat-login"), "{names:?}");
    assert!(names.contains(&"feat-no-wt"), "{names:?}");
    let repo_ids: std::collections::BTreeSet<&str> =
        snapshot.repos.iter().map(|r| r.id.as_str()).collect();
    assert!(
        snapshot
            .work
            .iter()
            .all(|w| repo_ids.contains(w.repo.as_str())),
        "{:?}",
        snapshot.work.iter().map(|w| &w.repo).collect::<Vec<_>>()
    );

    // The git shapes the world sets up all land as rows: a detached
    // checkout names its sha, the remote-gone branch says so, the squashed
    // one reads as landed, and the dirty worktree is dirty.
    let row = |name: &str| {
        snapshot
            .work
            .iter()
            .find(|w| w.name == name)
            .unwrap_or_else(|| panic!("a row named {name}: {names:?}"))
    };
    assert!(row("feat-login").dirty == Some(true));
    assert_eq!(row("feat-gone").upstream, Upstream::RemoteGone);
    assert_eq!(row("feat-unknown").upstream, Upstream::Unknown);
    assert_eq!(row("feat-merged").landed, Some(Landed::Ancestor));
    assert_eq!(row("feat-squashed").landed, Some(Landed::Content));
    assert!(snapshot.work.iter().any(|w| w.kind == WorkKind::Detached));
    assert!(
        snapshot
            .work
            .iter()
            .any(|w| w.name.starts_with("detached @")),
        "{names:?}"
    );
    assert!(names.iter().any(|n| n.contains("unborn-head")), "{names:?}");

    // The conversation inside the detached worktree anchors on it with no
    // branch; the one in the mainless repo's checkout still lands.
    let det = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == "88888888-9999-0000-1111-222222222222")
        .unwrap();
    assert!(det.repo.is_some() && det.worktree.is_some());
    assert!(det.branch.is_none());

    // A collect that knows its own pane records it in the snapshot.
    let root = world.home.join(".claude");
    let mut collector = Collector::new(root);
    let runtime = Runtime::observe_over(std::slice::from_ref(&world.tmux.socket));
    let own = agent_sessions::tmux::PaneRef {
        socket: world.tmux.socket.clone(),
        pane: agent_sessions::tmux::PaneId::parse(&world.pane_id).expect("the pane id parses"),
    };
    let with_pane = collector.collect(&runtime, Some(&own));
    assert_eq!(with_pane.own_pane.as_deref(), Some(world.pane_id.as_str()));
}

#[test]
fn list_json_is_the_complete_unfiltered_snapshot() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    // A socket left by a killed server beside the live one: counted, not
    // queried.
    let dead = world.tmux.socket.with_file_name("dead");
    let started = Command::new("tmux")
        .arg("-S")
        .arg(&dead)
        .args(["-f", "/dev/null", "new-session", "-d", "sleep 300"])
        .status()
        .expect("tmux runs");
    assert!(started.success());
    Command::new("tmux")
        .arg("-S")
        .arg(&dead)
        .arg("kill-server")
        .status()
        .expect("tmux runs");
    // `kill-server` returns before the server closes its listener.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&dead).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "the killed server lingers"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = Command::new(BIN)
        .args(["list", "--json"])
        .env("HOME", world.home.path())
        .env("CLAUDE_CONFIG_DIR", world.home.join(".claude"))
        .env("TMUX_TMPDIR", world.tmux.socket_root())
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("the binary runs");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the snapshot is JSON");

    assert_eq!(json["schema_version"], SCHEMA_VERSION);
    assert_eq!(json["stale_sockets"], 1, "the dead socket is counted");
    let conversations = json["conversations"].as_array().expect("conversations");
    assert_eq!(conversations.len(), 8);
    let ids: Vec<&str> = conversations
        .iter()
        .filter_map(|c| c["session_id"].as_str())
        .collect();
    assert!(ids.contains(&LIVE_ID) && ids.contains(&MERGED_ID) && ids.contains(&OLD_ID));

    let live = conversations
        .iter()
        .find(|c| c["session_id"] == LIVE_ID)
        .expect("the live row");
    assert_eq!(live["state"], "waiting");
    assert_eq!(live["provider"], "claude");
    assert_eq!(
        live["resume_argv"],
        serde_json::json!(["claude", "--resume", LIVE_ID])
    );
    assert_eq!(live["attachment"]["pane_source"], "published");
    assert_eq!(live["attachment"]["liveness"], "instance");
    // Parsed detail the evidence view later renders is already in the
    // document: the transcript's malformed-line count and the skip list.
    assert_eq!(live["malformed_lines"], 0);
    assert!(
        json["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().is_some_and(|p| p.ends_with("notes.jsonl"))),
        "{:?}",
        json["skipped"]
    );

    // Non-live history carries no attachment and an unknown state.
    let merged = conversations
        .iter()
        .find(|c| c["session_id"] == MERGED_ID)
        .unwrap();
    assert_eq!(merged["live"], false);
    assert_eq!(merged["state"], "unknown");
    assert!(merged["attachment"].is_null());
    assert!(merged["transcript"].is_string());

    // Errors are isolated in the document, not fatal to it.
    assert!(json["errors"].as_array().unwrap().len() >= 2);
    assert!(json["repos"].as_array().unwrap().len() >= 2);
    assert!(!json["work"].as_array().unwrap().is_empty());
}

#[test]
fn a_dead_pid_cannot_raise_a_transcript() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    // A live file for a pid that is not running is dead evidence, not a live
    // conversation: the record stays visible with its liveness spelled out.
    let dead_pid = 4_000_000; // beyond the pid range
    session_file(&world.home, dead_pid, MERGED_ID, "busy", "");
    let snapshot = collect(&world);
    let merged = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == MERGED_ID)
        .unwrap();
    assert!(merged.live, "the file exists; liveness is separate");
    let attachment = merged.attachment.as_ref().expect("the claim resolved");
    assert_eq!(attachment.liveness, AttachmentLiveness::Dead);
    assert!(attachment.pane.is_none());

    // A stale file on a dead pid feeds no live rollup: the worktree row
    // counts only the genuinely running session, and the repo agrees.
    let login = snapshot
        .work
        .iter()
        .find(|w| w.name == "feat-login")
        .expect("the worktree row");
    assert_eq!(login.live_sessions, 1);
    assert_eq!(login.live_pids, 1);
    let repo = snapshot
        .repos
        .iter()
        .find(|r| r.id == login.repo)
        .expect("the repo row");
    assert_eq!(repo.live, 1);
}

#[test]
fn a_project_space_and_no_cwd_stay_honest() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    // A conversation whose cwd is a non-git directory is a project space,
    // and one with no cwd claims nothing.
    let space = world.home.join("plain-dir");
    fs::create_dir_all(&space).unwrap();
    transcript(
        &world.home,
        "-p",
        "55555555-6666-7777-8888-999999999999",
        &space,
        &[("user", "notes")],
    );
    let snapshot = collect(&world);
    let space_row = snapshot
        .repos
        .iter()
        .find(|r| !r.git)
        .expect("a non-git repo row");
    assert_eq!(space_row.name, "plain-dir");
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == "55555555-6666-7777-8888-999999999999")
        .unwrap();
    assert_eq!(conv.repo.as_deref(), Some(space_row.id.as_str()));

    // A second conversation in the same space still makes one row - the
    // space is an anchor, not a per-conversation record.
    transcript(
        &world.home,
        "-p2",
        "12121212-3434-5656-7878-909090909090",
        &space,
        &[("user", "more notes")],
    );
    let snapshot = collect(&world);
    let space_rows = snapshot
        .work
        .iter()
        .filter(|w| w.name == "plain-dir")
        .count();
    assert_eq!(
        space_rows,
        1,
        "{:?}",
        snapshot.work.iter().map(|w| &w.name).collect::<Vec<_>>()
    );
    let space_repo = snapshot
        .repos
        .iter()
        .find(|r| r.name == "plain-dir")
        .expect("the space's repo row");
    assert_eq!(space_repo.work, 1);

    // A conversation whose cwd is gone keeps its record with no anchor.
    transcript(
        &world.home,
        "-gone",
        "66666666-7777-8888-9999-000000000000",
        Path::new("/definitely/gone/path"),
        &[("user", "lost")],
    );
    let snapshot = collect(&world);
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == "66666666-7777-8888-9999-000000000000")
        .unwrap();
    assert!(conv.repo.is_none() && conv.worktree.is_none());
    assert!(
        !snapshot
            .errors
            .iter()
            .any(|e: &SourceError| { e.detail.contains("gone/path") && e.source == "git resolve" })
    );
}

/// Every publish a staged pass emits. `collect_staged` hands them through
/// the callback; the sequence - not any single snapshot - is what the
/// progressive contract is.
fn staged(world: &World) -> Vec<Snapshot> {
    let root = world.home.join(".claude");
    // One worker keeps the repo order exactly the priority order, so the
    // landing sequence the assertion reads is deterministic.
    let mut collector = Collector::new(root).with_workers(1);
    let runtime = Runtime::observe_over(std::slice::from_ref(&world.tmux.socket));
    let mut published = Vec::new();
    collector.collect_staged(&runtime, None, &mut |snapshot| {
        published.push(snapshot);
        true
    });
    published
}

#[test]
fn a_staged_pass_streams_inventory_local_git_remote_then_completes() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    let published = staged(&world);
    assert!(
        published.len() >= 3,
        "stages publish separately: {}",
        published.len()
    );

    // The first publish is the stage-1 inventory: conversations and their
    // runtime state with every Git-derived field still unknown - proof it
    // landed before any Git read could have.
    let first = &published[0];
    assert!(!first.complete);
    assert_eq!(first.conversations.len(), 8, "{:?}", first.conversations);
    assert!(first.repos.is_empty() && first.work.is_empty());
    assert!(
        first
            .conversations
            .iter()
            .all(|c| c.repo.is_none() && c.worktree.is_none() && c.branch.is_none()),
        "stage 1 cannot hold resolved placement"
    );
    // But it is already a complete renderable view: the live conversation's
    // published state is there.
    let live = first
        .conversations
        .iter()
        .find(|c| c.session_id == LIVE_ID)
        .expect("the live conversation");
    assert_eq!(live.state, ConversationState::Waiting);

    // Every publish is a full snapshot - incomplete never means partial.
    for snapshot in &published {
        assert_eq!(snapshot.schema_version, SCHEMA_VERSION);
        assert_eq!(snapshot.conversations.len(), 8);
    }

    // The repos land newest-activity first: the repo holding the live
    // (recent) conversation's checkout before the second repo's, whose only
    // conversation is an old transcript.
    let repo_a = git::Repo::discover(&world.worktree)
        .expect("discover")
        .expect("a repo")
        .common_dir()
        .display()
        .to_string();
    let repo_b = git::Repo::discover(&world._repo2.main)
        .expect("discover")
        .expect("a repo")
        .common_dir()
        .display()
        .to_string();
    let lands = |id: &str| {
        published
            .iter()
            .position(|s| s.repos.iter().any(|r| r.id == id && r.work > 0))
            .unwrap_or_else(|| panic!("{id} never lands"))
    };
    assert!(
        lands(&repo_a) < lands(&repo_b),
        "newest-activity repo lands first"
    );

    // The final publish is complete and holds the same rows the
    // single-shot collect produced - staged and unstaged classify alike.
    let last = published.last().unwrap();
    assert!(last.complete);
    assert_eq!(last.conversations.len(), 8);
    let login = last
        .work
        .iter()
        .find(|w| w.name == "feat-login")
        .expect("the worktree row");
    assert_eq!(login.live_sessions, 1);
    // Remote fields landed - no `?` placeholder survives the final publish.
    assert_eq!(login.upstream, Upstream::NeverPushed);
    let row = |name: &str| last.work.iter().find(|w| w.name == name).expect(name);
    assert_eq!(row("feat-gone").upstream, Upstream::RemoteGone);
    assert_eq!(row("feat-merged").landed, Some(Landed::Ancestor));
    assert!(
        last.work
            .iter()
            .all(|w| w.upstream_detail.as_deref() != Some("collection pending"))
    );

    // And it agrees with the direct one-pass collect field for field.
    let direct = collect(&world);
    assert_eq!(direct.complete, last.complete);
    assert_eq!(direct.repos.len(), last.repos.len());
    assert_eq!(direct.work.len(), last.work.len());
    assert_eq!(direct.conversations.len(), last.conversations.len());
    let direct_work = serde_json::to_value(&direct).unwrap()["work"].clone();
    let staged_work = serde_json::to_value(last).unwrap()["work"].clone();
    {
        let d_order: Vec<_> = direct_work
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_owned())
            .collect();
        let s_order: Vec<_> = staged_work
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(d_order, s_order, "row order");
    }
    for (d, s) in direct_work
        .as_array()
        .unwrap()
        .iter()
        .zip(staged_work.as_array().unwrap())
    {
        assert_eq!(d, s, "row {}", s["name"]);
    }
    assert!(
        serde_json::to_value(&direct).unwrap()["repos"]
            == serde_json::to_value(last).unwrap()["repos"]
    );
    assert!(
        serde_json::to_value(&direct).unwrap()["conversations"]
            == serde_json::to_value(last).unwrap()["conversations"]
    );
}

#[test]
fn a_publish_that_returns_false_stops_the_pass() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    // Refusing at each publish boundary ends the pass there: stage 1, the
    // stage-2 placements publish, mid-stage-2 (a repo merge), mid-stage-3
    // (an apply). No refusal reaches the end - the pass is complete.
    for stop_at in [1usize, 2, 4, 6] {
        let root = world.home.join(".claude");
        let mut collector = Collector::new(root).with_workers(1);
        let runtime = Runtime::observe_over(std::slice::from_ref(&world.tmux.socket));
        let mut count = 0;
        let mut last = None;
        collector.collect_staged(&runtime, None, &mut |s| {
            count += 1;
            last = Some(s);
            count < stop_at
        });
        assert_eq!(
            count, stop_at,
            "the pass stops at the {stop_at}th refused publish"
        );
        assert!(
            !last.unwrap().complete,
            "a refused pass never publishes a complete snapshot"
        );
    }
}

#[test]
fn a_second_pass_carries_remote_fields_until_they_are_replaced() {
    if !tmux_or_skip() {
        return;
    }
    let world = world();
    let root = world.home.join(".claude");
    let mut collector = Collector::new(root).with_workers(1);
    let runtime = Runtime::observe_over(std::slice::from_ref(&world.tmux.socket));

    let mut first = Vec::new();
    collector.collect_staged(&runtime, None, &mut |s| {
        first.push(s);
        true
    });
    let mut second = Vec::new();
    collector.collect_staged(&runtime, None, &mut |s| {
        second.push(s);
        true
    });

    // Remote evidence is deadline-cached across passes: the second pass
    // proves nothing twice, but carries it forward - `feat-gone` shows
    // remote_gone in the first stage-2 publish, before stage 3 ran at all.
    let early = second
        .iter()
        .find(|s| s.work.iter().any(|w| w.name == "feat-gone"))
        .expect("a publish with the work rows");
    let gone = early.work.iter().find(|w| w.name == "feat-gone").unwrap();
    assert_eq!(
        gone.upstream,
        Upstream::RemoteGone,
        "carried from the last pass, not reset to ?"
    );

    // And the second pass's final snapshot agrees with the first's.
    let last_a = serde_json::to_value(first.last().unwrap()).unwrap();
    let last_b = serde_json::to_value(second.last().unwrap()).unwrap();
    for key in ["repos", "work", "conversations"] {
        assert_eq!(last_a[key], last_b[key], "{key} diverged across passes");
    }
}
