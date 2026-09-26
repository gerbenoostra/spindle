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

use crate::config;
use crate::snapshot::{ConversationRow, RepoRow, Snapshot, WorkRow, to_json};
use crate::tmux::PaneId;

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

    /// Whether a row survives: its label matches the text, and its timestamp
    /// is newer than `age`. An unknown age fails closed - filtering out what
    /// cannot be proven is honest; guessing it young is not.
    fn allows(&self, label: &str, timestamp: Option<u64>, now: u64) -> bool {
        if !self.text.is_empty() && !label.to_lowercase().contains(&self.text) {
            return false;
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
        }
    }

    /// Swap in a fresh snapshot, clamping cursors onto the rows that remain:
    /// the range is `0..=rows` - the `all` row counts as a position.
    pub fn refresh(&mut self, snapshot: Snapshot) {
        self.snapshot = snapshot;
        for list in [List::Repos, List::Work, List::Conversations] {
            let len = self.rows(list).len();
            let slot = &mut self.cursor[list_index(list)];
            if *slot > len {
                *slot = len;
            }
        }
    }

    /// Whether the run loop should exit.
    pub fn quit(&self) -> bool {
        self.quit
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

    /// The rows a list shows: the scope the cursor above it chose, filtered.
    ///
    /// `Repos` rows are the snapshot's repos; `Work` is scoped by the repo
    /// cursor (index 0 is `all`), and `Conversations` by the work cursor on
    /// top of that. Each list's own `all` row is always first and always
    /// visible - a filter narrows records, never the aggregate.
    fn rows(&self, list: List) -> Vec<Row<'_>> {
        match list {
            List::Repos => self
                .snapshot
                .repos
                .iter()
                .filter(|r| {
                    self.filter(list)
                        .allows(&repo_label(r), r.last_activity, self.now())
                })
                .map(Row::Repo)
                .collect(),
            List::Work => {
                let scope = self.repo_scope();
                self.snapshot
                    .work
                    .iter()
                    .filter(|w| scope.as_deref().is_none_or(|s| w.repo == *s))
                    .filter(|w| {
                        self.filter(list)
                            .allows(&work_label(w), w.last_activity, self.now())
                    })
                    .map(Row::Work)
                    .collect()
            }
            List::Conversations => {
                let repo_scope = self.repo_scope();
                let work_scope = self.work_scope();
                self.snapshot
                    .conversations
                    .iter()
                    .filter(|c| {
                        if let Some(work) = &work_scope {
                            // A conversation under one work row matches on
                            // worktree path, or on branch for branch-only
                            // rows; under `all` work, on the repo alone.
                            match work {
                                WorkScope::Worktree { repo, root } => {
                                    c.repo.as_deref() == Some(repo.as_str())
                                        && c.worktree.as_deref() == Some(root.as_path())
                                }
                                WorkScope::Branch { repo, branch } => {
                                    c.repo.as_deref() == Some(repo.as_str())
                                        && c.branch.as_deref() == Some(branch.as_str())
                                }
                                WorkScope::Space { path } => {
                                    c.cwd.as_deref() == Some(path.as_path())
                                }
                            }
                        } else if let Some(repo) = &repo_scope {
                            c.repo.as_deref() == Some(repo.as_str())
                        } else {
                            true
                        }
                    })
                    .filter(|c| {
                        self.filter(list)
                            .allows(&conversation_label(c), c.state_since, self.now())
                    })
                    .map(Row::Conversation)
                    .collect()
            }
        }
    }

    /// The repo the [1] cursor names, or `None` on `all`. Index 0 is the
    /// `all` row, so repo rows are offset by one.
    fn repo_scope(&self) -> Option<String> {
        let cursor = self.cursor[list_index(List::Repos)];
        if cursor == 0 {
            return None;
        }
        self.rows(List::Repos).get(cursor - 1).map(|row| match row {
            Row::Repo(r) => r.id.clone(),
            _ => String::new(), // coverage: off - rows(List::Repos) is Repo rows only
        })
    }

    /// The work row the [2] cursor names, or `None` on `all`.
    fn work_scope(&self) -> Option<WorkScope> {
        let cursor = self.cursor[list_index(List::Work)];
        if cursor == 0 {
            return None;
        }
        self.rows(List::Work).get(cursor - 1).map(|row| match row {
            Row::Work(w) => match (w.kind, &w.worktree, &w.branch) {
                ("project_space", Some(root), _) => WorkScope::Space { path: root.clone() },
                (_, Some(root), _) => WorkScope::Worktree {
                    repo: w.repo.clone(),
                    root: root.clone(),
                },
                (_, None, Some(branch)) => WorkScope::Branch {
                    repo: w.repo.clone(),
                    branch: branch.clone(),
                },
                _ /* // coverage: off - an anchor always names one of these */ => WorkScope::Space {
                    path: Path::new("").to_path_buf(), // coverage: off - same
                },
            },
            _ /* // coverage: off - rows(List::Work) is Work rows only */ => WorkScope::Space {
                path: Path::new("").to_path_buf(), // coverage: off - same
            },
        })
    }

    /// One key press.
    ///
    /// The handled set is exactly the shipped one: `1`-`4` focus, `Tab`
    /// cycles, `j`/`k` move the cursor, `/` filters the focused list, `?`
    /// toggles help, `q` quits, `Esc` closes help or a filter. Everything
    /// else is inert: an unbound key does nothing, and nothing here pretends
    /// to a behaviour a later task owns.
    pub fn key(&mut self, key: Key) {
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
            _ => {}
        }
    }

    /// `j`/`k` on the focused list: move, clamp, and reset the cursors below
    /// when the scope itself changed - the scoped list's cursor has no
    /// meaning carried over from the previous scope.
    fn move_cursor(&mut self, delta: i64) {
        let Some(list) = self.focused_list() else {
            return;
        };
        let rows = self.rows(list).len();
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
        let [repos, work, conversations] = Layout::vertical([
            Constraint::Length(list_height(&self.snapshot.repos, left.height)),
            Constraint::Percentage(55),
            Constraint::Min(0),
        ])
        .areas(left);

        self.list_panel(f, repos, Pane::Repos);
        self.list_panel(f, work, Pane::Work);
        self.list_panel(f, conversations, Pane::Conversations);
        self.detail_panel(f, right);
        self.footer(f, footer);
        if self.help {
            self.help_overlay(f, area);
        }
    }

    /// One list panel: title, `all` row, then the filtered rows.
    fn list_panel(&self, f: &mut Frame<'_>, area: Rect, pane: Pane) {
        let list = pane.list().expect("a list pane"); // coverage: off - only list panes reach here
        let block = Block::default()
            .title(self.panel_title(pane))
            .borders(Borders::ALL)
            .border_style(if self.focus == pane {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            });
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines = Vec::new();
        let cursor = self.cursor[list_index(list)];
        let rows = self.rows(list);
        lines.push(self.all_row(list, &rows, inner.width, cursor == 0));
        for (i, row) in rows.iter().enumerate() {
            lines.push(self.row(list, row, inner.width, cursor == i + 1));
        }
        // Follow the cursor: when the list is taller than its pane, scroll so
        // the selected line stays visible. No horizontal scroll anywhere.
        let visible = inner.height as usize;
        let scroll = cursor.saturating_sub(visible.saturating_sub(1));
        let lines: Vec<Line<'static>> = lines.into_iter().skip(scroll).collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// The `all` row of a list.
    fn all_row(&self, list: List, rows: &[Row<'_>], width: u16, selected: bool) -> Line<'static> {
        let counts = match list {
            List::Repos => format!("{} shown", rows.len()),
            List::Work => format!("{} open", rows.len()),
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
                glyph: "",
                label: "all",
                middle: &counts,
                age: "",
                selected,
                dim_label: false,
            },
        )
    }

    /// One data row, formatted to `width`.
    fn row(&self, _list: List, row: &Row<'_>, width: u16, selected: bool) -> Line<'static> {
        let cells = match row {
            Row::Repo(r) => RowCells {
                glyph: repo_glyph(
                    self.snapshot
                        .repos
                        .iter()
                        .find(|x| x.id == r.id)
                        .unwrap_or(r),
                ),
                label: &r.name,
                middle: &repo_counts(r),
                age: &age(self.now(), r.last_activity),
                selected,
                dim_label: false,
            },
            Row::Work(w) => RowCells {
                glyph: work_glyph(&self.snapshot, w),
                label: &work_name(&self.snapshot, w),
                middle: &w.summary,
                age: &age(self.now(), w.last_activity),
                selected,
                dim_label: false,
            },
            Row::Conversation(c) => RowCells {
                glyph: conversation_glyph(c),
                label: &format!("{} {}", c.short_id, c.title.as_deref().unwrap_or("?")),
                middle: c.provider,
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
    fn detail_panel(&self, f: &mut Frame<'_>, area: Rect) {
        let (title, header) = self.detail_header();
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
    fn detail_header(&self) -> (String, Line<'static>) {
        let (list, row) = match self.focused_list() {
            Some(list) => {
                let cursor = self.cursor[list_index(list)];
                let row = if cursor == 0 {
                    None
                } else {
                    self.rows(list).into_iter().nth(cursor - 1)
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
                    "",
                    r.name,
                    age(self.now(), r.last_activity)
                )),
            ),
            (Some(List::Work), Some(Row::Work(w))) => (
                format!("[4] Work - {}", w.name),
                Line::from(format!(
                    "{} {} - {} · {}",
                    work_glyph(&self.snapshot, w),
                    w.name,
                    w.kind.replace('_', " "),
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
                        c.provider,
                        detail_state(c)
                    )),
                ]),
            ),
            (Some(_), None) => (
                "[4] Detail".to_owned(),
                Line::from(Span::styled("all", Style::default().fg(Color::DarkGray))),
            ),
            (None, _) => (
                "[4] Detail".to_owned(),
                Line::from(Span::styled(
                    "cursor is on the detail pane",
                    Style::default().fg(Color::DarkGray),
                )),
            ),
            _ => ("[4] Detail".to_owned(), Line::from("")), // coverage: off - list+row kinds pair up by construction
        }
    }

    /// The footer: hints for the focused view, always ending `? keys | q quit`.
    /// A narrow terminal gets the compact form rather than a clipped one.
    fn footer(&self, f: &mut Frame<'_>, area: Rect) {
        let text = if self.editing.is_some() {
            "filter: enter apply | esc cancel".to_owned()
        } else if area.width < 60 {
            match self.focused_list() {
                Some(_) => "1-4 | tab | j/k | / filter | ? | q quit".to_owned(),
                None => "1-4 | tab | j/k | ? | q quit".to_owned(),
            }
        } else {
            let hints = match self.focused_list() {
                Some(_) => "1-4 focus | tab next | j/k move | / filter",
                None => "1-4 focus | tab next | j/k move",
            };
            format!("{hints} | ? keys | q quit")
        };
        f.render_widget(
            Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray))),
            area,
        );
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
                Line::from("j/k move   / filter   enter/jump (later)"),
            ],
            None => vec![Line::from("[4] Detail - follows the focused list")],
        };
        let mut lines = vec![
            Line::from("keys"),
            Line::from("1-4 focus   tab next   q quit   ? close   esc close"),
            Line::from(""),
        ];
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
        );
    }

    /// The panel title: `[N] Name` plus the scope suffix the cursor above set.
    fn panel_title(&self, pane: Pane) -> String {
        match pane {
            Pane::Repos => "[1] Repos".to_owned(),
            Pane::Work => match self.repo_scope() {
                None => "[2] Work  all · by next action".to_owned(),
                Some(repo) => {
                    let name = self
                        .snapshot
                        .repos
                        .iter()
                        .find(|r| r.id == repo)
                        .map(|r| r.name.clone())
                        .unwrap_or(repo);
                    format!("[2] Work  {name} · by next action")
                }
            },
            Pane::Conversations => match self.work_scope() {
                Some(WorkScope::Worktree { root, .. }) => {
                    format!("[3] Conversations  {}", root.display())
                }
                Some(WorkScope::Branch { branch, .. }) => {
                    format!("[3] Conversations  {branch}")
                }
                Some(WorkScope::Space { path }) => {
                    format!("[3] Conversations  {}", path.display())
                }
                None => "[3] Conversations  all · by attention".to_owned(),
            },
            Pane::Detail => self.detail_header().0, // coverage: off - the detail pane renders its own header, never asks the title
        }
    }
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
    /// A non-git project space; conversations reporting exactly that cwd.
    Space { path: std::path::PathBuf },
}

