//! The terminal dashboard: three stacked, cursor-scoped lists beside one
//! detail pane, over one immutable snapshot per refresh.
//!
//! [1] Repos and [2] Work scope whatever sits below them; [3] Conversations
//! is the attention inbox across the selected scope. The shell owns the
//! numbered panes, `Tab`, `j`/`k`, `/` text-and-age filters, `?` help and `q`,
//! and the Enter/`o` navigation actions - keys other tasks have not shipped
//! stay absent and inert.
//!
//! The render path reads only the snapshot: collectors do all subprocess and
//! file work behind it, so a bursty filesystem or a slow remote can never
//! stall a redraw. Every cell the evidence could not fill renders a dim `?`.

use std::io;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::action::{self, ActionOutcome, ActionRequest, ResumePlan, work_key};
use crate::attention::{Attention, ClaimOutcome};
use crate::config;
use crate::forge::{Pipeline, WorkItem};
use crate::snapshot::{
    AttachmentLiveness, AttachmentRow, ConversationRow, ConversationState, ConversationSummary,
    IncarnationRow, ReferenceKind, RelationStrength, RepoRow, Snapshot, Upstream, WorkKind,
    WorkRow, WorkSection, to_json,
};
use crate::store::{self, Store};
use crate::text::escape_text;
use crate::tmux::{self, PaneRef};

/// The four panes, in `Tab` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Repos,
    Work,
    Conversations,
    Detail,
}

impl Pane {
    fn next(self) -> Pane {
        match self {
            Pane::Repos => Pane::Work,
            Pane::Work => Pane::Conversations,
            Pane::Conversations => Pane::Detail,
            Pane::Detail => Pane::Repos,
        }
    }

    /// The list this pane drives, if it is one.
    fn list(self) -> Option<List> {
        match self {
            Pane::Repos => Some(List::Repos),
            Pane::Work => Some(List::Work),
            Pane::Conversations => Some(List::Conversations),
            Pane::Detail => None,
        }
    }
}

/// The three lists the cursor scoping runs through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum List {
    Repos,
    Work,
    Conversations,
}

/// What `/` typed into a list currently holds: the raw text and the parsed
/// text-plus-age pair the filter applies.
#[derive(Debug, Default, Clone)]
struct Filter {
    text: String,
    /// `age:<duration>` terms, parsed: retain only rows newer than this.
    age: Option<Duration>,
}

impl Filter {
    /// Parse `raw`: whitespace-separated words, where `age:<duration>` is a
    /// duration filter and everything else is case-insensitive text.
    fn parse(raw: &str) -> Filter {
        let mut text = Vec::new();
        let mut age = None;
        for word in raw.split_whitespace() {
            if let Some(dur) = word
                .strip_prefix("age:")
                .and_then(|d| config::parse_duration(d).ok())
            {
                age = Some(dur);
            } else {
                text.push(word);
            }
        }
        Filter {
            text: text.join(" ").to_lowercase(),
            age,
        }
    }

    /// Whether a row survives: its fields, space-joined, match the text, and
    /// its timestamp is newer than `age`. An unknown age fails closed -
    /// filtering out what cannot be proven is honest; guessing it young is
    /// not. `lower` is the caller's scratch buffer for the case-folded
    /// fields - one allocation per pass instead of one per row per frame.
    fn allows<'a>(
        &self,
        fields: impl IntoIterator<Item = &'a str>,
        timestamp: Option<u64>,
        now: u64,
        lower: &mut String,
    ) -> bool {
        if !self.text.is_empty() {
            lower.clear();
            for (i, field) in fields.into_iter().enumerate() {
                if i > 0 {
                    lower.push(' ');
                }
                lower.extend(field.chars().flat_map(char::to_lowercase));
            }
            if !lower.contains(&self.text) {
                return false;
            }
        }
        if let Some(age) = self.age {
            match timestamp {
                Some(t) if now.saturating_sub(t) <= age.as_secs() => {}
                _ => return false,
            }
        }
        true
    }
}

/// The application: the snapshot plus the only state the render path may
/// hold - cursor, focus, filters and overlays.
pub struct App {
    pub snapshot: Snapshot,
    focus: Pane,
    /// Cursor per list, indexing its *filtered* row vector.
    cursor: [usize; 3],
    /// Raw filter text per list; the parsed filter derives from it.
    filter_raw: [String; 3],
    /// `/` editing session: which list, and the in-progress buffer.
    editing: Option<(List, String)>,
    help: bool,
    /// `h`: whether [2] also lists the retained earlier same-name
    /// incarnations below their active rows, marked excluded.
    history: bool,
    /// `e`: the read-only evidence overlay over the detail pane.
    evidence: bool,
    /// The list the detail pane last followed: `4` focuses the pane to
    /// scroll it without losing the row it renders.
    detail_list: List,
    /// The detail/evidence pane's vertical scroll offset. `Cell` because
    /// a draw is `&self` and clamps the value against the current size.
    detail_scroll: std::cell::Cell<usize>,
    quit: bool,
    /// The collector thread died: the last snapshot stays on screen and the
    /// footer says so instead of letting the dashboard look live.
    collector_dead: bool,
    /// The store `space` writes acknowledgements and not-busy marks into
    /// and `p` toggles `parked` in; `None` where no state dir could be
    /// placed, making both inert.
    store: Option<Store>,
    /// The configured `forgotten_after` - what a `p` reclassification
    /// judges `Forgotten` against.
    forgotten_after: Duration,
    /// A failed action's message, shown in the footer in place of the hints
    /// until the next key - a keypress that wrote nothing must not look
    /// like it worked.
    notice: Option<String>,
    /// The Enter/`o` request a keypress left for the run loop to
    /// re-resolve and act on.
    action: Option<ActionRequest>,
    /// The spinner's frame index while a snapshot is still incomplete.
    /// `Cell` because a draw is `&self`: the animation ticks by rendering.
    spin: std::cell::Cell<u64>,
}

