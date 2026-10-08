//! The detail pane and the `e` evidence overlay over fabricated
//! snapshots: every row kind's fields, honest `?`s where the evidence
//! never landed, escaped and truncated prompts, related conversations in
//! proven-strength order, gone work and its live references, and the
//! claim/latch/rejected-record evidence - at both the narrow and the
//! wide terminal widths. Line widths are asserted in the TUI's unit tests.

use agent_sessions::attention::{Attention, ClaimOutcome, ClaimRow, ClaimSource};
use agent_sessions::forge::{Pipeline, WorkItem};
use agent_sessions::provider::SourceError;
use agent_sessions::runtime::{EvidenceSource, PaneSource, Provider};
use agent_sessions::snapshot::{
    AttachmentLiveness, AttachmentRow, CommitRow, ConversationRow, ConversationState,
    ConversationSummary, EvidenceRow, IncarnationRow, LatchRow, PaneRow, ReferenceKind,
    ReferenceRow, RelatedRow, RelationStrength, RepoCounts, RepoRow, SCHEMA_VERSION, Snapshot,
    Upstream, WorkKind, WorkRow, WorkSection,
};
use agent_sessions::store::{
    ActivityEvent, ActivitySource, Confidence, ContinuityEvidence, Exec, Mark, NormEvent,
    ObservationEvent, ObservationSource, RejectedRecord, SessionContext, TouchProvenance,
};
use agent_sessions::tui::{App, Key};
use agent_sessions::verdict::{ActionVerdict, Verdict};
mod support;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::PathBuf;

const NOW: u64 = 1_800_000_000;
const NOW_MS: u64 = NOW * 1000;

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

fn repo(git: bool) -> RepoRow {
    RepoRow {
        id: "/repos/a/.git".to_owned(),
        name: "a".to_owned(),
        path: PathBuf::from("/repos/a"),
        git,
        work: 2,
        live: 1,
        attention: Attention::Waiting,
        open: 2,
        clean: 0,
        last_activity: Some(NOW - 120),
        default_branch: git.then(|| "main".to_owned()),
        remote: git.then(|| "origin".to_owned()),
        counts: RepoCounts {
            needs_you: 1,
            active: 0,
            follow_up: 1,
            forgotten: 0,
            ready_to_clean: 0,
            cleanup_review: 0,
        },
    }
}

fn incarnation(id: &str, ref_name: &str, number: usize) -> IncarnationRow {
    IncarnationRow {
        id: id.to_owned(),
        number,
        repo: "/repos/a/.git".to_owned(),
        ref_name: ref_name.to_owned(),
        first_observed_at: NOW - 7200,
        last_observed_at: NOW - 120,
        creation_head: Some("aaa1111bbb222".to_owned()),
        creation_at: Some(NOW - 7200),
        head: Some("ccc3333ddd444".to_owned()),
        ended_at: None,
        continuity: ContinuityEvidence::SameReflogCreation,
        excluded: false,
    }
}

fn work() -> WorkRow {
    WorkRow {
        repo: "/repos/a/.git".to_owned(),
        repo_name: "a".to_owned(),
        kind: WorkKind::Branch,
        name: "feat/login".to_owned(),
        worktree: Some(PathBuf::from("/repos/a-login")),
        branch: Some("feat/login".to_owned()),
        dirty: Some(true),
        broken: None,
        activities: Vec::new(),
        conversation_summaries: Vec::new(),
        observations: Vec::new(),
        commits_ahead: Some(3),
        unpushed: Some(3),
        upstream: Upstream::Tracked,
        upstream_detail: Some("origin/feat/login".to_owned()),
        landed: Some(agent_sessions::snapshot::Landed::No),
        base: Some("origin/main".to_owned()),
        windows: 1,
        live_pids: 1,
        live_sessions: 1,
        past_sessions: 2,
        last_activity: Some(NOW - 120),
        attention: Attention::Waiting,
        identity: Some("i111".to_owned()),
        incarnation: Some(incarnation("i111", "feat/login", 1)),
        same_name_history: Vec::new(),
        parked: false,
        forge: WorkItem::Open,
        pipeline: Pipeline::Failed,
        forge_label: Some("PR #191".to_owned()),
        forge_url: Some("https://github.com/o/r/pull/191".to_owned()),
        commits_behind: Some(2),
        commits: None,
        panes: vec![PaneRow {
            handle: "workmux:1.2".to_owned(),
            command: "claude".to_owned(),
            target: None,
        }],
        gone: None,
        references: Vec::new(),
        worktree_removal: Some(ActionVerdict {
            verdict: Verdict::Blocked,
            reasons: vec!["uncommitted changes".to_owned()],
        }),
        branch_deletion: Some(ActionVerdict {
            verdict: Verdict::Review,
            reasons: vec!["would need git branch -D".to_owned()],
        }),
        section: WorkSection::NeedsYou,
        summary: "waiting: permission prompt · ↑3 ~dirty PR #191".to_owned(),
    }
}

fn attachment() -> AttachmentRow {
    AttachmentRow {
        pid: 4200,
        pid_start: Some(NOW - 5000),
        liveness: AttachmentLiveness::Instance,
        liveness_detail: None,
        pane: Some("workmux:1.2".to_owned()),
        target: None,
        pane_source: Some(PaneSource::Published),
        placement_detail: None,
        source: EvidenceSource::Published,
        observed_at: NOW - 30,
    }
}

