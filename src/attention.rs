//! Attention and effective state: the deterministic reduction of a
//! conversation's journal fold, published state, liveness and authored
//! marks into what the row shows.
//!
//! Everything here is pure and clock-injected - `now` is an argument, so
//! tests drive time. Execution and attention stay separate: a live `Busy`
//! can sit beside a retained, unacknowledged `error`; a dead process
//! claims no execution but keeps an unacknowledged `end` or `error`.
//!
//! Two orderings serve two questions. Per-conversation precedence -
//! `error > done > waiting > working` - answers "which latch do you still
//! need to see". The cross-conversation rank - `waiting > error > done >
//! working` - answers "which row needs you first" and orders both the [3]
//! inbox and the work-row rollup.

use serde::Serialize;

use crate::provider::PublishedStatus;
use crate::store::{Exec, Fold, Mark, NormEvent, Seen};

/// What a row is asking of the user, or proving about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    /// Blocked on the human.
    Waiting,
    /// The turn aborted, unseen.
    Error,
    /// A clean turn end, unseen.
    CompletedUnseen,
    /// Working right now.
    Working,
    /// Nothing pending.
    None,
    /// The signal exists but cannot be established - live with no
    /// applicable evidence, or acknowledgement unverifiable.
    Unknown,
}

impl Attention {
    /// Per-conversation precedence: which of two latches on one row still
    /// shows - `error > done > waiting > working`, because it answers which
    /// of two events you still need to see, not which is most urgent.
    pub fn precedence(self) -> u8 {
        match self {
            Attention::Error => 0,
            Attention::CompletedUnseen => 1,
            Attention::Waiting => 2,
            Attention::Working => 3,
            Attention::Unknown => 4,
            Attention::None => 5,
        }
    }

    /// The cross-row rank - `waiting > error > done > working` - the inbox
    /// sort and the work-row rollup order.
    pub fn rank(self) -> u8 {
        match self {
            Attention::Waiting => 0,
            Attention::Error => 1,
            Attention::CompletedUnseen => 2,
            Attention::Working => 3,
            Attention::Unknown => 4,
            Attention::None => 5,
        }
    }

    /// The leading glyph: attention only.
    pub fn glyph(self) -> &'static str {
        match self {
            Attention::Waiting => "!",
            Attention::Error => "✗",
            Attention::CompletedUnseen => "✓",
            Attention::Working => "●",
            Attention::Unknown => "?",
            Attention::None => "",
        }
    }

    /// The word the summary field and detail header use.
    pub fn label(self) -> &'static str {
        match self {
            Attention::Waiting => "waiting",
            Attention::Error => "error",
            Attention::CompletedUnseen => "done",
            Attention::Working => "working",
            Attention::Unknown => "unknown",
            Attention::None => "",
        }
    }
}

/// The execution state the provider published, with its times.
#[derive(Debug, Clone)]
pub struct Published {
    /// The mapped status; `None` when the provider's raw value is unknown
    /// to the mapping - the record then proves nothing.
    pub status: Option<PublishedStatus>,
    pub waiting_for: Option<String>,
    /// When the provider last touched its own record.
    pub observed_ms: u64,
    /// When the provider says the state began.
    pub since_ms: Option<u64>,
}

/// Everything one derive pass needs about one conversation.
pub struct Inputs<'a> {
    /// The journal reduction; `None` (or empty) means no events arrived.
    pub fold: Option<&'a Fold>,
    /// What the user has acknowledged: retained events at or below its
    /// sequence, and the live wait episode it names.
    pub seen: Seen,
    /// The authored not-busy mark, when one exists.
    pub mark: Option<&'a Mark>,
    /// The provider-published state, present only when it is bound to a
    /// live attachment - a stale file's claim is history, not evidence.
    pub published: Option<Published>,
    /// The live attachment's process instance `(pid, pid_start_secs)`;
    /// `None` when the process is proven dead or there is no claim.
    pub live: Option<(u32, Option<u64>)>,
    /// Now, epoch milliseconds - the only clock this code reads.
    pub now_ms: u64,
    /// Whether the seen-state file could be read. When it could not, an
    /// unacknowledged latch may actually be acknowledged - the attention
    /// is `Unknown` rather than a guess.
    pub ack_ok: bool,
    /// The weak `Busy -> Idle` stabilizer, carried across passes by the
    /// collector so confirmations can accumulate.
    pub idle: &'a mut WeakIdle,
}