/// The row shapes one list can hold - views over the snapshot, never copies
/// of it.
enum Row<'a> {
    Repo(&'a RepoRow),
    Work(&'a WorkRow),
    /// An excluded earlier same-name incarnation, injected by `h` after
    /// its active row: selectable so [3] scopes to exactly it, inert for
    /// `space` and `p`.
    History(&'a WorkRow, &'a IncarnationRow),
    Conversation(&'a ConversationRow),
}

/// The cells one row renders: attention glyph, label, the right-aligned
/// middle field and the right-edge age. `selected` inverts, `dim_label` is
/// for an unknown title.
struct RowCells<'a> {
    glyph: &'a str,
    label: &'a str,
    middle: &'a str,
    age: &'a str,
    selected: bool,
    dim_label: bool,
}

impl App {
    /// An app on a snapshot; focus opens on [1], the topmost list, and the
    /// detail pane follows it.
    pub fn new(snapshot: Snapshot) -> App {
        App {
            snapshot,
            focus: Pane::Repos,
            cursor: [0, 0, 0],
            filter_raw: [String::new(), String::new(), String::new()],
            editing: None,
            help: false,
            history: false,
            evidence: false,
            detail_list: List::Repos,
            detail_scroll: std::cell::Cell::new(0),
            quit: false,
            collector_dead: false,
            store: None,
            forgotten_after: config::DEFAULT_FORGOTTEN_AFTER,
            notice: None,
            action: None,
            spin: std::cell::Cell::new(0),
        }
    }

    /// The store `space` and `p` write to: `acknowledge`, the not-busy
    /// mark and the parked flag land here.
    pub fn with_store(mut self, store: Store) -> App {
        self.store = Some(store);
        self
    }

    /// The `forgotten_after` the collector classified with, so `p`'s
    /// in-place reclassification judges the same threshold.
    pub fn with_forgotten_after(mut self, forgotten_after: Duration) -> App {
        self.forgotten_after = forgotten_after;
        self
    }

    /// Swap in a fresh snapshot. Rows are re-sorted on every collect, so a
    /// kept cursor index would silently select a different record; each
    /// cursor instead tracks its row's identity and falls back to `all`
    /// when that record vanished - widening scope rather than retargeting
    /// it to a different row.
    pub fn refresh(&mut self, snapshot: Snapshot) {
        let keys = self.selection_keys();
        self.snapshot = snapshot;
        self.reseat(keys.clone());
        // A selection that survived the swap keeps its scroll; a moved
        // one re-anchors the detail at the top.
        if self.selection_keys() != keys {
            self.detail_scroll.set(0);
        }
    }

    /// Each list cursor's selected row key, `None` on `all`.
    fn selection_keys(&self) -> [Option<String>; 3] {
        let before = self.view();
        [List::Repos, List::Work, List::Conversations].map(|list| {
            let cursor = self.cursor[list_index(list)];
            if cursor == 0 {
                None
            } else {
                before.rows(list).get(cursor - 1).map(selection_key)
            }
        })
    }

    /// Point every cursor back at the row its key still names. Resolve
    /// parent to child: a list's rows are scoped by the cursor above it,
    /// so each cursor update rebuilds the view before the next selection
    /// is searched - otherwise a re-sorted repo row would look up work
    /// and conversation keys under the stale parent scope.
    fn reseat(&mut self, keys: [Option<String>; 3]) {
        for (list, key) in [List::Repos, List::Work, List::Conversations]
            .into_iter()
            .zip(keys)
        {
            let position = self
                .view()
                .rows(list)
                .iter()
                .position(|row| key.as_ref().is_some_and(|key| selection_key(row) == *key));
            self.cursor[list_index(list)] = position.map_or(0, |p| p + 1);
        }
    }

    /// Whether the run loop should exit.
    pub fn quit(&self) -> bool {
        self.quit
    }

    /// Mark the collector dead: no more snapshots will arrive, and the
    /// footer says so rather than letting stale rows look live.
    fn collector_stopped(&mut self) {
        self.collector_dead = true;
    }

    /// Which list is focused.
    fn focused_list(&self) -> Option<List> {
        self.focus.list()
    }

    /// The snapshot time as epoch seconds.
    fn now(&self) -> u64 {
        self.snapshot.observed_at
    }

    /// The focused list's parsed filter.
    fn filter(&self, list: List) -> Filter {
        Filter::parse(&self.filter_raw[list_index(list)])
    }

    /// The view: the three lists' filtered rows plus the scopes the cursors
    /// select, built once so panels, titles and the detail header all agree
    /// on the same row sets. `Repos` rows are the snapshot's repos; `Work`
    /// is scoped by the repo cursor (index 0 is `all`), and `Conversations`
    /// by the work cursor on top of that. Each list's own `all` row is
    /// always first and always visible - a filter narrows records, never
    /// the aggregate.
    fn view(&self) -> View<'_> {
        let now = self.now();
        let mut lower = String::new();
        let repos = self
            .snapshot
            .repos
            .iter()
            .filter(|r| {
                self.filter(List::Repos).allows(
                    [r.name.as_str(), r.id.as_str()],
                    r.last_activity,
                    now,
                    &mut lower,
                )
            })
            .map(Row::Repo)
            .collect::<Vec<_>>();
        let repo_scope = match self.cursor[list_index(List::Repos)] /* // coverage: off - the get-miss arm is unreachable: cursors clamp before a view */ {
            0 => None,
            cursor => repos.get(cursor - 1).map(|row| match row { // coverage: off - same
                Row::Repo(r) => r.id.clone(),
                _ => String::new(), // coverage: off - repos holds Repo rows only
            }),
        };
        let work_rows = self
            .snapshot
            .work
            .iter()
            .filter(|w| repo_scope.as_deref().is_none_or(|s| w.repo == *s))
            .filter(|w| {
                self.filter(List::Work).allows(
                    [w.repo_name.as_str(), w.name.as_str(), w.summary.as_str()],
                    w.last_activity,
                    now,
                    &mut lower,
                )
            })
            .collect::<Vec<_>>();
        // The `all` row's counts cover the whole scope, collapsed or not -
        // over the active rows only: `h`'s injected history never changes
        // the lifecycle counts.
        let work_open = work_rows.iter().filter(|w| w.section.open()).count();
        let work_clean = work_rows.len() - work_open;
        // Under `all`, the cleanup sections collapse into one summary line;
        // under a repo they list their rows like any section. History rows
        // inject directly after their active row when `h` is on.
        let mut work = Vec::new();
        let mut safe = 0usize;
        let mut review = 0usize;
        let collapse_cleanup = repo_scope.is_none();
        for w in work_rows {
            if collapse_cleanup {
                match w.section {
                    WorkSection::ReadyToClean => {
                        safe += 1;
                        continue;
                    }
                    WorkSection::CleanupReview => {
                        review += 1;
                        continue;
                    }
                    _ => {}
                }
            }
            work.push(Row::Work(w));
            if self.history {
                for h in &w.same_name_history {
                    work.push(Row::History(w, h));
                }
            }
        }
        let cleanup = (collapse_cleanup && safe + review > 0).then_some((safe, review));
        let work_scope = match self.cursor[list_index(List::Work)] /* // coverage: off - the get-miss arm is unreachable: cursors clamp before a view */ {
            0 => None, // coverage: off - the unreachable arm's match edge lands here
            cursor => work // coverage: off - same
                .get(cursor - 1)
                .map(|row| match row { // coverage: off - same
                Row::Work(w) => scope_of(w),
                Row::History(_, h) => WorkScope::Incarnation {
                    id: h.id.clone(),
                    label: format!("{}#{}", h.ref_name, h.number),
                    excluded: true,
                },
                _ /* // coverage: off - work holds Work rows only */ => WorkScope::Space {
                    id: String::new(),                    // coverage: off - same
                    path: Path::new("").to_path_buf(),    // coverage: off - same
                },
            }),
        };
        let conversations = self
            .snapshot
            .conversations
            .iter()
            .filter(|c| {
                if let Some(work) = &work_scope {
                    // A conversation under one work row matches on worktree
                    // path for a detached row, on touches to exactly that
                    // incarnation for a branch-bearing one - active or
                    // excluded history - or on the space's canonical id.
                    match work {
                        WorkScope::Worktree { repo, root } => {
                            c.repo.as_deref() == Some(repo.as_str())
                                && c.worktree.as_deref() == Some(root.as_path())
                        }
                        WorkScope::Incarnation { id, .. } => {
                            c.touches.iter().any(|t| t.incarnation_id == *id)
                        }
                        WorkScope::Space { id, .. } => c.repo.as_deref() == Some(id.as_str()),
                    }
                } else if let Some(repo) = &repo_scope {
                    c.repo.as_deref() == Some(repo.as_str())
                } else {
                    true
                }
            })
            .filter(|c| {
                self.filter(List::Conversations).allows(
                    [
                        c.short_id.as_str(),
                        c.title.as_deref().unwrap_or(""),
                        c.provider.as_str(),
                    ],
                    c.state_since,
                    now,
                    &mut lower,
                )
            })
            .map(Row::Conversation)
            .collect();
        View {
            repos,
            work,
            conversations,
            repo_scope,
            work_scope,
            work_open,
            work_clean,
            cleanup,
        }
    }

    /// One key press.
    ///
    /// The handled set is exactly the shipped one: `1`-`4` focus, `Tab`
    /// cycles, `j`/`k` move the cursor, `/` filters the focused list,
    /// `space` acknowledges attention or marks a Busy row not-busy, `p`
    /// parks a Work row (suppressing only its `Forgotten` placement),
    /// `enter` selects a conversation pane or a bound work window - or
    /// resumes a stopped conversation - and `o` opens a work row's
    /// forge item; both leave a request the run loop resolves. `?`
    /// toggles help, `q` quits, `Esc` closes help or a filter. Everything
    /// else is inert: an unbound key does nothing, and nothing here pretends
    /// to a behaviour a later task owns.
    pub fn key(&mut self, key: Key) {
        self.notice = None;
        if let Some((list, buffer)) = &mut self.editing {
            match key {
                Key::Char(c) => buffer.push(c),
                Key::Backspace => {
                    buffer.pop();
                }
                Key::Enter | Key::Esc => {
                    if key == Key::Enter {
                        self.filter_raw[list_index(*list)] = buffer.clone();
                    }
                    self.editing = None;
                }
                _ => {}
            }
            return;
        }
        if self.help {
            match key {
                Key::Esc | Key::Char('?') | Key::Char('q') => self.help = false,
                _ => {}
            }
            return;
        }
        match key {
            Key::Char('q') => self.quit = true,
            Key::Char('?') => self.help = true,
            Key::Char('1') => self.set_focus(Pane::Repos),
            Key::Char('2') => self.set_focus(Pane::Work),
            Key::Char('3') => self.set_focus(Pane::Conversations),
            Key::Char('4') => self.set_focus(Pane::Detail),
            Key::Tab => self.set_focus(self.focus.next()),
            Key::Char('j') | Key::Down => self.move_cursor(1),
            Key::Char('k') | Key::Up => self.move_cursor(-1),
            Key::Char('/') => {
                if let Some(list) = self.focused_list() {
                    self.editing = Some((list, self.filter_raw[list_index(list)].clone()));
                }
            }
            // The read-only evidence overlay: `e` opens and closes it,
            // `Esc` only closes.
            Key::Char('e') => {
                self.evidence = !self.evidence;
                self.detail_scroll.set(0);
            }
            Key::Esc => {
                if self.evidence {
                    self.evidence = false;
                    self.detail_scroll.set(0);
                }
            }
            Key::Char(' ') => self.space(),
            Key::Char('p') => self.park(),
            Key::Char('h') => self.toggle_history(),
            Key::Enter => self.enter(),
            Key::Char('o') => self.open_forge(),
            _ => {}
        }
    }

    /// `enter`: the focused row's navigation request, left for the run
    /// loop. Only concrete rows carry one - a conversation selects its
    /// pane or resumes, an active work row selects its bound window.
    /// `all`, repo, excluded history and gone rows, and the detail
    /// pane, are inert.
    fn enter(&mut self) {
        let Some(list) = self.focused_list() else {
            return;
        };
        let cursor = self.cursor[list_index(list)];
        if cursor == 0 {
            return;
        }
        let view = self.view();
        let Some(row) = view.rows(list).get(cursor - 1) else {
            return; // coverage: off - the get-miss arm is unreachable: cursors clamp before a view
        };
        self.action = match row {
            Row::Conversation(c) => Some(ActionRequest::EnterConversation {
                provider: c.provider,
                session_id: c.session_id.clone(),
            }),
            Row::Work(w) if w.gone.is_none() => Some(ActionRequest::EnterWork { key: work_key(w) }),
            // `all`, repos, excluded history and gone rows carry no action.
            _ => None,
        };
    }

    /// `o`: the open-forge request - only an active concrete work row
    /// carries one, along with its recorded verdict and URL; everything
    /// else is inert.
    fn open_forge(&mut self) {
        if self.focused_list() != Some(List::Work) {
            return;
        }
        let cursor = self.cursor[list_index(List::Work)];
        if cursor == 0 {
            return;
        }
        let view = self.view();
        let Some(Row::Work(w)) = view.work.get(cursor - 1) else {
            return;
        };
        if w.gone.is_some() {
            return;
        }
        self.action = Some(ActionRequest::OpenForge {
            key: work_key(w),
            item: w.forge,
            url: w.forge_url.clone(),
        });
    }

    /// The pending Enter/`o` request, taken once: the loop re-resolves
    /// it against a fresh snapshot rather than trusting the frame the
    /// user saw.
    pub fn take_action(&mut self) -> Option<ActionRequest> {
        self.action.take()
    }

    /// What the last action left to say - a failure's reason or a
    /// success's word - shown in the footer until the next key.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    /// `space` on the focused row - the write goes to the journal's
    /// authored files, then the next collect reflects it.
    ///
    /// On a row carrying unacknowledged attention, `space` writes
    /// seen-state through the newest unacknowledged event and the live
    /// wait it shows: a deliberate acknowledgement, the same one a focus
    /// observation writes. On a row that is `Busy` with none of that, `space` is the
    /// authored not-busy mark instead - it names the dismissed `Busy`'s
    /// `effective_since` and is superseded by any newer event or
    /// observation. A `repos` row does neither. A work row decides once
    /// for every bound conversation: one pending latch makes the keypress
    /// acknowledgements only. A write the store refuses is reported in the
    /// footer.
    fn space(&mut self) {
        self.notice = self
            .space_writes()
            .map(|e| format!("space: not saved - {e}"));
    }

    /// The writes `space` performs; the first refusal, when there is one.
    fn space_writes(&self) -> Option<std::io::Error> {
        let store = self.store.as_ref()?;
        let list = self.focused_list()?;
        let view = self.view();
        let cursor = self.cursor[list_index(list)];
        if cursor == 0 {
            return None;
        };
        let Some(row) = view.rows(list).get(cursor - 1) else {
            return None; // coverage: off - the get-miss arm is unreachable: cursors clamp before a view
        };
        let convs: Vec<&ConversationRow> = match row {
            Row::Conversation(c) => vec![*c],
            Row::Work(w) => self
                .snapshot
                .conversations
                .iter()
                .filter(|c| crate::snapshot::binds(w, c))
                .collect::<Vec<_>>(),
            // Excluded history is read-only: `space` acknowledges and
            // marks nothing on it.
            Row::Repo(_) | Row::History(..) => Vec::new(),
        };
        // A work row decides once for all its bound conversations: a single
        // pending latch turns the whole keypress into acknowledgements -
        // a not-busy mark written beside unacknowledged attention would
        // contradict it. The two authored kinds are exclusive - a work row
        // by that rule, a conversation row by producing one action - so
        // the keypress runs at most one acknowledgement batch and at most
        // one mark batch, each a single store transaction.
        let pending =
            |c: &ConversationRow| c.attention_seq.is_some() || c.attention_wait_ms.is_some();
        let ack_only = matches!(row, Row::Work(_)) && convs.iter().any(|c| pending(c));
        let mut acks: Vec<(String, u64, Option<u64>)> = Vec::new();
        let mut marks: Vec<(String, u64, u64)> = Vec::new();
        for c in convs {
            let key = store::conversation_key(c.provider.as_str(), &c.session_id);
            if pending(c) {
                acks.push((key, c.attention_seq.unwrap_or(0), c.attention_wait_ms));
            } else if !ack_only
                && c.attention == Attention::Working
                && let Some(since_ms) = c.state_since_ms
            {
                // Only a Busy row earns the mark: a busy conversation with
                // no higher attention has nothing to acknowledge. A Busy
                // only the provider published has no journal sequence yet;
                // the mark sits at zero and any first event supersedes it.
                marks.push((key, since_ms, c.journal_seq.unwrap_or(0)));
            }
        }
        let acks: Vec<(&str, u64, Option<u64>)> = acks
            .iter()
            .map(|(key, seq, wait)| (key.as_str(), *seq, *wait))
            .collect();
        let marks: Vec<(&str, u64, u64)> = marks
            .iter()
            .map(|(key, since, seq)| (key.as_str(), *since, *seq))
            .collect();
        if let Err(e) = store.acknowledge_many(&acks) {
            return Some(e);
        }
        store.mark_not_busy_many(&marks).err()
    }

    /// `h`: toggle the excluded-history rows. The row vector changes, so
    /// the cursors reseat by identity like a refresh: a selection on a
    /// hidden history row falls back to `all`, an active one stays put.
    fn toggle_history(&mut self) {
        let keys = self.selection_keys();
        self.history = !self.history;
        self.reseat(keys.clone());
        if self.selection_keys() != keys {
            self.detail_scroll.set(0);
        }
    }

    /// Move focus, remembering the list the detail pane follows - the
    /// detail pane itself is not a list, so `4` keeps the last one.
    fn set_focus(&mut self, pane: Pane) {
        self.focus = pane;
        if let Some(list) = pane.list() {
            self.detail_list = list;
        }
    }

    /// `p` on a concrete Work row: flip the authored `parked` on its exact
    /// work identity - the branch incarnation's id, or the canonical path
    /// for a detached worktree or project space - then reclassify the row
    /// in place so it leaves or enters `Forgotten` without waiting for the
    /// next collect. Every other row and list is inert; a write the store
    /// refuses surfaces as `park: not saved: ...`.
    fn park(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        if self.focused_list() != Some(List::Work) {
            return;
        }
        let cursor = self.cursor[list_index(List::Work)];
        if cursor == 0 {
            return;
        }
        let view = self.view();
        let Some(Row::Work(w)) = view.work.get(cursor - 1) else {
            return; // coverage: off - the get-miss arm is unreachable: cursors clamp before a view
        };
        // A gone row is read-only: its record closed with the work it
        // names, and `parked` has no live record to land on.
        if w.gone.is_some() {
            return;
        }
        let Some(id) = w.identity.clone() else {
            return;
        };
        let identity = match w.branch {
            Some(_) => store::WorkIdentity::Branch(id),
            None => store::WorkIdentity::Path(id),
        };
        let selected = work_key(w);
        let repo_id = w.repo.clone();
        let keys = self.selection_keys();
        match store.toggle_parked(&identity) {
            Ok(parked) => {
                if let Some(row) = self
                    .snapshot
                    .work
                    .iter_mut()
                    .find(|r| work_key(r) == selected)
                {
                    row.parked = parked;
                    crate::snapshot::classify_work(
                        row,
                        &self.snapshot.conversations,
                        self.forgotten_after,
                        self.snapshot.observed_at,
                    );
                } // coverage: off - the row the cursor names is in this list
                crate::snapshot::sort_work(&mut self.snapshot.work);
                if let Some(repo) = self.snapshot.repos.iter_mut().find(|r| r.id == repo_id) {
                    repo.roll_up(&self.snapshot.work);
                } // coverage: off - the row's repo is one of the snapshot's repos
                self.reseat(keys);
            }
            Err(e) => self.notice = Some(format!("park: not saved: {e}")),
        }
    }

    /// `j`/`k` on the focused list: move, clamp, and reset the cursors below
    /// when the scope itself changed - the scoped list's cursor has no
    /// meaning carried over from the previous scope. On the detail pane
    /// they scroll its body instead; the render clamps the offset.
    fn move_cursor(&mut self, delta: i64) {
        let Some(list) = self.focused_list() else {
            self.detail_scroll.set(
                self.detail_scroll
                    .get()
                    .saturating_add_signed(delta as isize),
            );
            return;
        };
        let rows = self.view().rows(list).len();
        let cursor = &mut self.cursor[list_index(list)];
        // `all` plus rows: cursor range is 0..=rows.
        let next = (*cursor as i64 + delta).clamp(0, rows as i64) as usize;
        if next != *cursor {
            // A new selection means new detail content: re-anchor at top.
            self.detail_scroll.set(0);
        }
        *cursor = next;
        match list {
            List::Repos => {
                self.cursor[list_index(List::Work)] = 0;
                self.cursor[list_index(List::Conversations)] = 0;
            }
            List::Work => self.cursor[list_index(List::Conversations)] = 0,
            List::Conversations => {}
        }
    }

    /// Draw the whole frame into `f`.
    pub fn render(&self, f: &mut Frame<'_>) {
        let area = f.area();
        let [main, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(40), Constraint::Min(0)]).areas(main);
        // One view per frame: scoping, titles and the detail pane all read
        // the same rows rather than recomputing them per panel.
        let view = self.view();
        let [repos, work, conversations] = Layout::vertical([
            Constraint::Length(list_height(view.repos.len(), left.height)),
            Constraint::Percentage(55),
            Constraint::Min(0),
        ])
        .areas(left);

        self.list_panel(f, repos, Pane::Repos, &view, &view.repos);
        self.list_panel(f, work, Pane::Work, &view, &view.work);
        self.list_panel(
            f,
            conversations,
            Pane::Conversations,
            &view,
            &view.conversations,
        );
        self.detail_panel(f, right, &view);
        self.footer(f, footer);
        if self.help {
            self.help_overlay(f, area);
        }
    }

    /// One list panel: title, `all` row, then the filtered rows.
    fn list_panel(
        &self,
        f: &mut Frame<'_>,
        area: Rect,
        pane: Pane,
        view: &View<'_>,
        rows: &[Row<'_>],
    ) {
        let list = pane.list().expect("a list pane"); // coverage: off - only list panes reach here
        let block = Block::default()
            .title(self.panel_title(pane, view))
            .borders(Borders::ALL)
            .border_style(if self.focus == pane {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            });
        let inner = block.inner(area);
        f.render_widget(block, area);

        // Follow the cursor: when the list is taller than its pane, scroll so
        // the selected line stays visible. Section headers are part of the
        // line stream - they scroll with the rows they head (the [2] Work
        // list is the only one with sections yet).
        let cursor = self.cursor[list_index(list)];
        let visible = inner.height as usize;
        let mut display: Vec<Line<'_>> = Vec::new();
        display.push(self.all_row(list, rows, view, inner.width, cursor == 0));
        let mut cursor_line = 0usize;
        let mut last_section = None;
        for (i, row) in rows.iter().enumerate() {
            if let Row::Work(w) = row
                && last_section != Some(w.section)
            {
                display.push(section_header(w.section));
                last_section = Some(w.section);
            }
            display.push(self.row(list, row, view, inner.width, cursor == i + 1));
            // The global work scope gives each conversation a dim context
            // line - part of the same cursor item, never a row of its
            // own; it disappears once a scope is selected.
            if let Row::Conversation(c) = row
                && list == List::Conversations
                && view.work_scope.is_none()
                && let Some(context) = conversation_context(c, view.repo_scope.as_deref())
            {
                display.push(Line::from(Span::styled(
                    fit(&format!("    {context}"), inner.width as usize),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            // Scroll is computed against the selected item's FINAL
            // display line - its optional context line included - so a
            // two-line item at the list's foot never clips.
            if cursor == i + 1 {
                cursor_line = display.len() - 1;
            }
        }
        // The `all`-scoped Work list ends on one collapsed cleanup line
        // instead of listing the Ready to clean / Cleanup review rows.
        if list == List::Work
            && let Some((safe, review)) = view.cleanup
        {
            display.push(Line::from(Span::styled(
                fit(
                    &format!("Cleanup {safe} safe · {review} review"),
                    inner.width as usize,
                ),
                Style::default().fg(Color::DarkGray),
            )));
        }
        let scroll = cursor_line.saturating_sub(visible.saturating_sub(1));
        let lines: Vec<Line<'_>> = display.into_iter().skip(scroll).take(visible).collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// The `all` row of a list.
    fn all_row(
        &self,
        list: List,
        rows: &[Row<'_>],
        view: &View<'_>,
        width: u16,
        selected: bool,
    ) -> Line<'static> {
        // Only [1]'s `all` row carries a glyph: the attention rolled up
        // over every repo in view.
        let mut glyph = "";
        let counts = match list {
            List::Repos => {
                let repos: Vec<&RepoRow> = rows
                    .iter()
                    .filter_map(|r| match r {
                        Row::Repo(r) => Some(*r),
                        _ => None, // coverage: off - repos holds Repo rows only
                    })
                    .collect();
                glyph = crate::attention::rollup(repos.iter().map(|r| &r.attention)).glyph();
                let open: usize = repos.iter().map(|r| r.open).sum();
                let clean: usize = repos.iter().map(|r| r.clean).sum();
                format!("{open} open · {clean} clean")
            }
            List::Work => format!("{} open · {} clean", view.work_open, view.work_clean),
            List::Conversations => {
                let live = rows
                    .iter()
                    .filter(|r| matches!(r, Row::Conversation(c) if c.running()))
                    .count();
                format!("{live} live · {} shown", rows.len())
            }
        };
        self.render_row(
            width,
            &RowCells {
                glyph,
                label: "all",
                middle: &counts,
                age: "",
                selected,
                dim_label: false,
            },
        )
    }

    /// One data row, formatted to `width`.
    fn row(
        &self,
        list: List,
        row: &Row<'_>,
        view: &View<'_>,
        width: u16,
        selected: bool,
    ) -> Line<'static> {
        let cells = match row {
            Row::Repo(r) => RowCells {
                glyph: repo_glyph(r),
                label: &r.name,
                middle: &repo_counts(r),
                age: &age(self.now(), r.last_activity),
                selected,
                dim_label: false,
            },
            Row::Work(w) => RowCells {
                glyph: work_glyph(w),
                // Under `all` the label carries the repo; scoped, it is
                // the branch alone.
                label: &work_name(w, list == List::Work && view.repo_scope.is_none()),
                middle: &w.summary,
                age: &age(self.now(), w.last_activity),
                selected,
                dim_label: false,
            },
            Row::History(w, h) => RowCells {
                // Excluded history: dim, no attention glyph, marked
                // `excluded`, aged by when the incarnation ended.
                glyph: "",
                label: &work_label(
                    w,
                    list == List::Work && view.repo_scope.is_none(),
                    Some(h.number),
                ),
                middle: "excluded",
                age: &age(self.now(), h.ended_at),
                selected,
                dim_label: true,
            },
            Row::Conversation(c) => RowCells {
                glyph: conversation_glyph(c),
                label: &format!("{} {}", c.short_id, c.title.as_deref().unwrap_or("?")),
                middle: &conversation_middle(c),
                age: &age(self.now(), c.state_since),
                selected,
                dim_label: c.title.is_none(),
            },
        };
        self.render_row(width, &cells)
    }

    /// A row assembled from fixed cells: glyph, label, right-aligned middle
    /// field and right-edge age. Width tiers, not clipping: a narrow pane
    /// gets glyph and label, a medium one adds the age and the compact
    /// summary. Nothing scrolls horizontally; the label is what shrinks.
    fn render_row(&self, width: u16, cells: &RowCells<'_>) -> Line<'static> {
        let width = width as usize;
        let glyph_w = glyph_width(cells.glyph);
        let base = if cells.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let label_style = if cells.dim_label {
            base.fg(Color::DarkGray)
        } else {
            base
        };
        let dim = base.fg(Color::DarkGray);
        // narrow: glyph + label. medium adds the age and the compact
        // summary - which is capped so it never starves the label below a
        // readable minimum.
        let show_fields = width >= 24;
        let age = if show_fields { cells.age } else { "" };
        let age_w = cell_width(age);
        let middle = if show_fields {
            fit(cells.middle, width.saturating_sub(glyph_w + age_w + 12))
        } else {
            String::new()
        };
        let middle_w = cell_width(&middle);
        let label_w = width.saturating_sub(glyph_w + middle_w + age_w + 4);
        let label = fit(cells.label, label_w);
        let pad = width
            .saturating_sub(glyph_w + cell_width(&label) + middle_w + age_w + 2)
            .max(1);
        Line::from(vec![
            Span::styled(
                format!("{:>glyph_w$} ", cells.glyph),
                glyph_style(cells.glyph).patch(base),
            ),
            Span::styled(label, label_style),
            Span::styled(" ".repeat(pad), base),
            Span::styled(middle.to_owned(), dim),
            Span::styled(" ".to_owned(), base),
            Span::styled(age.to_owned(), dim),
        ])
    }

    /// The detail pane: a pinned `glyph target - what · state age`
    /// header, then the wrapped body - every field read from the
    /// snapshot, every unknown an honest `?`. `e` swaps the body for the
    /// evidence view. `j`/`k` scroll the body while the pane is focused;
    /// the stored offset clamps against the content's current length.
    fn detail_panel(&self, f: &mut Frame<'_>, area: Rect, view: &View<'_>) {
        let (title, header) = self.detail_header(view);
        let title = if self.evidence {
            match self.detail_target(view) {
                Some((List::Repos, Row::Repo(r))) => format!("[4] Evidence - {}", r.name),
                Some((List::Work, Row::Work(w))) => {
                    format!("[4] Evidence - {}", numbered_name(w))
                }
                Some((List::Work, Row::History(_, h))) => {
                    format!("[4] Evidence - {}#{}", h.ref_name, h.number)
                }
                Some((List::Conversations, Row::Conversation(c))) => {
                    format!("[4] Evidence - {}", c.short_id)
                }
                _ => "[4] Evidence".to_owned(),
            }
        } else {
            title
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(if self.focus == Pane::Detail {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            });
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [head, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        f.render_widget(Paragraph::new(header), head);
        let width = body.width.max(1) as usize;
        let lines = if self.evidence {
            self.evidence_lines(view, width)
        } else {
            self.detail_lines(view, width)
        };
        let max_scroll = lines.len().saturating_sub(body.height as usize);
        let scroll = self.detail_scroll.get().min(max_scroll);
        self.detail_scroll.set(scroll);
        let visible: Vec<Line> = lines
            .into_iter()
            .skip(scroll)
            .take(body.height as usize)
            .collect();
        f.render_widget(Paragraph::new(visible), body);
    }

    /// The row the detail pane renders: the cursor of the list the pane
    /// last followed - focusing `[4]` to scroll never loses the target.
    fn detail_target<'a>(&self, view: &'a View<'a>) -> Option<(List, &'a Row<'a>)> {
        let list = self.detail_list;
        let cursor = self.cursor[list_index(list)];
        if cursor == 0 {
            return None;
        }
        view.rows(list).get(cursor - 1).map(|row| (list, row))
    }

    /// `[4] <what>` plus the `glyph target - what · state age` header,
    /// taken from the row the detail pane follows.
    fn detail_header(&self, view: &View<'_>) -> (String, Line<'static>) {
        match self.detail_target(view) {
            Some((List::Repos, Row::Repo(r))) => (
                format!("[4] Repo - {}", r.name),
                Line::from(format!(
                    "{} {} - repo · {}",
                    repo_glyph(r),
                    r.name,
                    age(self.now(), r.last_activity)
                )),
            ),
            Some((List::Work, Row::Work(w))) => (
                format!("[4] Work - {}", numbered_name(w)),
                Line::from(format!(
                    "{} {} - {} · {}",
                    work_glyph(w),
                    numbered_name(w),
                    w.kind.as_str().replace('_', " "),
                    age(self.now(), w.last_activity)
                )),
            ),
            Some((List::Work, Row::History(_, h))) => (
                format!("[4] Work - {}#{}", h.ref_name, h.number),
                Line::from(format!(
                    "  {}#{} - incarnation · excluded · observed {} · ended {}",
                    h.ref_name,
                    h.number,
                    age(self.now(), Some(h.first_observed_at)),
                    age(self.now(), h.ended_at)
                )),
            ),
            Some((List::Conversations, Row::Conversation(c))) => (
                format!("[4] Conversation - {}", c.title.as_deref().unwrap_or("?")),
                Line::from(vec![
                    Span::styled(
                        format!("{} ", conversation_glyph(c)),
                        glyph_style(conversation_glyph(c)),
                    ),
                    Span::raw(format!(
                        "{} {} - {}, {}",
                        c.short_id,
                        c.title.as_deref().unwrap_or("?"),
                        c.provider.as_str(),
                        detail_state(c)
                    )),
                ]),
            ),
            _ => (
                "[4] Detail".to_owned(),
                Line::from(Span::styled("all", Style::default().fg(Color::DarkGray))),
            ),
        }
    }

    /// The detail body for the selected row, already wrapped to the
    /// panel's width: lines the renderer never has to clip.
    fn detail_lines(&self, view: &View<'_>, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        match self.detail_target(view) {
            Some((List::Repos, Row::Repo(r))) => self.repo_body(r, &mut out, width),
            Some((List::Work, Row::Work(w))) => self.work_body(w, &mut out, width),
            Some((List::Work, Row::History(_, h))) => {
                incarnation_body(h, self.now(), &mut out, width)
            }
            Some((List::Conversations, Row::Conversation(c))) => {
                self.conversation_body(c, &mut out, width)
            }
            _ => push_text(&mut out, "everything in scope".to_owned(), width),
        }
        out
    }

    /// The evidence body for the selected row: a conversation's claims,
    /// latches, marks and rejected records, or an incarnation's
    /// continuity evidence - then the collector's errors.
    fn evidence_lines(&self, view: &View<'_>, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        match self.detail_target(view) {
            Some((List::Conversations, Row::Conversation(c))) => {
                self.conversation_evidence(c, &mut out, width)
            }
            Some((List::Work, Row::Work(w))) => match &w.incarnation {
                Some(i) => incarnation_body(i, self.now(), &mut out, width),
                None => push_text(&mut out, "no incarnation evidence".to_owned(), width),
            },
            Some((List::Work, Row::History(_, h))) => {
                incarnation_body(h, self.now(), &mut out, width)
            }
            _ => {}
        }
        let s = &self.snapshot;
        push_head(
            &mut out,
            format!(
                "collector: {} errors · {} stale sockets",
                s.errors.len(),
                s.stale_sockets
            ),
            width,
        );
        for e in &s.errors {
            push_text(&mut out, format!("  {}: {}", e.source, e.detail), width);
        }
        out
    }

    /// A Repo row's fields: path, proven default branch or `?`, remote,
    /// the section counts and the live conversation count.
    fn repo_body(&self, r: &RepoRow, out: &mut Vec<Line<'static>>, width: usize) {
        push_text(out, format!("path: {}", r.path.display()), width);
        push_text(
            out,
            format!("default branch: {}", opt(&r.default_branch)),
            width,
        );
        push_text(out, format!("remote: {}", opt(&r.remote)), width);
        push_text(out, format!("work rows: {}", r.work), width);
        push_text(out, format!("live conversations: {}", r.live), width);
        let c = &r.counts;
        push_text(
            out,
            format!(
                "needs you {} · active {} · follow up {}",
                c.needs_you, c.active, c.follow_up
            ),
            width,
        );
        push_text(
            out,
            format!(
                "forgotten {} · ready to clean {} · review {}",
                c.forgotten, c.ready_to_clean, c.cleanup_review
            ),
            width,
        );
    }

    /// A Work row's fields - the incarnation's evidence, worktree, the
    /// delivery readings against the proven base, upstream, forge, tmux,
    /// activity, same-name history, and both independent cleanup
    /// verdicts with their blockers. A gone row leads with what vanished
    /// and everything still referencing it.
    fn work_body(&self, w: &WorkRow, out: &mut Vec<Line<'static>>, width: usize) {
        let now = self.now();
        push_text(out, format!("repo: {}", w.repo_name), width);
        if let Some(i) = &w.incarnation {
            push_head(out, "incarnation:".to_owned(), width);
            push_text(
                out,
                format!("  {}#{} · id {}", i.ref_name, i.number, i.id),
                width,
            );
            let ended = i
                .ended_at
                .map_or_else(|| "open".to_owned(), |_| age(now, i.ended_at));
            push_text(
                out,
                format!(
                    "  observed {} - {} · ended {}",
                    age(now, Some(i.first_observed_at)),
                    age(now, Some(i.last_observed_at)),
                    ended
                ),
                width,
            );
            push_text(
                out,
                format!(
                    "  ref creation {} · tip {} · created {}",
                    sha(&i.creation_head),
                    sha(&i.head),
                    age(now, i.creation_at)
                ),
                width,
            );
            push_text(
                out,
                format!("  continuity: {}", i.continuity.as_str()),
                width,
            );
        }
        if let Some(gone) = &w.gone {
            push_text(out, format!("gone: {gone}"), width);
            push_head(out, "references:".to_owned(), width);
            for r in &w.references {
                push_text(out, format!("  {} {}", ref_kind(r.kind), r.label), width);
            }
        }
        let worktree = w
            .worktree
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".to_owned());
        push_text(out, format!("worktree: {worktree}"), width);
        if let Some(broken) = &w.broken {
            push_text(out, format!("state: broken - {broken}"), width);
            let dirty = match w.dirty {
                Some(true) => "yes",
                Some(false) => "no",
                None => "?",
            };
            push_text(out, format!("dirty: {dirty}"), width);
        }
        let local = match &w.base {
            Some(base) => {
                let ahead = num(w.commits_ahead);
                let behind = num(w.commits_behind);
                let dirty = match w.dirty {
                    Some(true) => "~dirty",
                    Some(false) => "clean",
                    None => "~?",
                };
                format!("↑{ahead} ↓{behind} {dirty} vs {base}")
            }
            None => "?".to_owned(),
        };
        push_text(out, format!("local: {local}"), width);
        let remote = match w.upstream {
            Upstream::Tracked => format!("{} · tracked", opt(&w.upstream_detail)),
            Upstream::NeverPushed => "no remote".to_owned(),
            Upstream::RemoteGone => format!("{} · remote gone", opt(&w.upstream_detail)),
            Upstream::NotApplicable => "n/a".to_owned(),
            Upstream::Unknown => format!("? ({})", opt(&w.upstream_detail)),
        };
        push_text(
            out,
            format!("remote: {remote} · unpushed {}", num(w.unpushed)),
            width,
        );
        let forge = match w.forge {
            WorkItem::Unknown => "?".to_owned(),
            WorkItem::NotExisting => "no work item".to_owned(),
            item => {
                let pipeline = match (item, w.pipeline) {
                    (WorkItem::Open, Pipeline::Unknown) => String::new(),
                    (WorkItem::Open, p) => format!(" · {}", p.as_str()),
                    _ => String::new(),
                };
                format!("{} · {}{}", opt(&w.forge_label), item.as_str(), pipeline)
            }
        };
        push_text(out, format!("forge: {forge}"), width);
        let landed = w.landed.map(|l| l.as_str()).unwrap_or("?");
        push_text(out, format!("landed: {landed}"), width);
        if !w.panes.is_empty() {
            push_head(out, "tmux:".to_owned(), width);
            for p in &w.panes {
                push_text(out, format!("  {} {}", p.handle, p.command), width);
            }
        }
        // Activity versus detection: `activity:` names the newest
        // source-backed occurrence (`?` when none is proven) and lists
        // each event at the time its source dated it; `observations:`
        // lists what passes learned, at the time they learned it - a
        // detection can never pass for work.
        push_text(
            out,
            format!(
                "activity: {}",
                store::newest_activity(&w.activities)
                    .map_or_else(|| "?".to_owned(), |ms| age_ms(now.saturating_mul(1000), ms))
            ),
            width,
        );
        for line in activity_lines(w, now.saturating_mul(1000)) {
            push_text(out, line, width);
        }
        // A working-tree observation whose linked activity still covers
        // every reason is the same evidence rendered twice: it stays in
        // the raw history but drops from the display. The filter runs
        // before the section check and the per-source cap, so a covered
        // newest event cannot push an uncovered older one out.
        let (observations, git_state) = partition_work_observations(w);
        for line in git_state_lines(&git_state, now.saturating_mul(1000)) {
            push_text(out, line, width);
        }
        if !observations.is_empty() {
            push_head(out, "observations:".to_owned(), width);
            for line in event_lines(
                &observations,
                |e| {
                    serde_json::to_value(e.source)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                },
                |e| e.observed_at_ms,
                |e| &e.reasons,
                now.saturating_mul(1000),
            ) {
                push_text(out, line, width);
            }
        }
        if !w.same_name_history.is_empty() {
            push_head(out, "same-name history:".to_owned(), width);
            for h in &w.same_name_history {
                push_text(
                    out,
                    format!(
                        "  {}#{} · ended {}",
                        h.ref_name,
                        h.number,
                        age(now, h.ended_at)
                    ),
                    width,
                );
            }
        }
        push_head(out, format!("commits not on {}:", opt(&w.base)), width);
        match &w.commits {
            None => push_text(out, "  ?".to_owned(), width),
            Some(commits) if commits.is_empty() => push_text(out, "  none".to_owned(), width),
            Some(commits) => {
                for c in commits {
                    push_text(
                        out,
                        format!(
                            "  {} {} · {} · {}",
                            sha(&Some(c.sha.clone())),
                            escape_text(&c.subject),
                            age(now, Some(c.at)),
                            c.conversation.as_deref().unwrap_or("?")
                        ),
                        width,
                    );
                }
                // The list is capped; the ahead count says how many more.
                let more = w
                    .commits_ahead
                    .map_or(0, |n| n.saturating_sub(commits.len() as u64));
                if more > 0 {
                    push_text(out, format!("  … {more} more"), width);
                }
            }
        }
        push_head(out, "cleanup:".to_owned(), width);
        for (action, verdict) in [
            ("worktree remove", &w.worktree_removal),
            ("branch delete", &w.branch_deletion),
        ] {
            match verdict {
                Some(v) => {
                    push_text(
                        out,
                        format!("  {action}: {}", v.verdict.as_str().replace('_', " ")),
                        width,
                    );
                    for reason in &v.reasons {
                        push_text(out, format!("    - {reason}"), width);
                    }
                }
                None => push_text(out, format!("  {action}: ?"), width),
            }
        }
    }

    /// A conversation's fields: identity, cwd, pane, state, resumability,
    /// activity, forge, every touch interval with its evidence, related
    /// conversations in proven-strength order, and the last prompts.
    fn conversation_body(&self, c: &ConversationRow, out: &mut Vec<Line<'static>>, width: usize) {
        let now = self.now();
        push_text(
            out,
            format!(
                "provider: {} · session {}",
                c.provider.as_str(),
                c.session_id
            ),
            width,
        );
        push_text(out, format!("title: {}", opt(&c.title)), width);
        let cwd = c
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".to_owned());
        push_text(out, format!("cwd: {cwd}"), width);
        let pane = match &c.attachment {
            Some(a) => match &a.pane {
                Some(p) => format!("{p} · {}", a.pane_source.map(|s| s.as_str()).unwrap_or("?")),
                None => {
                    if c.live && !c.running() {
                        "process exited".to_owned()
                    } else {
                        format!("unbound: {}", a.placement_detail.as_deref().unwrap_or("?"))
                    }
                }
            },
            None => "?".to_owned(),
        };
        push_text(out, format!("pane: {pane}"), width);
        let mut state = format!(
            "state: {} · since {}",
            c.state.as_str(),
            age(now, c.state_since)
        );
        if let Some(raw) = &c.state_raw {
            state.push_str(&format!(" · raw \"{}\"", escape_text(raw)));
        }
        push_text(out, state, width);
        if let Some(reason) = &c.waiting_for {
            push_text(out, format!("waiting for: {}", escape_text(reason)), width);
        }
        let resume = if c.resume_argv.is_empty() {
            "?".to_owned()
        } else {
            format!("{} ({})", "resumable", c.resume_argv.join(" "))
        };
        // No provider exposes epochs (a `/clear` count, prompts per
        // epoch) yet: the field is an honest `?`.
        push_text(out, "epoch: ?".to_owned(), width);
        push_text(out, format!("resume: {resume}"), width);
        push_text(
            out,
            format!(
                "activity: started {} · last turn {}",
                age(now, c.started_at),
                age(now, c.last_activity)
            ),
            width,
        );
        // The forge state of the incarnation the newest open touch names.
        let forge = c
            .current_incarnation
            .as_ref()
            .and_then(|id| {
                self.snapshot
                    .work
                    .iter()
                    .find(|w| w.incarnation.as_ref().is_some_and(|i| &i.id == id))
            })
            .and_then(|w| w.forge_label.clone());
        push_text(
            out,
            format!("forge: {}", forge.unwrap_or_else(|| "?".to_owned())),
            width,
        );
        push_head(out, "branch incarnations touched:".to_owned(), width);
        if c.touches.is_empty() {
            push_text(out, "  none".to_owned(), width);
        }
        for t in &c.touches {
            let until = t
                .valid_until
                .map_or_else(|| "open".to_owned(), |u| age(now, Some(u)));
            push_text(
                out,
                format!(
                    "  {}#{} · {} → {} · head {} · files ? · commits ? · created repo ? · {} · {}",
                    t.ref_name,
                    t.incarnation,
                    age(now, Some(t.valid_from)),
                    until,
                    sha(&t.head),
                    t.provenance.as_str(),
                    t.confidence.as_str()
                ),
                width,
            );
        }
        push_head(out, "related conversations:".to_owned(), width);
        if c.related.is_empty() {
            push_text(out, "  none proven".to_owned(), width);
        }
        for (strength, header) in [
            (
                RelationStrength::ProviderLineage,
                "  lineage - provider declared:",
            ),
            (
                RelationStrength::ProcessAncestry,
                "  lineage - observed process ancestry:",
            ),
            (RelationStrength::SameIncarnation, "  same incarnation:"),
        ] {
            let mut group = c
                .related
                .iter()
                .filter(|r| r.strength == strength)
                .peekable();
            if group.peek().is_none() {
                continue;
            }
            push_head(out, header.to_owned(), width);
            for r in group {
                push_text(
                    out,
                    format!(
                        "    {} {} {} - {} · {} · {}",
                        r.attention.glyph(),
                        r.short_id,
                        r.title.as_deref().unwrap_or("?"),
                        r.label,
                        r.provenance,
                        age(now, r.state_since)
                    ),
                    width,
                );
            }
        }
        push_head(out, "last prompts:".to_owned(), width);
        for (label, text) in [("prompt", &c.latest_prompt), ("reply", &c.latest_reply)] {
            let text = text
                .as_ref()
                .map(|t| fit(&escape_text(t), width.saturating_sub(10)))
                .unwrap_or_else(|| "?".to_owned());
            push_text(out, format!("  {label}: {text}"), width);
        }
    }

    /// A conversation's evidence view: the attachment's identity and
    /// verdicts, every arbitration claim with its outcome, the retained
    /// latches and their acknowledgement, the mark, the sequences, and
    /// every record the fold rejected.
    fn conversation_evidence(
        &self,
        c: &ConversationRow,
        out: &mut Vec<Line<'static>>,
        width: usize,
    ) {
        let now_ms = self.now() * 1000;
        push_text(
            out,
            format!(
                "provider: {} · session {}",
                c.provider.as_str(),
                c.session_id
            ),
            width,
        );
        match &c.attachment {
            Some(a) => {
                let mut process = format!(
                    "process: pid {} · started {} · {}",
                    a.pid,
                    age(self.now(), a.pid_start),
                    liveness_word(a)
                );
                if let Some(detail) = &a.liveness_detail {
                    process.push_str(&format!(" ({})", escape_text(detail)));
                }
                push_text(out, process, width);
                let pane = match (&a.pane, &a.placement_detail) {
                    (Some(p), _) => {
                        format!(
                            "pane: {p} · {}",
                            a.pane_source.map(|s| s.as_str()).unwrap_or("?")
                        )
                    }
                    (None, Some(d)) => format!("pane: unbound: {}", escape_text(d)),
                    (None, None) => "pane: ?".to_owned(),
                };
                push_text(out, pane, width);
                push_text(
                    out,
                    format!(
                        "claim source: {} · observed {}",
                        a.source.as_str(),
                        age(self.now(), Some(a.observed_at))
                    ),
                    width,
                );
            }
            None => push_text(out, "attachment: no claim".to_owned(), width),
        }
        let e = &c.evidence;
        push_head(out, "claims:".to_owned(), width);
        if e.claims.is_empty() {
            push_text(out, "  none".to_owned(), width);
        }
        for claim in &e.claims {
            let marker = match claim.outcome {
                ClaimOutcome::Winner => "*",
                _ => " ",
            };
            let mut line = format!(
                "  {marker} {} {} · observed {}",
                claim.source.as_str(),
                claim.exec.as_str(),
                age_ms(now_ms, claim.observed_ms)
            );
            if let Some(since) = claim.since_ms {
                line.push_str(&format!(" · since {}", age_ms(now_ms, since)));
            }
            if let Some(seq) = claim.seq {
                line.push_str(&format!(" · seq {seq}"));
            }
            if let Some(detail) = &claim.detail {
                line.push_str(&format!(" · \"{}\"", escape_text(detail)));
            }
            line.push_str(&format!(" - {}", claim.outcome.as_str()));
            push_text(out, line, width);
            if let Some(note) = &claim.note {
                push_text(out, format!("      {note}"), width);
            }
        }
        let ack = match (e.seen_seq, e.seen_wait_ms) {
            (None, None) => "nothing".to_owned(),
            (seq, wait) => format!(
                "through seq {} · wait seen at {}",
                seq.map(|s| s.to_string()).unwrap_or_else(|| "?".to_owned()),
                wait.map(|ms| age_ms(now_ms, ms))
                    .unwrap_or_else(|| "?".to_owned())
            ),
        };
        push_text(out, format!("acknowledged: {ack}"), width);
        if !e.latches.is_empty() {
            push_head(out, "latches:".to_owned(), width);
            for l in &e.latches {
                let acked = if l.acknowledged { "acked" } else { "unacked" };
                push_text(
                    out,
                    format!(
                        "  seq {} {} · {} · {} · {acked}",
                        l.seq,
                        l.kind.as_str(),
                        age_ms(now_ms, l.at_ms),
                        l.reason.as_deref().unwrap_or("?")
                    ),
                    width,
                );
            }
        }
        if let Some(mark) = &e.mark {
            let mut line = format!(
                "mark: not-busy · seq {} · since {} · written {}",
                mark.seq,
                age_ms(now_ms, mark.since_ms),
                age_ms(now_ms, mark.at_ms)
            );
            if e.mark_suppressed {
                line.push_str(" · suppressed the busy claim");
            }
            push_text(out, line, width);
        }
        push_text(
            out,
            format!(
                "sequences: journal {} · producer {}",
                e.journal_seq
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".to_owned()),
                e.producer_seq
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "?".to_owned())
            ),
            width,
        );
        if !e.rejected.is_empty() {
            push_head(out, "rejected / stale:".to_owned(), width);
            for r in &e.rejected {
                push_text(
                    out,
                    format!(
                        "  seq {} {} · {} · pseq {} · {}",
                        r.seq,
                        r.native,
                        age_ms(now_ms, r.at_ms),
                        r.pseq
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "?".to_owned()),
                        r.reason
                    ),
                    width,
                );
            }
        }
        if let Some(t) = &c.transcript {
            push_text(
                out,
                format!(
                    "transcript: {} · malformed lines {}",
                    t.display(),
                    c.malformed_lines
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".to_owned())
                ),
                width,
            );
        }
    }

    /// The footer: hints for the focused view, always ending `? keys | q quit`.
    /// A narrow terminal gets the compact form rather than a clipped one.
    /// While the snapshot is incomplete a spinner leads - collection is in
    /// flight - and a dead collector replaces the line entirely.
    fn footer(&self, f: &mut Frame<'_>, area: Rect) {
        let text = if self.collector_dead {
            "collector stopped - last snapshot | q quit".to_owned()
        } else if let Some(notice) = &self.notice {
            notice.clone()
        } else {
            let spinner = self.spinner(area.width);
            let hints = if self.editing.is_some() {
                "filter: enter apply | esc cancel".to_owned()
            } else if area.width < 60 {
                match self.focused_list() {
                    Some(List::Work) => {
                        "1-4 | tab | j/k | ⏎ | o | e | p | h | / | ? | q quit".to_owned()
                    }
                    Some(List::Conversations) => {
                        "1-4 | tab | j/k | ⏎ | e | / filter | ? | q quit".to_owned()
                    }
                    Some(_) => "1-4 | tab | j/k | e | / filter | ? | q quit".to_owned(),
                    None => "1-4 | tab | j/k scroll | e | ? | q quit".to_owned(),
                }
            } else {
                let hints = match self.focused_list() {
                    Some(List::Repos) => "1-4 focus | tab next | j/k move | e evidence | / filter",
                    Some(List::Work) => {
                        "1-4 focus | tab next | j/k move | enter jump | o open | e evidence | / filter | space ack | p park | h history"
                    }
                    Some(_) => {
                        "1-4 focus | tab next | j/k move | enter jump | e evidence | / filter | space ack"
                    }
                    None => "1-4 focus | tab next | j/k scroll | e evidence | esc close",
                };
                format!("{hints} | ? keys | q quit")
            };
            format!("{spinner}{hints}")
        };
        f.render_widget(
            Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray))),
            area,
        );
    }

    /// The collection-in-flight glyph for the footer, cycling one frame per
    /// draw. An empty string when the snapshot is complete - no spinner is
    /// better than a decorative one. Narrow terminals get the bare glyph;
    /// wide ones can afford "collecting" beside it.
    fn spinner(&self, width: u16) -> String {
        if self.snapshot.complete {
            return String::new();
        };
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = FRAMES[(self.spin.get() as usize) % FRAMES.len()];
        self.spin.set(self.spin.get() + 1);
        if width >= 80 {
            format!("{frame} collecting  ")
        } else {
            format!("{frame} ")
        }
    }

    /// `?` - the focused pane's keys, plus the shared ones.
    fn help_overlay(&self, f: &mut Frame<'_>, area: Rect) {
        let rows: Vec<Line<'static>> = match self.focused_list() {
            Some(list) => vec![
                Line::from(match list {
                    List::Repos => "[1] Repos - which project needs me",
                    List::Work => "[2] Work - what needs attention, follow-up or cleanup",
                    List::Conversations => "[3] Conversations - what needs me right now",
                }),
                Line::from(""),
                Line::from(match list {
                    List::Work => {
                        "j/k move   / filter   e evidence   space ack/mark   p park   h history   enter jump   o open"
                    }
                    List::Conversations => {
                        "j/k move   / filter   e evidence   space ack/mark   enter jump"
                    }
                    _ => "j/k move   / filter   e evidence   space ack/mark",
                }),
            ],
            None => vec![
                Line::from("[4] Detail - follows the focused list"),
                Line::from(""),
                Line::from("j/k scroll   e evidence   esc close"),
            ],
        };
        let mut lines = vec![
            Line::from("keys"),
            Line::from("1-4 focus   tab next   q quit   ? close   esc close"), // coverage: off - the unexecuted instantiation's region edge
            Line::from(""),
        ]; // coverage: off - the unexecuted instantiation's region edge
        lines.extend(rows);
        let w = area.width.min(60);
        let h = (lines.len() as u16 + 2).min(area.height);
        let popup = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + (area.height.saturating_sub(h)) / 2,
            width: w,
            height: h,
        };
        f.render_widget(Clear, popup);
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("help")),
            popup,
        ); // coverage: off - the unexecuted instantiation's region edge
    } // coverage: off - same
    // coverage: off - the instantiation edge lands on this line
    /// The panel title: `[N] Name` plus the scope suffix the cursor above set.
    fn panel_title(&self, pane: Pane, view: &View<'_>) -> String {
        // coverage: off - the Pane::Detail arm never runs: the detail pane renders its own header
        match pane {
            Pane::Repos => "[1] Repos".to_owned(),
            Pane::Work => match &view.repo_scope {
                None => "[2] Work  all · by next action".to_owned(),
                Some(repo) => {
                    let name = self
                        .snapshot
                        .repos
                        .iter()
                        .find(|r| &r.id == repo)
                        .map(|r| r.name.clone())
                        .unwrap_or_else(|| repo.clone()); // coverage: off - the scope's id always names a repo row
                    format!("[2] Work  {name} · by next action")
                }
            },
            Pane::Conversations => match &view.work_scope {
                Some(WorkScope::Worktree { root, .. }) => {
                    format!("[3] Conversations  {}", root.display())
                }
                Some(WorkScope::Incarnation {
                    label, excluded, ..
                }) => {
                    let state = if *excluded {
                        "excluded history"
                    } else {
                        "current incarnation"
                    };
                    format!("[3] Conversations  {label} · {state}")
                }
                Some(WorkScope::Space { path, .. }) => {
                    format!("[3] Conversations  {}", path.display())
                }
                None => "[3] Conversations  all · by attention".to_owned(),
            },
            Pane::Detail => self.detail_header(view).0, // coverage: off - the detail pane renders its own header, never asks the title
        }
    }
}

