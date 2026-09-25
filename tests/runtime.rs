//! The runtime substrate against a real but disposable tmux server:
//! pane inventory, process-instance liveness and pid-to-pane resolution.
//!
//! Every server here is a `tmux -L` under a test-owned socket dir; nothing
//! reads or touches a user's real tmux.

mod support;

use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use agent_sessions::evidence::Evidence;
use agent_sessions::git;
use agent_sessions::process::{self, Liveness, ProcessInstance, ProcessStart};
use agent_sessions::runtime::{EvidenceSource, PaneSource, Placement, ProcessClaim, Runtime};
use agent_sessions::tmux::{PaneInventory, PaneRef};
use support::tempdir::TempDir;
use support::tmux::TmuxServer;

/// A claim for `pid` with nothing else known.
fn claim(pid: u32) -> ProcessClaim {
    ProcessClaim {
        session: None,
        pid,
        pid_start: ProcessStart::Unavailable,
        expected_exe: None,
        published_pane: None,
        source: EvidenceSource::Derived,
        observed_at: SystemTime::now(),
    }
}

fn observe(server: &TmuxServer) -> Runtime {
    Runtime::observe_over(std::slice::from_ref(&server.socket))
}

/// Retry `f` until it returns `Some`, briefly: pane and process state
/// settle in well under a second but never at a fixed instant.
fn until<T>(f: impl Fn() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(t) = f() {
            return t;
        }
        assert!(
            Instant::now() < deadline,
            "condition did not arrive in 10 s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The pane in `session`, after the server has listed it.
fn pane_in<'a>(rt: &'a Runtime, session: &str) -> &'a agent_sessions::tmux::Pane {
    rt.panes
        .panes
        .iter()
        .find(|pane| pane.session_name == session)
        .unwrap_or_else(|| {
            panic!(
                "a pane in session {session}: panes {:?}, servers {:?}, warnings {:?}",
                rt.panes
                    .panes
                    .iter()
                    .map(|p| p.session_name.clone())
                    .collect::<Vec<_>>(),
                rt.panes.servers,
                rt.panes.warnings
            )
        })
}

/// The pane's `PaneRef` for assertions.
fn pane_ref(pane: &agent_sessions::tmux::Pane) -> PaneRef {
    PaneRef {
        socket: pane.socket.clone(),
        pane: pane.id.clone(),
    }
}

#[test]
fn a_shell_only_pane_resolves_by_ancestry() {
    if !support::tmux_or_skip() {
        return;
    }
    let server = TmuxServer::new();
    server.new_session("t", "bash");
    let rt = observe(&server);
    let pane = pane_in(&rt, "t");

    // The pane's own shell resolves to its own pane with no hops at all:
    // the ancestry walk starts at the pid itself.
    let binding = rt.resolve_pane(pane.pid, None);
    let Evidence::Known(binding) = binding else {
        panic!("the pane's shell resolves: {binding:?}")
    };
    assert_eq!(binding.pane, pane_ref(pane));
    assert_eq!(binding.source, PaneSource::Ancestry);
}

#[test]
fn a_two_hop_child_resolves_to_its_pane() {
    if !support::tmux_or_skip() {
        return;
    }
    let server = TmuxServer::new();
    server.new_session("t", "sh -c 'sh -c \"sleep 300 & wait\" & wait'");
    let rt = observe(&server);
    let pane = pane_in(&rt, "t");
    let pane_pid = pane.pid;

    // The grandchild: a `sleep` whose ancestry climbs two hops to
    // `pane_pid` - the shape an agent inside a shell takes.
    let (rt, sleep) = until(|| {
        let rt = observe(&server);
        let found = rt.processes.as_ref()?.rows().find_map(|row| {
            let chain = rt.processes.as_ref().unwrap().ancestors(row.pid);
            (row.exe.as_deref() == Some("sleep") && chain[1..].contains(&pane_pid))
                .then_some(row.pid)
        });
        found.map(|pid| (rt, pid))
    });
    let table = rt.processes.as_ref().unwrap();
    let chain = table.ancestors(sleep);
    assert_eq!(
        chain.iter().position(|p| *p == pane.pid),
        Some(2),
        "{chain:?}"
    );

    let binding = rt.resolve_pane(sleep, None);
    let Evidence::Known(binding) = binding else {
        panic!("the grandchild resolves: {binding:?}")
    };
    assert_eq!(binding.pane, pane_ref(pane));
    assert_eq!(binding.source, PaneSource::Ancestry);
}