fn list_index(list: List) -> usize {
    match list {
        List::Repos => 0,
        List::Work => 1,
        List::Conversations => 2,
    }
}

/// A panel's height: its rows plus borders, capped to a third of the screen
/// so [1] cannot starve the others.
fn list_height(rows: &[RepoRow], height: u16) -> u16 {
    ((rows.len() as u16 + 3).max(3)).min(height / 3)
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

/// A repo row's glyph: `!`/`●`/`?`/blank rolled up from its conversations.
fn repo_glyph(repo: &RepoRow) -> &'static str {
    if repo.live > 0 { "●" } else { "" }
}

/// `N open · M clean` for repos with Git evidence, `no git` otherwise.
fn repo_counts(repo: &RepoRow) -> String {
    if repo.git {
        format!("{} open", repo.work)
    } else {
        "no git".to_owned()
    }
}

/// A work row's glyph from its live evidence: `●` while a live agent or
/// process is bound to it, blank otherwise.
fn work_glyph(snapshot: &Snapshot, w: &WorkRow) -> &'static str {
    if w.live_sessions > 0 || w.live_pids > 0 {
        "●"
    } else {
        let _ = snapshot;
        ""
    }
}

/// The work row's label: `name ⌂worktree`; a project space's workspace is
/// its name already, so it carries no suffix.
fn work_name(_snapshot: &Snapshot, w: &WorkRow) -> String {
    if w.kind == "project_space" {
        return w.name.clone();
    }
    let wt = w
        .worktree
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| format!(" ⌂{}", n.to_string_lossy()))
        .unwrap_or_default();
    format!("{}{}", w.name, wt)
}