fn conversation(id: &str, title: Option<&str>) -> ConversationRow {
    ConversationRow {
        provider: Provider::Claude,
        session_id: format!("{id}-1111-2222-3333-444444444444"),
        short_id: id.to_owned(),
        title: title.map(str::to_owned),
        state: ConversationState::Waiting,
        state_raw: Some("waiting".to_owned()),
        waiting_for: Some("permission prompt".to_owned()),
        state_since: Some(NOW - 120),
        state_since_ms: Some((NOW - 120) * 1000),
        attention: Attention::Waiting,
        attention_detail: Some("permission prompt".to_owned()),
        attention_seq: Some(4),
        attention_wait_ms: Some((NOW - 120) * 1000),
        journal_seq: Some(7),
        last_activity: Some(NOW - 120),
        live: true,
        attachment: Some(attachment()),
        cwd: Some(PathBuf::from("/repos/a-login")),
        transcript: Some(PathBuf::from("/h/.claude/t.jsonl")),
        malformed_lines: Some(2),
        resume_argv: vec!["claude".to_owned(), "--resume".to_owned(), id.to_owned()],
        latest_prompt: None,
        latest_reply: None,
        repo: Some("/repos/a/.git".to_owned()),
        worktree: Some(PathBuf::from("/repos/a-login")),
        branch: Some("feat/login".to_owned()),
        touches: vec![
            agent_sessions::snapshot::TouchRow {
                incarnation_id: "i111".to_owned(),
                repo: "/repos/a/.git".to_owned(),
                ref_name: "feat/login".to_owned(),
                incarnation: 1,
                head: Some("aaa1111".to_owned()),
                valid_from: NOW - 3600,
                valid_until: Some(NOW - 1200),
                provenance: TouchProvenance::Cwd,
                confidence: Confidence::Exact,
            },
            agent_sessions::snapshot::TouchRow {
                incarnation_id: "i111".to_owned(),
                repo: "/repos/a/.git".to_owned(),
                ref_name: "feat/login".to_owned(),
                incarnation: 1,
                head: Some("ccc3333".to_owned()),
                valid_from: NOW - 900,
                valid_until: None,
                provenance: TouchProvenance::ProviderBranch,
                confidence: Confidence::Exact,
            },
        ],
        current_incarnation: Some("i111".to_owned()),
        started_at: Some(NOW - 7200),
        related: Vec::new(),
        evidence: EvidenceRow::default(),
    }
}

/// A snapshot with one repo, one live work row and one conversation.
fn fixture() -> Snapshot {
    Snapshot {
        schema_version: SCHEMA_VERSION,
        observed_at: NOW,
        own_pane: None,
        complete: true,
        repos: vec![repo(true)],
        work: vec![work()],
        conversations: vec![conversation("8f423bbb", Some("update pane labels"))],
        errors: vec![],
        skipped: vec![],
        stale_sockets: 0,
    }
}

#[test]
fn repo_detail_shows_path_counts_and_unknowns_at_both_widths() {
    let mut app = App::new(fixture());
    press(&mut app, &[Key::Char('1'), Key::Char('j')]);
    for width in [55, 200] {
        let text = render(&app, width, 30);
        assert!(text.contains("path: /repos/a"), "{text}");
        assert!(text.contains("default branch: main"), "{text}");
        assert!(text.contains("remote: origin"), "{text}");
        assert!(text.contains("live conversations: 1"), "{text}");
        assert!(text.contains("needs you 1"), "{text}");
    }
    // A repo whose base never proved renders the honest unknowns.
    let mut snapshot = fixture();
    snapshot.repos[0].default_branch = None;
    snapshot.repos[0].remote = None;
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('1'), Key::Char('j')]);
    let text = render(&app, 200, 30);
    assert!(text.contains("default branch: ?"), "{text}");
    assert!(text.contains("remote: ?"), "{text}");
}

#[test]
fn work_detail_shows_incarnation_delivery_upstream_forge_and_cleanup() {
    let mut app = App::new(fixture());
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    for width in [55, 200] {
        let text = render(&app, width, 40);
        assert!(text.contains("incarnation:"), "{text}");
        assert!(text.contains("feat/login#1"), "{text}");
        assert!(text.contains("worktree:"), "{text}");
        assert!(text.contains("local:"), "{text}");
        assert!(text.contains("remote:"), "{text}");
        assert!(text.contains("forge:"), "{text}");
        assert!(text.contains("activity:"), "{text}");
        assert!(text.contains("cleanup:"), "{text}");
        assert!(text.contains("worktree remove: blocked"), "{text}");
        assert!(text.contains("branch delete: review"), "{text}");
    }
    let text = render(&app, 200, 40);
    assert!(text.contains("vs origin/main"), "{text}");
    assert!(text.contains("origin/feat/login"), "{text}");
    assert!(text.contains("PR #191"), "{text}");
    assert!(text.contains("workmux:1.2"), "{text}");
    assert!(text.contains("uncommitted changes"), "{text}");
    assert!(text.contains("would need git branch -D"), "{text}");
}

#[test]
fn activity_and_observation_histories_render_separately_and_cap_at_seven() {
    let mut snapshot = fixture();
    let mut activities = Vec::new();
    for i in 0..8u64 {
        activities.push(ActivityEvent {
            source: ActivitySource::Commit,
            occurred_at_ms: (NOW - 600 - i * 60) * 1000,
            reasons: vec![format!("commit{i:02} subject {i}")],
        });
    }
    activities.push(ActivityEvent {
        source: ActivitySource::WorkingTree,
        occurred_at_ms: (NOW - 60) * 1000,
        reasons: vec!["deleted README.md".to_owned()],
    });
    activities.push(ActivityEvent {
        source: ActivitySource::Conversation,
        occurred_at_ms: (NOW - 30) * 1000,
        reasons: vec!["8f423bbb update pane labels".to_owned()],
    });
    snapshot.work[0].activities = activities;
    snapshot.work[0].observations = vec![ObservationEvent {
        source: ObservationSource::Lifecycle,
        observed_at_ms: (NOW - 90) * 1000,
        reasons: vec!["upstream: a -> b".to_owned(), "ahead: 1 -> 2".to_owned()],
    }];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    for width in [55, 200] {
        let text = render(&app, width, 50);
        assert!(text.contains("activity:"), "{text}");
        assert!(text.contains("observations:"), "{text}");
        assert!(
            text.find("activity:").unwrap() < text.find("observations:").unwrap(),
            "activity lists first: {text}"
        );
        // Activity sources group by their newest occurrence.
        let conversation = text.find("  conversation:").expect("conversation group");
        let working = text.find("  working tree:").expect("working tree group");
        let commit = text.find("  commit:").expect("commit group");
        assert!(
            conversation < working && working < commit,
            "activity sources order by their newest event: {text}"
        );
        assert!(
            text.find("commit00").unwrap() < text.find("commit01").unwrap(),
            "newest event first inside the source: {text}"
        );
        assert!(
            !text.contains("commit07"),
            "the eighth commit event is past the display cap: {text}"
        );
        assert!(text.contains("deleted README.md"), "{text}");
        // The lifecycle transition is detection evidence, rendered under
        // `observations:` at its own time - never under `activity:`.
        assert!(text.contains("upstream: a -> b"), "{text}");
        assert!(text.contains("ahead: 1 -> 2"), "{text}");
        assert!(!text.contains("last change"), "{text}");
    }
    let text = render(&app, 200, 50);
    // No cursor-backed summaries: the raw conversation trail stays - the
    // legacy fallback a record without cursors renders.
    assert!(text.contains("8f423bbb update pane labels"), "{text}");
    assert!(!text.contains("last activity"), "{text}");

    // Two observation sources tied on their newest time order by name;
    // within a source, events sort newest first.
    let mut tied = fixture();
    tied.work[0].observations = vec![
        ObservationEvent {
            source: ObservationSource::Forge,
            observed_at_ms: (NOW - 60) * 1000,
            reasons: vec!["forge old".to_owned()],
        },
        ObservationEvent {
            source: ObservationSource::Forge,
            observed_at_ms: (NOW - 30) * 1000,
            reasons: vec!["forge new".to_owned()],
        },
        ObservationEvent {
            source: ObservationSource::Lifecycle,
            observed_at_ms: (NOW - 30) * 1000,
            reasons: vec!["lifecycle tied".to_owned()],
        },
    ];
    let mut tied = App::new(tied);
    press(&mut tied, &[Key::Char('2'), Key::Char('j')]);
    let text = render(&tied, 200, 40);
    let forge = text.find("  forge:").expect("forge group");
    let lifecycle = text.find("  lifecycle:").expect("lifecycle group");
    assert!(forge < lifecycle, "tied groups order by name: {text}");
    assert!(
        text.find("forge new").unwrap() < text.find("forge old").unwrap(),
        "events newest first: {text}"
    );

    let mut quiet = App::new(fixture());
    press(&mut quiet, &[Key::Char('2'), Key::Char('j')]);
    let text = render(&quiet, 200, 40);
    assert!(
        !text.contains("observations:"),
        "a row with no observations renders no section: {text}"
    );
}

