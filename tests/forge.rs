//! The forge matrix: every work-item and pipeline state, plus every failure
//! mode, through stub `gh`/`glab` executables on a controlled search path -
//! so no real CLI and no network is ever involved.

mod support;

use std::time::{Duration, Instant};

use agent_sessions::forge::{Forge, ForgeCache, ForgeStatus, Pipeline, WorkItem};
use support::tempdir::TempDir;

/// A stub CLI: logs its argv (one line per call) to `<log>` and replies with
/// `body` and `code`. The log path is baked into the script so the child
/// needs no test-specific environment.
fn stub(dir: &TempDir, name: &str, body: &str, code: u8) {
    let log = dir.join(format!("{name}.log"));
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s' '{}'\nexit {code}\n",
        log.display(),
        body.replace('\'', "'\\''")
    );
    let path = dir.join(name);
    std::fs::write(&path, script).expect("stub");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// A stub that answers `body` only when the query asks for items of every
/// state (`--state all` for `gh`, `--all` for `glab`) and an empty list to
/// the open-only query - which is what exercises the second, existence,
/// step of the two-query design.
fn stub_all(dir: &TempDir, name: &str, flag: &str, body: &str) {
    let log = dir.join(format!("{name}.log"));
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$*\" in\n  *\"{flag}\"*) printf '%s' '{body}' ;;\n  *) printf '[]' ;;\nesac\nexit 0\n",
        log = log.display(),
        flag = flag,
        body = body.replace('\'', "'\\''"),
    );
    let path = dir.join(name);
    std::fs::write(&path, script).expect("stub");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

fn forge_at(dir: &TempDir) -> Forge {
    Forge::with_path(dir.path().as_os_str().to_owned())
}