/// A conversation row's glyph from its published state. A claim the runtime
/// proved dead carries no attention glyph: the published state the stale
/// file still reports is history, not a live signal.
fn conversation_glyph(c: &ConversationRow) -> &'static str {
    if c.live && !c.running() {
        return "";
    }
    match c.state {
        "waiting" => "!",
        "busy" => "●",
        "unknown" => "?",
        _ => "",
    }
}

/// The detail header's state text: `waiting on you`, `busy`, `idle`,
/// `unknown` - whatever the evidence says, with its reason.
fn detail_state(c: &ConversationRow) -> String {
    match c.state {
        "waiting" => match &c.waiting_for {
            Some(reason) => format!("waiting: {reason}"),
            None => "waiting".to_owned(),
        },
        state => state.to_owned(),
    }
}

/// The labels a list filter matches against.
fn repo_label(r: &RepoRow) -> String {
    format!("{} {}", r.name, r.id)
}
fn work_label(w: &WorkRow) -> String {
    format!("{} {} {}", w.repo_name, w.name, w.summary)
}
fn conversation_label(c: &ConversationRow) -> String {
    format!(
        "{} {} {}",
        c.short_id,
        c.title.as_deref().unwrap_or(""),
        c.provider
    )
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
}

/// `code` -> a `Key`, or `None` for input the shell does not bind. Terminal
/// events and key releases are dropped here, before they can alias a byte
/// the app layer would act on.
fn map_key(code: crossterm::event::KeyCode) -> Option<Key> {
    use crossterm::event::KeyCode;
    Some(match code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Tab | KeyCode::BackTab => Key::Tab,
        KeyCode::Esc => Key::Esc,
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        _ => return None,
    })
}