#[test]
fn work_detail_renders_question_marks_for_unproven_fields() {
    let mut row = work();
    row.base = None;
    row.upstream = Upstream::Unknown;
    row.upstream_detail = Some("probe timed out".to_owned());
    row.commits_ahead = None;
    row.commits_behind = None;
    row.forge = WorkItem::Unknown;
    row.forge_label = None;
    row.worktree_removal = None;
    row.branch_deletion = None;
    row.activities = Vec::new();
    row.observations = Vec::new();
    let mut snapshot = fixture();
    snapshot.work = vec![row];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    let text = render(&app, 200, 40);
    assert!(text.contains("local: ?"), "{text}");
    assert!(
        text.contains("remote: ? (probe timed out) · unpushed 3"),
        "{text}"
    );
    assert!(text.contains("forge: ?"), "{text}");
    assert!(text.contains("activity: ?"), "{text}");
    assert!(text.contains("worktree remove: ?"), "{text}");
    assert!(text.contains("branch delete: ?"), "{text}");
    // An unproven base lists no commits: the honest cell.
    let at = text.find("commits not on ?:").expect("the commits head");
    assert!(text[at..].contains("  ?"), "{text}");
}

#[test]
fn work_detail_lists_the_commits_not_on_the_base() {
    let mut row = work();
    // Three ahead, two listed: the cap names the rest.
    row.commits_ahead = Some(3);
    row.commits = Some(vec![
        CommitRow {
            sha: "9ac21f0e5d3b".to_owned(),
            subject: "fix\tlabels".to_owned(),
            at: NOW - 600,
            conversation: Some("8f423bbb".to_owned()),
        },
        CommitRow {
            sha: "7bd22aa01c4e".to_owned(),
            subject: "start login".to_owned(),
            at: NOW - 7_200,
            conversation: None,
        },
    ]);
    let mut landed = work();
    landed.name = "feat/landed".to_owned();
    landed.commits_ahead = Some(0);
    landed.commits = Some(Vec::new());
    let mut snapshot = fixture();
    snapshot.work = vec![row, landed];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("commits not on origin/main:"), "{text}");
    assert!(
        text.contains("9ac21f0 fix\\tlabels · 10m · 8f423bbb"),
        "{text}"
    );
    assert!(text.contains("7bd22aa start login · 2h · ?"), "{text}");
    assert!(text.contains("… 1 more"), "{text}");
    press(&mut app, &[Key::Char('j')]);
    let text = render(&app, 200, 50);
    let at = text.find("commits not on origin/main:").expect("the head");
    assert!(text[at..].contains("  none"), "{text}");
    assert!(!text.contains("more"), "{text}");
}

#[test]
fn a_broken_worktree_row_states_its_broken_link_and_dirty_flag() {
    let mut row = work();
    row.broken = Some(".git missing; metadata retained by a".to_owned());
    let mut snapshot = fixture();
    snapshot.work = vec![row];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    let text = render(&app, 55, 40);
    assert!(text.contains("state: broken - .git missi"), "{text}");
    assert!(text.contains("dirty: yes"), "{text}");
    let text = render(&app, 200, 40);
    assert!(text.contains("worktree: /repos/a-login"), "{text}");
    assert!(
        text.contains("state: broken - .git missing; metadata retained by a"),
        "{text}"
    );
    assert!(text.contains("dirty: yes"), "{text}");

    let mut row = work();
    row.broken = Some("checkout moved".to_owned());
    row.dirty = None;
    let mut snapshot = fixture();
    snapshot.work = vec![row];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    let text = render(&app, 200, 40);
    assert!(text.contains("state: broken - checkout moved"), "{text}");
    assert!(text.contains("dirty: ?"), "{text}");

    let mut row = work();
    row.broken = Some("checkout moved".to_owned());
    row.dirty = Some(false);
    let mut snapshot = fixture();
    snapshot.work = vec![row];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    let text = render(&app, 200, 40);
    assert!(text.contains("dirty: no"), "{text}");

    let mut app = App::new(fixture());
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    let text = render(&app, 200, 40);
    assert!(!text.contains("state: broken"), "{text}");
    assert!(!text.contains("dirty:"), "{text}");
}