/// The three lists' filtered rows plus the scopes the cursors select, built
/// once per render or keypress. Views over the snapshot, never copies of it.
struct View<'a> {
    repos: Vec<Row<'a>>,
    work: Vec<Row<'a>>,
    conversations: Vec<Row<'a>>,
    /// The repo the [1] cursor names; `None` on `all`.
    repo_scope: Option<String>,
    /// The work row the [2] cursor names; `None` on `all`.
    work_scope: Option<WorkScope>,
    /// `N open · M clean` across the scoped work rows - collapsed or not.
    work_open: usize,
    work_clean: usize,
    /// `(safe, review)` counts when the cleanup sections are collapsed
    /// under `all`; `None` under a repo, where the rows list.
    cleanup: Option<(usize, usize)>,
}

impl View<'_> {
    /// The rows of one list.
    #[rustfmt::skip]
    fn rows(&self, list: List) -> &[Row<'_>] { // coverage: off - unexecuted-instantiation edges land on this fn's lines
        match list { // coverage: off - same
            List::Repos => &self.repos, // coverage: off - same
            List::Work => &self.work, // coverage: off - same
            List::Conversations => &self.conversations,
        }
    } // coverage: off - the tail edge of the unexecuted instantiation lands here
} // coverage: off - same

/// The identity a cursor selection tracks across refreshes.
fn selection_key(row: &Row<'_>) -> String {
    match row {
        Row::Repo(r) => r.id.clone(),
        Row::Work(w) => work_key(w),
        // The incarnation id itself: the selection follows the exact
        // history row, not the name it shares with the active one.
        Row::History(_, h) => h.id.clone(),
        Row::Conversation(c) => format!("{}\u{0}{}", c.provider.as_str(), c.session_id),
    }
}