#[test]
fn panes_and_windows_resolve_independently() {
    if !support::tmux_or_skip() {
        return;
    }
    let server = TmuxServer::new();
    server.new_session("t", "sleep 300");
    server.tmux(&["split-window", "-d", "-t", "t", "sleep 300"]);
    server.tmux(&["new-window", "-d", "-a", "-t", "t", "sleep 300"]);

    let rt = until(|| {
        let rt = observe(&server);
        (rt.panes
            .panes
            .iter()
            .filter(|p| p.session_name == "t")
            .count()
            == 3)
            .then_some(rt)
    });
    let panes: Vec<_> = rt
        .panes
        .panes
        .iter()
        .filter(|p| p.session_name == "t")
        .collect();
    assert_eq!(panes.len(), 3);

    // Each pane's root pid resolves to exactly its own pane, and exactly
    // one window differs from the other two.
    let mut windows = Vec::new();
    for pane in &panes {
        let Evidence::Known(binding) = rt.resolve_pane(pane.pid, None) else {
            panic!("pane {} resolves", pane.id)
        };
        assert_eq!(binding.pane, pane_ref(pane));
        windows.push(pane.window.clone());
    }
    windows.sort();
    windows.dedup();
    assert_eq!(windows.len(), 2, "three panes across two windows");
}

#[test]
fn two_servers_merge_into_one_inventory() {
    if !support::tmux_or_skip() {
        return;
    }
    let base = support::tmux::base_dir();
    let one = TmuxServer::in_dir(&base);
    let two = TmuxServer::in_dir(&base);
    one.new_session("s1", "sleep 300");
    two.new_session("s2", "sleep 300");

    // Discovery sees both sockets through the shared base directory.
    let rt = Runtime::observe_in(Some(base.path().as_os_str()), None);
    assert_eq!(rt.panes.servers.len(), 2, "two answering servers");
    let a = pane_in(&rt, "s1");
    let b = pane_in(&rt, "s2");
    assert_eq!(a.socket, one.socket);
    assert_eq!(b.socket, two.socket);

    // Both servers name a `%0`-ish pane; a bare published pane id is
    // therefore ambiguous and ancestry wins the fallthrough.
    let Evidence::Known(binding) = rt.resolve_pane(a.pid, Some(a.id.as_str())) else {
        panic!("s1's pane resolves")
    };
    assert_eq!(binding.pane, pane_ref(a));

    // Fully qualified, the same pane resolves through the published
    // handle even while its pane id is not unique.
    let handle = format!("{}:{}.{}", a.session_name, a.window, a.id);
    let Evidence::Known(binding) = rt.resolve_pane(a.pid, Some(&handle)) else {
        panic!("the qualified handle resolves")
    };
    assert_eq!(binding.pane, pane_ref(a));
    assert_eq!(binding.source, PaneSource::Published);
}

#[test]
fn detached_and_attached_sessions_report_attachment() {
    if !support::tmux_or_skip() {
        return;
    }
    let server = TmuxServer::new();
    server.new_session("t", "sleep 300");
    let rt = observe(&server);
    let pane = pane_in(&rt, "t");
    assert_eq!(pane.session_attached, 0);
    assert!(
        rt.panes
            .in_detached_sessions()
            .iter()
            .any(|p| p.id == pane.id)
    );

    // A control-mode client attaches without a terminal.
    let mut client = server.attach_client("t");
    let rt = until(|| {
        let rt = observe(&server);
        (pane_in(&rt, "t").session_attached == 1).then_some(rt)
    });
    assert!(
        rt.panes
            .in_detached_sessions()
            .iter()
            .all(|p| p.id != pane.id)
    );
    let _ = client.kill();
    let _ = client.wait();
    until(|| {
        let rt = observe(&server);
        (pane_in(&rt, "t").session_attached == 0).then_some(())
    });
}