#[test]
fn gone_work_names_what_vanished_and_every_live_reference() {
    let mut row = work();
    row.gone = Some("branch and worktree gone".to_owned());
    row.references = vec![
        ReferenceRow {
            kind: ReferenceKind::Pane,
            label: "workmux:1.2".to_owned(),
        },
        ReferenceRow {
            kind: ReferenceKind::Window,
            label: "workmux:1".to_owned(),
        },
        ReferenceRow {
            kind: ReferenceKind::TmuxSession,
            label: "workmux".to_owned(),
        },
        ReferenceRow {
            kind: ReferenceKind::Process,
            label: "pid 4200".to_owned(),
        },
        ReferenceRow {
            kind: ReferenceKind::AgentSession,
            label: "claude:8f423bbb".to_owned(),
        },
    ];
    row.section = WorkSection::CleanupReview;
    let mut snapshot = fixture();
    snapshot.work = vec![row];
    let mut app = App::new(snapshot);
    // Under `all` the cleanup sections collapse into counts; the repo
    // scope lists the row for selection.
    press(
        &mut app,
        &[
            Key::Char('1'),
            Key::Char('j'),
            Key::Char('2'),
            Key::Char('j'),
        ],
    );
    for width in [55, 200] {
        let text = render(&app, width, 40);
        assert!(text.contains("branch and worktree gone"), "{text}");
        for label in [
            "pane workmux:1.2",
            "window workmux:1",
            "session workmux",
            "process pid 4200",
            "agent claude:8f423bbb",
        ] {
            assert!(text.contains(label), "{label} missing:\n{text}");
        }
    }
}

#[test]
fn conversation_detail_lists_touches_relations_and_last_prompts() {
    let mut conv = conversation("8f423bbb", Some("update pane labels"));
    conv.latest_prompt = Some("rename\tthese\npanes\x01".to_owned());
    conv.latest_reply = Some("a very long reply ".repeat(30));
    conv.related = vec![
        RelatedRow {
            strength: RelationStrength::ProviderLineage,
            label: "child".to_owned(),
            provenance: "provider lineage record".to_owned(),
            provider: Provider::Claude,
            session_id: "child-1".to_owned(),
            short_id: "child001".to_owned(),
            title: Some("spawned work".to_owned()),
            attention: Attention::None,
            state_since: Some(NOW - 600),
        },
        RelatedRow {
            strength: RelationStrength::ProcessAncestry,
            label: "ancestor".to_owned(),
            provenance: "live process ancestry".to_owned(),
            provider: Provider::Claude,
            session_id: "parent-1".to_owned(),
            short_id: "parent01".to_owned(),
            title: None,
            attention: Attention::Working,
            state_since: Some(NOW - 700),
        },
        RelatedRow {
            strength: RelationStrength::SameIncarnation,
            label: "same incarnation".to_owned(),
            provenance: "touch feat/login#1".to_owned(),
            provider: Provider::Claude,
            session_id: "sibling-1".to_owned(),
            short_id: "sibl001".to_owned(),
            title: None,
            attention: Attention::None,
            state_since: Some(NOW - 900),
        },
    ];
    let mut snapshot = fixture();
    snapshot.conversations = vec![conv];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('3'), Key::Char('j')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("cwd: /repos/a-login"), "{text}");
    assert!(text.contains("pane: workmux:1.2 · published"), "{text}");
    assert!(text.contains("state: waiting · since 2m"), "{text}");
    assert!(text.contains("waiting for: permission prompt"), "{text}");
    assert!(text.contains("resumable"), "{text}");
    assert!(text.contains("started 2h · last turn 2m"), "{text}");
    assert!(text.contains("forge: PR #191"), "{text}");
    // Every touch interval: head, provenance and confidence.
    assert!(text.contains("provider_branch · exact"), "{text}");
    // Epochs and per-touch files and commits are not exposed: `?`.
    assert!(text.contains("epoch: ?"), "{text}");
    assert!(
        text.contains("files ? · commits ? · created repo ?"),
        "{text}"
    );
    assert!(text.contains("cwd · exact"), "{text}");
    // Related groups in strength order.
    let at = |needle: &str| text.find(needle).unwrap_or(usize::MAX);
    assert!(text.contains("lineage - provider declared:"), "{text}");
    assert!(text.contains("observed process ancestry"), "{text}");
    assert!(!text.contains("pane-creation"), "{text}");
    assert!(text.contains("same incarnation:"), "{text}");
    assert!(
        at("lineage - provider declared:") < at("observed process ancestry")
            && at("observed process ancestry") < at("same incarnation:"),
        "{text}"
    );
    // Prompts are escaped and truncated to one line each.
    assert!(text.contains("rename\\tthese\\npanes\\u0001"), "{text}");
    let text55 = render(&app, 55, 50);
    assert!(!text55.contains('\t'), "{text55}");
    // Unknown values stay `?` where the transcript carried nothing.
    let mut snapshot = fixture();
    snapshot.conversations[0].related.clear();
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('3'), Key::Char('j')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("none proven"), "{text}");
    assert!(text.contains("prompt: ?"), "{text}");
    assert!(text.contains("reply: ?"), "{text}");
}