/// The scope a work row selects in [3].
fn scope_of(w: &WorkRow) -> WorkScope {
    match (w.kind, &w.worktree, &w.branch) {
        (WorkKind::ProjectSpace, Some(root), _) => WorkScope::Space {
            id: w.repo.clone(),
            path: root.clone(),
        },
        (WorkKind::Detached, Some(root), _) => WorkScope::Worktree {
            repo: w.repo.clone(),
            root: root.clone(),
        },
        (_, _, Some(branch)) => WorkScope::Incarnation {
            id: w.identity.clone().unwrap_or_default(),
            label: match w.incarnation.as_ref() {
                Some(i) => format!("{}#{}", branch, i.number),
                None => branch.clone(),
            },
            // A gone row's incarnation is closed: it scopes like history.
            excluded: w.gone.is_some(),
        },
        _ /* // coverage: off - an anchor always names one of these */ => WorkScope::Space {
            id: String::new(),               // coverage: off - same
            path: Path::new("").to_path_buf(), // coverage: off - same
        },
    } // coverage: off - the unexecuted instantiation's exit edge
}

/// A work-scope selector: which slice of conversations the [2] cursor means.
enum WorkScope {
    /// A checkout's path; conversations inside it.
    Worktree {
        repo: String,
        root: std::path::PathBuf,
    },
    /// One branch incarnation, active or excluded history; conversations
    /// with a touch to exactly this id.
    Incarnation {
        id: String,
        /// `<branch>#N` for the [3] title.
        label: String,
        /// Whether the row scopes to a retained earlier incarnation.
        excluded: bool,
    },
    /// A non-git project space; conversations anchored on its canonical id
    /// (`repo`), whatever spelling their recorded cwd carries.
    Space {
        id: String,
        path: std::path::PathBuf,
    },
}

fn list_index(list: List) -> usize {
    match list {
        List::Repos => 0,
        List::Work => 1,
        List::Conversations => 2,
    }
}

/// A panel's height: its filtered rows plus borders, capped to a third of
/// the screen so [1] cannot starve the others.
fn list_height(rows: usize, height: u16) -> u16 {
    ((rows as u16 + 3).max(3)).min(height / 3)
}

/// What the age column shows: `2m`, `1h`, `9d`, or `?` when unknown.
fn age(now: u64, then: Option<u64>) -> String {
    let Some(then) = then else {
        return "?".to_owned();
    };
    let secs = now.saturating_sub(then);
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 90 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 36 * 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// The terminal cells `text` occupies: a double-width character takes
/// two, a control character none.
fn cell_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// One character's terminal cells.
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// `label` clipped to `w` cells with an ellipsis when it loses a character.
fn fit(label: &str, w: usize) -> String {
    if cell_width(label) <= w {
        return label.to_owned();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in label.chars() {
        let cw = char_width(c);
        if used + cw > w - 1 {
            break;
        }
        used += cw;
        out.push(c);
    }
    out.push('…');
    out
}

/// Width the leading glyph column occupies: all glyphs are one cell except
/// the empty slot, which keeps the column present.
fn glyph_width(glyph: &str) -> usize {
    glyph.chars().count().max(1)
}

/// The leading glyph's colour: attention is `!`/`✗`/`✓`/`●`, unknown is a
/// dim `?`, and an empty slot takes no colour.
fn glyph_style(glyph: &str) -> Style {
    match glyph {
        "!" => Style::default().fg(Color::LightYellow),
        "✗" => Style::default().fg(Color::LightRed),
        "✓" => Style::default().fg(Color::LightGreen),
        "●" => Style::default().fg(Color::LightBlue),
        "?" => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

/// A repo row's glyph: the rolled-up attention of its work rows.
fn repo_glyph(repo: &RepoRow) -> &'static str {
    repo.attention.glyph()
}

/// `N open · M clean` for repos with Git evidence, `no git` otherwise.
fn repo_counts(repo: &RepoRow) -> String {
    if repo.git {
        format!("{} open · {} clean", repo.open, repo.clean)
    } else {
        "no git".to_owned()
    }
}

/// A work row's glyph: the rolled-up attention of its bound
/// conversations. A non-agent process (`live_pids` with no session) is
/// not attention - it counts in the row's fields, not its glyph.
fn work_glyph(w: &WorkRow) -> &'static str {
    w.attention.glyph()
}

/// The work row's label: `name#N ⌂worktree`, prefixed with the repo's
/// name under the global `all` scope; a project space's workspace is its
/// name already, so it carries no suffix.
fn work_name(w: &WorkRow, global: bool) -> String {
    work_label(w, global, w.incarnation.as_ref().map(|i| i.number))
}

/// The row's name with its `#N`: `feat#2`, or the bare name where no
/// incarnation numbers it.
fn numbered_name(w: &WorkRow) -> String {
    match &w.incarnation {
        Some(i) => format!("{}#{}", w.name, i.number),
        None => w.name.clone(),
    }
}

/// `work_name` with the incarnation number spelled out: `name` is the
/// branch name plus its `#N` when a number is known.
fn work_label(w: &WorkRow, global: bool, number: Option<usize>) -> String {
    if w.kind == WorkKind::ProjectSpace {
        return w.name.clone();
    }
    let name = match number {
        Some(n) => format!("{}#{}", w.name, n),
        None => w.name.clone(),
    };
    let wt = w
        .worktree
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| format!(" ⌂{}", n.to_string_lossy()))
        .unwrap_or_default();
    if global {
        format!("{}/{}{}", w.repo_name, name, wt)
    } else {
        format!("{}{}", name, wt)
    }
}

/// The conversation's dim second line under the global work scope:
/// `a#N → b#M` when its ordered distinct touches name several in-scope
/// incarnations, else the `repo · branch#N · worktree` context of the one
/// it carries - or its bare repo/worktree context when it touched none.
/// `repo_scope` narrows "in scope" to touches in the selected repo.
fn conversation_context(c: &ConversationRow, repo_scope: Option<&str>) -> Option<String> {
    let mut distinct: Vec<&crate::snapshot::TouchRow> = Vec::new();
    for t in &c.touches {
        if !repo_scope.is_none_or(|s| t.repo == s) {
            continue;
        }
        if !distinct
            .iter()
            .any(|d| d.incarnation_id == t.incarnation_id)
        {
            distinct.push(t);
        }
    }
    let multi_repo = distinct
        .iter()
        .map(|t| t.repo.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1;
    let label = |t: &crate::snapshot::TouchRow| {
        if multi_repo {
            format!("{}/{}#{}", repo_label(&t.repo), t.ref_name, t.incarnation)
        } else {
            format!("{}#{}", t.ref_name, t.incarnation)
        }
    };
    if distinct.len() > 1 {
        Some(
            distinct
                .iter()
                .map(|t| label(t))
                .collect::<Vec<_>>()
                .join(" → "),
        )
    } else if let Some(t) = distinct.first() {
        let mut parts = vec![
            repo_label(&t.repo),
            format!("{}#{}", t.ref_name, t.incarnation),
        ];
        if let Some(wt) = &c.worktree {
            parts.push(path_label(wt));
        }
        Some(parts.join(" · "))
    } else {
        // No touch yet: the resolved identity is all the context there is.
        let mut parts = Vec::new();
        if let Some(repo) = &c.repo {
            parts.push(repo_label(repo));
        }
        if let Some(branch) = &c.branch {
            parts.push(branch.clone());
        }
        if let Some(wt) = &c.worktree {
            parts.push(path_label(wt));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// The repo's display name from its id: the basename, or the `.git`
/// directory's parent's.
fn repo_label(repo: &str) -> String {
    let p = Path::new(repo);
    let target = if p.file_name().is_some_and(|n| n == ".git") {
        p.parent().unwrap_or(p)
    } else {
        p
    };
    target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.to_owned())
}

/// A worktree's display name: its basename.
fn path_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The conversation row's middle field: the provider, plus `working`
/// beside a retained latch when the agent has gone back to grinding -
/// the retained `error`/`done` asks for you while the state field says
/// the turn runs.
fn conversation_middle(c: &ConversationRow) -> String {
    match c.attention {
        Attention::Error | Attention::CompletedUnseen if c.state == ConversationState::Busy => {
            format!("{} · working", c.provider.as_str())
        }
        _ => c.provider.as_str().to_owned(),
    }
}

/// A `[2]` section header line: the section's name, dimmed, unselectable
/// in spirit - the cursor counts rows, not headers.
fn section_header(section: crate::snapshot::WorkSection) -> Line<'static> {
    Line::from(Span::styled(
        section.title().to_owned(),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ))
}

/// A conversation row's glyph is its attention: `!`/`✗`/`✓`/`●`/`?`/blank.
/// A retained `end` or `error` survives the process's death - the latch is
/// what the row owes you, not what the process is doing.
fn conversation_glyph(c: &ConversationRow) -> &'static str {
    c.attention.glyph()
}

/// The detail header's state text: the attention's word when the row
/// carries one (`waiting: permission prompt`, `error`, `done`), then the
/// effective state. A claim the runtime proved dead reads `dead` beside
/// whatever latch survives it.
fn detail_state(c: &ConversationRow) -> String {
    let attention = match c.attention {
        Attention::Waiting => match c.attention_detail.as_deref().or(c.waiting_for.as_deref()) {
            Some(reason) => format!("waiting: {reason}"),
            None => "waiting".to_owned(),
        },
        Attention::Error => match &c.attention_detail {
            Some(detail) => format!("error: {detail}"),
            None => "error".to_owned(),
        },
        Attention::CompletedUnseen => "done".to_owned(),
        Attention::Working => "working".to_owned(),
        Attention::Unknown | Attention::None => String::new(),
    };
    // The effective state's own word, when it adds something the attention
    // label does not already say: `error · working`, not `waiting ·
    // waiting`.
    let state = if c.live && !c.running() {
        Some("dead")
    } else {
        match c.state {
            ConversationState::Waiting if c.attention != Attention::Waiting => {
                Some(c.state.as_str())
            }
            ConversationState::Busy if c.attention != Attention::Working => Some("working"),
            ConversationState::Idle if c.attention == Attention::None => Some("idle"),
            _ => None,
        }
    };
    match (attention.is_empty(), state) {
        (true, Some(s)) => s.to_owned(),
        (true, None) => c.state.as_str().to_owned(),
        (false, Some(s)) => format!("{attention} · {s}"),
        (false, None) => attention,
    }
}

/// A wrapped content line into the detail body.
fn push_text(out: &mut Vec<Line<'static>>, text: String, width: usize) {
    for line in wrap_text(&text, width) {
        out.push(Line::from(line));
    }
}

/// A wrapped section header into the detail body, dimmed.
fn push_head(out: &mut Vec<Line<'static>>, text: String, width: usize) {
    for line in wrap_text(&text, width) {
        out.push(Line::from(Span::styled(
            line,
            Style::default().fg(Color::DarkGray),
        )));
    }
}

/// `text` as lines that never exceed `width` cells: the detail pane wraps
/// rather than clips or scrolls sideways. A double-width character that
/// would straddle the edge moves to the next line.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cw = char_width(ch);
        if used + cw > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += cw;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    } // coverage: off - the skip edge: non-empty input always leaves a partial line, empty input leaves none
    lines
}

/// The string, or `?` where the field never proved one.
fn opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "?".to_owned())
}

/// The count, or `?` where the evidence never produced one.
fn num(n: Option<u64>) -> String {
    n.map(|n| n.to_string()).unwrap_or_else(|| "?".to_owned())
}

/// The head's first seven characters, or `?` where unrecorded.
fn sha(head: &Option<String>) -> String {
    head.as_ref()
        .map(|h| h.chars().take(7).collect())
        .unwrap_or_else(|| "?".to_owned())
}

/// The attachment's liveness as a word for the evidence view.
fn liveness_word(a: &AttachmentRow) -> &'static str {
    match a.liveness {
        AttachmentLiveness::Instance => "instance",
        AttachmentLiveness::PidOnly => "pid only",
        AttachmentLiveness::Unverifiable => "unverifiable",
        AttachmentLiveness::Dead => "dead",
    }
}

/// A gone-row reference's kind word, in the detail's `references:` block.
fn ref_kind(kind: ReferenceKind) -> &'static str {
    match kind {
        ReferenceKind::Pane => "pane",
        ReferenceKind::Window => "window",
        ReferenceKind::TmuxSession => "session",
        ReferenceKind::Process => "process",
        ReferenceKind::AgentSession => "agent",
    }
}

const EVENT_DISPLAY_PER_SOURCE: usize = 7;

/// The activity source's serialized name - the label a group header
/// renders with `_` as spaces.
fn activity_source_name(e: &store::ActivityEvent) -> String {
    serde_json::to_value(e.source)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The conversation's short id for a summary row: the first eight
/// characters of the session part of its provider-qualified key, or of
/// the whole key where an opaque legacy key carries no provider prefix.
fn short_conversation_id(key: &str) -> String {
    let session = key.split_once('\u{0}').map(|(_, s)| s).unwrap_or(key);
    session.chars().take(8).collect()
}

/// The `activity:` section's lines: every source's events grouped and
/// ordered exactly as `event_lines` renders them, except the conversation
/// group - where the record carries cursor-backed summaries it shows one
/// compact row per conversation (newest occurrence first, then full key,
/// capped at [`EVENT_DISPLAY_PER_SOURCE`] after compaction) in place of
/// the raw per-turn reasons. A record without summaries keeps the raw
/// trail as its fallback.
fn activity_lines(w: &WorkRow, now_ms: u64) -> Vec<String> {
    let raw: Vec<&store::ActivityEvent> = w
        .activities
        .iter()
        .filter(|event| {
            event.source != store::ActivitySource::Conversation
                || w.conversation_summaries.is_empty()
        })
        .collect();
    let mut groups = event_groups(
        &raw,
        |e| activity_source_name(e),
        |e| e.occurred_at_ms,
        |e| e.reasons.as_slice(),
        now_ms,
    );
    if !w.conversation_summaries.is_empty() {
        let mut summaries: Vec<&ConversationSummary> = w.conversation_summaries.iter().collect();
        summaries.sort_by(|a, b| {
            b.occurred_at_ms
                .cmp(&a.occurred_at_ms)
                .then(a.key.cmp(&b.key))
        });
        let newest = summaries.first().map(|s| s.occurred_at_ms).unwrap_or(0);
        let mut lines = vec!["  conversation:".to_owned()];
        for summary in summaries.into_iter().take(EVENT_DISPLAY_PER_SOURCE) {
            let when = age_ms(now_ms, summary.occurred_at_ms);
            let id = escape_text(&short_conversation_id(&summary.key));
            let context = summary.context.as_ref();
            let prompt = context.and_then(|c| c.prompt_excerpt.as_deref());
            let title = context.and_then(|c| c.title.as_deref());
            let detail = match (prompt, title) {
                (Some(prompt), _) => format!(" - prompt: {}", escape_text(prompt)),
                (None, Some(title)) => format!(" - {}", escape_text(title)),
                (None, None) => String::new(),
            };
            lines.push(format!("    last activity {when} {id}{detail}"));
        }
        groups.push((newest, "conversation".to_owned(), lines));
    }
    grouped_lines(groups)
}

/// A row's events rendered grouped by source, groups ordered by their
/// newest event and each event listed at its own `at` - the shape the
/// `activity:` and `observations:` sections share, where `at` is the
/// occurrence or the detection time respectively.
fn event_lines<E>(
    events: &[E],
    source: impl Fn(&E) -> String,
    at: impl Fn(&E) -> u64,
    reasons: impl Fn(&E) -> &[String],
    now_ms: u64,
) -> Vec<String> {
    grouped_lines(event_groups(events, source, at, reasons, now_ms))
}

fn event_groups<E>(
    events: &[E],
    source: impl Fn(&E) -> String,
    at: impl Fn(&E) -> u64,
    reasons: impl Fn(&E) -> &[String],
    now_ms: u64,
) -> Vec<(u64, String, Vec<String>)> {
    let mut by_source: std::collections::BTreeMap<String, Vec<&E>> =
        std::collections::BTreeMap::new();
    for event in events {
        by_source.entry(source(event)).or_default().push(event);
    }
    by_source
        .into_iter()
        .map(|(source, mut events)| {
            let newest = events.iter().map(|e| at(e)).max().unwrap_or(0);
            let mut lines = vec![format!("  {}:", source.replace('_', " "))];
            events.sort_by_key(|e| std::cmp::Reverse(at(e)));
            for event in events.iter().take(EVENT_DISPLAY_PER_SOURCE) {
                let when = age_ms(now_ms, at(event));
                for reason in reasons(event) {
                    lines.push(format!("    {when} {}", escape_text(reason)));
                }
            }
            (newest, source, lines)
        })
        .collect()
}

fn grouped_lines(mut groups: Vec<(u64, String, Vec<String>)>) -> Vec<String> {
    groups.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    groups.into_iter().flat_map(|(_, _, lines)| lines).collect()
}

fn partition_work_observations(
    w: &WorkRow,
) -> (Vec<store::ObservationEvent>, Vec<store::ObservationEvent>) {
    const GIT_STATE_PREFIXES: [&str; 3] = ["ahead: ", "behind: ", "unpushed: "];
    let is_count = |r: &&String| GIT_STATE_PREFIXES.iter().any(|p| r.starts_with(p));
    let mut remaining = Vec::new();
    let mut git_state = Vec::new();
    for event in w
        .observations
        .iter()
        .filter(|e| !e.is_covered_by(&w.activities))
    {
        if event.source != store::ObservationSource::Lifecycle {
            remaining.push(event.clone());
            continue;
        }
        let mut counts: Vec<String> = event.reasons.iter().filter(is_count).cloned().collect();
        if counts.is_empty() {
            remaining.push(event.clone());
            continue;
        }
        counts.sort();
        git_state.push(store::ObservationEvent {
            reasons: counts,
            ..event.clone()
        });
        let rest: Vec<String> = event
            .reasons
            .iter()
            .filter(|r| !is_count(r))
            .cloned()
            .collect();
        if !rest.is_empty() {
            remaining.push(store::ObservationEvent {
                reasons: rest,
                ..event.clone()
            });
        }
    }
    (remaining, git_state)
}

fn git_state_lines(events: &[store::ObservationEvent], now_ms: u64) -> Vec<String> {
    if events.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&store::ObservationEvent> = events.iter().collect();
    sorted.sort_by(|a, b| {
        b.observed_at_ms
            .cmp(&a.observed_at_ms)
            .then(a.reasons.cmp(&b.reasons))
    });
    let mut lines = vec!["  git state (observed):".to_owned()];
    for event in sorted.iter().take(EVENT_DISPLAY_PER_SOURCE) {
        let when = age_ms(now_ms, event.observed_at_ms);
        for reason in &event.reasons {
            lines.push(format!("    observed {when} {}", escape_text(reason)));
        }
    }
    lines
}

/// An epoch-ms evidence timestamp as an age against `now_ms`.
fn age_ms(now_ms: u64, then_ms: u64) -> String {
    age(now_ms / 1000, Some(then_ms / 1000))
}

/// An incarnation's evidence block: the record's dates, creation
/// evidence, tip and continuity decision - the same block a work row
/// embeds and a history row's whole body.
fn incarnation_body(i: &IncarnationRow, now: u64, out: &mut Vec<Line<'static>>, width: usize) {
    push_head(out, "incarnation:".to_owned(), width);
    push_text(
        out,
        format!("  {}#{} · id {}", i.ref_name, i.number, i.id),
        width,
    );
    push_text(
        out,
        format!("  ref: {} · repo {}", i.ref_name, i.repo),
        width,
    );
    let ended = i
        .ended_at
        .map_or_else(|| "open".to_owned(), |_| age(now, i.ended_at));
    push_text(
        out,
        format!(
            "  observed {} - {} · ended {}",
            age(now, Some(i.first_observed_at)),
            age(now, Some(i.last_observed_at)),
            ended
        ),
        width,
    );
    push_text(
        out,
        format!(
            "  ref creation {} · at {} · tip {}",
            sha(&i.creation_head),
            age(now, i.creation_at),
            sha(&i.head)
        ),
        width,
    );
    push_text(
        out,
        format!("  continuity: {}", i.continuity.as_str()),
        width,
    );
    if i.excluded {
        push_text(out, "  excluded from the current row".to_owned(), width);
    }
}

/// The keys the input loop translates; `Tab`, `Esc`, `Enter`, `Backspace`
/// and the arrows keep their own variants so no key ever aliases a byte the
/// terminal might also send for something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Tab,
    Esc,
    Enter,
    Backspace,
    Up,
    Down,
} // coverage: off - the unexecuted instantiation's exit edge
// coverage: off - the instantiation edge lands on this line
/// `code` -> a `Key`, or `None` for input the shell does not bind. Terminal // coverage: off - the zero regions on this doc and `map_key`'s edges are unexecuted-instantiation copies
/// events and key releases are dropped here, before they can alias a byte // coverage: off - same
/// the app layer would act on. // coverage: off - same
#[rustfmt::skip]
fn map_key(code: crossterm::event::KeyCode) -> Option<Key> { // coverage: off - the unexecuted instantiation's entry edge
    use crossterm::event::KeyCode;
    Some(match code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Tab | KeyCode::BackTab => Key::Tab, // coverage: off - a key event only ever exercises one arm per instantiation
        KeyCode::Esc => Key::Esc,
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down, // coverage: off - same
        _ => return None, // coverage: off - same
    }) // coverage: off - the unexecuted instantiation's exit edge
} // coverage: off - same
// coverage: off - the instantiation edge lands on this line
// coverage: off - same
/// The driver: crossterm event loop around an `App`, with collection on a // coverage: off - same
/// worker thread and finished snapshots swapped in through a bounded // coverage: off - the unexecuted instantiation's region edge
/// channel. Subprocess work stays in the collector, never on the input or // coverage: off - the line's zero region is an unexecuted instantiation edge
/// render paths - the loop only swaps in snapshots the channel already
/// collected. The loop starts on whatever snapshot `app` holds - an
/// incomplete `Snapshot::empty()` paints the frame before stage 1 lands - // coverage: off - same
/// and `refresh` streams a pass's snapshots through its publish callback. // coverage: off - same
///
/// Only the terminal setup and the worker spawn live here; the loop itself
/// is `run_loop`, which any backend can drive - the tests drive it on
/// `TestBackend` with a scripted snapshot source.
pub fn run(
    mut app: App,
    refresh: impl FnMut(&mut dyn FnMut(Snapshot) -> bool) + Send + 'static,
) -> io::Result<()> {
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    enable_raw_mode()?; // coverage: off - `?` needs a broken terminal
    let mut stdout = io::stdout(); // coverage: off - `?` above can only fail there
    crossterm::execute!(stdout, EnterAlternateScreen)?; // coverage: off - `?` needs a broken terminal
    let backend = ratatui::backend::CrosstermBackend::new(stdout); // coverage: off - same
    let mut terminal = Terminal::new(backend)?; // coverage: off - `?` needs a broken terminal
    // One pending snapshot at most: the worker computes the next pass only
    // once the loop has taken the previous one, so a slow collect can delay
    // the next swap but never a redraw or a key press.
    let pace = Duration::from_secs(1); // coverage: off - the worker only runs under a real terminal
    let (tx, rx) = std::sync::mpsc::sync_channel::<Snapshot>(1); // coverage: off - same
    std::thread::spawn(move || collect_worker(tx, refresh, pace)); // coverage: off - same

    let feed = move || match rx.try_recv() {
        Ok(snapshot) => Feed::Snapshot(snapshot), // coverage: off - `run` itself needs a real terminal
        Err(std::sync::mpsc::TryRecvError::Empty) => Feed::Idle, // coverage: off - the unexecuted instantiation's arm edge
        Err(std::sync::mpsc::TryRecvError::Disconnected) => Feed::Dead, // coverage: off - needs the worker to die while the loop runs
    }; // coverage: off - the unexecuted instantiation's region edge
    let result = run_loop(
        &mut terminal,
        &mut app,
        feed,
        poll_event,
        act_on,
        suspend_for,
    ); // coverage: off - `?` needs a broken terminal
    disable_raw_mode()?; // coverage: off - `?` needs a broken terminal
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?; // coverage: off - same
    result // coverage: off - same
} // coverage: off - the unexecuted instantiation's exit edge
// coverage: off - the instantiation edge lands on this line
/// The loop's Enter/`o` resolution: the request a keypress named is // coverage: off - `run` itself needs a real terminal
/// re-resolved against fresh local evidence - a fresh collector, a // coverage: off - same
/// fresh runtime, the dashboard's own pane - so a row that moved or // coverage: off - same
/// vanished since the frame was drawn never drives an action, and no // coverage: off - same
/// remote or forge ask stalls the loop on a keypress. // coverage: off - same
// The `coverage: off` markers below must sit on the exact line a zero
// region lands on, so rustfmt - which relocates trailing comments after
// `{` - is asked to leave these four functions alone.
#[rustfmt::skip]
fn act_on(request: &ActionRequest) -> ActionOutcome { // coverage: off - `run` itself needs a real terminal
    match collector() { // coverage: off - same
        Ok(mut collector) => { // coverage: off - same
            let runtime = crate::runtime::Runtime::observe(); // coverage: off - same
            let snapshot = collector.collect_local(&runtime, own_pane().as_ref()); // coverage: off - same
            let store = app_store(); // coverage: off - same
            let path = std::env::var_os("PATH"); // coverage: off - same
            action::act(request, &snapshot, store.as_ref(), path.as_deref()) // coverage: off - same
        } // coverage: off - same
        Err(e) => ActionOutcome::Failed(e), // coverage: off - same
    } // coverage: off - same
} // coverage: off - the unexecuted instantiation's exit edge
// coverage: off - the instantiation edge lands on this line
/// The loop's resume executor: the dashboard's terminal is suspended // coverage: off - a resume needs a real terminal
/// before exec so the agent inherits an ordinary pane, and a failed // coverage: off - same
/// exec puts the screen back so the loop can report the failure // coverage: off - same
/// instead of leaving a broken terminal. // coverage: off - same
#[rustfmt::skip]
fn suspend_for(plan: &ResumePlan) -> Result<(), String> { // coverage: off - a resume needs a real terminal
    if let Err(e) = suspend_terminal() { // coverage: off - same
        return Err(format!("terminal suspend: {e}")); // coverage: off - same
    } // coverage: off - same
    match action::exec_resume(plan, app_store().as_ref()) { // coverage: off - same
        Err(e) => { // coverage: off - same
            let _ = restore_terminal(); // coverage: off - same
            Err(e) // coverage: off - same
        } // coverage: off - same
        Ok(never) => match never {}, // coverage: off - exec does not return on success
    } // coverage: off - same
} // coverage: off - same
// coverage: off - the instantiation edge lands on this line
/// Raw mode off and the alternate screen left: the dashboard yields the // coverage: off - the unexecuted instantiation's region edge
/// terminal to the process a resume execs. // coverage: off - same
#[rustfmt::skip]
fn suspend_terminal() -> io::Result<()> { // coverage: off - a resume needs a real terminal
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode}; // coverage: off - same
    disable_raw_mode()?; // coverage: off - `?` needs a broken terminal
    crossterm::execute!(io::stdout(), LeaveAlternateScreen) // coverage: off - same
} // coverage: off - same
// coverage: off - the instantiation edge lands on this line
/// The dashboard's screen back after an exec that failed to replace the // coverage: off - same
/// process. // coverage: off - same
#[rustfmt::skip]
fn restore_terminal() -> io::Result<()> { // coverage: off - a resume needs a real terminal
    use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode}; // coverage: off - same
    enable_raw_mode()?; // coverage: off - `?` needs a broken terminal
    crossterm::execute!(io::stdout(), EnterAlternateScreen) // coverage: off - same
} // coverage: off - same
/// The collector's own loop, on its own thread: one staged pass streams
/// its snapshots through `publish`, each handed over once the previous one
/// was taken (the bounded channel paces the worker), then `interval` of
/// rest and the next pass. A dropped receiver ends the worker.
fn collect_worker(
    tx: std::sync::mpsc::SyncSender<Snapshot>,
    mut refresh: impl FnMut(&mut dyn FnMut(Snapshot) -> bool),
    interval: Duration,
) {
    loop {
        let mut alive = true;
        refresh(&mut |snapshot| {
            alive = tx.send(snapshot).is_ok();
            alive
        });
        if !alive {
            return;
        }
        std::thread::sleep(interval);
    }
} // coverage: off - same
/// What the collector channel produced since the last draw.
#[rustfmt::skip] // coverage: off - the unexecuted instantiation's region edge
enum Feed { // coverage: off - the unexecuted instantiation's region edge
    /// Nothing new. // coverage: off - same
    Idle,
    /// A finished snapshot, ready to swap in. // coverage: off - same
    Snapshot(Snapshot), // coverage: off - same
    /// The collector thread is gone; what is on screen is the last snapshot. // coverage: off - same
    Dead,
}