/// One pass's verdict for one conversation.
#[derive(Debug, Clone)]
pub struct Derived {
    /// The effective execution state.
    pub exec: Exec,
    /// When it began, epoch milliseconds; `None` when unproven.
    pub since_ms: Option<u64>,
    /// The wait reason while `exec` is `Waiting`.
    pub waiting_for: Option<String>,
    /// The row's attention (precedence winner) and its reason.
    pub attention: Attention,
    pub attention_detail: Option<String>,
    /// The commit sequence an acknowledgement writes through: the highest
    /// unacknowledged retained event, `0` when nothing awaits.
    pub ack_through: u64,
    /// The live wait episode an acknowledgement records: its `since`
    /// while `exec` is a `Waiting` the user has not seen, else `None`.
    pub wait_ms: Option<u64>,
    /// The newest event the conversation has - a not-busy mark written at
    /// this sequence is superseded by anything above it.
    pub journal_seq: u64,
    /// Whether the not-busy mark suppressed a live `Busy`.
    pub marked: bool,
}

/// The unmapped ping's weak `Busy` lease.
const PING_LEASE_MS: u64 = 5_000;

/// A newly attached process spends this long unproven: only explicit
/// evidence (published state, a mapped event) speaks during grace.
const STARTUP_GRACE_MS: u64 = 3_000;

/// The weak `Busy -> Idle` debounce: three confirmations a hundred
/// milliseconds apart, released after seven hundred at the latest -
/// Herdr's measured boundary, applied to a published `idle` that arrives
/// while the event stream still claims the turn runs.
#[derive(Debug, Default)]
pub struct WeakIdle {
    hits: u8,
    first_ms: u64,
    last_ms: u64,
}

impl WeakIdle {
    /// One weak-idle observation; `true` once the transition may publish:
    /// three confirmations at least 100 ms apart, or the 700 ms cap. A
    /// long gap between reads is a confirm: the source held `idle` the
    /// whole span.
    pub fn confirm(&mut self, now_ms: u64) -> bool {
        if self.last_ms == 0 {
            *self = WeakIdle {
                hits: 1,
                first_ms: now_ms,
                last_ms: now_ms,
            };
            return false;
        }
        if now_ms.saturating_sub(self.last_ms) >= 100 {
            self.hits = self.hits.saturating_add(1);
            self.last_ms = now_ms;
        }
        self.hits >= 3 || now_ms.saturating_sub(self.first_ms) >= 700
    }

    /// Forget the episode - the candidate no longer holds.
    pub fn reset(&mut self) {
        *self = WeakIdle::default();
    }
}

/// Whether the record's process instance can still speak for the
/// attachment: same pid, and start times that agree when both carry one.
/// A record that never resolved a process cannot be discriminated and
/// still applies - a limitation of pid-less payloads, not a choice.
fn same_instance(
    record_pid: Option<u32>,
    record_start: Option<u64>,
    live: (u32, Option<u64>),
) -> bool {
    let (pid, start) = live;
    match record_pid {
        Some(p) if p != pid => return false,
        None => return true,
        _ => {}
    }
    match (record_start, start) {
        (Some(a), Some(b)) => a.abs_diff(b) <= 1,
        _ => true,
    }
}

/// One candidate execution claim, ranked: published state bound to the
/// live attachment is strongest, mapped hook events next, the unmapped
/// ping's weak lease last. A newer weaker claim never displaces applicable
/// stronger evidence.
struct Candidate {
    rank: u8,
    observed_ms: u64,
    since_ms: u64,
    exec: Exec,
    waiting_for: Option<String>,
    /// The event sequence behind it, when it came from the journal.
    seq: u64,
}