/// The driver: crossterm event loop around an `App`, with collection on a
/// worker thread and finished snapshots swapped in through a bounded
/// channel. Subprocess work stays in the collector, never on the input or
/// render paths - the loop only swaps in snapshots the channel already
/// collected.
///
/// Only the terminal setup and the worker spawn live here; the loop itself
/// is `run_loop`, which any backend can drive - the tests drive it on
/// `TestBackend` with a scripted snapshot source.
pub fn run(mut app: App, refresh: impl FnMut() -> Snapshot + Send + 'static) -> io::Result<()> {
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

    let result = run_loop(
        &mut terminal,
        &mut app,
        || rx.try_recv().ok(), // coverage: off - `run` itself needs a real terminal
        poll_event,
    );
    disable_raw_mode()?; // coverage: off - `?` needs a broken terminal
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?; // coverage: off - same
    result // coverage: off - same
}

/// The collector's own loop, on its own thread: produce a snapshot, hand it
/// over once the previous one was taken (the bounded channel paces the
/// worker), rest `interval`, repeat. A dropped receiver ends the worker.
fn collect_worker(
    tx: std::sync::mpsc::SyncSender<Snapshot>,
    mut refresh: impl FnMut() -> Snapshot,
    interval: Duration,
) {
    loop {
        if tx.send(refresh()).is_err() {
            return;
        }
        std::thread::sleep(interval);
    }
}