#[test]
fn the_evidence_overlay_shows_claims_latches_marks_and_rejects() {
    let mut conv = conversation("8f423bbb", Some("update pane labels"));
    conv.evidence = EvidenceRow {
        claims: vec![
            ClaimRow {
                source: ClaimSource::Published,
                exec: Exec::Waiting,
                observed_ms: NOW_MS - 30_000,
                since_ms: Some(NOW_MS - 120_000),
                seq: None,
                detail: Some("permission prompt".to_owned()),
                outcome: ClaimOutcome::Winner,
                note: None,
            },
            ClaimRow {
                source: ClaimSource::Journal,
                exec: Exec::Idle,
                observed_ms: NOW_MS - 300_000,
                since_ms: Some(NOW_MS - 300_000),
                seq: Some(3),
                detail: None,
                outcome: ClaimOutcome::Outranked,
                note: Some("older than the published state".to_owned()),
            },
        ],
        latches: vec![
            LatchRow {
                seq: 2,
                kind: NormEvent::End,
                at_ms: NOW_MS - 600_000,
                reason: None,
                acknowledged: true,
            },
            LatchRow {
                seq: 4,
                kind: NormEvent::Awaiting,
                at_ms: NOW_MS - 120_000,
                reason: Some("permission prompt".to_owned()),
                acknowledged: false,
            },
        ],
        rejected: vec![RejectedRecord {
            conversation: "claude:8f423bbb".to_owned(),
            seq: 5,
            at_ms: NOW_MS - 60_000,
            pseq: Some(11),
            native: "PreToolUse".to_owned(),
            reason: "no mapped event".to_owned(),
        }],
        seen_seq: Some(2),
        seen_wait_ms: Some(NOW_MS - 600_000),
        mark: Some(Mark {
            since_ms: NOW_MS - 900_000,
            seq: 6,
            at_ms: NOW_MS - 400_000,
        }),
        mark_suppressed: false,
        journal_seq: Some(7),
        producer_seq: Some(12),
    };
    let mut snapshot = fixture();
    snapshot.conversations = vec![conv];
    snapshot.errors = vec![SourceError {
        source: "git".to_owned(),
        detail: "rev-list failed".to_owned(),
    }];
    snapshot.skipped = vec!["not-a-uuid.jsonl".to_owned()];
    snapshot.stale_sockets = 3;
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char('e')]);
    for width in [55, 200] {
        let text = render(&app, width, 50);
        assert!(text.contains("[4] Evidence"), "{text}");
        assert!(text.contains("claims:"), "{text}");
        assert!(text.contains("published waiting"), "{text}");
        assert!(text.contains("journal idle"), "{text}");
        assert!(text.contains("outranked"), "{text}");
        assert!(text.contains("winner"), "{text}");
        assert!(text.contains("unacked"), "{text}");
        assert!(text.contains("acked"), "{text}");
        assert!(text.contains("mark: not-busy"), "{text}");
        assert!(text.contains("sequences: journal"), "{text}");
        assert!(text.contains("rejected / stale:"), "{text}");
        assert!(text.contains("PreToolUse"), "{text}");
        assert!(text.contains("no mapped event"), "{text}");
        assert!(text.contains("collector:"), "{text}");
        assert!(text.contains("rev-list failed"), "{text}");
        assert!(text.contains("not-a-uuid.jsonl"), "{text}");
        assert!(text.contains("3 stale sockets"), "{text}");
    }
    let text = render(&app, 200, 50);
    assert!(text.contains("journal 7 · producer 12"), "{text}");
    // `Esc` closes it back to the detail; `e` reopens and `e` closes.
    press(&mut app, &[Key::Esc]);
    let text = render(&app, 200, 50);
    assert!(text.contains("[4] Conversation"), "{text}");
    press(&mut app, &[Key::Char('e'), Key::Char('e')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("[4] Conversation"), "{text}");
}