/// Derive one conversation's effective state and attention.
pub fn derive(inputs: Inputs<'_>) -> Derived {
    let empty = Fold::default();
    let fold = inputs.fold.unwrap_or(&empty);

    let unacked: Vec<&crate::store::Retained> = fold
        .retained
        .iter()
        .filter(|r| r.seq > inputs.seen.seq)
        .collect();
    let ack_through = unacked.iter().map(|r| r.seq).max().unwrap_or(0);

    // Execution evidence, evaluated only while a process may be live - a
    // dead `(pid, pid_start)` invalidates every live-state claim but never
    // the latches.
    let mut exec = Exec::Unknown;
    let mut since_ms = None;
    let mut waiting_for = None;
    let mut winner_seq = 0u64;
    let mut marked = false;
    if let Some(live) = inputs.live {
        let mut candidates: Vec<Candidate> = Vec::new();
        if let Some(p) = &inputs.published
            && let Some(status) = p.status
        {
            candidates.push(Candidate {
                rank: 1,
                observed_ms: p.observed_ms,
                since_ms: p.since_ms.unwrap_or(p.observed_ms),
                exec: match status {
                    PublishedStatus::Busy => Exec::Busy,
                    PublishedStatus::Idle => Exec::Idle,
                    PublishedStatus::Waiting => Exec::Waiting,
                },
                waiting_for: p.waiting_for.clone(),
                seq: 0,
            });
        }
        if let Some(event) = &fold.last_event
            && same_instance(event.pid, event.pid_start, live)
        {
            candidates.push(Candidate {
                rank: 2,
                observed_ms: event.observed_ms,
                since_ms: event.since_ms,
                exec: event.kind.execution().unwrap_or(Exec::Unknown),
                waiting_for: event.reason.clone(),
                seq: event.seq,
            });
        }
        // The newest applicable strong claim wins; a mapped event that
        // outdates the provider's file still publishes immediately.
        if let Some(winner) = candidates.iter().max_by_key(|c| c.observed_ms) {
            exec = winner.exec;
            since_ms = Some(winner.since_ms);
            waiting_for = winner.waiting_for.clone();
            winner_seq = winner.seq;
        } else {
            // No strong claim: the unmapped ping's weak lease applies only
            // outside the startup grace and only while it is fresh.
            let in_grace = live
                .1
                .is_some_and(|start| inputs.now_ms.saturating_sub(start * 1000) < STARTUP_GRACE_MS);
            let pinged = fold
                .ping
                .is_some_and(|(_, at)| inputs.now_ms.saturating_sub(at) < PING_LEASE_MS);
            if pinged && !in_grace {
                exec = Exec::Busy;
                since_ms = fold.ping.map(|(_, at)| at);
            }
        }

        // A weak `Busy -> Idle`: the provider published `idle` while the
        // journal's lifecycle claim is still Busy-class and no `end`
        // arrived - an inference, so it confirms before publishing.
        let weak_idle = exec == Exec::Idle
            && candidates
                .iter()
                .any(|c| c.rank == 2 && matches!(c.exec, Exec::Busy));
        if weak_idle && !inputs.idle.confirm(inputs.now_ms) {
            // Still confirming: the Busy stands until the third read or
            // the cap.
            exec = Exec::Busy;
            since_ms = fold.last_event.as_ref().map(|e| e.since_ms).or(since_ms);
        } else if !weak_idle {
            inputs.idle.reset();
        }

        // The authored not-busy mark: names the dismissed `Busy`'s
        // `effective_since`, lives only while nothing newer contradicts it.
        if exec == Exec::Busy
            && let Some(mark) = inputs.mark
            && mark.since_ms == since_ms.unwrap_or(0)
            && mark.seq >= fold.last_seq.max(winner_seq)
            && inputs
                .published
                .as_ref()
                .is_none_or(|p| p.observed_ms <= mark.at_ms)
        {
            exec = Exec::Idle;
            marked = true;
        }
    } else {
        inputs.idle.reset();
    }

    // A live wait is seen once its episode was acknowledged - through the
    // sequence of the event that opened it, or by a `since` at or before
    // the newest wait acknowledged - and shows again only when a newer
    // wait begins. The comparison is by time, not identity: the hook event
    // and the provider's record date one wait a few milliseconds apart, and
    // whichever wins arbitration must read the same acknowledgement.
    let wait_seen = exec == Exec::Waiting
        && (since_ms.is_some_and(|s| inputs.seen.wait_ms.is_some_and(|w| s <= w))
            || winner_seq > 0 && winner_seq <= inputs.seen.seq);
    let wait_ms = if exec == Exec::Waiting && !wait_seen {
        since_ms
    } else {
        None
    };

    // Attention sources: every unacknowledged latch, plus the live claim.
    let mut attention = Attention::None;
    let mut detail = None;
    let mut consider = |kind: Attention, reason: Option<&str>| {
        if kind.precedence() < attention.precedence() {
            attention = kind;
            detail = reason.map(str::to_owned);
        }
    };
    for r in &unacked {
        let kind = match r.kind {
            NormEvent::Awaiting => Attention::Waiting,
            NormEvent::Error => Attention::Error,
            NormEvent::End => Attention::CompletedUnseen,
            _ => continue, // coverage: off - only latching kinds are ever retained
        };
        consider(kind, r.reason.as_deref());
    }
    match exec {
        Exec::Waiting if !wait_seen => consider(Attention::Waiting, waiting_for.as_deref()),
        Exec::Busy if !marked => consider(Attention::Working, None),
        _ => {}
    }
    if !inputs.ack_ok && (ack_through > 0 || wait_ms.is_some()) {
        // Attention exists but its acknowledgement cannot be established:
        // `?`, never a guessed glyph.
        attention = Attention::Unknown;
        detail = None;
    }
    if attention == Attention::None && inputs.live.is_some() && since_ms.is_none() {
        // Live but with no applicable evidence at all: `?`, not a guess.
        // Evidence that proves no execution - an acknowledged `error` -
        // still answers, as does a live conversation that is simply idle:
        // neither carries a glyph.
        attention = Attention::Unknown;
    }

    Derived {
        exec,
        since_ms,
        waiting_for,
        attention,
        attention_detail: detail,
        ack_through,
        wait_ms,
        journal_seq: fold.last_seq,
        marked,
    }
}