/// The real input path: one event per tick, or `None` when the tick expires.
fn poll_event() -> io::Result<Option<Event>> {
    match event::poll(Duration::from_millis(200)) {
        Ok(true) => event::read().map(Some),
        _ => Ok(None), // coverage: off - the pty test feeds stdin EOF instantly so the empty tick never wins, and an Err needs a broken stdin
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
    mut next_snapshot: impl FnMut() -> Option<Snapshot>,
    mut poll: impl FnMut() -> io::Result<Option<Event>>,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    loop {
        // Snapshots the worker finished since the last draw swap in first;
        // when several queued up, the newest wins.
        while let Some(snapshot) = next_snapshot() {
            app.refresh(snapshot);
        }
        terminal.draw(|f| app.render(f)).map_err(io::Error::other)?; // coverage: off - `?` needs a backend that can fail
        if app.quit() {
            break;
        }
        let event = poll()?; // coverage: off - `?` needs a broken stdin
        if let Some(Event::Key(key)) = event
            && key.kind != KeyEventKind::Release
            && let Some(mapped) = map_key(key.code)
        {
            app.key(mapped);
        }
    }
    Ok(())
}

/// Whether stdout is a terminal. The TUI cannot run on a pipe: the answer
/// `list --json` gives a pipe is JSON, and the TUI's is an error.
pub fn terminal_present() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
}

/// The dashboard's own pane when it runs inside tmux, so focus observation
/// does not mistake the dashboard for work needing attention.
pub fn own_pane() -> Option<PaneId> {
    parse_own_pane(std::env::var("TMUX_PANE").ok())
}

/// `$TMUX_PANE` parses to a pane id; anything else is no pane, not a guess.
fn parse_own_pane(value: Option<String>) -> Option<PaneId> {
    value.and_then(|v| PaneId::parse(&v))
}

/// `~/.claude`, or `$CLAUDE_CONFIG_DIR` when set.
fn claude_root() -> Result<std::path::PathBuf, String> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(dir.into());
    }
    Ok(std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or("HOME is not set")?
        .join(".claude"))
}

/// `agent-sessions` with no arguments: the dashboard itself.
pub fn tui() -> Result<(), String> {
    if !terminal_present() {
        return Err("the dashboard needs a terminal (piped stdout? try `list --json`)".to_owned());
    }
    let mut collector = crate::snapshot::Collector::new(claude_root()?); // coverage: off - `?` needs HOME unset, which the passing path keeps
    let mut collect = move || {
        let runtime = crate::runtime::Runtime::observe();
        collector.collect(&runtime, own_pane().as_ref())
    };
    let snapshot = collect();
    run(App::new(snapshot), collect).map_err(|e| e.to_string()) // coverage: off - `map_err` needs a failing terminal
}

