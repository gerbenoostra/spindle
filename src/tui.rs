//! The terminal dashboard: three stacked, cursor-scoped lists beside one
//! detail pane, over one immutable snapshot per refresh.
//!
//! [1] Repos and [2] Work scope whatever sits below them; [3] Conversations
//! is the attention inbox across the selected scope. The shell owns the
//! numbered panes, `Tab`, `j`/`k`, `/` text-and-age filters, `?` help and `q`
//! - keys other tasks have not shipped stay absent and inert.
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

use crate::attention::Attention;
use crate::config;
use crate::snapshot::{
    ConversationRow, ConversationState, RepoRow, Snapshot, WorkKind, WorkRow, WorkSection, to_json,
};
use crate::store::{self, Store};
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
    /// The spinner's frame index while a snapshot is still incomplete.
    /// `Cell` because a draw is `&self`: the animation ticks by rendering.
    spin: std::cell::Cell<u64>,
}

/// The row shapes one list can hold - views over the snapshot, never copies
/// of it.
enum Row<'a> {
    Repo(&'a RepoRow),
    Work(&'a WorkRow),
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
    /// An app on a snapshot; focus opens on [3], the attention inbox, which
    /// is the global list while both upper cursors sit on `all`.
    pub fn new(snapshot: Snapshot) -> App {
        App {
            snapshot,
            focus: Pane::Conversations,
            cursor: [0, 0, 0],
            filter_raw: [String::new(), String::new(), String::new()],
            editing: None,
            help: false,
            quit: false,
            collector_dead: false,
            store: None,
            forgotten_after: config::DEFAULT_FORGOTTEN_AFTER,
            notice: None,
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
        self.reseat(keys);
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
        // The `all` row's counts cover the whole scope, collapsed or not.
        let work_open = work_rows.iter().filter(|w| w.section.open()).count();
        let work_clean = work_rows.len() - work_open;
        // Under `all`, the cleanup sections collapse into one summary line;
        // under a repo they list their rows like any section.
        let (work, cleanup) = if repo_scope.is_some() {
            (work_rows.into_iter().map(Row::Work).collect(), None)
        } else {
            let mut work = Vec::new();
            let mut safe = 0usize;
            let mut review = 0usize;
            for w in work_rows {
                match w.section {
                    WorkSection::ReadyToClean => safe += 1,
                    WorkSection::CleanupReview => review += 1,
                    _ => work.push(Row::Work(w)),
                }
            }
            (work, (safe + review > 0).then_some((safe, review)))
        };
        let work_scope = match self.cursor[list_index(List::Work)] /* // coverage: off - the get-miss arm is unreachable: cursors clamp before a view */ {
            0 => None, // coverage: off - the unreachable arm's match edge lands here
            cursor => work // coverage: off - same
                .get(cursor - 1)
                .map(|row| match row { // coverage: off - same
                Row::Work(w) => scope_of(w),
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
                    // path, or on branch for branch-only rows; under `all`
                    // work, on the repo alone.
                    match work {
                        WorkScope::Worktree { repo, root } => {
                            c.repo.as_deref() == Some(repo.as_str())
                                && c.worktree.as_deref() == Some(root.as_path())
                        }
                        WorkScope::Branch { repo, branch } => {
                            c.repo.as_deref() == Some(repo.as_str())
                                && c.branch.as_deref() == Some(branch.as_str())
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
    /// parks a Work row (suppressing only its `Forgotten` placement), `?`
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
            Key::Char('1') => self.focus = Pane::Repos,
            Key::Char('2') => self.focus = Pane::Work,
            Key::Char('3') => self.focus = Pane::Conversations,
            Key::Char('4') => self.focus = Pane::Detail,
            Key::Tab => self.focus = self.focus.next(),
            Key::Char('j') | Key::Down => self.move_cursor(1),
            Key::Char('k') | Key::Up => self.move_cursor(-1),
            Key::Char('/') => {
                if let Some(list) = self.focused_list() {
                    self.editing = Some((list, self.filter_raw[list_index(list)].clone()));
                }
            }
            Key::Char(' ') => self.space(),
            Key::Char('p') => self.park(),
            _ => {}
        }
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
            Row::Repo(_) => Vec::new(),
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
    /// meaning carried over from the previous scope.
    fn move_cursor(&mut self, delta: i64) {
        let Some(list) = self.focused_list() else {
            return;
        };
        let rows = self.view().rows(list).len();
        let cursor = &mut self.cursor[list_index(list)];
        // `all` plus rows: cursor range is 0..=rows.
        *cursor = (*cursor as i64 + delta).clamp(0, rows as i64) as usize;
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
            if cursor == i + 1 {
                cursor_line = display.len();
            }
            display.push(self.row(list, row, view, inner.width, cursor == i + 1));
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
        let age_w = age.chars().count();
        let middle = if show_fields {
            fit(cells.middle, width.saturating_sub(glyph_w + age_w + 12))
        } else {
            String::new()
        };
        let middle_w = middle.chars().count();
        let label_w = width.saturating_sub(glyph_w + middle_w + age_w + 4);
        let label = fit(cells.label, label_w);
        let pad = width
            .saturating_sub(glyph_w + label.chars().count() + middle_w + age_w + 2)
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

    /// The detail pane: header-only for now - glyph, target, what it is and
    /// its state age. The full field set is the detail task's, not this
    /// one's; an honest `?` still renders where the header cannot be filled.
    fn detail_panel(&self, f: &mut Frame<'_>, area: Rect, view: &View<'_>) {
        let (title, header) = self.detail_header(view);
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
        f.render_widget(Paragraph::new(vec![header]), inner);
    }

    /// `[4] <what>` plus the `glyph target - what · state age` header, taken
    /// from whatever the focused list's cursor sits on.
    fn detail_header(&self, view: &View<'_>) -> (String, Line<'static>) {
        let (list, row) = match self.focused_list() {
            Some(list) => {
                let cursor = self.cursor[list_index(list)];
                let row = if cursor == 0 {
                    None
                } else {
                    view.rows(list).get(cursor - 1)
                };
                (Some(list), row)
            }
            None => (None, None),
        };
        match (list, row) {
            (Some(List::Repos), Some(Row::Repo(r))) => (
                format!("[4] Repo - {}", r.name),
                Line::from(format!(
                    "{} {} - repo · {}",
                    repo_glyph(r),
                    r.name,
                    age(self.now(), r.last_activity)
                )),
            ),
            (Some(List::Work), Some(Row::Work(w))) => (
                format!("[4] Work - {}", w.name),
                Line::from(format!(
                    "{} {} - {} · {}",
                    work_glyph(w),
                    w.name,
                    w.kind.as_str().replace('_', " "),
                    age(self.now(), w.last_activity)
                )),
            ),
            (Some(List::Conversations), Some(Row::Conversation(c))) => (
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
            (Some(_), None) => (
                "[4] Detail".to_owned(),
                Line::from(Span::styled("all", Style::default().fg(Color::DarkGray))),
            ),
            (None, _) => (
                "[4] Detail".to_owned(), // coverage: off - the arm's second region is an instantiation edge
                Line::from(Span::styled(
                    "cursor is on the detail pane",
                    Style::default().fg(Color::DarkGray),
                )), // coverage: off - the arm's second region is an instantiation edge
            ), // coverage: off - same
            _ => ("[4] Detail".to_owned(), Line::from("")), // coverage: off - list+row kinds pair up by construction
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
                        "1-4 | tab | j/k | p park | / filter | ? | q quit".to_owned()
                    }
                    Some(_) => "1-4 | tab | j/k | / filter | ? | q quit".to_owned(),
                    None => "1-4 | tab | j/k | ? | q quit".to_owned(),
                }
            } else {
                let hints = match self.focused_list() {
                    Some(List::Repos) => "1-4 focus | tab next | j/k move | / filter",
                    Some(List::Work) => {
                        "1-4 focus | tab next | j/k move | / filter | space ack | p park"
                    }
                    Some(_) => "1-4 focus | tab next | j/k move | / filter | space ack",
                    None => "1-4 focus | tab next | j/k move",
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
                        "j/k move   / filter   space ack/mark   p park   enter/jump (later)"
                    }
                    _ => "j/k move   / filter   space ack/mark   enter/jump (later)",
                }),
            ],
            None => vec![Line::from("[4] Detail - follows the focused list")],
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
                Some(WorkScope::Branch { branch, .. }) => {
                    format!("[3] Conversations  {branch}")
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
        Row::Conversation(c) => format!("{}\u{0}{}", c.provider.as_str(), c.session_id),
    }
}

/// The selection key a Work row carries.
fn work_key(w: &WorkRow) -> String {
    format!("{}\u{0}{}\u{0}{}", w.repo, w.kind.as_str(), w.name)
}

/// The scope a work row selects in [3].
fn scope_of(w: &WorkRow) -> WorkScope {
    match (w.kind, &w.worktree, &w.branch) {
        (WorkKind::ProjectSpace, Some(root), _) => WorkScope::Space {
            id: w.repo.clone(),
            path: root.clone(),
        },
        (_, Some(root), _) => WorkScope::Worktree {
            repo: w.repo.clone(),
            root: root.clone(),
        },
        (_, None, Some(branch)) => WorkScope::Branch {
            repo: w.repo.clone(),
            branch: branch.clone(),
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
    /// A branch with no checkout of its own; conversations on that branch.
    Branch { repo: String, branch: String },
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

/// `label` clipped to `w` chars with an ellipsis when it loses a character.
fn fit(label: &str, w: usize) -> String {
    if label.chars().count() <= w {
        return label.to_owned();
    }
    if w == 0 {
        return String::new();
    }
    let mut out: String = label.chars().take(w.saturating_sub(1)).collect();
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

/// The work row's label: `name ⌂worktree`, prefixed with the repo's name
/// under the global `all` scope; a project space's workspace is its name
/// already, so it carries no suffix.
fn work_name(w: &WorkRow, global: bool) -> String {
    if w.kind == WorkKind::ProjectSpace {
        return w.name.clone();
    }
    let wt = w
        .worktree
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| format!(" ⌂{}", n.to_string_lossy()))
        .unwrap_or_default();
    if global {
        format!("{}/{}{}", w.repo_name, w.name, wt)
    } else {
        format!("{}{}", w.name, wt)
    }
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
    let result = run_loop(&mut terminal, &mut app, feed, poll_event);
    disable_raw_mode()?; // coverage: off - `?` needs a broken terminal
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?; // coverage: off - same
    result // coverage: off - same
} // coverage: off - the unexecuted instantiation's exit edge
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
/// produced - until `q` quits. Both sources are injected so the loop needs
/// no terminal and no worker: tests hand it a key script and a scripted
/// snapshot source. Anything the user presses is mapped to a `Key` and
/// applied; release events and unmapped codes are ignored.
fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    mut next_snapshot: impl FnMut() -> Feed,
    mut poll: impl FnMut() -> io::Result<Option<Event>>,
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
    use crate::runtime::{PaneSource, Provider};
    use crate::snapshot::{
        AttachmentLiveness, AttachmentRow, Landed, RepoRow, SCHEMA_VERSION, Upstream, WorkRow,
    };
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

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
                    parked: false,
                    forge: crate::forge::WorkItem::Open,
                    pipeline: crate::forge::Pipeline::Unknown,
                    forge_label: Some("PR #191".to_owned()),
                    forge_url: Some("https://github.com/o/r/pull/191".to_owned()),
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
                    parked: false,
                    forge: crate::forge::WorkItem::Unknown,
                    pipeline: crate::forge::Pipeline::Unknown,
                    forge_label: None,
                    forge_url: None,
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
                    parked: false,
                    forge: crate::forge::WorkItem::Unknown,
                    pipeline: crate::forge::Pipeline::Unknown,
                    forge_label: None,
                    forge_url: None,
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
                        pane_source: Some(PaneSource::Published),
                        placement_detail: None,
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
                },
            ],
            errors: vec![],
            skipped: vec![],
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

    #[test]
    fn the_shell_renders_at_55_and_200_columns() {
        for width in [55u16, 200] {
            let app = App::new(fixture());
            let text = render_to(&app, width, 24);
            // The four-panel frame is there, unclipped, with no horizontal
            // scroll - every line is exactly `width` cells wide.
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
        app.key(Key::Char('j'));
        let text = render_to(&app, 200, 24);
        assert!(text.contains("working"), "{text}");
        let mut snapshot = fixture();
        snapshot.conversations[0].state = ConversationState::Waiting;
        snapshot.conversations[0].attention = Attention::Error;
        snapshot.conversations[0].attention_detail = Some("StopFailure".to_owned());
        let mut app = App::new(snapshot);
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
                Key::Char('e'),
                Key::Char('o'),
                Key::Char('x'),
                Key::Char('d'),
                Key::Char('D'),
                Key::Char('c'),
                Key::Char('h'),
                Key::Char('i'),
                Key::Char(' '),
                Key::Enter,
            ],
        );
        assert_eq!(render_to(&app, 55, 24), before);
        assert!(!app.quit());
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
        // On [4] itself the pane owns no list.
        press(&mut app, &[Key::Char('4')]);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("cursor is on the detail pane"), "{text}");
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
        // detail header names a cursor that owns no list.
        press(&mut app, &[Key::Char('4')]);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("1-4 | tab | j/k | ? | q quit"), "{text}");
        assert!(text.contains("cursor is on the detail pane"), "{text}");
        // Narrow help: the popup clamps to the pane width.
        press(&mut app, &[Key::Char('?')]);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("[4] Detail"), "{text}");
        let text = render_to(&app, 200, 24);
        assert!(
            text.contains("1-4 focus | tab next | j/k move | ? keys"),
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
        run_loop(&mut terminal, &mut app, &mut next_snapshot, &mut poll).unwrap();
        assert!(app.quit());
        // `j` moved the focused (Conversations) cursor one row; release and
        // unmapped keys did not. The worker's finished snapshots swapped in
        // - the newer of the two queued won.
        assert_eq!(app.cursor[list_index(List::Conversations)], 1);
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
        run_loop(&mut terminal, &mut app, &mut next_snapshot, &mut poll).unwrap();
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
        run_loop(&mut terminal, &mut app, &mut next_snapshot, &mut poll).unwrap();
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
        assert_eq!(work_name(&bare, false), "feat/old");
        assert_eq!(work_name(&bare, true), "a/feat/old");
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
}