/// The attention a work row rolls up: the best rank among the
/// conversations bound to it - a waiting pane outranks everything.
pub fn rollup<'a>(attentions: impl Iterator<Item = &'a Attention>) -> Attention {
    attentions
        .copied()
        .min_by_key(|a| a.rank())
        .unwrap_or(Attention::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fold that saw exactly `events`, in order, one second apart.
    fn fold_with(events: &[(NormEvent, u64)]) -> Fold {
        let mut fold = Fold::default();
        for (i, (kind, at)) in events.iter().enumerate() {
            let seq = (i + 1) as u64;
            let record = crate::store::Record {
                v: 1,
                seq,
                at: *at,
                writer: "test".to_owned(),
                provider: "claude".to_owned(),
                session: "s1".to_owned(),
                native: format!("{kind:?}"),
                event: Some(*kind),
                pts: None,
                pseq: None,
                pid: None,
                pid_start: None,
                cwd: None,
                reason: None,
            };
            fold.apply(&record);
        }
        fold
    }

    /// Inputs for a live conversation with `fold`, nothing published.
    fn inputs<'a>(
        fold: &'a Fold,
        live: Option<(u32, Option<u64>)>,
        idle: &'a mut WeakIdle,
    ) -> Inputs<'a> {
        Inputs {
            fold: Some(fold),
            seen: Seen::default(),
            mark: None,
            published: None,
            live,
            now_ms: 100_000,
            ack_ok: true,
            idle,
        }
    }

    #[test]
    fn busy_waiting_end_and_error_walk_through_the_projection() {
        let mut idle = WeakIdle::default();
        // Busy -> Waiting -> Idle/unseen -> error retained: the parity
        // table's rows, in order.
        let fold = fold_with(&[(NormEvent::Start, 50_000)]);
        let d = derive(inputs(&fold, Some((7, Some(90))), &mut idle));
        assert_eq!(d.exec, Exec::Busy);
        assert_eq!(d.attention, Attention::Working);
        assert_eq!(d.since_ms, Some(50_000));

        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::Awaiting, 60_000)]);
        let d = derive(inputs(&fold, Some((7, Some(90))), &mut idle));
        assert_eq!(d.exec, Exec::Waiting);
        assert_eq!(d.attention, Attention::Waiting);
        assert_eq!(d.ack_through, 2);

        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::End, 60_000)]);
        let d = derive(inputs(&fold, Some((7, Some(90))), &mut idle));
        assert_eq!(d.exec, Exec::Idle);
        assert_eq!(d.attention, Attention::CompletedUnseen);

        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::Error, 60_000)]);
        let d = derive(inputs(&fold, Some((7, Some(90))), &mut idle));
        assert_eq!(d.exec, Exec::Unknown);
        assert_eq!(d.attention, Attention::Error);
        // A later `activity` resumes Busy beside the retained `error` -
        // precedence still shows the latch.
        let fold = fold_with(&[
            (NormEvent::Start, 50_000),
            (NormEvent::Error, 60_000),
            (NormEvent::Activity, 70_000),
        ]);
        let d = derive(inputs(&fold, Some((7, Some(90))), &mut idle));
        assert_eq!(d.exec, Exec::Busy);
        assert_eq!(d.attention, Attention::Error);
    }

    #[test]
    fn acknowledged_events_do_not_latch() {
        let mut idle = WeakIdle::default();
        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::End, 60_000)]);
        let mut in_ = inputs(&fold, Some((7, Some(90))), &mut idle);
        in_.seen.seq = 2;
        let d = derive(in_);
        assert_eq!(d.attention, Attention::None);
        assert_eq!(d.ack_through, 0);
        // A `start` acknowledges too - the retained set is already empty.
        let fold = fold_with(&[
            (NormEvent::Start, 50_000),
            (NormEvent::End, 60_000),
            (NormEvent::Start, 70_000),
        ]);
        let d = derive(inputs(&fold, Some((7, Some(90))), &mut idle));
        assert_eq!(d.exec, Exec::Busy);
        assert_eq!(d.attention, Attention::Working);
    }

    #[test]
    fn an_acknowledged_wait_clears_until_a_newer_wait_begins() {
        let mut idle = WeakIdle::default();
        // A hook-driven wait, acknowledged through its event's sequence:
        // the agent still waits, but nothing asks to be seen.
        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::Awaiting, 60_000)]);
        let d = derive(inputs(&fold, Some((7, Some(90))), &mut idle));
        assert_eq!(d.wait_ms, Some(60_000));
        let mut in_ = inputs(&fold, Some((7, Some(90))), &mut idle);
        in_.seen.seq = 2;
        let d = derive(in_);
        assert_eq!(d.exec, Exec::Waiting);
        assert_eq!(d.attention, Attention::None);
        assert_eq!((d.ack_through, d.wait_ms), (0, None));

        // A wait only the provider published carries no sequence: its
        // episode is acknowledged by its `since`.
        let published = |since: u64| Published {
            status: Some(PublishedStatus::Waiting),
            waiting_for: Some("permission prompt".to_owned()),
            observed_ms: since,
            since_ms: Some(since),
        };
        let empty = Fold::default();
        let mut in_ = inputs(&empty, Some((7, Some(90))), &mut idle);
        in_.published = Some(published(90_000));
        let d = derive(in_);
        assert_eq!(d.attention, Attention::Waiting);
        assert_eq!((d.ack_through, d.wait_ms), (0, Some(90_000)));
        let mut in_ = inputs(&empty, Some((7, Some(90))), &mut idle);
        in_.published = Some(published(90_000));
        in_.seen.wait_ms = Some(90_000);
        let d = derive(in_);
        assert_eq!(d.exec, Exec::Waiting);
        assert_eq!(d.attention, Attention::None);
        assert_eq!(d.wait_ms, None);
        // The same wait, now dated by the other source a little earlier -
        // the provider touched its record after a hook-driven wait was
        // acknowledged - stays seen.
        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::Awaiting, 95_000)]);
        let mut in_ = inputs(&fold, Some((7, Some(90))), &mut idle);
        let mut touched = published(94_990);
        touched.observed_ms = 99_000;
        in_.published = Some(touched);
        in_.seen = Seen {
            seq: 2,
            wait_ms: Some(95_000),
        };
        assert_eq!(derive(in_).attention, Attention::None);
        // A newer wait episode is unseen again.
        let mut in_ = inputs(&empty, Some((7, Some(90))), &mut idle);
        in_.published = Some(published(95_000));
        in_.seen.wait_ms = Some(90_000);
        assert_eq!(derive(in_).attention, Attention::Waiting);
        // An acknowledged `error` on a live process proves no execution,
        // but it is evidence: no glyph, not `?`.
        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::Error, 60_000)]);
        let mut in_ = inputs(&fold, Some((7, Some(90))), &mut idle);
        in_.seen.seq = 2;
        let d = derive(in_);
        assert_eq!((d.exec, d.attention), (Exec::Unknown, Attention::None));
        // And with seen-state unreadable a live wait is `?`, not a guess.
        let mut in_ = inputs(&empty, Some((7, Some(90))), &mut idle);
        in_.published = Some(published(90_000));
        in_.ack_ok = false;
        assert_eq!(derive(in_).attention, Attention::Unknown);
    }

    #[test]
    fn death_keeps_the_latch_but_claims_no_execution() {
        let mut idle = WeakIdle::default();
        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::Error, 60_000)]);
        // Dead process: no live claim, but the error is durable.
        let d = derive(inputs(&fold, None, &mut idle));
        assert_eq!(d.exec, Exec::Unknown);
        assert_eq!(d.attention, Attention::Error);
        // An acknowledged error on a dead conversation is clean history.
        let mut in_ = inputs(&fold, None, &mut idle);
        in_.seen.seq = 2;
        let d = derive(in_);
        assert_eq!(d.attention, Attention::None);
    }

    #[test]
    fn published_state_beats_older_events_and_loses_to_newer() {
        let mut idle = WeakIdle::default();
        let fold = fold_with(&[(NormEvent::End, 60_000)]);
        // The provider's file still claims `busy` but is older than the
        // `end`: the explicit event publishes immediately.
        let mut in_ = inputs(&fold, Some((7, Some(90))), &mut idle);
        in_.published = Some(Published {
            status: Some(PublishedStatus::Busy),
            waiting_for: None,
            observed_ms: 55_000,
            since_ms: Some(40_000),
        });
        let d = derive(in_);
        assert_eq!(d.exec, Exec::Idle);
        assert_eq!(d.attention, Attention::CompletedUnseen);

        // And the same file updated after the event wins back - waiting
        // beats an old end for the execution claim.
        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::End, 60_000)]);
        let mut in_ = inputs(&fold, Some((7, Some(90))), &mut idle);
        in_.published = Some(Published {
            status: Some(PublishedStatus::Waiting),
            waiting_for: Some("permission prompt".to_owned()),
            observed_ms: 90_000,
            since_ms: Some(90_000),
        });
        let d = derive(in_);
        assert_eq!(d.exec, Exec::Waiting);
        assert_eq!(d.waiting_for.as_deref(), Some("permission prompt"));
        // The retained `end` still latches, and on one row precedence is
        // `done > waiting`: the unseen completion is what still shows.
        assert_eq!(d.attention, Attention::CompletedUnseen);
    }

    #[test]
    fn the_ping_lease_is_weak_and_bounded() {
        let mut idle = WeakIdle::default();
        // An unmapped ping on a live process: five seconds of weak Busy.
        let mut fold = Fold::default();
        let mut ping = crate::store::Record::new("claude", "s1", "StrangeEvent");
        ping.seq = 1;
        ping.at = 97_000;
        fold.apply(&ping);
        let d = derive(inputs(&fold, Some((7, Some(0))), &mut idle));
        assert_eq!(d.exec, Exec::Busy);
        assert_eq!(d.attention, Attention::Working);
        assert_eq!(d.since_ms, Some(97_000));

        // Inside the startup grace the ping is not evidence.
        let d = derive(inputs(&fold, Some((7, Some(99))), &mut idle));
        assert_eq!(d.exec, Exec::Unknown);
        assert_eq!(d.attention, Attention::Unknown);

        // Past the lease it ages to Unknown.
        let mut in_ = inputs(&fold, Some((7, Some(0))), &mut idle);
        in_.now_ms = 103_000;
        let d = derive(in_);
        assert_eq!(d.exec, Exec::Unknown);
        assert_eq!(d.attention, Attention::Unknown);

        // A ping never displaces applicable stronger evidence: after `end`,
        // a ping is diagnostic, not a Busy claim.
        let mut fold = fold_with(&[(NormEvent::End, 60_000)]);
        let mut ping = crate::store::Record::new("claude", "s1", "Odd");
        ping.seq = 3;
        ping.at = 99_000;
        fold.apply(&ping);
        let d = derive(inputs(&fold, Some((7, Some(0))), &mut idle));
        assert_eq!(d.exec, Exec::Idle);
        assert_eq!(d.attention, Attention::CompletedUnseen);
    }

    #[test]
    fn the_mark_suppresses_busy_until_newer_evidence_lands() {
        let mut idle = WeakIdle::default();
        let fold = fold_with(&[(NormEvent::Start, 50_000)]);
        let mark = Mark {
            since_ms: 50_000,
            seq: 1,
            at_ms: 90_000,
        };
        let mut in_ = inputs(&fold, Some((7, Some(0))), &mut idle);
        in_.mark = Some(&mark);
        let d = derive(in_);
        assert_eq!(d.exec, Exec::Idle);
        assert!(d.marked);
        assert_eq!(d.attention, Attention::None);

        // A newer event supersedes the mark: Busy returns on its own.
        let fold = fold_with(&[(NormEvent::Start, 50_000), (NormEvent::Activity, 95_000)]);
        let mut in_ = inputs(&fold, Some((7, Some(0))), &mut idle);
        in_.mark = Some(&mark);
        let d = derive(in_);
        assert_eq!(d.exec, Exec::Busy);
        assert_eq!(d.attention, Attention::Working);

        // So does a newer published observation.
        let fold = fold_with(&[(NormEvent::Start, 50_000)]);
        let mut in_ = inputs(&fold, Some((7, Some(0))), &mut idle);
        in_.mark = Some(&mark);
        in_.published = Some(Published {
            status: Some(PublishedStatus::Busy),
            waiting_for: None,
            observed_ms: 96_000,
            since_ms: Some(50_000),
        });
        let d = derive(in_);
        assert_eq!(d.exec, Exec::Busy);
    }

    #[test]
    fn weak_idle_stabilizes_over_three_reads_or_the_cap() {
        let mut idle = WeakIdle::default();
        // The journal still claims Busy; the provider file flipped to idle.
        // That is an inference - it must confirm before it publishes.
        let fold = fold_with(&[(NormEvent::Start, 50_000)]);
        let published = || {
            Some(Published {
                status: Some(PublishedStatus::Idle),
                waiting_for: None,
                observed_ms: 90_000,
                since_ms: Some(90_000),
            })
        };
        let at = |now: u64, idle: &mut WeakIdle| {
            let mut in_ = inputs(&fold, Some((7, Some(0))), idle);
            in_.now_ms = now;
            in_.published = published();
            derive(in_)
        };
        assert_eq!(at(100_000, &mut idle).exec, Exec::Busy);
        assert_eq!(at(100_050, &mut idle).exec, Exec::Busy); // <100ms apart: not a read
        assert_eq!(at(100_150, &mut idle).exec, Exec::Busy); // second confirmation
        assert_eq!(at(100_300, &mut idle).exec, Exec::Idle); // third: published
        // Once published, the reading does not flap back.
        assert_eq!(at(200_000, &mut idle).exec, Exec::Idle);
        // The cap alone also releases a fresh episode: two reads 700 ms
        // apart publish on the second.
        let mut idle = WeakIdle::default();
        assert_eq!(at(200_000, &mut idle).exec, Exec::Busy);
        assert_eq!(at(200_800, &mut idle).exec, Exec::Idle);
        // And when the inference stops holding, the episode resets: the
        // provider claims busy again and the stabilizer forgets the
        // episode rather than drifting idle.
        let mut in_ = inputs(&fold, Some((7, Some(0))), &mut idle);
        in_.published = Some(Published {
            status: Some(PublishedStatus::Busy),
            waiting_for: None,
            observed_ms: 201_000,
            since_ms: Some(50_000),
        });
        in_.now_ms = 201_000;
        assert_eq!(derive(in_).exec, Exec::Busy);
        let mut in_ = inputs(&fold, Some((7, Some(0))), &mut idle);
        in_.published = published();
        in_.now_ms = 202_000;
        assert_eq!(derive(in_).exec, Exec::Busy);
    }

    #[test]
    fn events_from_a_demoted_attachment_do_not_speak_for_the_replacement() {
        let mut idle = WeakIdle::default();
        // The journal's `start` named pid 7; the live attachment is pid 8 -
        // the old process's heartbeat cannot make the replacement busy.
        let mut fold = Fold::default();
        let mut r = crate::store::Record::new("claude", "s1", "SessionStart");
        r.event = Some(NormEvent::Start);
        r.seq = 1;
        r.at = 50_000;
        r.pid = Some(7);
        r.pid_start = Some(40);
        fold.apply(&r);
        let d = derive(inputs(&fold, Some((8, Some(90))), &mut idle));
        assert_eq!(d.exec, Exec::Unknown);
        // But the retained latch is conversation history, not process
        // state: an `end` from the old instance still asks to be seen.
        let mut r = crate::store::Record::new("claude", "s1", "Stop");
        r.event = Some(NormEvent::End);
        r.seq = 2;
        r.at = 55_000;
        r.pid = Some(7);
        fold.apply(&r);
        let d = derive(inputs(&fold, Some((8, Some(90))), &mut idle));
        assert_eq!(d.exec, Exec::Unknown);
        assert_eq!(d.attention, Attention::CompletedUnseen);
    }

    #[test]
    fn orderings_are_the_shared_contract() {
        // Per-pane latch precedence: error > done > waiting > working.
        assert!(Attention::Error.precedence() < Attention::CompletedUnseen.precedence());
        assert!(Attention::CompletedUnseen.precedence() < Attention::Waiting.precedence());
        assert!(Attention::Waiting.precedence() < Attention::Working.precedence());
        // Cross-pane rank: waiting > error > done > working.
        assert!(Attention::Waiting.rank() < Attention::Error.rank());
        assert!(Attention::Error.rank() < Attention::CompletedUnseen.rank());
        assert!(Attention::CompletedUnseen.rank() < Attention::Working.rank());
        // Rollup picks the best rank, precedence picks the latch.
        let v = [Attention::Working, Attention::Waiting, Attention::Error];
        assert_eq!(rollup(v.iter()), Attention::Waiting);
        assert_eq!(rollup([].iter()), Attention::None);
        // The ordered tail is stated too: every variant has a place in
        // both orderings and a word for the summary.
        assert_eq!(Attention::Unknown.rank(), 4);
        assert_eq!(Attention::None.rank(), 5);
        assert_eq!(Attention::Unknown.precedence(), 4);
        assert_eq!(Attention::None.precedence(), 5);
        assert_eq!(Attention::Unknown.label(), "unknown");
        assert_eq!(Attention::None.label(), "");
        assert_eq!(Attention::CompletedUnseen.label(), "done");
    }

    #[test]
    fn the_same_instance_check_reads_both_fields() {
        // Matching pid plus matching start: the same process.
        assert!(same_instance(Some(7), Some(100), (7, Some(100))));
        // Start off by more than a second: a reused pid, not the record's
        // process.
        assert!(!same_instance(Some(7), Some(100), (7, Some(200))));
        // One side that cannot prove the start wins by pid alone.
        assert!(same_instance(Some(7), None, (7, Some(100))));
        assert!(same_instance(Some(7), Some(100), (7, None)));
        // No pid at all in the record: nothing to disagree with.
        assert!(same_instance(None, Some(100), (7, Some(100))));
        // And a pid mismatch is a different process regardless.
        assert!(!same_instance(Some(8), Some(100), (7, None)));
    }

    #[test]
    fn unverifiable_seen_state_reads_unknown_not_guessed() {
        let mut idle = WeakIdle::default();
        let mut fold = Fold::default();
        let mut r = crate::store::Record::new("claude", "s1", "Stop");
        r.event = Some(NormEvent::End);
        r.seq = 1;
        r.at = 1_000;
        fold.apply(&r);
        // The latch exists but its acknowledgement cannot be established
        // (seen.json unreadable): `?`, never a guessed glyph.
        let mut in_ = inputs(&fold, Some((7, Some(90))), &mut idle);
        in_.ack_ok = false;
        let d = derive(in_);
        assert_eq!(d.attention, Attention::Unknown);
        assert_eq!(d.attention_detail, None);
    }

    #[test]
    fn retained_latches_carry_their_reasons() {
        let mut idle = WeakIdle::default();
        let mut fold = Fold::default();
        let mut r = crate::store::Record::new("claude", "s1", "PermissionRequest");
        r.event = Some(NormEvent::Awaiting);
        r.seq = 1;
        r.at = 50_000;
        r.reason = Some("permission prompt".to_owned());
        fold.apply(&r);
        let d = derive(inputs(&fold, Some((7, Some(0))), &mut idle));
        assert_eq!(d.attention, Attention::Waiting);
        assert_eq!(d.attention_detail.as_deref(), Some("permission prompt"));
    }
}