#[test]
fn a_dead_pane_is_dead_within_one_refresh() {
    if !support::tmux_or_skip() {
        return;
    }
    let server = TmuxServer::new();
    server.new_session("t", "sleep 300");
    server.new_session("keeper", "sleep 300");
    let rt = observe(&server);
    let pane = pane_in(&rt, "t");
    let pid = pane.pid;
    let start = rt
        .processes
        .as_ref()
        .and_then(|t| t.get(pid).map(|r| r.start))
        .unwrap();

    server.tmux(&["kill-pane", "-t", pane.id.as_str()]);

    // One refresh is enough: a fresh observation carries no cache, so the
    // pane is absent and the pid reports dead immediately.
    let rt = until(|| {
        let rt = observe(&server);
        rt.panes.panes.iter().all(|p| p.id != pane.id).then_some(rt)
    });
    let table = rt.processes.as_ref().unwrap();
    let instance = ProcessInstance {
        pid,
        pid_start: start,
    };
    assert!(matches!(table.is_live(&instance, None), Liveness::Dead(_)));
    let resolved = rt.resolve_attachments(&[claim(pid)]);
    assert!(matches!(
        resolved[0].placement,
        Placement::Dead(_) | Placement::Unknown(_)
    ));
}

#[test]
fn claims_are_validated_by_start_time_and_executable() {
    if !support::tmux_or_skip() {
        return;
    }
    let server = TmuxServer::new();
    server.new_session("t", "sleep 300");
    let rt = observe(&server);
    let table = rt.processes.as_ref().unwrap();
    let pid = pane_in(&rt, "t").pid;
    let ProcessStart::At(start) = table.get(pid).unwrap().start else {
        panic!("the pane's process has a start time")
    };

    // The true instance validates.
    assert_eq!(
        table.is_live(
            &ProcessInstance {
                pid,
                pid_start: ProcessStart::At(start)
            },
            Some("sleep")
        ),
        Liveness::Instance
    );
    // A pid reused by a process started at another time rejects.
    assert!(matches!(
        table.is_live(
            &ProcessInstance {
                pid,
                pid_start: ProcessStart::At(start + 600)
            },
            None
        ),
        Liveness::Dead(_)
    ));
    // A reused executable rejects; a claim that knows the basename asks
    // for `sleep`, not whatever took the pid.
    assert!(matches!(
        table.is_live(
            &ProcessInstance {
                pid,
                pid_start: ProcessStart::At(start)
            },
            Some("vim")
        ),
        Liveness::Dead(_)
    ));
    // No start time on the claim is pid-only evidence.
    assert!(matches!(
        table.is_live(
            &ProcessInstance {
                pid,
                pid_start: ProcessStart::Unavailable
            },
            None
        ),
        Liveness::PidOnly(_)
    ));
}

