//! The Claude provider end to end: a fixture `~/.claude`, a real (disposable)
//! process standing in for the agent, a throwaway tmux server and a fixture
//! git repository, fused into the snapshot and printed as `list --json`.
//!
//! Nothing here touches the user's real stores, processes or servers: the
//! store is a tempdir, the agent a renamed `sleep`, the tmux a `-L` server.

mod support;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use agent_sessions::provider::SourceError;
use agent_sessions::runtime::Runtime;
use agent_sessions::snapshot::{Collector, SCHEMA_VERSION};
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

/// A `claude` process that is really `sleep` - a symlink, not a copy, because
/// macOS kills a relocated copy of a signed system binary, while `comm` still
/// reports the invoked name: the basename is what liveness checks.
fn fake_agent(dir: &TempDir) -> std::path::PathBuf {
    let exe = dir.join("claude");
    std::os::unix::fs::symlink("/bin/sleep", &exe).expect("sleep links");
    exe
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
/// Computed from the live process's `etime` so the `(pid, pid_start)` pair
/// validates as the same instance.
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
    utc_ctime(epoch)
}

/// Epoch seconds -> `Thu Sep 22 16:18:53 2026` UTC, without a date library.
fn utc_ctime(epoch: u64) -> String {
    const WDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = (epoch / 86400) as i64;
    let secs = epoch % 86400;
    // civil_from_days (Hinnant): days since epoch -> year/month/day.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{} {} {} {:02}:{:02}:{:02} {}",
        WDAYS[(days % 7) as usize],
        MONTHS[(m - 1) as usize],
        d,
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        y
    )
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
    let exe = fake_agent(&home);
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
        &format!("exec {} 300", exe.display()),
    ]);
    let (pid, handle) = wait_for_pane(&tmux, "agents");

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

/// The pid and `session:@window.%pane` handle of the `agents` session's
/// pane, once the server has listed it.
fn wait_for_pane(tmux: &TmuxServer, session: &str) -> (u32, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = tmux.tmux(&[
            "list-panes",
            "-t",
            session,
            "-F",
            "#{pane_pid}|#{session_name}:#{window_id}.#{pane_id}",
        ]);
        if let Some(line) = out.lines().next() {
            let (pid, handle) = line.split_once('|').expect("the format has a |");
            if let Ok(pid) = pid.parse::<u32>() {
                return (pid, handle.to_owned());
            }
        }
        assert!(Instant::now() < deadline, "the pane never listed");
        std::thread::sleep(Duration::from_millis(50));
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
    assert_eq!(live.state, "waiting");
    assert_eq!(live.waiting_for.as_deref(), Some("permission prompt"));
    assert_eq!(live.title.as_deref(), Some("fix login"));
    assert_eq!(live.latest_prompt.as_deref(), Some("add the form"));
    assert_eq!(live.latest_reply.as_deref(), Some("done"));
    // Its transcript merged on the same id, and the published pane bound.
    assert!(live.transcript.is_some());
    let attachment = live.attachment.as_ref().expect("a live claim resolves");
    assert_eq!(attachment.pid, world.live_pid);
    assert_eq!(attachment.liveness, "instance", "{attachment:?}");
    // The published handle bound: `socket:%id`, ending in the agent's pane.
    assert!(
        attachment
            .pane
            .as_deref()
            .is_some_and(|p| p.ends_with(&world.pane_id)),
        "{attachment:?} vs {}",
        world.pane_id
    );
    assert_eq!(attachment.pane_source, Some("published"));

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
    assert_eq!(merged.state, "unknown");
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
    assert_eq!(row("feat-gone").upstream, "remote_gone");
    assert_eq!(row("feat-unknown").upstream, "unknown");
    assert_eq!(row("feat-merged").landed, Some("ancestor"));
    assert_eq!(row("feat-squashed").landed, Some("content"));
    assert!(snapshot.work.iter().any(|w| w.kind == "detached"));
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
    let own = agent_sessions::tmux::PaneId::parse(&world.pane_id);
    let with_pane = collector.collect(&runtime, own.as_ref());
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
    assert_eq!(attachment.liveness, "dead");
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