/// The real input path: one event per tick, or `None` when the tick expires.
fn poll_event() -> io::Result<Option<Event>> {
    match event::poll(Duration::from_millis(200)) {
        // coverage: off - the unexecuted instantiation's arm edge lands here
        Ok(true) => event::read().map(Some),
        Ok(false) => Ok(None), // coverage: off - the pty test feeds stdin EOF instantly, so the empty tick never wins
        Err(e) => Err(e),      // coverage: off - needs a broken stdin
    }
}

/// Draw, consume one event, swap in whatever the collector already
/// produced - until `q` quits. The sources are injected so the loop
/// needs no terminal and no worker: tests hand it a key script, a
/// scripted snapshot source, and scripted actions. Anything the user
/// presses is mapped to a `Key` and applied; release events and
/// unmapped codes are ignored. An Enter/`o` request the key left is
/// resolved through `act` against a fresh snapshot - a resume outcome
/// goes to `suspend`, which replaces the process in production and
/// just records in tests.
fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    mut next_snapshot: impl FnMut() -> Feed,
    mut poll: impl FnMut() -> io::Result<Option<Event>>,
    mut act: impl FnMut(&ActionRequest) -> ActionOutcome,
    mut suspend: impl FnMut(&ResumePlan) -> Result<(), String>,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    loop {
        // Snapshots the worker finished since the last draw swap in first;
        // when several queued up, the newest wins. A dead collector is
        // surfaced on the footer - the last snapshot stays on screen.
        loop {
            match next_snapshot() {
                Feed::Snapshot(snapshot) => app.refresh(snapshot),
                Feed::Dead => {
                    app.collector_stopped();
                    break; // coverage: off - the unexecuted instantiation's arm edge
                }
                Feed::Idle => break, // coverage: off - the unexecuted instantiation's arm edge
            } // coverage: off - same
        }
        terminal.draw(|f| app.render(f)).map_err(io::Error::other)?; // coverage: off - `?` needs a backend that can fail
        match app.quit() /* // coverage: off - the quit arm's edge is the unexecuted instantiation's */ {
            true => break,
            false => {}
        }
        let event = poll()?; // coverage: off - `?` needs a broken stdin
        dispatch_event(app, event);
        if let Some(request) = app.take_action() {
            match act(&request) {
                ActionOutcome::Done(message) => app.set_notice(message),
                ActionOutcome::Failed(message) => app.set_notice(Some(message)),
                // A plan replaces this process; `Ok` means the dashboard
                // is gone - unreachable in production, where exec
                // diverges or fails.
                ActionOutcome::Resume(plan) => match suspend(&plan) {
                    Ok(()) => break,
                    Err(message) => app.set_notice(Some(message)),
                },
            }
        }
    } // coverage: off - the unexecuted instantiation's region edge
    Ok(()) // coverage: off - same
}

/// One polled event applied to the app: mapped keys act; releases and
/// unmapped codes are dropped.
#[rustfmt::skip]
fn dispatch_event(app: &mut App, event: Option<Event>) {
    if let Some(Event::Key(key)) = event
        && key.kind != KeyEventKind::Release // coverage: off - the unexecuted instantiation's edge
        && let Some(mapped) = map_key(key.code) // coverage: off - same
    { // coverage: off - the unexecuted instantiation's edge lands on the brace
        app.key(mapped); // coverage: off - same
    }
}

/// Whether stdout is a terminal. The TUI cannot run on a pipe: the answer
/// `list --json` gives a pipe is JSON, and the TUI's is an error. // coverage: off - the line's zero region is an unexecuted instantiation edge
pub fn terminal_present() -> bool {
    // coverage: off - the unexecuted instantiation's entry edge
    use std::io::IsTerminal;
    io::stdout().is_terminal()
}

/// The dashboard's own pane - socket and id - when it runs inside tmux, so
/// focus observation does not mistake the dashboard for work needing
/// attention.
pub fn own_pane() -> Option<PaneRef> {
    tmux::pane_ref_from_env(
        std::env::var_os("TMUX").as_deref(),
        std::env::var("TMUX_PANE").ok().as_deref(),
    )
}

/// The collector for the configured root, store and config: `~/.claude`
/// (or `$CLAUDE_CONFIG_DIR`), `$XDG_STATE_HOME/agent-sessions` and the
/// resolved `config.toml`/env when the environment places them.
fn collector() -> Result<crate::snapshot::Collector, String> {
    let claude = crate::claude::default_root()?;
    let collector = crate::snapshot::Collector::new(claude).with_config(config::Config::load());
    Ok(
        match config::Config::state_dir(&|name| std::env::var(name).ok()) {
            Some(dir) => collector.with_store(dir),
            None => collector, // coverage: off - needs neither XDG_STATE_HOME nor HOME, which the passing path keeps
        },
    )
}

/// The store `space` on an app writes to: `None` where no state dir could
/// be placed, which is also what makes `space` inert.
fn app_store() -> Option<Store> {
    config::Config::state_dir(&|name| std::env::var(name).ok()).map(Store::open)
}

/// `agent-sessions` with no arguments: the dashboard itself.
pub fn tui() -> Result<(), String> {
    if !terminal_present() {
        return Err("the dashboard needs a terminal (piped stdout? try `list --json`)".to_owned());
    }
    let mut collector = collector()?; // coverage: off - `?` needs neither CLAUDE_CONFIG_DIR nor HOME, which the passing path keeps
    let collect = move |publish: &mut dyn FnMut(Snapshot) -> bool| {
        let runtime = crate::runtime::Runtime::observe(); // coverage: off - the closure only runs inside `run`, which needs a real terminal
        collector.collect_staged(&runtime, own_pane().as_ref(), publish) // coverage: off - same
    };
    // The event loop starts before stage 1 lands: an empty, incomplete
    // snapshot paints the frame while collection fills it in.
    let app = App::new(Snapshot::empty())
        .with_forgotten_after(config::Config::load().config.forgotten_after);
    let app = match app_store() {
        Some(store) => app.with_store(store),
        None => app, // coverage: off - needs neither XDG_STATE_HOME nor HOME, which the passing path keeps
    };
    run(app, collect).map_err(|e| e.to_string()) // coverage: off - `map_err` needs a failing terminal
}