#[test]
fn evidence_on_work_shows_the_incarnation_continuity_verdict() {
    let mut app = App::new(fixture());
    press(&mut app, &[Key::Char('2'), Key::Char('j'), Key::Char('e')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("[4] Evidence"), "{text}");
    assert!(text.contains("same_reflog_creation"), "{text}");
    assert!(text.contains("ref creation aaa1111"), "{text}");
    assert!(text.contains("tip ccc3333"), "{text}");
    // A history row shows the same evidence block for its own record.
    let mut snapshot = fixture();
    let mut closed = incarnation("i000", "feat/login", 1);
    closed.ended_at = Some(NOW - 3600);
    closed.excluded = true;
    snapshot.work[0].same_name_history = vec![closed];
    snapshot.work[0].incarnation.as_mut().unwrap().number = 2;
    let mut app = App::new(snapshot);
    press(
        &mut app,
        &[
            Key::Char('1'),
            Key::Char('j'),
            Key::Char('h'),
            Key::Char('2'),
        ],
    );
    // Walk [2] until the cursor lands on the history row itself.
    for _ in 0..8 {
        let text = render(&app, 200, 50);
        if text.contains("[4] Work - feat/login#1") {
            break;
        }
        app.key(Key::Char('j'));
    }
    let text = render(&app, 200, 50);
    assert!(text.contains("[4] Work - feat/login#1"), "{text}");
    assert!(text.contains("excluded"), "{text}");
    press(&mut app, &[Key::Char('e')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("[4] Evidence - feat/login#1"), "{text}");
    assert!(text.contains("continuity:"), "{text}");
    assert!(text.contains("excluded"), "{text}");
}

#[test]
fn the_detail_pane_scrolls_with_jk_only_while_focused() {
    let mut app = App::new(fixture());
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    // Scrolling on [4] moves the body, not a list cursor.
    press(&mut app, &[Key::Char('4')]);
    let before = render(&app, 55, 12);
    press(&mut app, &[Key::Char('j'), Key::Char('j')]);
    let after = render(&app, 55, 12);
    assert_ne!(before, after, "j scrolled nothing:\n{after}");
    // `k` scrolls back to the top.
    press(&mut app, &[Key::Char('k'), Key::Char('k'), Key::Char('k')]);
    let back = render(&app, 55, 12);
    assert_eq!(before, back);
    // On a list, j/k still moves cursors, not the detail.
    press(&mut app, &[Key::Char('2'), Key::Char('k')]);
    let text = render(&app, 55, 24);
    assert!(text.contains("  all"), "{text}");
}

/// A fixture with a second, gone work row in `Cleanup review`.
fn fixture_with_gone() -> Snapshot {
    let mut snapshot = fixture();
    let mut gone = work();
    gone.name = "feat/gone".to_owned();
    gone.branch = Some("feat/gone".to_owned());
    gone.gone = Some("branch deleted".to_owned());
    gone.references = vec![
        ReferenceRow {
            kind: ReferenceKind::TmuxSession,
            label: "workmux".to_owned(),
        },
        ReferenceRow {
            kind: ReferenceKind::Process,
            label: "pid 99".to_owned(),
        },
    ];
    gone.section = WorkSection::CleanupReview;
    snapshot.work.push(gone);
    snapshot
}

#[test]
fn every_detail_target_renders_at_both_widths() {
    // Every target kind, detail and evidence, draws at both widths. Line
    // widths are asserted where the lines are built, in the TUI's unit
    // tests: a rendered buffer is always exactly the terminal's width, so
    // it cannot show a clipped line.
    for keys in [
        &[Key::Char('1'), Key::Char('j')][..],
        &[Key::Char('2'), Key::Char('j')][..],
        &[Key::Char('2'), Key::Char('j'), Key::Char('j')][..],
        &[Key::Char('3'), Key::Char('j')][..],
    ] {
        let mut app = App::new(fixture_with_gone());
        press(&mut app, keys);
        for width in [55, 200] {
            let text = render(&app, width, 30);
            assert!(text.contains("[4] "), "{text}");
            press(&mut app, &[Key::Char('e')]);
            let text = render(&app, width, 30);
            assert!(text.contains("[4] Evidence"), "{text}");
            press(&mut app, &[Key::Char('e')]);
        }
    }
    // An `all` target under `e` shows only the collector block.
    let mut app = App::new(fixture());
    press(&mut app, &[Key::Char('e')]);
    let text = render(&app, 200, 30);
    assert!(text.contains("collector:"), "{text}");
}

/// A variant pass over the branches the main fixture never takes:
/// remote-gone and unproven upstreams, closed forge items, a dead
/// process, an unbound pane, a missing attachment, an acknowledged-set
/// reading, a suppressed mark, and references on a gone row whose
/// incarnation ended.
#[test]
fn the_variant_arms_render_honestly() {
    let mut row = work();
    row.dirty = None;
    row.upstream = Upstream::RemoteGone;
    row.upstream_detail = Some("origin/feat/login".to_owned());
    row.forge = WorkItem::Closed;
    row.forge_label = Some("PR #99".to_owned());
    row.pipeline = Pipeline::Succeeded;
    row.worktree = None;
    row.panes = Vec::new();
    row.incarnation.as_mut().unwrap().ended_at = Some(NOW - 3_600);
    // A branch with no work item at all says so plainly.
    let mut noitem = work();
    noitem.name = "feat/quiet".to_owned();
    noitem.forge = WorkItem::NotExisting;
    noitem.forge_label = None;
    let mut snapshot = fixture();
    snapshot.work = vec![row, noitem];
    let mut app = App::new(snapshot);
    press(
        &mut app,
        &[
            Key::Char('1'),
            Key::Char('j'),
            Key::Char('2'),
            Key::Char('j'),
        ],
    );
    let text = render(&app, 200, 40);
    assert!(text.contains("remote gone"), "{text}");
    assert!(text.contains("PR #99"), "{text}");
    assert!(text.contains("worktree: ?"), "{text}");
    press(&mut app, &[Key::Char('j')]);
    let text = render(&app, 200, 40);
    assert!(text.contains("no work item"), "{text}");

    let mut conv = conversation("8f423bbb", None);
    conv.live = true;
    conv.attachment = Some(AttachmentRow {
        pid: 4200,
        pid_start: Some(NOW - 5000),
        liveness: AttachmentLiveness::PidOnly,
        liveness_detail: Some("pid reused".to_owned()),
        pane: None,
        target: None,
        pane_source: None,
        placement_detail: Some("no pane evidence".to_owned()),
        source: EvidenceSource::Derived,
        observed_at: NOW - 30,
    });
    conv.evidence.seen_seq = None;
    conv.evidence.seen_wait_ms = Some(NOW_MS - 600_000);
    conv.evidence.mark_suppressed = true;
    conv.evidence.mark = Some(Mark {
        since_ms: NOW_MS - 900_000,
        seq: 6,
        at_ms: NOW_MS - 400_000,
    });
    conv.evidence.rejected = vec![RejectedRecord {
        conversation: "claude:8f423bbb".to_owned(),
        seq: 5,
        at_ms: NOW_MS - 60_000,
        pseq: None,
        native: "Stop".to_owned(),
        reason: "out of order".to_owned(),
    }];
    conv.evidence.claims = vec![ClaimRow {
        source: ClaimSource::Ping,
        exec: Exec::Unknown,
        observed_ms: NOW_MS - 10_000,
        since_ms: None,
        seq: None,
        detail: None,
        outcome: ClaimOutcome::Outranked,
        note: None,
    }];
    conv.transcript = Some(PathBuf::from("/h/.claude/t.jsonl"));
    conv.malformed_lines = None;
    let mut snapshot = fixture();
    snapshot.conversations = vec![conv];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('3'), Key::Char('j')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("unbound: no pane evidence"), "{text}");
    press(&mut app, &[Key::Char('e')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("pid only (pid reused)"), "{text}");
    assert!(text.contains("through seq ?"), "{text}");
    assert!(text.contains("suppressed the busy claim"), "{text}");
    assert!(text.contains("pseq ?"), "{text}");
    assert!(text.contains("malformed lines ?"), "{text}");
    assert!(text.contains("claim source: derived"), "{text}");
    assert!(text.contains("ping unknown"), "{text}");
    assert!(text.contains("unbound: no pane evidence"), "{text}");
    // A reading that names only a seen sequence says `wait seen at ?`.
    let mut snapshot = fixture();
    snapshot.conversations[0].evidence.seen_seq = Some(3);
    snapshot.conversations[0].evidence.seen_wait_ms = None;
    snapshot.conversations[0].transcript = None;
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char('e')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("wait seen at ?"), "{text}");

    // A live claim the runtime proved dead says so beside its latch; a
    // transcript-only row has no attachment evidence at all.
    let mut dead = conversation("8f423bbb", None);
    dead.live = true;
    dead.attachment = Some(AttachmentRow {
        pid: 4200,
        pid_start: Some(NOW - 5000),
        liveness: AttachmentLiveness::Dead,
        liveness_detail: Some("pid gone".to_owned()),
        pane: None,
        target: None,
        pane_source: None,
        placement_detail: Some("process exited".to_owned()),
        source: EvidenceSource::Lock,
        observed_at: NOW - 30,
    });
    let mut bare = conversation("44dd0bbb", None);
    bare.attachment = Some(AttachmentRow {
        pid: 4200,
        pid_start: None,
        liveness: AttachmentLiveness::Unverifiable,
        liveness_detail: None,
        pane: None,
        target: None,
        pane_source: None,
        placement_detail: None,
        source: EvidenceSource::Published,
        observed_at: NOW - 30,
    });
    let mut snapshot = fixture();
    snapshot.conversations = vec![dead, bare];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('3'), Key::Char('j')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("process exited"), "{text}");
    // Its evidence names the dead process and the unbound placement.
    press(&mut app, &[Key::Char('e')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("dead (pid gone)"), "{text}");
    assert!(text.contains("unbound: process exited"), "{text}");
    press(&mut app, &[Key::Char('e'), Key::Char('j'), Key::Char('e')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("pane: ?"), "{text}");
    assert!(text.contains("unverifiable"), "{text}");
    // And a row with no attachment evidence at all says so.
    let mut snapshot = fixture();
    snapshot.conversations[0].attachment = None;
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char('e')]);
    let text = render(&app, 200, 50);
    assert!(text.contains("attachment: no claim"), "{text}");
    // `Esc` with no overlay open is inert.
    press(&mut app, &[Key::Char('e'), Key::Esc, Key::Esc]);
    let text = render(&app, 200, 50);
    assert!(text.contains("[4] Conversation"), "{text}");
}