/// What the stub recorded: one line per invocation.
fn calls(dir: &TempDir, name: &str) -> Vec<String> {
    std::fs::read_to_string(dir.join(format!("{name}.log")))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn gh_pr(number: u64, state: &str, rollup: &str) -> String {
    format!(
        r#"[{{"number":{number},"state":"{state}","url":"https://github.com/o/r/pull/{number}","statusCheckRollup":{rollup}}}]"#
    )
}

fn glab_mr(iid: u64, state: &str, pipeline: &str) -> String {
    format!(
        r#"[{{"iid":{iid},"state":"{state}","web_url":"https://gitlab.com/o/r/-/merge_requests/{iid}","head_pipeline":{pipeline}}}]"#
    )
}

#[test]
fn github_hosts_route_to_gh_and_map_every_state() {
    let dir = TempDir::new("forge-gh");
    stub(
        &dir,
        "gh",
        &gh_pr(
            191,
            "OPEN",
            r#"[{"status":"COMPLETED","conclusion":"SUCCESS"}]"#,
        ),
        0,
    );
    let forge = forge_at(&dir);

    let status = forge.status("https://github.com/o/r.git", "feat");
    assert_eq!(status.item, WorkItem::Open);
    assert_eq!(status.pipeline, Pipeline::Succeeded);
    assert_eq!(status.label.as_deref(), Some("PR #191"));
    // Routing: the repo argument carries host/owner/repo for `gh`, and the
    // open items are asked for first.
    let argv = calls(&dir, "gh").join(" ").replace('\n', " ");
    assert!(argv.contains("pr list"), "{argv}");
    assert!(argv.contains("--repo github.com/o/r"), "{argv}");
    assert!(argv.contains("--head feat"), "{argv}");
    assert!(argv.contains("--state open"), "{argv}");

    // The scp-style remote spelling routes identically.
    let status = forge.status("git@github.com:o/r.git", "feat");
    assert_eq!(status.item, WorkItem::Open);
    assert!(
        calls(&dir, "gh")
            .join(" ")
            .contains("--repo github.com/o/r")
    );
}

#[test]
fn github_pipeline_states() {
    let cases = [
        (r#"[{"status":"IN_PROGRESS"}]"#, Pipeline::Busy),
        (r#"[{"state":"PENDING"}]"#, Pipeline::Busy),
        (
            r#"[{"status":"COMPLETED","conclusion":"FAILURE"}]"#,
            Pipeline::Failed,
        ),
        (r#"[{"state":"ERROR"}]"#, Pipeline::Failed),
        (
            r#"[{"status":"COMPLETED","conclusion":"SUCCESS"},{"status":"COMPLETED","conclusion":"SKIPPED"}]"#,
            Pipeline::Succeeded,
        ),
        // A shape neither pending, failed nor succeeded is unknown.
        (r#"[{"unexpected":"shape"}]"#, Pipeline::Unknown),
        (r#"[]"#, Pipeline::Unknown),
        (r#"null"#, Pipeline::Unknown),
    ];
    for (rollup, expected) in cases {
        let dir = TempDir::new("forge-gh-pipe");
        stub(&dir, "gh", &gh_pr(1, "OPEN", rollup), 0);
        let status = forge_at(&dir).status("https://github.com/o/r", "b");
        assert_eq!(status.pipeline, expected, "rollup {rollup}");
    }
    // Failed wins over still-running, and busy wins over nothing.
    let dir = TempDir::new("forge-gh-mixed");
    stub(
        &dir,
        "gh",
        &gh_pr(
            1,
            "OPEN",
            r#"[{"status":"IN_PROGRESS"},{"status":"COMPLETED","conclusion":"FAILURE"}]"#,
        ),
        0,
    );
    assert_eq!(
        forge_at(&dir)
            .status("https://github.com/o/r", "b")
            .pipeline,
        Pipeline::Failed
    );
}

#[test]
fn github_item_states() {
    let cases = [
        ("OPEN", WorkItem::Open),
        ("MERGED", WorkItem::Closed),
        ("CLOSED", WorkItem::Closed),
        ("SOMETHING_NEW", WorkItem::Unknown),
    ];
    for (state, expected) in cases {
        let dir = TempDir::new("forge-gh-item");
        stub(&dir, "gh", &gh_pr(1, state, "[]"), 0);
        let status = forge_at(&dir).status("https://github.com/o/r", "b");
        assert_eq!(status.item, expected, "state {state}");
    }
    // Only closed items exist.
    let dir = TempDir::new("forge-gh-closed");
    stub(
        &dir,
        "gh",
        &format!(
            "[{},{}]",
            gh_pr(2, "CLOSED", "[]").trim_matches(['[', ']']),
            gh_pr(1, "MERGED", "[]").trim_matches(['[', ']'])
        ),
        0,
    );
    assert_eq!(
        forge_at(&dir).status("https://github.com/o/r", "b").item,
        WorkItem::Closed
    );
    // None at all.
    let dir = TempDir::new("forge-gh-none");
    stub(&dir, "gh", "[]", 0);
    assert_eq!(
        forge_at(&dir).status("https://github.com/o/r", "b").item,
        WorkItem::NotExisting
    );
}

#[test]
fn gitlab_hosts_route_to_glab_and_map_every_state() {
    let cases = [
        ("opened", WorkItem::Open),
        ("merged", WorkItem::Closed),
        ("closed", WorkItem::Closed),
        ("locked", WorkItem::Closed),
        ("surprising", WorkItem::Unknown),
    ];
    for (state, expected) in cases {
        let dir = TempDir::new("forge-glab-item");
        stub(&dir, "glab", &glab_mr(7, state, "null"), 0);
        let status = forge_at(&dir).status("git@gitlab.com:o/r.git", "feat");
        assert_eq!(status.item, expected, "state {state}");
        if expected == WorkItem::Open {
            assert_eq!(status.label.as_deref(), Some("MR !7"));
        }
        let argv = calls(&dir, "glab").join(" ").replace('\n', " ");
        assert!(argv.contains("mr list"), "{argv}");
        assert!(argv.contains("--repo gitlab.com/o/r"), "{argv}");
        assert!(argv.contains("--source-branch feat"), "{argv}");
    }
    // Pipeline states from head_pipeline.status.
    let cases = [
        (r#"{"status":"running"}"#, Pipeline::Busy),
        (r#"{"status":"pending"}"#, Pipeline::Busy),
        (r#"{"status":"success"}"#, Pipeline::Succeeded),
        (r#"{"status":"failed"}"#, Pipeline::Failed),
        (r#"{"status":"canceled"}"#, Pipeline::Unknown),
        ("null", Pipeline::Unknown),
    ];
    for (pipeline, expected) in cases {
        let dir = TempDir::new("forge-glab-pipe");
        stub(&dir, "glab", &glab_mr(7, "opened", pipeline), 0);
        let status = forge_at(&dir).status("https://gitlab.com/o/r", "b");
        assert_eq!(status.pipeline, expected, "pipeline {pipeline}");
    }
    let dir = TempDir::new("forge-glab-none");
    stub(&dir, "glab", "[]", 0);
    assert_eq!(
        forge_at(&dir).status("https://gitlab.com/o/r", "b").item,
        WorkItem::NotExisting
    );
}

#[test]
fn every_failure_mode_is_unknown_and_nonfatal() {
    // No CLI for the host at all.
    let dir = TempDir::new("forge-misc");
    let status = forge_at(&dir).status("https://git.example.com/o/r", "b");
    assert_eq!(status.item, WorkItem::Unknown);
    assert!(status.reason.unwrap().contains("no forge CLI"));

    // A local path is not a forge remote.
    let status = forge_at(&dir).status("/srv/repos/r.git", "b");
    assert_eq!(status.item, WorkItem::Unknown);
    assert!(status.reason.unwrap().contains("not a forge remote"));

    // The host's CLI is not installed.
    let status = forge_at(&dir).status("https://github.com/o/r", "b");
    assert_eq!(status.item, WorkItem::Unknown);
    assert!(status.reason.unwrap().contains("not on PATH"));

    // Authentication failure and command failure are both `unknown`, on
    // either forge CLI.
    for (code, stderr) in [(4u8, "gh auth login"), (1u8, "boom")] {
        let dir = TempDir::new("forge-fail");
        stub(&dir, "gh", stderr, code);
        let status = forge_at(&dir).status("https://github.com/o/r", "b");
        assert_eq!(status.item, WorkItem::Unknown, "exit {code}");
        assert!(status.reason.unwrap().contains("exited"));

        let dir = TempDir::new("forge-fail-glab");
        stub(&dir, "glab", stderr, code);
        let status = forge_at(&dir).status("https://gitlab.com/o/r", "b");
        assert_eq!(status.item, WorkItem::Unknown, "glab exit {code}");
    }

    // Unparseable success output is still `unknown`.
    let dir = TempDir::new("forge-garbage");
    stub(&dir, "gh", "not json", 0);
    let status = forge_at(&dir).status("https://github.com/o/r", "b");
    assert_eq!(status.item, WorkItem::Unknown);
    assert!(status.reason.unwrap().contains("unparseable"));

    let dir = TempDir::new("forge-notlist");
    stub(&dir, "gh", "{}", 0);
    let status = forge_at(&dir).status("https://github.com/o/r", "b");
    assert_eq!(status.item, WorkItem::Unknown);

    let dir = TempDir::new("forge-glab-garbage");
    stub(&dir, "glab", "not json", 0);
    let status = forge_at(&dir).status("https://gitlab.com/o/r", "b");
    assert_eq!(status.item, WorkItem::Unknown);
    assert!(status.reason.unwrap().contains("unparseable"));

    let dir = TempDir::new("forge-glab-notlist");
    stub(&dir, "glab", "{}", 0);
    let status = forge_at(&dir).status("https://gitlab.com/o/r", "b");
    assert_eq!(status.item, WorkItem::Unknown);
}

#[test]
fn an_empty_open_page_sends_a_second_existence_query() {
    // gh: the open page answers empty, the all-state page holds the item.
    let dir = TempDir::new("forge-gh-exists");
    stub_all(&dir, "gh", "--state all", &gh_pr(2, "CLOSED", "[]"));
    let status = forge_at(&dir).status("https://github.com/o/r", "b");
    assert_eq!(status.item, WorkItem::Closed);
    assert_eq!(calls(&dir, "gh").len(), 2, "open query, then all states");

    // An open item on the all-state page still wins - the client-side
    // filter keeps a misbehaving filter from hiding it.
    let dir = TempDir::new("forge-gh-exists-open");
    stub_all(&dir, "gh", "--state all", &gh_pr(3, "OPEN", "[]"));
    let status = forge_at(&dir).status("https://github.com/o/r", "b");
    assert_eq!(status.item, WorkItem::Open);
    assert_eq!(status.label.as_deref(), Some("PR #3"));

    // Empty on both pages is not_existing, and costs the second call.
    let dir = TempDir::new("forge-gh-exists-none");
    stub_all(&dir, "gh", "--state all", "[]");
    let status = forge_at(&dir).status("https://github.com/o/r", "b");
    assert_eq!(status.item, WorkItem::NotExisting);
    assert_eq!(calls(&dir, "gh").len(), 2);

    // glab: same shape, with `--all` as the existence flag.
    let dir = TempDir::new("forge-glab-exists");
    stub_all(&dir, "glab", "--all", &glab_mr(7, "merged", "null"));
    let status = forge_at(&dir).status("https://gitlab.com/o/r", "b");
    assert_eq!(status.item, WorkItem::Closed);
    assert_eq!(calls(&dir, "glab").len(), 2, "open query, then all states");

    let dir = TempDir::new("forge-glab-exists-none");
    stub_all(&dir, "glab", "--all", "[]");
    let status = forge_at(&dir).status("https://gitlab.com/o/r", "b");
    assert_eq!(status.item, WorkItem::NotExisting);
}

#[test]
fn the_cache_serves_network_facts_without_reasking() {
    let dir = TempDir::new("forge-cache");
    stub(&dir, "gh", &gh_pr(1, "OPEN", "[]"), 0);
    let forge = forge_at(&dir);
    let mut cache = ForgeCache::new(Duration::from_secs(300));
    let t0 = Instant::now();

    let url = "https://github.com/o/r";
    assert_eq!(cache.status(&forge, url, "b", t0).item, WorkItem::Open);
    assert_eq!(cache.status(&forge, url, "b", t0).item, WorkItem::Open);
    assert_eq!(calls(&dir, "gh").len(), 1, "the second ask is cached");
    // A different branch is a different fact.
    cache.status(&forge, url, "other", t0);
    assert_eq!(calls(&dir, "gh").len(), 2);
    // Past the TTL the fact is re-collected.
    cache.status(&forge, url, "b", t0 + Duration::from_secs(301));
    assert_eq!(calls(&dir, "gh").len(), 3);
}

#[test]
fn forge_status_is_a_plain_value() {
    // Unknown is informational by construction: it carries its reason and
    // blocks nothing.
    let status = ForgeStatus {
        item: WorkItem::Unknown,
        pipeline: Pipeline::Unknown,
        label: None,
        url: None,
        reason: Some("offline".to_owned()),
    };
    assert_eq!(status.item, WorkItem::Unknown);
}