/// `agent-sessions list --json`: the complete unfiltered snapshot.
pub fn list_json() -> Result<String, String> {
    let mut collector = collector()?;
    let runtime = crate::runtime::Runtime::observe();
    let snapshot = collector.collect(&runtime, own_pane().as_ref());
    to_json(&snapshot) // coverage: off - `list_json` runs only inside the binary
        .map_err(|e| format!("the snapshot cannot be serialized: {e}")) // coverage: off - same
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::EvidenceSource;
    use crate::runtime::{PaneSource, Provider};
    use crate::snapshot::{
        AttachmentLiveness, AttachmentRow, EvidenceRow, Landed, RepoCounts, RepoRow,
        SCHEMA_VERSION, Upstream, WorkRow,
    };
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    /// The incarnation row a fixture work row carries, `number` within
    /// its `(repo, ref_name)`.
    fn incarnation(id: &str, repo: &str, ref_name: &str, number: usize) -> IncarnationRow {
        IncarnationRow {
            id: id.to_owned(),
            number,
            repo: repo.to_owned(),
            ref_name: ref_name.to_owned(),
            first_observed_at: 1_800_000_000 - 7200,
            last_observed_at: 1_800_000_000 - 120,
            creation_head: None,
            creation_at: None,
            head: None,
            ended_at: None,
            continuity: crate::store::ContinuityEvidence::FirstObservation,
            excluded: false,
        }
    }

    /// The open touch binding a fixture conversation to `id`.
    fn touch(id: &str, repo: &str, ref_name: &str) -> crate::snapshot::TouchRow {
        crate::snapshot::TouchRow {
            incarnation_id: id.to_owned(),
            repo: repo.to_owned(),
            ref_name: ref_name.to_owned(),
            incarnation: 1,
            head: Some("aaaaaa".to_owned()),
            valid_from: 1_800_000_000 - 3600,
            valid_until: None,
            provenance: crate::store::TouchProvenance::Cwd,
            confidence: crate::store::Confidence::Exact,
        }
    }

    /// A snapshot the render path can be exercised against - known and
    /// unknown fields, all providers' row shapes, at a fixed instant.
    fn fixture() -> Snapshot {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            observed_at: 1_800_000_000,
            own_pane: None,
            complete: true,
            repos: vec![
                RepoRow {
                    id: "/repos/a/.git".to_owned(),
                    name: "a".to_owned(),
                    path: PathBuf::from("/repos/a"),
                    git: true,
                    work: 2,
                    live: 1,
                    attention: Attention::Waiting,
                    open: 2,
                    clean: 0,
                    last_activity: Some(1_800_000_000 - 120),
                    default_branch: Some("main".to_owned()),
                    remote: Some("origin".to_owned()),
                    counts: RepoCounts::default(),
                },
                RepoRow {
                    id: "/spaces/notes".to_owned(),
                    name: "notes".to_owned(),
                    path: PathBuf::from("/spaces/notes"),
                    git: false,
                    work: 1,
                    live: 0,
                    attention: Attention::None,
                    open: 1,
                    clean: 0,
                    last_activity: None,
                    default_branch: None,
                    remote: None,
                    counts: RepoCounts::default(),
                },
            ],
            work: vec![
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
                    landed: Some(Landed::No),
                    base: Some("origin/main".to_owned()),
                    windows: 1,
                    live_pids: 1,
                    live_sessions: 1,
                    past_sessions: 2,
                    last_activity: Some(1_800_000_000 - 120),
                    attention: Attention::Waiting,
                    identity: Some("i111".to_owned()),
                    incarnation: Some(incarnation("i111", "/repos/a/.git", "feat/login", 1)),
                    same_name_history: Vec::new(),
                    parked: false,
                    forge: crate::forge::WorkItem::Open,
                    pipeline: crate::forge::Pipeline::Unknown,
                    forge_label: Some("PR #191".to_owned()),
                    forge_url: Some("https://github.com/o/r/pull/191".to_owned()),
                    commits_behind: Some(0),
                    commits: None,
                    panes: Vec::new(),
                    gone: None,
                    references: Vec::new(),
                    worktree_removal: Some(crate::verdict::ActionVerdict {
                        verdict: crate::verdict::Verdict::Blocked,
                        reasons: vec!["uncommitted changes".to_owned()],
                    }),
                    branch_deletion: Some(crate::verdict::ActionVerdict {
                        verdict: crate::verdict::Verdict::Blocked,
                        reasons: vec!["3 unpushed commits".to_owned()],
                    }),
                    section: crate::snapshot::WorkSection::NeedsYou,
                    summary: "waiting: permission prompt · ↑3 ~dirty PR #191".to_owned(),
                },
                WorkRow {
                    repo: "/repos/a/.git".to_owned(),
                    repo_name: "a".to_owned(),
                    kind: WorkKind::Branch,
                    name: "feat/old".to_owned(),
                    worktree: None,
                    branch: Some("feat/old".to_owned()),
                    dirty: Some(false),
                    broken: None,
                    activities: Vec::new(),
                    conversation_summaries: Vec::new(),
                    observations: Vec::new(),
                    commits_ahead: Some(7),
                    unpushed: Some(7),
                    upstream: Upstream::NeverPushed,
                    upstream_detail: None,
                    landed: None,
                    base: None,
                    windows: 0,
                    live_pids: 0,
                    live_sessions: 0,
                    past_sessions: 0,
                    last_activity: Some(1_800_000_000 - 9 * 86400),
                    attention: Attention::None,
                    identity: Some("i222".to_owned()),
                    incarnation: Some(incarnation("i222", "/repos/a/.git", "feat/old", 1)),
                    same_name_history: Vec::new(),
                    parked: false,
                    forge: crate::forge::WorkItem::Unknown,
                    pipeline: crate::forge::Pipeline::Unknown,
                    forge_label: None,
                    forge_url: None,
                    commits_behind: None,
                    commits: None,
                    panes: Vec::new(),
                    gone: None,
                    references: Vec::new(),
                    worktree_removal: Some(crate::verdict::ActionVerdict {
                        verdict: crate::verdict::Verdict::NotApplicable,
                        reasons: vec![],
                    }),
                    branch_deletion: Some(crate::verdict::ActionVerdict {
                        verdict: crate::verdict::Verdict::Blocked,
                        reasons: vec!["7 unpushed commits".to_owned()],
                    }),
                    section: crate::snapshot::WorkSection::FollowUp,
                    summary: "unpushed 7 · no wt no remote ↑7".to_owned(),
                },
                WorkRow {
                    repo: "/spaces/notes".to_owned(),
                    repo_name: "notes".to_owned(),
                    kind: WorkKind::ProjectSpace,
                    name: "notes".to_owned(),
                    worktree: Some(PathBuf::from("/spaces/notes")),
                    branch: None,
                    dirty: None,
                    broken: None,
                    activities: Vec::new(),
                    conversation_summaries: Vec::new(),
                    observations: Vec::new(),
                    commits_ahead: None,
                    unpushed: None,
                    upstream: Upstream::NotApplicable,
                    upstream_detail: None,
                    landed: None,
                    base: None,
                    windows: 0,
                    live_pids: 0,
                    live_sessions: 0,
                    past_sessions: 0,
                    last_activity: None,
                    attention: Attention::None,
                    identity: Some("/spaces/notes".to_owned()),
                    incarnation: None,
                    same_name_history: Vec::new(),
                    parked: false,
                    forge: crate::forge::WorkItem::Unknown,
                    pipeline: crate::forge::Pipeline::Unknown,
                    forge_label: None,
                    forge_url: None,
                    commits_behind: None,
                    commits: None,
                    panes: Vec::new(),
                    gone: None,
                    references: Vec::new(),
                    worktree_removal: None,
                    branch_deletion: None,
                    section: crate::snapshot::WorkSection::FollowUp,
                    summary: "idle project · no git".to_owned(),
                },
            ],
            conversations: vec![
                ConversationRow {
                    provider: Provider::Claude,
                    session_id: "8f423bbb-1111-2222-3333-444444444444".to_owned(),
                    short_id: "8f423bbb".to_owned(),
                    title: Some("update pane labels".to_owned()),
                    state: ConversationState::Waiting,
                    state_raw: Some("waiting".to_owned()),
                    waiting_for: Some("permission prompt".to_owned()),
                    state_since: Some(1_800_000_000 - 120),
                    state_since_ms: Some((1_800_000_000 - 120) * 1000),
                    attention: Attention::Waiting,
                    attention_detail: Some("permission prompt".to_owned()),
                    attention_seq: Some(4),
                    attention_wait_ms: Some((1_800_000_000 - 120) * 1000),
                    journal_seq: Some(7),
                    last_activity: Some(1_800_000_000 - 120),
                    live: true,
                    attachment: Some(AttachmentRow {
                        pid: 4200,
                        pid_start: Some(1_790_093_933),
                        liveness: AttachmentLiveness::Instance,
                        liveness_detail: None,
                        pane: Some("workmux:@149.%162".to_owned()),
                        target: None,
                        pane_source: Some(PaneSource::Published),
                        placement_detail: None,
                        source: EvidenceSource::Published,
                        observed_at: 1_800_000_000,
                    }),
                    cwd: Some(PathBuf::from("/repos/a-login")),
                    transcript: Some(PathBuf::from(
                        "/h/.claude/projects/-r-a-l/8f423bbb-1111-2222-3333-444444444444.jsonl",
                    )),
                    malformed_lines: Some(0),
                    resume_argv: vec![
                        "claude".to_owned(),
                        "--resume".to_owned(),
                        "8f423bbb-1111-2222-3333-444444444444".to_owned(),
                    ],
                    latest_prompt: Some("rename pane titles".to_owned()),
                    latest_reply: Some("renamed three panes".to_owned()),
                    repo: Some("/repos/a/.git".to_owned()),
                    worktree: Some(PathBuf::from("/repos/a-login")),
                    branch: Some("feat/login".to_owned()),
                    touches: vec![touch("i111", "/repos/a/.git", "feat/login")],
                    current_incarnation: Some("i111".to_owned()),
                    started_at: Some(1_800_000_000 - 7200),
                    related: Vec::new(),
                    evidence: EvidenceRow::default(),
                },
                ConversationRow {
                    provider: Provider::Claude,
                    session_id: "02aa0bbb-1111-2222-3333-444444444444".to_owned(),
                    short_id: "02aa0bbb".to_owned(),
                    title: None,
                    state: ConversationState::Idle,
                    state_raw: Some("idle".to_owned()),
                    waiting_for: None,
                    state_since: Some(1_800_000_000 - 3600),
                    state_since_ms: Some((1_800_000_000 - 3600) * 1000),
                    attention: Attention::None,
                    attention_detail: None,
                    attention_seq: None,
                    attention_wait_ms: None,
                    journal_seq: None,
                    last_activity: Some(1_800_000_000 - 3600),
                    live: false,
                    attachment: None,
                    cwd: Some(PathBuf::from("/repos/a-login")),
                    transcript: Some(PathBuf::from("/h/.claude/projects/-r-a-l/t.jsonl")),
                    malformed_lines: Some(2),
                    resume_argv: vec![
                        "claude".to_owned(),
                        "--resume".to_owned(),
                        "02aa0bbb-1111-2222-3333-444444444444".to_owned(),
                    ],
                    latest_prompt: None,
                    latest_reply: None,
                    repo: Some("/repos/a/.git".to_owned()),
                    worktree: Some(PathBuf::from("/repos/a-login")),
                    branch: Some("feat/login".to_owned()),
                    touches: vec![touch("i111", "/repos/a/.git", "feat/login")],
                    current_incarnation: Some("i111".to_owned()),
                    started_at: Some(1_800_000_000 - 7200),
                    related: Vec::new(),
                    evidence: EvidenceRow::default(),
                },
                ConversationRow {
                    provider: Provider::Claude,
                    session_id: "33cc0bbb-1111-2222-3333-444444444444".to_owned(),
                    short_id: "33cc0bbb".to_owned(),
                    title: Some("untitled".to_owned()),
                    state: ConversationState::Unknown,
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
                    resume_argv: Vec::new(),
                    latest_prompt: None,
                    latest_reply: None,
                    repo: None,
                    worktree: None,
                    branch: None,
                    touches: Vec::new(),
                    current_incarnation: None,
                    started_at: None,
                    related: Vec::new(),
                    evidence: EvidenceRow::default(),
                },
            ],
            errors: vec![],
            stale_sockets: 0, // coverage: off - the unexecuted instantiation's region edge
        }
    }

    /// Render `app` at `w`×`h` into a text buffer for assertions.
    #[rustfmt::skip] // coverage: off - the unexecuted instantiation's entry edge
    fn render_to(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h); // coverage: off - the unexecuted instantiation's region edge
        let mut terminal = Terminal::new(backend).unwrap(); // coverage: off - the panic edge is a failed assertion
        terminal.draw(|f| app.render(f)).unwrap(); // coverage: off - same
        let buffer = terminal.backend().buffer();
        let mut out = String::new(); // coverage: off - the unexecuted instantiation's region edge
        for y in 0..h { // coverage: off - same
            let line: String = (0..w).map(|x| buffer[(x, y)].symbol()).collect(); // coverage: off - same
            out.push_str(&line);
            out.push('\n');
        } // coverage: off - same
        out
    } // coverage: off - the unexecuted instantiation's exit edge
    // coverage: off - the instantiation edge lands on this line
    /// Feed `keys` into `app` - a scripted session, not a terminal.
    fn press(app: &mut App, keys: &[Key]) {
        // coverage: off - same
        for &key in keys {
            app.key(key);
        } // coverage: off - same
    } // coverage: off - same

    #[test]
    fn a_refresh_keeps_the_selection_on_its_record_not_its_index() {
        const IDLE_ID: &str = "02aa0bbb-1111-2222-3333-444444444444";
        let mut app = App::new(fixture());
        // Select a row on every list: repo "a", work "feat/login", and the
        // idle conversation (feat/old is a branch-only row and scopes no
        // conversations).
        press(
            &mut app,
            &[
                Key::Char('1'),
                Key::Char('j'),
                Key::Char('2'),
                Key::Char('j'),
            ],
        );
        press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char('j')]);
        assert_eq!(app.cursor, [1, 1, 2]);

        // The next collect re-sorts everything: the repo rows trade places,
        // the unknown conversation jumps to the front.
        let mut next = fixture();
        next.repos.swap(0, 1);
        next.conversations.swap(0, 2);
        app.refresh(next);
        let view = app.view();
        // The repo cursor still names "a", under whose scope the work
        // selection still names "feat/login".
        let cursor = app.cursor[list_index(List::Repos)];
        let Some(Row::Repo(r)) = view.repos.get(cursor - 1) else {
            panic!("cursor lands on a repo row"); // coverage: off - failure path
        };
        assert_eq!(r.name, "a");
        let cursor = app.cursor[list_index(List::Work)];
        let Some(Row::Work(w)) = view.work.get(cursor - 1) else {
            panic!("cursor lands on a work row"); // coverage: off - failure path
        };
        assert_eq!(w.name, "feat/login");
        let cursor = app.cursor[list_index(List::Conversations)];
        let Some(Row::Conversation(c)) = view.conversations.get(cursor - 1) else {
            panic!("cursor lands on a conversation row"); // coverage: off - failure path
        };
        assert_eq!(c.session_id, IDLE_ID);

        // A record that vanished drops the cursor to `all`, which widens
        // the scope rather than silently retargeting a different row.
        let mut next = fixture();
        next.conversations.retain(|c| c.session_id != IDLE_ID);
        app.refresh(next);
        assert_eq!(app.cursor[list_index(List::Conversations)], 0);
    }

    /// Every detail and evidence body line, for every target the lists
    /// offer, fits the width it was built for. The body is a list of
    /// `Line`s the renderer would otherwise clip silently, so the width
    /// is asserted on the lines themselves, not on a rendered buffer that
    /// is always exactly as wide as the terminal.
    #[test]
    fn every_detail_and_evidence_line_fits_its_width() {
        let mut snapshot = fixture();
        let long = "ペインのラベルを更新する ".repeat(12);
        let path = PathBuf::from(format!("/repos/{}", "deeply-nested-segment/".repeat(10)));
        for c in &mut snapshot.conversations {
            c.title = Some(long.clone());
            c.latest_prompt = Some(format!("{long}\n\t{long}"));
            c.latest_reply = Some(long.clone());
            c.waiting_for = Some(long.clone());
            c.cwd = Some(path.clone());
            c.evidence.claims.push(crate::attention::ClaimRow {
                source: crate::attention::ClaimSource::Journal,
                exec: crate::store::Exec::Busy,
                observed_ms: 1_800_000_000_000,
                since_ms: Some(1_800_000_000_000),
                seq: Some(7),
                detail: Some(long.clone()),
                outcome: crate::attention::ClaimOutcome::Outranked,
                note: Some(long.clone()),
            });
            c.evidence.rejected.push(crate::store::RejectedRecord {
                conversation: c.session_id.clone(),
                seq: 3,
                at_ms: 1_800_000_000_000,
                pseq: Some(1),
                native: long.clone(),
                reason: long.clone(),
            });
        }
        for w in &mut snapshot.work {
            w.worktree = Some(path.clone());
            w.summary = long.clone();
            w.gone = Some(long.clone());
            w.references = vec![crate::snapshot::ReferenceRow {
                kind: ReferenceKind::Pane,
                label: long.clone(),
            }];
        }
        let mut app = App::new(snapshot);
        app.history = true;
        let mut checked = 0;
        for list in [List::Repos, List::Work, List::Conversations] {
            app.detail_list = list;
            let rows = app.view().rows(list).len();
            for cursor in 0..=rows {
                app.cursor[list_index(list)] = cursor;
                let view = app.view();
                for width in [2usize, 9, 53, 198] {
                    for line in app
                        .detail_lines(&view, width)
                        .into_iter()
                        .chain(app.evidence_lines(&view, width))
                    {
                        assert!(line.width() <= width, "{width}: {line:?}");
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 0);
    }

    #[test]
    fn a_double_width_label_never_overflows_its_row() {
        let app = App::new(fixture());
        for width in [20u16, 55] {
            let line = app.render_row(
                width,
                &RowCells {
                    glyph: "!",
                    label: "ペインのラベルを更新する長いタイトル",
                    middle: "日本語 · 要約",
                    age: "2m",
                    selected: false,
                    dim_label: false,
                },
            );
            assert!(line.width() <= width as usize, "{line:?}");
        }
    }

    #[test]
    fn the_shell_renders_at_55_and_200_columns() {
        for width in [55u16, 200] {
            let app = App::new(fixture());
            let text = render_to(&app, width, 24);
            // The four-panel frame is there with no horizontal scroll -
            // every buffer line is exactly `width` cells wide. A buffer
            // cannot show clipping; row and detail line widths are asserted
            // on the built lines instead.
            for line in text.lines() {
                assert_eq!(line.chars().count(), width as usize, "{line}");
            }
            assert!(text.contains("[1] Repos"), "{text}");
            assert!(text.contains("[2] Work"), "{text}");
            assert!(text.contains("[3] Conversations"), "{text}");
            assert!(text.contains("[4]"), "{text}");
            // Known and unknown fields both render: `?` where the evidence
            // is absent, never a plausible default.
            assert!(text.contains("?"), "{text}");
            // The full hints fit at 200 columns; the compact footer still
            // ends `? keys | q quit` at 55.
            assert!(text.contains("q quit"), "{text}");
        }
        let app = App::new(fixture());
        assert!(render_to(&app, 200, 24).contains("? keys | q quit"));
        // The medium tier carries the compact summary and the age too, not
        // just the label: at 90 columns the left lists are ~34 cells wide.
        let text = render_to(&app, 90, 24);
        assert!(text.contains("unpushed 7"), "{text}");
        assert!(text.contains("2m"), "{text}");
    }

    #[test]
    fn attention_sections_and_glyphs_render_at_55_and_200_columns() {
        // The fixture's waiting conversation makes `feat/login` a `Needs
        // you` row: `!` on the work row and the conversation, the section
        // header leading the list, the reason in the summary.
        for width in [55u16, 200] {
            let app = App::new(fixture());
            let text = render_to(&app, width, 24);
            assert!(text.contains("Needs you"), "{text}");
            assert!(text.contains("! a/feat/lo"), "{text}");
            // [1]'s `all` row rolls the repos' attention up; the other
            // lists' `all` rows carry no glyph.
            assert_eq!(text.matches("! all").count(), 1, "{text}");
            assert!(text.contains("! 8f423bbb"), "{text}");
            let waiting = text
                .lines()
                .position(|l| l.contains("Needs you"))
                .expect("the section");
            let old = text
                .lines()
                .position(|l| l.contains("feat/old"))
                .expect("the flat row");
            assert!(waiting < old, "{text}");
        }
        // The reason text fits only where the pane can afford it: 200
        // columns show it whole, 55 clips it inside the narrow list.
        let text = render_to(&App::new(fixture()), 200, 24);
        assert!(text.contains("waiting: permission prompt"), "{text}");
        let text = render_to(&App::new(fixture()), 55, 24);
        assert!(!text.contains("permission prompt"), "{text}");
        // A retained error beside a live `busy`: ✗, `error · working`, and
        // the row still under `Needs you`.
        let mut snapshot = fixture();
        snapshot.conversations[0].attention = Attention::Error;
        snapshot.conversations[0].attention_detail = Some("StopFailure".to_owned());
        snapshot.conversations[0].state = ConversationState::Busy;
        snapshot.work[0].attention = Attention::Error;
        snapshot.work[0].summary = "error: StopFailure · working · ↑3 ~dirty".to_owned();
        let app = App::new(snapshot);
        for width in [55u16, 200] {
            let text = render_to(&app, width, 24);
            assert!(text.contains("✗ a/feat/lo"), "{text}");
            assert!(text.contains("✗ 8f423bbb"), "{text}");
        }
        let text = render_to(&app, 200, 24);
        assert!(text.contains("error: StopFailure · working"), "{text}");
        assert!(text.contains("claude · working"), "{text}");
        // A completed-unseen latch: ✓ under `Needs you`.
        let mut snapshot = fixture();
        snapshot.conversations[0].attention = Attention::CompletedUnseen;
        snapshot.conversations[0].attention_detail = None;
        snapshot.conversations[0].state = ConversationState::Idle;
        snapshot.work[0].attention = Attention::CompletedUnseen;
        snapshot.work[0].summary = "done · ↑3 ~dirty".to_owned();
        let app = App::new(snapshot);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("✓ a/feat/lo"), "{text}");
        assert!(text.contains("✓ 8f423bbb"), "{text}");
        // An `Active` row: ● under its own header.
        let mut snapshot = fixture();
        snapshot.conversations[0].attention = Attention::Working;
        snapshot.conversations[0].attention_detail = None;
        snapshot.conversations[0].state = ConversationState::Busy;
        snapshot.work[0].attention = Attention::Working;
        snapshot.work[0].section = crate::snapshot::WorkSection::Active;
        snapshot.work[0].summary = "working · ↑3 ~dirty".to_owned();
        let mut app = App::new(snapshot);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("Active"), "{text}");
        assert!(text.contains("● a/feat/login"), "{text}");
        assert!(text.contains("working · ↑3 ~dirty"), "{text}");
        assert!(text.contains("● 8f423bbb"), "{text}");
        // The detail header reads `working` for a Working latch, and the
        // retained-error-over-waiting shape keeps both words.
        app.key(Key::Char('3'));
        app.key(Key::Char('j'));
        let text = render_to(&app, 200, 24);
        assert!(text.contains("working"), "{text}");
        let mut snapshot = fixture();
        snapshot.conversations[0].state = ConversationState::Waiting;
        snapshot.conversations[0].attention = Attention::Error;
        snapshot.conversations[0].attention_detail = Some("StopFailure".to_owned());
        let mut app = App::new(snapshot);
        app.key(Key::Char('3'));
        app.key(Key::Char('j'));
        let text = render_to(&app, 200, 24);
        assert!(text.contains("error: StopFailure · waiting"), "{text}");
    }

    #[test]
    fn space_writes_seen_state_and_not_busy_marks() {
        let dir = std::env::temp_dir().join(format!("as-space-{}", std::process::id()));
        let store = crate::store::Store::open(dir.clone());
        let mut app = App::new(fixture()).with_store(store);
        // Cursor on the waiting conversation: `space` writes seen-state
        // through its `attention_seq` and the live wait it shows.
        press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char(' ')]);
        let seen = crate::store::Store::open(dir.clone()).load().seen;
        let key = store::conversation_key("claude", "8f423bbb-1111-2222-3333-444444444444");
        let wait = Some((1_800_000_000 - 120) * 1000);
        assert_eq!(
            seen.get(&key).copied(),
            Some(store::Seen {
                seq: 4,
                wait_ms: wait
            }),
            "{seen:?}"
        );
        // A wait only the provider published has no sequence: `space`
        // acknowledges its episode alone.
        let other = store::conversation_key("claude", "published-wait");
        let mut snapshot = fixture();
        snapshot.conversations[0].session_id = "published-wait".to_owned();
        snapshot.conversations[0].attention_seq = None;
        app = App::new(snapshot).with_store(crate::store::Store::open(dir.clone()));
        press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char(' ')]);
        let seen = crate::store::Store::open(dir.clone()).load().seen;
        assert_eq!(
            seen.get(&other).copied(),
            Some(store::Seen {
                seq: 0,
                wait_ms: wait
            }),
            "{seen:?}"
        );
        // Cursor on a plain `Busy` conversation with nothing to ack: `space`
        // writes the not-busy mark naming the `Busy`'s `effective_since`.
        let mut snapshot = fixture();
        snapshot.conversations[0].attention = Attention::Working;
        snapshot.conversations[0].attention_detail = None;
        snapshot.conversations[0].attention_seq = None;
        snapshot.conversations[0].attention_wait_ms = None;
        snapshot.conversations[0].state = ConversationState::Busy;
        app = App::new(snapshot).with_store(crate::store::Store::open(dir.clone()));
        press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char(' ')]);
        let marks = crate::store::Store::open(dir.clone()).load().marks;
        let mark = marks.get(&key).expect("a mark landed");
        assert_eq!(mark.since_ms, (1_800_000_000 - 120) * 1000);
        assert_eq!(mark.seq, 7);
        // A `Busy` only the provider published - no hook event yet - is
        // marked too, at sequence zero: any first event supersedes it.
        let mut snapshot = fixture();
        snapshot.conversations[0].attention = Attention::Working;
        snapshot.conversations[0].attention_detail = None;
        snapshot.conversations[0].attention_seq = None;
        snapshot.conversations[0].attention_wait_ms = None;
        snapshot.conversations[0].journal_seq = None;
        snapshot.conversations[0].state = ConversationState::Busy;
        app = App::new(snapshot).with_store(crate::store::Store::open(dir.clone()));
        press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char(' ')]);
        let marks = crate::store::Store::open(dir.clone()).load().marks;
        assert_eq!(marks.get(&key).map(|m| m.seq), Some(0), "{marks:?}");

        // Every inert arm of `space`: no store, a non-list focus, the `all`
        // row, a `repos` row, and a conversation that is neither latched nor
        // Busy.
        let mut bare = App::new(fixture());
        bare.key(Key::Char(' ')); // no store: writes nothing
        let mut app = App::new(fixture()).with_store(crate::store::Store::open(dir.clone()));
        for keys in [
            vec![Key::Char('4'), Key::Char(' ')], // Detail focus: no list
            vec![
                Key::Char('1'),
                Key::Char('j'),
                Key::Char('j'),
                Key::Char(' '),
            ], // a repo row
            vec![Key::Char('2'), Key::Char(' ')], // Work's `all` row
        ] {
            press(&mut app, &keys);
        }
        // A work row's `space` acknowledges every bound conversation.
        press(&mut app, &[Key::Char('2'), Key::Char('j'), Key::Char(' ')]);
        // And a conversation that is neither latched nor Busy is skipped,
        // not miswritten.
        let mut snapshot = fixture();
        snapshot.conversations[0].attention = Attention::None;
        snapshot.conversations[0].attention_seq = None;
        snapshot.conversations[0].attention_wait_ms = None;
        snapshot.conversations[0].journal_seq = None;
        app = App::new(snapshot).with_store(crate::store::Store::open(dir.clone()));
        press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char(' ')]);
        let loaded = crate::store::Store::open(dir.clone()).load();
        assert_eq!(loaded.marks.len(), 1, "no extra mark: {:?}", loaded.marks);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_space_write_says_so_until_the_next_key() {
        // A seen-state file the store cannot carry refuses the rewrite;
        // the keypress must not look like it worked.
        let dir = std::env::temp_dir().join(format!("as-space-err-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("seen.json"), "{oops").unwrap();
        let mut app = App::new(fixture()).with_store(crate::store::Store::open(dir.clone()));
        press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char(' ')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("space: not saved"), "{text}");
        // So does a refused not-busy mark.
        std::fs::write(dir.join("marks.json"), "{oops").unwrap();
        let mut snapshot = fixture();
        snapshot.conversations[0].attention = Attention::Working;
        snapshot.conversations[0].attention_seq = None;
        snapshot.conversations[0].attention_wait_ms = None;
        snapshot.conversations[0].state = ConversationState::Busy;
        let mut app = App::new(snapshot).with_store(crate::store::Store::open(dir.clone()));
        press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Char(' ')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("space: not saved"), "{text}");
        // The next key clears it: the footer is hints again.
        press(&mut app, &[Key::Char('k')]);
        let text = render_to(&app, 200, 24);
        assert!(!text.contains("space: not saved"), "{text}");
        assert!(text.contains("q quit"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_work_row_space_batches_one_kind_of_authored_action() {
        // Both conversations bound to the work row carry pending
        // attention: one `space` acknowledges every bound key, and the
        // pending latch keeps the keypress acknowledgements only.
        let dir = std::env::temp_dir().join(format!("as-space-wa-{}", std::process::id()));
        let mut snapshot = fixture();
        snapshot.conversations[1].attention = Attention::Waiting;
        snapshot.conversations[1].attention_detail = Some("permission prompt".to_owned());
        snapshot.conversations[1].attention_seq = Some(6);
        snapshot.conversations[1].attention_wait_ms = Some((1_800_000_000 - 3600) * 1000);
        let mut app = App::new(snapshot).with_store(crate::store::Store::open(dir.clone()));
        press(&mut app, &[Key::Char('2'), Key::Char('j'), Key::Char(' ')]);
        let loaded = crate::store::Store::open(dir.clone()).load();
        let first = store::conversation_key("claude", "8f423bbb-1111-2222-3333-444444444444");
        let second = store::conversation_key("claude", "02aa0bbb-1111-2222-3333-444444444444");
        assert_eq!(
            loaded.seen.get(&first).copied(),
            Some(store::Seen {
                seq: 4,
                wait_ms: Some((1_800_000_000 - 120) * 1000)
            }),
            "{:?}",
            loaded.seen
        );
        assert_eq!(
            loaded.seen.get(&second).copied(),
            Some(store::Seen {
                seq: 6,
                wait_ms: Some((1_800_000_000 - 3600) * 1000)
            }),
            "{:?}",
            loaded.seen
        );
        assert!(loaded.marks.is_empty(), "{:?}", loaded.marks);
        let _ = std::fs::remove_dir_all(&dir);
        // A fresh store and snapshot: both bound conversations Working
        // with nothing to acknowledge - the same keypress marks both.
        let dir = std::env::temp_dir().join(format!("as-space-wm-{}", std::process::id()));
        let mut snapshot = fixture();
        for c in &mut snapshot.conversations[..2] {
            c.attention = Attention::Working;
            c.attention_detail = None;
            c.attention_seq = None;
            c.attention_wait_ms = None;
            c.state = ConversationState::Busy;
        }
        let mut app = App::new(snapshot).with_store(crate::store::Store::open(dir.clone()));
        press(&mut app, &[Key::Char('2'), Key::Char('j'), Key::Char(' ')]);
        let loaded = crate::store::Store::open(dir.clone()).load();
        assert!(loaded.seen.is_empty(), "{:?}", loaded.seen);
        assert_eq!(
            loaded.marks.get(&first).map(|m| (m.since_ms, m.seq)),
            Some(((1_800_000_000 - 120) * 1000, 7)),
            "{:?}",
            loaded.marks
        );
        assert_eq!(
            loaded.marks.get(&second).map(|m| (m.since_ms, m.seq)),
            Some(((1_800_000_000 - 3600) * 1000, 0)),
            "{:?}",
            loaded.marks
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_filtered_list_does_not_reserve_height_for_hidden_rows() {
        let mut app = App::new(fixture());
        press(
            &mut app,
            &[
                Key::Char('1'),
                Key::Char('/'),
                Key::Char('n'),
                Key::Char('o'),
                Key::Char('t'),
                Key::Enter,
            ],
        );
        let text = render_to(&app, 90, 24);
        // `all` plus one repo plus two borders: the filtered-down panel is
        // four rows, so [2] starts on row 4 rather than row 5.
        let y = text
            .lines()
            .position(|l| l.contains("[2] Work"))
            .expect("the work panel");
        assert_eq!(y, 4, "{text}");
    }

    #[test]
    fn cursor_scopes_the_lists_below_it() {
        let mut app = App::new(fixture());
        // [1] on `a`: only that repo's work remains below.
        press(&mut app, &[Key::Char('1'), Key::Char('j')]);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("feat/login"), "{text}");
        assert!(text.contains("feat/old"), "{text}");
        assert!(!text.contains("notes -"), "{text}");
        // [2] on a work row: [3] shows only that row's conversations.
        press(&mut app, &[Key::Char('2'), Key::Char('j')]);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("8f423bbb"), "{text}");
        assert!(!text.contains("33cc0bbb"), "{text}");
        // Work cursor back on `all`: still repo-scoped - [1] sits on `a`.
        press(&mut app, &[Key::Char('2'), Key::Char('k')]);
        let text = render_to(&app, 55, 24);
        assert!(!text.contains("33cc0bbb"), "{text}");
        // Repo cursor back on `all` too: the global inbox is whole again.
        press(&mut app, &[Key::Char('1'), Key::Char('k')]);
        let text = render_to(&app, 55, 24);
        // The pane is shorter than the list, so `33cc` is below the fold
        // until the cursor scrolls down to it.
        assert!(!text.contains("33cc0bbb"), "{text}");
        press(
            &mut app,
            &[
                Key::Char('3'),
                Key::Char('j'),
                Key::Char('j'),
                Key::Char('j'),
            ],
        );
        let text = render_to(&app, 55, 24);
        assert!(text.contains("33cc0bbb"), "{text}");
    }

    #[test]
    fn navigation_scoping_and_filtering_are_scripted() {
        let mut app = App::new(fixture());
        // Tab cycles 1→2→3→4→1; 1-4 jump straight.
        press(&mut app, &[Key::Char('1'), Key::Tab]);
        assert_eq!(app.focus, Pane::Work);
        press(&mut app, &[Key::Char('4'), Key::Char('2')]);
        assert_eq!(app.focus, Pane::Work);
        // j/k move and clamp.
        press(&mut app, &[Key::Char('k')]);
        assert_eq!(app.cursor[1], 0);
        press(
            &mut app,
            &[
                Key::Char('j'),
                Key::Char('j'),
                Key::Char('j'),
                Key::Char('j'),
            ],
        );
        assert_eq!(app.cursor[1], 3); // all + 3 rows, clamped at the last index
        // `/` filters the focused list by text.
        press(
            &mut app,
            &[
                Key::Char('1'),
                Key::Char('/'),
                Key::Char('n'),
                Key::Char('o'),
                Key::Char('t'),
                Key::Enter,
            ],
        );
        let text = render_to(&app, 55, 24);
        assert!(text.contains("notes"), "{text}");
        // The `a` repo row filtered out; only `notes` remains.
        assert!(!text.contains("● a"), "{text}");
        // A fresh `/` starts from the committed filter; Esc cancels the edit.
        press(&mut app, &[Key::Char('/'), Key::Char('x'), Key::Esc]);
        assert_eq!(app.filter_raw[0], "not");
        // age: narrows by time - the old work row drops out of [2].
        let mut app = App::new(fixture());
        press(
            &mut app,
            &[
                Key::Char('2'),
                Key::Char('/'),
                Key::Char('a'),
                Key::Char('g'),
                Key::Char('e'),
                Key::Char(':'),
                Key::Char('1'),
                Key::Char('h'),
                Key::Enter,
            ],
        );
        let text = render_to(&app, 55, 24);
        assert!(text.contains("feat/login"), "{text}");
        assert!(!text.contains("feat/old"), "{text}");
    }

    #[test]
    fn help_and_quit_work() {
        let mut app = App::new(fixture());
        press(&mut app, &[Key::Char('?')]);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("help"), "{text}");
        assert!(text.contains("1-4 focus"), "{text}");
        press(&mut app, &[Key::Esc]);
        assert!(!app.help);
        press(&mut app, &[Key::Char('q')]);
        assert!(app.quit());
    }

    #[test]
    fn unshipped_keys_stay_inert() {
        let mut app = App::new(fixture());
        let before = render_to(&app, 55, 24);
        press(
            &mut app,
            &[
                Key::Char('p'),
                Key::Char('x'),
                Key::Char('d'),
                Key::Char('D'),
                Key::Char('c'),
                Key::Char('i'),
                Key::Char(' '),
            ],
        );
        assert_eq!(render_to(&app, 55, 24), before);
        assert!(!app.quit());
    }

    /// Which rows `enter` and `o` answer: concrete rows leave a request,
    /// `all`, repo, history and gone rows - and the detail pane - stay
    /// inert.
    #[test]
    fn enter_and_o_request_the_row_under_the_cursor() {
        let mut app = App::new(fixture());

        // [3] on a conversation: Enter requests provider plus full id.
        press(&mut app, &[Key::Char('3'), Key::Char('j'), Key::Enter]);
        assert_eq!(
            app.take_action(),
            Some(ActionRequest::EnterConversation {
                provider: Provider::Claude,
                session_id: "8f423bbb-1111-2222-3333-444444444444".to_owned(),
            })
        );
        assert!(app.take_action().is_none(), "a request is taken once");

        // [2] on the active `feat/login` work row: Enter and `o` request
        // the same stable work key the cursor tracks; `o` also carries
        // the row's recorded forge verdict and URL.
        let fixture_work = fixture().work[0].clone();
        let key = work_key(&fixture_work);
        press(&mut app, &[Key::Char('2'), Key::Char('j')]);
        app.key(Key::Enter);
        assert_eq!(
            app.take_action(),
            Some(ActionRequest::EnterWork { key: key.clone() })
        );
        app.key(Key::Char('o'));
        assert_eq!(
            app.take_action(),
            Some(ActionRequest::OpenForge {
                key,
                item: fixture_work.forge,
                url: fixture_work.forge_url.clone(),
            })
        );

        // `o` away from the work list, `o` and `enter` on `all`, a repo
        // row and the detail pane are all inert.
        press(&mut app, &[Key::Char('3'), Key::Char('o')]);
        assert!(app.take_action().is_none(), "o is a work-row key");
        press(&mut app, &[Key::Char('2'), Key::Char('k')]);
        app.key(Key::Enter);
        app.key(Key::Char('o'));
        assert!(app.take_action().is_none(), "the all row carries nothing");
        press(&mut app, &[Key::Char('1'), Key::Char('j'), Key::Enter]);
        assert!(app.take_action().is_none(), "a repo row carries nothing");
        press(&mut app, &[Key::Char('4'), Key::Enter]);
        assert!(app.take_action().is_none(), "the detail pane is inert");

        // A gone work row is read-only for both keys.
        let mut app = App::new(fixture());
        app.snapshot.work[0].gone = Some("branch deleted".to_owned());
        press(&mut app, &[Key::Char('2'), Key::Char('j')]);
        app.key(Key::Enter);
        app.key(Key::Char('o'));
        assert!(app.take_action().is_none(), "gone work carries no action");

        // And an excluded history row is read-only too.
        let mut app = App::new(fixture());
        app.snapshot.work[0].same_name_history.push(incarnation(
            "i000",
            "/repos/a/.git",
            "feat/login",
            0,
        ));
        app.history = true;
        press(&mut app, &[Key::Char('2'), Key::Char('j'), Key::Char('j')]);
        app.key(Key::Enter);
        app.key(Key::Char('o'));
        assert!(app.take_action().is_none(), "excluded history is inert");
    }

    /// Enter leaves a request; the loop resolves it through the injected
    /// seams - `act` answers Done/Failed into the footer, a resume plan
    /// goes to `suspend`.
    #[test]
    fn the_loop_routes_actions_to_the_injected_executor() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let mut app = App::new(fixture());
        press(&mut app, &[Key::Char('3'), Key::Char('j')]);
        // The poll script dies after Enter: a notice survives to assert
        // only while no later key has cleared it.
        let mut events = std::collections::VecDeque::from([
            Ok(Some(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))),
            Err(io::Error::other("stdin died")),
        ]);
        let mut poll = move || events.pop_front().unwrap_or(Ok(None));
        let mut asked = Vec::new();
        let mut act = |request: &ActionRequest| -> ActionOutcome {
            asked.push(format!("{request:?}"));
            ActionOutcome::Done(Some("did it".to_owned()))
        };
        let mut suspend =
            |_: &ResumePlan| -> Result<(), String> { unreachable!("no resume in this script") }; // coverage: off - the script never reaches it
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let result = run_loop(
            &mut terminal,
            &mut app,
            &mut || Feed::Idle,
            &mut poll,
            &mut act,
            &mut suspend,
        );
        assert!(result.is_err(), "a dead stdin ends the loop");
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].contains("8f423bbb-1111-2222-3333-444444444444"),
            "{asked:?}"
        );
        assert_eq!(app.notice.as_deref(), Some("did it"));

        // A failure lands on the footer too, and the loop runs on to `q`.
        let mut app = App::new(fixture());
        press(&mut app, &[Key::Char('3'), Key::Char('j')]);
        let mut events = std::collections::VecDeque::from([
            Ok(Some(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))),
            Err(io::Error::other("stdin died")),
        ]);
        let mut poll = move || events.pop_front().unwrap_or(Ok(None));
        let mut act =
            |_: &ActionRequest| -> ActionOutcome { ActionOutcome::Failed("nope".to_owned()) };
        let mut suspend =
            |_: &ResumePlan| -> Result<(), String> { unreachable!("no resume in this script") }; // coverage: off - the script never reaches it
        let result = run_loop(
            &mut terminal,
            &mut app,
            &mut || Feed::Idle,
            &mut poll,
            &mut act,
            &mut suspend,
        );
        assert!(result.is_err());
        assert_eq!(app.notice.as_deref(), Some("nope"));

        // A resume plan goes to `suspend`: a recorded replacement ends
        // the loop outright and its executor writes the plan's
        // acknowledgement, a failed one preserves seen-state, leaves a
        // footer notice and runs on.
        let key = store::conversation_key("claude", "8f423bbb-1111-2222-3333-444444444444");
        for replaced in [true, false] {
            let dir =
                std::env::temp_dir().join(format!("as-resume-{}-{replaced}", std::process::id()));
            let mut app = App::new(fixture());
            press(&mut app, &[Key::Char('3'), Key::Char('j')]);
            // A replaced process breaks the loop outright; a failed one
            // is followed by a dead stdin, so its notice survives to
            // assert.
            let mut events = std::collections::VecDeque::from([
                Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )))),
                Err(io::Error::other("stdin died")),
            ]);
            let mut poll = move || events.pop_front().unwrap_or(Ok(None));
            let plan = ResumePlan {
                executable: std::ffi::OsString::from("claude"),
                argv: vec![
                    std::ffi::OsString::from("--resume"),
                    std::ffi::OsString::from("id"),
                ],
                cwd: PathBuf::from("/tmp"),
                acknowledgement: Some((key.clone(), 3, Some(7))),
            };
            let store = crate::store::Store::open(dir.clone());
            let mut suspended = Vec::new();
            let mut act =
                move |_: &ActionRequest| -> ActionOutcome { ActionOutcome::Resume(plan.clone()) };
            let mut suspend = |plan: &ResumePlan| -> Result<(), String> {
                suspended.push(plan.executable.clone());
                if replaced {
                    // The injected executor stands where exec would be:
                    // a successful replacement records the acknowledgement.
                    let (key, seq, wait_ms) = plan.acknowledgement.clone().expect("a pending ack");
                    store.acknowledge(&key, seq, wait_ms).expect("ack lands");
                    Ok(())
                } else {
                    Err("could not exec".to_owned())
                }
            };
            let result = run_loop(
                &mut terminal,
                &mut app,
                &mut || Feed::Idle,
                &mut poll,
                &mut act,
                &mut suspend,
            );
            if replaced {
                result.unwrap();
            } else {
                assert!(result.is_err(), "the dead stdin ends the loop");
            }
            assert_eq!(suspended, [std::ffi::OsString::from("claude")]);
            let seen = crate::store::Store::open(dir.clone()).load().seen;
            if replaced {
                assert!(!app.quit(), "the process is gone, not quit");
                assert_eq!(
                    seen.get(&key).copied(),
                    Some(store::Seen {
                        seq: 3,
                        wait_ms: Some(7)
                    })
                );
            } else {
                assert_eq!(app.notice.as_deref(), Some("could not exec"));
                assert!(seen.is_empty(), "a failed resume acknowledges nothing");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn the_context_line_spells_one_touch_many_touches_and_none() {
        let touch = |id: &str, repo: &str, name: &str, n: usize| crate::snapshot::TouchRow {
            incarnation_id: id.to_owned(),
            repo: repo.to_owned(),
            ref_name: name.to_owned(),
            incarnation: n,
            head: Some("aaaaaa".to_owned()),
            valid_from: 1_000,
            valid_until: None,
            provenance: crate::store::TouchProvenance::Cwd,
            confidence: crate::store::Confidence::Exact,
        };
        // One touch: `repo · branch#N · worktree`.
        let mut c = fixture().conversations[0].clone();
        assert_eq!(
            conversation_context(&c, None).as_deref(),
            Some("a · feat/login#1 · a-login")
        );
        // A same-incarnation repeat - a head correction - collapses into
        // the one it corrects, never a second leg of the path.
        c.touches
            .push(touch("i111", "/repos/a/.git", "feat/login", 1));
        assert_eq!(
            conversation_context(&c, None).as_deref(),
            Some("a · feat/login#1 · a-login")
        );
        // Two incarnations one repo: the ordered path `a#N → b#M`.
        c.touches
            .push(touch("i222", "/repos/a/.git", "feat/old", 2));
        assert_eq!(
            conversation_context(&c, None).as_deref(),
            Some("feat/login#1 → feat/old#2")
        );
        // A path crossing repos qualifies each leg with its repo.
        c.touches.push(touch("i9", "/repos/b/.git", "b", 1));
        assert_eq!(
            conversation_context(&c, None).as_deref(),
            Some("a/feat/login#1 → a/feat/old#2 → b/b#1")
        );
        // A repo scope keeps only that repo's legs.
        assert_eq!(
            conversation_context(&c, Some("/repos/a/.git")).as_deref(),
            Some("feat/login#1 → feat/old#2")
        );
        assert_eq!(
            conversation_context(&c, Some("/repos/b/.git")).as_deref(),
            Some("b · b#1 · a-login")
        );
        // A touch without a worktree resolves the same, shorter.
        let mut c = fixture().conversations[0].clone();
        c.worktree = None;
        assert_eq!(
            conversation_context(&c, None).as_deref(),
            Some("a · feat/login#1")
        );
        // No touch at all: the resolved identity is the context, with or
        // without a worktree.
        let mut bare = fixture().conversations[2].clone();
        bare.repo = Some("/repos/a/.git".to_owned());
        bare.branch = Some("feat".to_owned());
        bare.worktree = None;
        assert_eq!(
            conversation_context(&bare, None).as_deref(),
            Some("a · feat")
        );
        bare.worktree = Some(PathBuf::from("/repos/a-feat"));
        assert_eq!(
            conversation_context(&bare, None).as_deref(),
            Some("a · feat · a-feat")
        );
        // And with nothing resolved, no line at all.
        let bare = fixture().conversations[2].clone();
        assert_eq!(conversation_context(&bare, None), None);
        // The label helpers refuse guesses: a nameless path renders its
        // whole spelling rather than a fabricated basename.
        assert_eq!(repo_label("/"), "/");
        assert_eq!(path_label(Path::new("/")), "/");
    }

    #[test]
    fn a_selected_conversation_keeps_its_context_line_in_view() {
        // The last row carries a two-line item: scrolling to it must
        // keep its context line, not just its primary, inside the pane.
        let mut snap = fixture();
        let last = &mut snap.conversations[2];
        last.touches = vec![crate::snapshot::TouchRow {
            incarnation_id: "i9".to_owned(),
            repo: "/repos/a/.git".to_owned(),
            ref_name: "feat".to_owned(),
            incarnation: 1,
            head: Some("aaaaaa".to_owned()),
            valid_from: 1_000,
            valid_until: None,
            provenance: crate::store::TouchProvenance::Cwd,
            confidence: crate::store::Confidence::Exact,
        }];
        last.worktree = Some(PathBuf::from("/repos/a-wt"));
        let mut app = App::new(snap);
        press(
            &mut app,
            &[
                Key::Char('3'),
                Key::Char('j'),
                Key::Char('j'),
                Key::Char('j'),
            ],
        );
        let text = render_to(&app, 55, 22);
        assert!(text.contains("33cc0bbb"), "{text}");
        assert!(text.contains("a · feat#1"), "{text}");
    }

    #[test]
    fn a_detached_row_scopes_by_its_worktree() {
        let mut snap = fixture();
        let detached = &mut snap.work[1];
        detached.kind = WorkKind::Detached;
        detached.branch = None;
        detached.incarnation = None;
        detached.worktree = Some(PathBuf::from("/repos/a-det"));
        // A conversation inside the detached checkout binds by path even
        // without a touch; the worktree one does not belong to it.
        snap.conversations[1].worktree = Some(PathBuf::from("/repos/a-det"));
        snap.conversations[1].touches = Vec::new();
        let mut app = App::new(snap);
        press(&mut app, &[Key::Char('2'), Key::Char('j'), Key::Char('j')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("[3] Conversations  /repos/a-det"), "{text}");
        assert!(text.contains("02aa0bbb"), "{text}");
        assert!(!text.contains("8f423bbb"), "{text}");
    }

    #[test]
    fn every_unfilled_cell_is_a_dim_question_mark() {
        let app = App::new(fixture());
        let text = render_to(&app, 55, 24);
        // The transcript-only row has no title, state or age - it renders `?`
        // in each, not a guess.
        assert!(text.contains("?"), "{text}");
        // The project space has no git: `no git`, not fabricated counts -
        // visible at the wide tier where the middle column renders.
        assert!(render_to(&app, 200, 24).contains("no git"));
    }

    #[test]
    fn age_formats_the_column() {
        assert_eq!(age(1_000, Some(950)), "50s");
        assert_eq!(age(1_000, Some(400)), "10m");
        assert_eq!(age(100_000, Some(0)), "27h");
        assert_eq!(age(400_000, Some(0)), "4d");
        assert_eq!(age(0, None), "?");
    }

    #[test]
    fn pane_cycles_and_detail_moves_nothing() {
        assert_eq!(Pane::Repos.next(), Pane::Work);
        assert_eq!(Pane::Work.next(), Pane::Conversations);
        assert_eq!(Pane::Conversations.next(), Pane::Detail);
        assert_eq!(Pane::Detail.next(), Pane::Repos);
        // The detail pane drives no list: j/k on it move nothing.
        let mut app = App::new(fixture());
        press(&mut app, &[Key::Char('4'), Key::Char('j'), Key::Char('k')]);
        assert_eq!(app.cursor, [0, 0, 0]);
    }

    #[test]
    fn refresh_clamps_cursors_onto_what_remains() {
        let mut app = App::new(fixture());
        press(&mut app, &[Key::Char('2'), Key::Char('j'), Key::Char('j')]);
        assert_eq!(app.cursor[1], 2);
        // A snapshot without the selected row: the [2] cursor falls back
        // to `all`, and the shrink survives a second refresh to an even
        // smaller snapshot.
        let mut next = fixture();
        next.work.truncate(1);
        app.refresh(next);
        assert_eq!(app.cursor[1], 0);
        next_refresh(&mut app);
        fn next_refresh(app: &mut App) {
            let mut empty = fixture();
            empty.repos.clear();
            empty.work.clear();
            empty.conversations.clear();
            app.refresh(empty);
        }
        assert_eq!(app.cursor, [0, 0, 0]);
    }

    #[test]
    fn branch_and_space_rows_scope_conversations() {
        let mut app = App::new(fixture());
        // [2] on the branch-only row `feat-old`: [3] shows conversations on
        // that branch - the fixture's two worktree convs carry feat-login,
        // so nothing shows.
        press(&mut app, &[Key::Char('2'), Key::Char('j'), Key::Char('j')]);
        let text = render_to(&app, 200, 24);
        assert!(!text.contains("8f423bbb"), "{text}");
        assert!(text.contains("Conversations  feat/old"), "{text}");
        // [2] on the project space: a conv anchored on the space's
        // canonical id shows even when its recorded cwd is another
        // spelling of the same directory.
        app.snapshot.conversations[1].repo = Some("/spaces/notes".to_owned());
        app.snapshot.conversations[1].cwd = Some(PathBuf::from("/spaces/./notes"));
        press(&mut app, &[Key::Char('j')]);
        let text = render_to(&app, 200, 24);
        assert!(!text.contains("8f423bbb"), "{text}");
        assert!(text.contains("02aa0bbb"), "{text}");
    }

    #[test]
    fn the_detail_pane_follows_every_focus() {
        let mut app = App::new(fixture());
        // Focused on [1] with a repo under the cursor.
        press(&mut app, &[Key::Char('1'), Key::Char('j')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("[4] Repo - a"), "{text}");
        // On [2] a work row is the detail target.
        press(&mut app, &[Key::Char('2'), Key::Char('j')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("[4] Work - feat/login"), "{text}");
        // On [3] a conversation is; a waiting one names its reason.
        press(&mut app, &[Key::Char('3'), Key::Char('j')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("waiting: permission prompt"), "{text}");
        // On [4] itself the pane keeps the last list's target: focus
        // moved to scroll the same detail, not to clear it.
        press(&mut app, &[Key::Char('4')]);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("[4] Conversation"), "{text}");
    }

    #[test]
    fn help_shows_the_focused_pane_and_filter_editing() {
        let mut app = App::new(fixture());
        // Help over [4] names it; over a list it names that list.
        press(&mut app, &[Key::Char('4'), Key::Char('?')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("[4] Detail"), "{text}");
        press(&mut app, &[Key::Esc, Key::Char('1'), Key::Char('?')]);
        let text = render_to(&app, 200, 24);
        assert!(
            text.contains("[1] Repos - which project needs me"),
            "{text}"
        );
        press(&mut app, &[Key::Char('?'), Key::Char('2'), Key::Char('?')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("[2] Work - what needs attention"), "{text}");
        press(&mut app, &[Key::Char('?'), Key::Char('3'), Key::Char('?')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("Conversations - what needs me"), "{text}");
        press(&mut app, &[Key::Esc]);
        // While a filter is open the footer narrates it.
        press(&mut app, &[Key::Char('/')]);
        let text = render_to(&app, 200, 24);
        assert!(text.contains("filter: enter apply | esc cancel"), "{text}");
        // Backspace edits; Enter commits what is left.
        press(&mut app, &[Key::Char('x'), Key::Backspace, Key::Enter]);
        assert_eq!(app.filter_raw[2], "");
        // Unbound keys inside a filter edit are inert - Tab does not end it.
        press(
            &mut app,
            &[Key::Char('/'), Key::Tab, Key::Char('z'), Key::Esc],
        );
        assert_eq!(app.filter_raw[2], "");
        // And in help, an unbound key closes nothing.
        press(&mut app, &[Key::Char('?'), Key::Char('x')]);
        assert!(app.help);
        press(&mut app, &[Key::Esc]);
        // `/` on the detail pane owns no list: no editing session opens.
        press(&mut app, &[Key::Char('4'), Key::Char('/')]);
        assert!(app.editing.is_none());
        // A narrow pane on [4] gets the no-filter footer variant, and the
        // detail keeps the last list's target: `all`, not a cleared pane.
        press(&mut app, &[Key::Char('4')]);
        let text = render_to(&app, 55, 24);
        assert!(
            text.contains("1-4 | tab | j/k scroll | e | ? | q quit"),
            "{text}"
        );
        assert!(text.contains("everything in scope"), "{text}");
        // Narrow help: the popup clamps to the pane width.
        press(&mut app, &[Key::Char('?')]);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("[4] Detail"), "{text}");
        let text = render_to(&app, 200, 24);
        assert!(
            text.contains("1-4 focus | tab next | j/k scroll | e evidence"),
            "{text}"
        );
    }

    #[test]
    fn filters_parse_age_and_text_and_fail_closed_on_unknown_age() {
        let f = Filter::parse("  fix   age:2h ");
        assert_eq!(f.text, "fix");
        assert_eq!(f.age, Some(Duration::from_secs(7200)));
        // Text narrows case-insensitively; age requires a known timestamp.
        let mut lower = String::new();
        assert!(f.allows(["FIX the thing"], Some(100), 200, &mut lower));
        assert!(!f.allows(["other"], Some(100), 200, &mut lower));
        assert!(!f.allows(["fix"], None, 200, &mut lower));
        assert!(!f.allows(["fix"], Some(1), 200_000, &mut lower));
        // A bogus age term is text, not a filter that lets nothing through.
        let f = Filter::parse("age:forever");
        assert_eq!(f.text, "age:forever");
        assert_eq!(f.age, None);
    }

    #[test]
    fn the_loop_reads_keys_ignores_noise_and_swaps_in_pending_snapshots() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let mut app = App::new(fixture());
        // A script: a mapped key, an unmapped key, a key release, an idle
        // tick, and `q` to leave the loop. Two queued snapshots swap in on
        // the first pass - the newest wins.
        let mut events = std::collections::VecDeque::from([
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('j'),
                KeyModifiers::NONE,
            ))),
            // Every mapped code gets exercised: Tab moves focus, the plain
            // keys are asserted below.
            Some(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))),
            Some(Event::Key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE))),
            Some(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('k'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ))),
            None,
            Some(Event::Resize(100, 30)),
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
            ))),
        ]);
        let mut poll = move || Ok(events.pop_front().unwrap_or(None));
        let mut pending = std::collections::VecDeque::from([
            {
                let mut s = fixture();
                s.observed_at = 11;
                s
            },
            {
                let mut s = fixture();
                s.observed_at = 42;
                s
            },
        ]);
        let mut next_snapshot = move || {
            pending
                .pop_front()
                .map(Feed::Snapshot)
                .unwrap_or(Feed::Idle)
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        #[rustfmt::skip]
        let mut act = |_: &ActionRequest| -> ActionOutcome { // coverage: off - the script presses no Enter or o
            unreachable!("the script presses no Enter or o") // coverage: off - the script never reaches it
        }; // coverage: off - same
        let mut suspend =
            |_: &ResumePlan| -> Result<(), String> { unreachable!("no resume outcome") }; // coverage: off - the script never reaches it
        run_loop(
            &mut terminal,
            &mut app,
            &mut next_snapshot,
            &mut poll,
            &mut act,
            &mut suspend,
        )
        .unwrap();
        assert!(app.quit());
        // `j` moved the focused (Repos) cursor one row; release and
        // unmapped keys did not. The worker's finished snapshots swapped in
        // - the newer of the two queued won.
        assert_eq!(app.cursor[list_index(List::Repos)], 1);
        assert_eq!(app.snapshot.observed_at, 42);

        // With no snapshot pending, an idle tick changes nothing at all.
        let mut app = App::new(fixture());
        let mut events = std::collections::VecDeque::from([
            None,
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
            ))),
        ]);
        let mut poll = move || Ok(events.pop_front().unwrap_or(None));
        let mut next_snapshot = || Feed::Idle;
        run_loop(
            &mut terminal,
            &mut app,
            &mut next_snapshot,
            &mut poll,
            &mut act,
            &mut suspend,
        )
        .unwrap();
        assert!(app.quit());
        assert_eq!(app.snapshot.observed_at, 1_800_000_000);
    }

    #[test]
    fn a_dead_collector_is_surfaced_not_silent() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::backend::TestBackend;

        let mut app = App::new(fixture());
        let mut events = std::collections::VecDeque::from([
            None,
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
            ))),
        ]);
        let mut poll = move || Ok(events.pop_front().unwrap_or(None));
        let mut feeds = std::collections::VecDeque::from([Feed::Dead]);
        let mut next_snapshot = move || feeds.pop_front().unwrap_or(Feed::Idle);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        #[rustfmt::skip]
        let mut act = |_: &ActionRequest| -> ActionOutcome { // coverage: off - the script presses no Enter or o
            unreachable!("the script presses no Enter or o") // coverage: off - the script never reaches it
        }; // coverage: off - same
        let mut suspend =
            |_: &ResumePlan| -> Result<(), String> { unreachable!("no resume outcome") }; // coverage: off - the script never reaches it
        run_loop(
            &mut terminal,
            &mut app,
            &mut next_snapshot,
            &mut poll,
            &mut act,
            &mut suspend,
        )
        .unwrap();
        assert!(app.quit());
        // A dead collector does not leave stale rows looking live: the
        // footer names it.
        assert!(app.collector_dead);
        assert!(render_to(&app, 80, 24).contains("collector stopped"));
    }

    /// The braille spinner frames the footer cycles through.
    const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    fn bottom_line(buffer: &str) -> &str {
        buffer.lines().last().expect("a rendered footer")
    }

    #[test]
    fn an_incomplete_snapshot_spins_in_the_status_bar() {
        let mut snapshot = fixture();
        snapshot.complete = false;
        let app = App::new(snapshot);

        // At 55 columns the spinner is the bare glyph leading the compact
        // hints; the hints themselves still fit.
        let narrow = render_to(&app, 55, 10);
        let footer = bottom_line(&narrow);
        assert!(SPINNER.iter().any(|f| footer.starts_with(f)), "{footer}");
        assert!(footer.contains("q quit"), "{footer}");

        // At 200 columns the glyph is spelled out as `collecting`.
        let wide = render_to(&app, 200, 24);
        assert!(bottom_line(&wide).contains("collecting"), "{wide:?}");
    }

    #[test]
    fn the_spinner_stops_when_the_snapshot_is_complete() {
        // `fixture` is complete: the footer is plain hints at both widths.
        let app = App::new(fixture());
        for width in [55, 200] {
            let rendered = render_to(&app, width, 10);
            let footer = bottom_line(&rendered);
            assert!(
                !SPINNER.iter().any(|f| footer.contains(f)),
                "{width}: {footer}"
            );
            assert!(footer.contains("q quit"), "{footer}");
        }

        // And a dead collector's status wins over the spinner: an
        // incomplete snapshot with a stopped collector still names the
        // failure instead of spinning.
        let mut app = App::new({
            let mut s = fixture();
            s.complete = false;
            s
        });
        app.collector_stopped();
        let rendered = render_to(&app, 80, 24);
        let footer = bottom_line(&rendered);
        assert!(footer.contains("collector stopped"), "{footer}");
        assert!(!SPINNER.iter().any(|f| footer.contains(f)), "{footer}");
    }

    #[test]
    fn the_worker_produces_until_the_receiver_drops() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Snapshot>(1);
        let mut calls = 0u64;
        let worker = std::thread::spawn(move || {
            collect_worker(
                tx,
                move |publish| {
                    calls += 1;
                    let mut s = fixture();
                    s.observed_at = calls;
                    publish(s);
                },
                Duration::ZERO,
            )
        });
        // One pass after another lands on the channel; dropping the
        // receiver ends the worker instead of leaving it parked.
        assert_eq!(rx.recv().unwrap().observed_at, 1);
        assert_eq!(rx.recv().unwrap().observed_at, 2);
        drop(rx);
        worker.join().unwrap();
    }

    #[test]
    fn every_glyph_and_state_text_maps() {
        let mut conv = ConversationRow {
            ..fixture().conversations[0].clone()
        };
        // The glyph is the attention, whatever the execution state is.
        for (attention, glyph) in [
            (Attention::Waiting, "!"),
            (Attention::Error, "✗"),
            (Attention::CompletedUnseen, "✓"),
            (Attention::Working, "●"),
            (Attention::Unknown, "?"),
            (Attention::None, ""),
        ] {
            conv.attention = attention;
            assert_eq!(conversation_glyph(&conv), glyph, "{attention:?}");
        }
        // Detail text: the attention's word beside the effective state's,
        // when the two differ.
        conv.attention = Attention::Waiting;
        conv.attention_detail = None;
        conv.waiting_for = None;
        conv.state = ConversationState::Waiting;
        assert_eq!(detail_state(&conv), "waiting");
        conv.attention = Attention::Error;
        conv.attention_detail = Some("StopFailure".to_owned());
        conv.state = ConversationState::Busy;
        assert_eq!(detail_state(&conv), "error: StopFailure · working");
        conv.attention = Attention::CompletedUnseen;
        conv.attention_detail = None;
        conv.state = ConversationState::Idle;
        assert_eq!(detail_state(&conv), "done");
        conv.attention = Attention::None;
        assert_eq!(detail_state(&conv), "idle");
        conv.attention = Attention::None;
        conv.state = ConversationState::Busy;
        assert_eq!(detail_state(&conv), "working");
        // A live file whose claimed pid is dead is a record, not a process:
        // it is not `running`; a retained latch still shows its glyph.
        let mut dead = fixture().conversations[0].clone();
        dead.attention = Attention::None;
        dead.attachment = dead.attachment.map(|a| AttachmentRow {
            liveness: AttachmentLiveness::Dead,
            ..a
        });
        assert!(!dead.running());
        assert_eq!(conversation_glyph(&dead), "");
        assert_eq!(detail_state(&dead), "dead");
        // ... while a dead conversation with an unseen error keeps the ✗.
        dead.attention = Attention::Error;
        dead.attention_detail = None;
        assert_eq!(conversation_glyph(&dead), "✗");
        assert_eq!(detail_state(&dead), "error · dead");
        // The middle field says `working` beside a retained latch.
        dead.state = ConversationState::Busy;
        assert_eq!(conversation_middle(&dead), "claude · working");
        dead.attention = Attention::Waiting;
        dead.state = ConversationState::Waiting;
        assert_eq!(conversation_middle(&dead), "claude");
        // Glyph colours by meaning; the empty glyph colours nothing.
        assert_eq!(glyph_style("✗").fg, Some(Color::LightRed));
        assert_eq!(glyph_style("✓").fg, Some(Color::LightGreen));
        assert_eq!(glyph_style("?").fg, Some(Color::DarkGray));
        // A repo whose work rows carry no attention shows no glyph.
        let dead = RepoRow {
            attention: Attention::None,
            ..fixture().repos[0].clone()
        };
        assert_eq!(repo_glyph(&dead), "");
        // fit() never overshoots and zero-width collapses to empty.
        assert_eq!(fit("abc", 0), "");
        // A work row with no checkout names itself plainly; under `all` the
        // label carries the repo, scoped it does not.
        let bare = WorkRow {
            worktree: None,
            ..fixture().work[1].clone()
        };
        assert_eq!(work_name(&bare, false), "feat/old#1");
        assert_eq!(work_name(&bare, true), "a/feat/old#1");
        // Key events map; unbound codes are None.
        use crossterm::event::KeyCode;
        assert_eq!(map_key(KeyCode::Tab), Some(Key::Tab));
        assert_eq!(map_key(KeyCode::BackTab), Some(Key::Tab));
        assert_eq!(map_key(KeyCode::Esc), Some(Key::Esc));
        assert_eq!(map_key(KeyCode::Enter), Some(Key::Enter));
        assert_eq!(map_key(KeyCode::Backspace), Some(Key::Backspace));
        assert_eq!(map_key(KeyCode::Up), Some(Key::Up));
        assert_eq!(map_key(KeyCode::Down), Some(Key::Down));
        assert_eq!(map_key(KeyCode::Char('z')), Some(Key::Char('z')));
        assert_eq!(map_key(KeyCode::F(1)), None);
    }

    #[test]
    fn own_pane_detection_is_honest() {
        // $TMUX + $TMUX_PANE must both name real values - socket and id -
        // or there is no own pane; the dashboard never invents one. The
        // ambient reads themselves stay untested: a test binary inherits
        // whatever environment it is given.
        fn tmux(s: &str) -> Option<&std::ffi::OsStr> {
            Some(std::ffi::OsStr::new(s))
        }
        let pref = tmux::pane_ref_from_env(tmux("/tmp/sock,1,0"), Some("%12")).unwrap();
        assert_eq!(pref.pane.as_str(), "%12");
        assert_eq!(pref.socket, PathBuf::from("/tmp/sock"));
        assert!(tmux::pane_ref_from_env(None, Some("%12")).is_none());
        assert!(tmux::pane_ref_from_env(tmux("/tmp/sock,1,0"), None).is_none());
        assert!(tmux::pane_ref_from_env(tmux(",1,0"), Some("%12")).is_none());
        assert!(tmux::pane_ref_from_env(tmux("/tmp/sock,1,0"), Some("junk")).is_none());
    }

    #[test]
    fn the_detail_helpers_wrap_escape_and_name() {
        // An empty body line still emits one line so sections keep their
        // spacing; a width of zero never panics.
        assert_eq!(wrap_text("", 4), vec![String::new()]);
        assert_eq!(wrap_text("abcdef", 2), vec!["ab", "cd", "ef"]);
        // Wrapping and truncation count terminal cells, not chars: a
        // double-width character takes two.
        assert_eq!(wrap_text("日本語テ", 4), vec!["日本", "語テ"]);
        assert_eq!(wrap_text("a日b", 2), vec!["a", "日", "b"]);
        // A double-width character in a one-cell pane still lands (on its
        // own line) rather than looping.
        assert_eq!(wrap_text("日", 1), vec!["日"]);
        assert_eq!(fit("日本語", 4), "日…");
        assert_eq!(fit("日本", 4), "日本");
        assert_eq!(fit("日本語", 0), "");
        // Control characters become visible escapes.
        assert_eq!(escape_text("a\rb\x1fc"), "a\\rb\\u001fc");
        // Liveness and reference kinds render a word each.
        let attachment = |l| AttachmentRow {
            pid: 1,
            pid_start: None,
            liveness: l,
            liveness_detail: None,
            pane: None,
            target: None,
            pane_source: None,
            placement_detail: None,
            source: EvidenceSource::Derived,
            observed_at: 0,
        };
        assert_eq!(
            liveness_word(&attachment(AttachmentLiveness::Instance)),
            "instance"
        );
        assert_eq!(
            liveness_word(&attachment(AttachmentLiveness::PidOnly)),
            "pid only"
        );
        assert_eq!(
            liveness_word(&attachment(AttachmentLiveness::Unverifiable)),
            "unverifiable"
        );
        assert_eq!(liveness_word(&attachment(AttachmentLiveness::Dead)), "dead");
        assert_eq!(ref_kind(ReferenceKind::Pane), "pane");
        assert_eq!(ref_kind(ReferenceKind::Window), "window");
        assert_eq!(ref_kind(ReferenceKind::TmuxSession), "session");
        assert_eq!(ref_kind(ReferenceKind::Process), "process");
        assert_eq!(ref_kind(ReferenceKind::AgentSession), "agent");
    }
}