/// `p` on a gone row is read-only: no store write, no parked flip.
#[test]
fn park_on_a_gone_row_writes_nothing() {
    let dir = support::tempdir::TempDir::new("detail-park");
    let store = agent_sessions::store::Store::open(dir.join("agent-sessions"));
    let mut gone = work();
    gone.gone = Some("worktree gone".to_owned());
    gone.section = WorkSection::CleanupReview;
    let mut snapshot = fixture();
    snapshot.work = vec![gone];
    let mut app = App::new(snapshot).with_store(store);
    press(
        &mut app,
        &[
            Key::Char('1'),
            Key::Char('j'),
            Key::Char('2'),
            Key::Char('j'),
            Key::Char('p'),
        ],
    );
    assert!(!app.snapshot.work[0].parked);
}

/// A work row without incarnation evidence under `e` says so plainly.
#[test]
fn evidence_on_a_space_or_incarnationless_row_is_honest() {
    let mut space = work();
    space.kind = WorkKind::ProjectSpace;
    space.incarnation = None;
    space.identity = Some("/spaces/notes".to_owned());
    let mut snapshot = fixture();
    snapshot.work = vec![space];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j'), Key::Char('e')]);
    let text = render(&app, 200, 40);
    assert!(text.contains("no incarnation evidence"), "{text}");
}

/// A journal record the fold rejected shows up in the emitted snapshot's
/// evidence: reordered `pseq` on load, then the collect's row carries it.
#[test]
fn a_rejected_journal_record_reaches_the_snapshot_evidence() {
    let home = support::tempdir::TempDir::new("detail-rejected");
    let projects = home.join(".claude/projects/t");
    std::fs::create_dir_all(&projects).expect("mkdir");
    let cwd = home.join("space");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::write(
        projects.join("8f423bbb-1111-2222-3333-444444444444.jsonl"),
        support::claude_turn("8f423bbb-1111-2222-3333-444444444444", &cwd, "the task"),
    )
    .expect("transcript writes");
    let store = agent_sessions::store::Store::open(home.join("state/agent-sessions"));
    let id = "8f423bbb-1111-2222-3333-444444444444";
    let mut high = agent_sessions::store::Record::new("claude", id, "Stop");
    high.event = Some(NormEvent::End);
    high.pseq = Some(5);
    store.append(high).expect("append");
    let mut stale = agent_sessions::store::Record::new("claude", id, "Stop");
    stale.event = Some(NormEvent::End);
    stale.pseq = Some(3);
    store.append(stale).expect("append");
    let runtime = agent_sessions::runtime::Runtime::observe_over(&[]);
    let snapshot = agent_sessions::snapshot::Collector::new(home.join(".claude"))
        .with_store(home.join("state/agent-sessions"))
        .collect(&runtime, None);
    let conv = snapshot
        .conversations
        .iter()
        .find(|c| c.session_id == id)
        .expect("the conversation row");
    assert_eq!(conv.evidence.rejected.len(), 1, "{:?}", conv.evidence);
    assert_eq!(conv.evidence.rejected[0].pseq, Some(3));
    assert!(
        conv.evidence.rejected[0]
            .reason
            .contains("below the high-water"),
        "{:?}",
        conv.evidence.rejected
    );
}

/// A conversation summary as the store projects it onto the row.
fn summary(
    key: &str,
    at_ms: u64,
    title: Option<&str>,
    prompt: Option<&str>,
) -> ConversationSummary {
    ConversationSummary {
        key: key.to_owned(),
        occurred_at_ms: at_ms,
        context: (title.is_some() || prompt.is_some()).then(|| SessionContext {
            title: title.map(str::to_owned),
            prompt_excerpt: prompt.map(str::to_owned),
        }),
    }
}

#[test]
fn conversation_summaries_compact_turns_into_one_row_per_conversation() {
    let mut snapshot = fixture();
    let row = &mut snapshot.work[0];
    // The raw history keeps every turn; the compacted group replaces it.
    row.activities = vec![
        ActivityEvent {
            source: ActivitySource::Conversation,
            occurred_at_ms: (NOW - 120) * 1000,
            reasons: vec!["8f423bbb update pane labels".to_owned()],
        },
        ActivityEvent {
            source: ActivitySource::Conversation,
            occurred_at_ms: (NOW - 60) * 1000,
            reasons: vec![
                "8f423bbb update pane labels".to_owned(),
                "aaaa1111 first".to_owned(),
            ],
        },
        ActivityEvent {
            source: ActivitySource::WorkingTree,
            occurred_at_ms: (NOW - 30) * 1000,
            reasons: vec!["modified main.rs".to_owned()],
        },
    ];
    // Two conversations at one timestamp keep two summaries in stable
    // full-key order - the provider-qualified keys with the same
    // eight-character session prefix stay distinct.
    row.conversation_summaries = vec![
        summary(
            "claude\u{0}aaaa1111-2222",
            (NOW - 60) * 1000,
            None,
            Some("fix the login form"),
        ),
        summary(
            "other\u{0}aaaa1111-9999",
            (NOW - 60) * 1000,
            Some("a titled one"),
            None,
        ),
        summary("claude\u{0}8f423bbb-3333", (NOW - 120) * 1000, None, None),
    ];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    for width in [55, 200] {
        let text = render(&app, width, 50);
        assert!(text.contains("  conversation:"), "{text}");
        // One row per conversation, newest occurrence first then key:
        // `claude\0aaaa...` sorts before `other\0aaaa...` at equal time.
        // Wrapping may split a row's id from its detail at 55 columns, so
        // the order check walks `last activity` occurrences in the text.
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("last activity"))
            .collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].contains("60s aaaa1111"), "{text}");
        assert!(lines[1].contains("60s aaaa1111"), "{text}");
        assert!(lines[2].contains("2m 8f423bbb"), "{text}");
        let prompt_at = text.find("prompt: fix the login form").expect("prompt");
        let title_at = text.find("a titled one").expect("title");
        assert!(prompt_at < title_at, "full-key order at equal time: {text}");
        assert!(
            !lines[2].contains(" - "),
            "id alone where the record captured no context: {text}"
        );
        // The raw turn reasons compacted away from the activity section
        // (the conversation list's own title column still shows it), and
        // the working-tree source's own group stays - newest first:
        // working tree (30s) beats conversation (60s).
        let activity = &text[text.find("activity:").unwrap()..text.find("commits not on").unwrap()];
        assert!(!activity.contains("update pane labels"), "{text}");
        assert!(text.contains("modified main.rs"), "{text}");
        assert!(
            text.find("  working tree:").unwrap() < text.find("  conversation:").unwrap(),
            "source groups order by their newest event: {text}"
        );
        // Wrapped cells never carry a terminal control.
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
    }
}