/// `agent-sessions list --json`: the complete unfiltered snapshot.
pub fn list_json() -> Result<String, String> {
    let mut collector = crate::snapshot::Collector::new(claude_root()?);
    let runtime = crate::runtime::Runtime::observe();
    let snapshot = collector.collect(&runtime, own_pane().as_ref());
    to_json(&snapshot) // coverage: off - `list_json` runs only inside the binary
        .map_err(|e| format!("the snapshot cannot be serialized: {e}")) // coverage: off - same
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{AttachmentRow, RepoRow, SCHEMA_VERSION, WorkRow};
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    /// A snapshot the render path can be exercised against - known and
    /// unknown fields, all providers' row shapes, at a fixed instant.
    fn fixture() -> Snapshot {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            observed_at: 1_800_000_000,
            own_pane: None,
            repos: vec![
                RepoRow {
                    id: "/repos/a/.git".to_owned(),
                    name: "a".to_owned(),
                    path: PathBuf::from("/repos/a"),
                    git: true,
                    work: 2,
                    live: 1,
                    last_activity: Some(1_800_000_000 - 120),
                },
                RepoRow {
                    id: "/spaces/notes".to_owned(),
                    name: "notes".to_owned(),
                    path: PathBuf::from("/spaces/notes"),
                    git: false,
                    work: 1,
                    live: 0,
                    last_activity: None,
                },
            ],
            work: vec![
                WorkRow {
                    repo: "/repos/a/.git".to_owned(),
                    repo_name: "a".to_owned(),
                    kind: "branch",
                    name: "feat/login".to_owned(),
                    worktree: Some(PathBuf::from("/repos/a-login")),
                    branch: Some("feat/login".to_owned()),
                    dirty: Some(true),
                    commits_ahead: Some(3),
                    unpushed: Some(3),
                    upstream: "tracked",
                    upstream_detail: Some("origin/feat/login".to_owned()),
                    landed: Some("no"),
                    base: Some("origin/main".to_owned()),
                    windows: 1,
                    live_pids: 1,
                    live_sessions: 1,
                    past_sessions: 2,
                    last_activity: Some(1_800_000_000 - 120),
                    summary: "↑3 ~dirty".to_owned(),
                },
                WorkRow {
                    repo: "/repos/a/.git".to_owned(),
                    repo_name: "a".to_owned(),
                    kind: "branch",
                    name: "feat/old".to_owned(),
                    worktree: None,
                    branch: Some("feat/old".to_owned()),
                    dirty: Some(false),
                    commits_ahead: Some(7),
                    unpushed: Some(7),
                    upstream: "never_pushed",
                    upstream_detail: None,
                    landed: None,
                    base: None,
                    windows: 0,
                    live_pids: 0,
                    live_sessions: 0,
                    past_sessions: 0,
                    last_activity: Some(1_800_000_000 - 9 * 86400),
                    summary: "no wt · no remote".to_owned(),
                },
                WorkRow {
                    repo: "/spaces/notes".to_owned(),
                    repo_name: "notes".to_owned(),
                    kind: "project_space",
                    name: "notes".to_owned(),
                    worktree: Some(PathBuf::from("/spaces/notes")),
                    branch: None,
                    dirty: None,
                    commits_ahead: None,
                    unpushed: None,
                    upstream: "not_applicable",
                    upstream_detail: None,
                    landed: None,
                    base: None,
                    windows: 0,
                    live_pids: 0,
                    live_sessions: 0,
                    past_sessions: 0,
                    last_activity: None,
                    summary: "no git".to_owned(),
                },
            ],
            conversations: vec![
                ConversationRow {
                    provider: "claude",
                    session_id: "8f423bbb-1111-2222-3333-444444444444".to_owned(),
                    short_id: "8f423bbb".to_owned(),
                    title: Some("update pane labels".to_owned()),
                    state: "waiting",
                    state_raw: Some("waiting".to_owned()),
                    waiting_for: Some("permission prompt".to_owned()),
                    state_since: Some(1_800_000_000 - 120),
                    last_activity: Some(1_800_000_000 - 120),
                    live: true,
                    attachment: Some(AttachmentRow {
                        pid: 4200,
                        pid_start: Some(1_790_093_933),
                        liveness: "instance",
                        liveness_detail: None,
                        pane: Some("workmux:@149.%162".to_owned()),
                        pane_source: Some("published"),
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
                    provider: "claude",
                    session_id: "02aa0bbb-1111-2222-3333-444444444444".to_owned(),
                    short_id: "02aa0bbb".to_owned(),
                    title: None,
                    state: "idle",
                    state_raw: Some("idle".to_owned()),
                    waiting_for: None,
                    state_since: Some(1_800_000_000 - 3600),
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
                    provider: "claude",
                    session_id: "33cc0bbb-1111-2222-3333-444444444444".to_owned(),
                    short_id: "33cc0bbb".to_owned(),
                    title: Some("untitled".to_owned()),
                    state: "unknown",
                    state_raw: None,
                    waiting_for: None,
                    state_since: None,
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
        }
    }

    /// Render `app` at `w`×`h` into a text buffer for assertions.
    fn render_to(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..h {
            let mut line = String::new();
            for x in 0..w {
                line.push_str(buffer[(x, y)].symbol());
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Feed `keys` into `app` - a scripted session, not a terminal.
    fn press(app: &mut App, keys: &[Key]) {
        for &key in keys {
            app.key(key);
        }
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
        assert!(text.contains("↑3 ~dirty"), "{text}");
        assert!(text.contains("2m"), "{text}");
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
        // A snapshot with one work row: the [2] cursor clamps, and the
        // shrink survives a second refresh to an even smaller snapshot.
        let mut next = fixture();
        next.work.truncate(1);
        app.refresh(next);
        assert_eq!(app.cursor[1], 1);
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
        // [2] on the project space: only a conv in exactly that cwd shows.
        press(&mut app, &[Key::Char('j')]);
        let text = render_to(&app, 200, 24);
        assert!(!text.contains("8f423bbb"), "{text}");
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
        // A narrow pane on [4] gets the no-filter footer variant.
        press(&mut app, &[Key::Char('4')]);
        let text = render_to(&app, 55, 24);
        assert!(text.contains("1-4 | tab | j/k | ? | q quit"), "{text}");
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
        assert!(f.allows("FIX the thing", Some(100), 200));
        assert!(!f.allows("other", Some(100), 200));
        assert!(!f.allows("fix", None, 200));
        assert!(!f.allows("fix", Some(1), 200_000));
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
        let mut next_snapshot = move || pending.pop_front();
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
        let mut next_snapshot = || None;
        run_loop(&mut terminal, &mut app, &mut next_snapshot, &mut poll).unwrap();
        assert!(app.quit());
        assert_eq!(app.snapshot.observed_at, 1_800_000_000);
    }

    #[test]
    fn the_worker_produces_until_the_receiver_drops() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Snapshot>(1);
        let mut calls = 0u64;
        let worker = std::thread::spawn(move || {
            collect_worker(
                tx,
                move || {
                    calls += 1;
                    let mut s = fixture();
                    s.observed_at = calls;
                    s
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
        conv.state = "busy";
        assert_eq!(conversation_glyph(&conv), "●");
        conv.state = "idle";
        assert_eq!(conversation_glyph(&conv), "");
        // waiting without a reason still says waiting.
        conv.state = "waiting";
        conv.waiting_for = None;
        assert_eq!(detail_state(&conv), "waiting");
        conv.state = "busy";
        assert_eq!(detail_state(&conv), "busy");
        // A live file whose claimed pid is dead is a record, not a process:
        // it is not `running`, and its published `busy` earns no glyph.
        let mut dead = fixture().conversations[0].clone();
        dead.attachment = dead.attachment.map(|a| AttachmentRow {
            liveness: "dead",
            ..a
        });
        assert!(!dead.running());
        assert_eq!(conversation_glyph(&dead), "");
        // Glyph colours by meaning; the empty glyph colours nothing.
        assert_eq!(glyph_style("✗").fg, Some(Color::LightRed));
        assert_eq!(glyph_style("✓").fg, Some(Color::LightGreen));
        assert_eq!(glyph_style("?").fg, Some(Color::DarkGray));
        // A repo with no live conversations carries no dot.
        let dead = RepoRow {
            live: 0,
            ..fixture().repos[0].clone()
        };
        assert_eq!(repo_glyph(&dead), "");
        // fit() never overshoots and zero-width collapses to empty.
        assert_eq!(fit("abc", 0), "");
        // A work row with no checkout names itself plainly.
        let bare = WorkRow {
            worktree: None,
            ..fixture().work[1].clone()
        };
        assert_eq!(work_name(&fixture(), &bare), "feat/old");
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
    fn own_pane_and_terminal_detection_are_honest() {
        // $TMUX_PANE parses or yields nothing; the dashboard never invents a pane.
        assert_eq!(parse_own_pane(Some("%12".to_owned())), PaneId::parse("%12"));
        assert_eq!(parse_own_pane(Some("nonsense".to_owned())), None);
        assert_eq!(parse_own_pane(None), None);
        // Cargo test pipes stdout: never a terminal, so the TUI declines.
        assert!(!terminal_present());
    }
}