#[test]
fn published_handles_bind_contradict_and_fall_through() {
    if !support::tmux_or_skip() {
        return;
    }
    let server = TmuxServer::new();
    server.new_session("t", "sleep 300");
    server.tmux(&["split-window", "-d", "-t", "t", "sleep 300"]);
    let rt = until(|| {
        let rt = observe(&server);
        (rt.panes
            .panes
            .iter()
            .filter(|p| p.session_name == "t")
            .count()
            == 2)
            .then_some(rt)
    });
    let mut panes: Vec<_> = rt
        .panes
        .panes
        .iter()
        .filter(|p| p.session_name == "t")
        .collect();
    panes.sort_by(|a, b| a.id.cmp(&b.id));
    let (a, b) = (panes[0], panes[1]);

    // A published handle naming a's pane binds through `Published`.
    let handle = format!("{}:{}.{}", a.session_name, a.window, a.id);
    let Evidence::Known(binding) = rt.resolve_pane(a.pid, Some(&handle)) else {
        panic!("the published handle binds")
    };
    assert_eq!(binding.pane, pane_ref(a));
    assert_eq!(binding.source, PaneSource::Published);

    // A published handle naming the *other* live pane contradicts the
    // ancestry evidence: the binding is Unknown, not a pick.
    let wrong = format!("{}:{}.{}", b.session_name, b.window, b.id);
    let binding = rt.resolve_pane(a.pid, Some(&wrong));
    assert!(
        matches!(binding, Evidence::Unknown(ref r) if r.contains("contradict") || r.contains("published")),
        "{binding:?}"
    );

    // A handle naming no live pane is stale and falls through to
    // ancestry, which still resolves exactly.
    let Evidence::Known(binding) = rt.resolve_pane(a.pid, Some("ghosts:@99.%999")) else {
        panic!("a stale handle falls through to ancestry")
    };
    assert_eq!(binding.pane, pane_ref(a));
    assert_eq!(binding.source, PaneSource::Ancestry);

    // The same for a handle whose session narrows it to nothing.
    let foreign = format!("nosuch:{}.{}", a.window, a.id);
    let Evidence::Known(binding) = rt.resolve_pane(a.pid, Some(&foreign)) else {
        panic!("a session-mismatched handle falls through")
    };
    assert_eq!(binding.pane, pane_ref(a));

    // A handle without a pane component cannot pinpoint a pane.
    let Evidence::Known(binding) = rt.resolve_pane(a.pid, Some(a.window.as_str())) else {
        panic!("a window-only handle falls through")
    };
    assert_eq!(binding.pane, pane_ref(a));
}

#[test]
fn an_orphan_keeps_its_tty_and_resolves() {
    if !support::tmux_or_skip() {
        return;
    }
    let server = TmuxServer::new();
    // `(sleep &) ; sleep`: the first sleep is orphaned when its subshell
    // exits; it keeps the pane's controlling tty but loses its ancestry.
    server.new_session("t", "sh -c '(sleep 300 &) ; sleep 300'");
    let rt = observe(&server);
    let pane_pid = pane_in(&rt, "t").pid;
    let pane_tty = pane_in(&rt, "t").tty.clone();

    let (rt, orphan, anchored) = until(|| {
        let rt = observe(&server);
        let found = (|| {
            let table = rt.processes.as_ref()?;
            let sleeps: Vec<u32> = table
                .rows()
                .filter(|row| row.exe.as_deref() == Some("sleep"))
                .map(|row| row.pid)
                .collect();
            // The sleep still in the pane's tree (possibly `pane_pid`
            // itself), and the one reparented away but still holding the
            // pane's controlling tty.
            let anchored = sleeps
                .iter()
                .find(|pid| table.ancestors(**pid).contains(&pane_pid));
            let orphan = sleeps.iter().find(|pid| {
                !table.ancestors(**pid).contains(&pane_pid)
                    && table
                        .get(**pid)
                        .is_some_and(|row| row.tty == pane_tty && row.tty.is_some())
            });
            anchored.zip(orphan).map(|(a, o)| (*o, *a))
        })();
        found.map(|(o, a)| (rt, o, a))
    });
    let pane = pane_in(&rt, "t");

    let Evidence::Known(binding) = rt.resolve_pane(orphan, None) else {
        panic!("the orphan resolves")
    };
    assert_eq!(binding.pane, pane_ref(pane));
    assert_eq!(binding.source, PaneSource::Tty);

    let Evidence::Known(binding) = rt.resolve_pane(anchored, None) else {
        panic!("the anchored child resolves")
    };
    assert_eq!(binding.pane, pane_ref(pane));
    assert_eq!(binding.source, PaneSource::Ancestry);
}