#[test]
fn conversation_summaries_cap_at_seven_and_escape_context() {
    let mut snapshot = fixture();
    let row = &mut snapshot.work[0];
    // Eight conversations compacted from twice as many raw events: the
    // seven newest keys show; the eighth - oldest - does not.
    let mut activities = Vec::new();
    row.conversation_summaries = (0..8u64)
        .map(|i| {
            // The eight-char short id distinguishes every key.
            let key = format!("claude\u{0}s{i:07}-xxxx");
            for turn in 0..2 {
                activities.push(ActivityEvent {
                    source: ActivitySource::Conversation,
                    occurred_at_ms: (NOW - 1000 + i * 10 + turn) * 1000,
                    reasons: vec![format!("s{i:07} turn{turn}")],
                });
            }
            // The oldest row's prompt carries escapes; it compacts out
            // with the row, so a prompt with controls must show on a
            // surviving row instead.
            let prompt = (i == 7).then(|| "tab\there\nand \u{7}bell".to_owned());
            summary(
                &key,
                (NOW - 1000 + i * 10 + 1) * 1000,
                None,
                prompt.as_deref(),
            )
        })
        .collect();
    row.activities = activities;
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    for width in [55, 200] {
        let text = render(&app, width, 60);
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("last activity"))
            .collect();
        assert_eq!(lines.len(), 7, "{text}");
        assert!(text.contains("s0000007"), "{text}");
        assert!(
            !text.contains("s0000000"),
            "the eighth compacts out: {text}"
        );
        assert!(!text.contains("s0000000 turn"), "{text}");
        assert!(!text.contains("turn1"), "{text}");
    }
    // The control characters in the stored excerpt render escaped,
    // never as terminal input - asserted unwrapped on the wide frame.
    let text = render(&app, 200, 60);
    assert!(
        text.contains("prompt: tab\\there\\nand \\u0007bell"),
        "{text}"
    );
    assert!(!text.contains('\t') && !text.contains('\u{7}'), "{text}");
}

#[test]
fn a_unicode_control_prompt_renders_bounded_and_escaped() {
    // The excerpt the store persists is raw and cell-bounded; the detail
    // renders it escaped once - no terminal control survives, at either
    // width.
    let temp = support::tempdir::TempDir::new("unicode-excerpt");
    let store = agent_sessions::store::Store::open(temp.path().to_path_buf());
    let obs = |name: &str| agent_sessions::store::ObservedRef {
        name: name.to_owned(),
        head: None,
        rewritten: false,
        creation: None,
        renamed_from: None,
        commit: None,
        activities: Vec::new(),
        inputs: agent_sessions::store::LifecycleInputs::default(),
    };
    store
        .sync_repo("/r/.git", &[obs("feat")], 1_000)
        .expect("sync");
    let id = store
        .load()
        .work
        .branch("/r/.git", "feat")
        .expect("the record")
        .id
        .clone();
    // CJK double-width, a combining mark, raw tab/newline/escape and a
    // tail long enough to force truncation past the cell bound.
    let prompt = format!(
        "日本語のe\u{301}xcerpt\ttab\nnewline\u{1b}[0m {}",
        "長い尾部".repeat(40)
    );
    store
        .sync_session_updates(&[agent_sessions::store::SessionUpdate {
            identity: agent_sessions::store::UpdateIdentity::Branch(id.clone()),
            conversation: agent_sessions::store::conversation_key("claude", "u0n1c0de-zz"),
            at_ms: 5_000,
            reason: "turn".to_owned(),
            context: SessionContext {
                title: None,
                prompt_excerpt: Some(prompt),
            },
        }])
        .expect("update");
    let record = store
        .load()
        .work
        .branches
        .get(&id)
        .expect("the record")
        .clone();
    let excerpt = record.session_context
        [&agent_sessions::store::conversation_key("claude", "u0n1c0de-zz")]
        .prompt_excerpt
        .clone()
        .expect("the bounded excerpt");
    // Raw storage, bounded on escape: it keeps control bytes verbatim but
    // never renders past 120 cells once escaped.
    assert!(excerpt.contains('\t') && excerpt.contains('\u{1b}'));
    assert!(excerpt.ends_with('…'));

    let mut snapshot = fixture();
    snapshot.work[0].conversation_summaries = vec![summary(
        "claude\u{0}u0n1c0de-zz",
        (NOW - 60) * 1000,
        None,
        Some(&excerpt),
    )];
    let mut app = App::new(snapshot);
    press(&mut app, &[Key::Char('2'), Key::Char('j')]);
    for width in [55, 200] {
        let text = render(&app, width, 50);
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'), "{text}");
        // Wrapping splits the row mid-string; the detail pane's cells,
        // rejoined past the pane borders, restore each logical line.
        let detail: String = text
            .lines()
            .filter_map(|l| l.rsplit('│').nth(1).map(str::to_owned))
            .map(|s| s.trim_end().to_owned())
            .collect();
        // The escaped controls and double-width text print literally -
        // buffer cells pad each wide glyph with a space, so the needles
        // are the fragments padding cannot split.
        assert!(detail.contains("prompt: 日"), "{text}");
        assert!(detail.contains("xcerpt"), "{text}");
        assert!(detail.contains("\\ttab"), "{text}");
        assert!(detail.contains("\\nnewline"), "{text}");
        assert!(detail.contains("\\u001b"), "{text}");
        assert!(detail.contains("…"), "{text}");
    }
}