#[test]
fn utc_and_platform_starts_agree_within_a_second() {
    if !support::tmux_or_skip() {
        return;
    }
    // A process spawned now: its platform-derived start agrees with the
    // wall clock, and the same epoch round-trips through the UTC ctime
    // form providers publish.
    let child = Command::new("sleep")
        .arg("300")
        .spawn()
        .expect("sleep spawns");
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut child = child;
    let table = process::ProcessTable::snapshot().expect("ps runs");
    let ProcessStart::At(start) = table
        .get(child.id())
        .expect("the spawned process is in the table")
        .start
    else {
        panic!("the spawned process has a start time")
    };
    assert!(start.abs_diff(now) <= 2, "start {start} vs now {now}");

    let ctime = epoch_to_utc_ctime(start);
    assert_eq!(
        process::parse_utc_ctime(&ctime),
        Some(start),
        "{ctime} round-trips"
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// `epoch` rendered as the UTC ctime shape providers publish
/// (`Tue Sep 22 16:18:53 2026`) - the exact inverse of the parser under
/// test, so the round trip stands alone on any `date` flavor.
fn epoch_to_utc_ctime(epoch: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let secs = epoch % 86_400;
    let z = (epoch / 86_400) as i64 + 719_468;
    // civil_from_days: the inverse of the parser's days_from_civil.
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{} {} {} {:02}:{:02}:{:02} {}",
        WEEKDAYS[(z - 719_468 + 4).rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        day,
        secs / 3_600,
        (secs / 60) % 60,
        secs % 60,
        year
    )
}

#[test]
fn pane_ownership_is_reconciled_per_pane() {
    if !support::tmux_or_skip() {
        return;
    }
    let server = TmuxServer::new();
    // Two live processes in one pane's tree: a pane hosts at most one
    // current attachment.
    server.new_session("t", "sh -c 'sleep 300 & sleep 301 & wait'");
    let rt = observe(&server);
    let pane_pid = pane_in(&rt, "t").pid;
    let (rt, pids) = until(|| {
        let rt = observe(&server);
        let found = (|| {
            let table = rt.processes.as_ref()?;
            let mut pids: Vec<u32> = table
                .rows()
                .filter(|row| {
                    row.exe.as_deref() == Some("sleep")
                        && table.ancestors(row.pid)[1..].contains(&pane_pid)
                })
                .map(|row| row.pid)
                .collect();
            pids.sort_unstable();
            (pids.len() == 2).then_some(pids)
        })();
        found.map(|pids| (rt, pids))
    });
    let pane = pane_in(&rt, "t");

    // Same observation instant: nothing orders the claims, so both are
    // contradictory Unknown rather than a guess.
    let at = SystemTime::now();
    let claims: Vec<ProcessClaim> = pids
        .iter()
        .map(|pid| ProcessClaim {
            observed_at: at,
            ..claim(*pid)
        })
        .collect();
    let resolved = rt.resolve_attachments(&claims);
    for r in &resolved {
        assert!(
            matches!(r.placement, Placement::Unknown(_)),
            "{:?}",
            r.placement
        );
        assert_eq!(r.attachment.pane, None);
    }

    // Order them and the newer claim owns the pane; the older binding is
    // superseded, its pane retired.
    let claims: Vec<ProcessClaim> = pids
        .iter()
        .enumerate()
        .map(|(i, pid)| ProcessClaim {
            observed_at: at + Duration::from_secs(i as u64),
            ..claim(*pid)
        })
        .collect();
    let resolved = rt.resolve_attachments(&claims);
    assert!(matches!(resolved[0].placement, Placement::Superseded));
    assert_eq!(resolved[0].attachment.pane, None);
    assert!(matches!(
        resolved[1].placement,
        Placement::Bound(PaneSource::Ancestry)
    ));
    assert_eq!(resolved[1].attachment.pane, Some(pane_ref(pane)));
}

#[test]
fn a_worktree_binds_windows_by_stored_then_derived_evidence() {
    if !support::tmux_or_skip() {
        return;
    }
    let worktree = TempDir::new("worktree");
    let server = TmuxServer::new();
    server.tmux(&[
        "new-session",
        "-d",
        "-s",
        "t",
        "-c",
        worktree.path().to_str().unwrap(),
        "-x",
        "100",
        "-y",
        "24",
        "sleep 300",
    ]);
    let rt = observe(&server);
    let pane = pane_in(&rt, "t");
    assert_eq!(pane.cwd.as_deref(), Some(worktree.path()));

    // No stored edge: the pane's cwd inside the worktree binds the window.
    assert_eq!(rt.panes.windows_bound(None, worktree.path()), 1);
    assert_eq!(
        rt.panes.windows_bound(Some("other-admin"), worktree.path()),
        1,
        "no stored edge means the derived edge still binds"
    );

    // A stored `@wt_adminid` decides outright: matching binds, and a
    // stored id naming another worktree unbinds even with cwd inside.
    server.tmux(&[
        "set-window-option",
        "-t",
        pane.window.as_str(),
        "@wt_adminid",
        "admin-1",
    ]);
    server.tmux(&[
        "set-window-option",
        "-t",
        pane.window.as_str(),
        "@wt_handle",
        "feature-x",
    ]);
    let rt = observe(&server);
    let pane = pane_in(&rt, "t");
    assert_eq!(pane.wt_adminid.as_deref(), Some("admin-1"));
    assert_eq!(pane.wt_handle.as_deref(), Some("feature-x"));
    assert_eq!(rt.panes.windows_bound(Some("admin-1"), worktree.path()), 1);
    assert_eq!(
        rt.panes.windows_bound(Some("other-admin"), worktree.path()),
        0,
        "the stored edge owns the binding when present"
    );
    assert_eq!(rt.panes.windows_bound(None, worktree.path()), 0);
}

#[test]
fn no_server_is_an_empty_inventory_and_unknown_panes() {
    if !support::tmux_or_skip() {
        return;
    }
    // A socket that answers nothing: recorded, contributes nothing.
    let dir = TempDir::new("tmux-empty");
    let bogus = dir.join("dead-socket");
    std::os::unix::net::UnixListener::bind(&bogus).unwrap();
    let inventory = PaneInventory::collect(std::slice::from_ref(&bogus));
    assert!(inventory.panes.is_empty());
    assert_eq!(inventory.servers.len(), 1);
    assert!(inventory.servers[0].error.is_some());

    // No sockets at all: still a valid empty inventory, and git evidence
    // is untouched by the missing tmux.
    let rt = Runtime::observe_over(&[]);
    assert!(rt.panes.panes.is_empty());
    let binding = rt.resolve_pane(std::process::id(), None);
    assert!(matches!(binding, Evidence::Unknown(_)));
    let repo = TempDir::new("repo");
    assert_eq!(
        git::resolve(repo.path()).unwrap(),
        git::Resolved::ProjectSpace(repo.path().canonicalize().unwrap())
    );

    // A duplicate socket in the list is asked once, not twice.
    let server = TmuxServer::new();
    server.new_session("t", "sleep 300");
    let inventory = PaneInventory::collect(&[server.socket.clone(), server.socket.clone()]);
    assert_eq!(inventory.servers.len(), 1);
}

#[test]
fn a_recorded_warning_keeps_the_rest_of_the_inventory() {
    if !support::tmux_or_skip() {
        return;
    }
    // A cwd containing the record separator inflates the field count:
    // the pane is dropped as a warning, not silently guessed.
    let server = TmuxServer::new();
    let weird = TempDir::new("wt|weird");
    server.tmux(&[
        "new-session",
        "-d",
        "-s",
        "weird",
        "-c",
        weird.path().to_str().unwrap(),
        "-x",
        "100",
        "-y",
        "24",
        "sleep 300",
    ]);
    let rt = observe(&server);
    assert!(
        rt.panes.warnings.iter().any(|w| w.contains("weird")),
        "{:?}",
        rt.panes.warnings
    );
    // The well-formed panes still parsed.
    assert!(rt.panes.panes.iter().any(|p| p.session_name == "holder"));
    assert!(rt.panes.servers.iter().all(|s| s.error.is_none()));
}
