//! The Enter and `o` actions: select a conversation's pane or a work
//! row's window, resume a stopped conversation in the dashboard's own
//! place, open a forge item's recorded URL.
//!
//! A request is resolved twice by design: `App::key` turns the row the
//! cursor names into an [`ActionRequest`], and [`act`] re-resolves it
//! against a snapshot collected fresh after the keypress - never
//! against the frame the user saw. A target that vanished reports a
//! footer notice and changes nothing.
//!
//! This is the actions' only subprocess boundary, and a narrow one:
//! `tmux -S <socket> select-window`/`select-pane` for a jump, the
//! platform opener for a forge URL, and Unix `exec` for a resume.
//! Nothing here builds a shell string, creates or renames tmux
//! topology, sets an option, or guesses a URL - session ids and URLs
//! travel as single argv elements, the id from the fresh snapshot and
//! the URL exactly as the row recorded it.

use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::forge::WorkItem;
use crate::runtime::Provider;
use crate::snapshot::{ConversationRow, Snapshot, WorkRow};
use crate::store::{self, Store};
use crate::tmux::PaneTarget;

/// The platform's URL opener, found on PATH at action time.
#[cfg(target_os = "macos")]
const OPENER: &str = "open";
/// The platform's URL opener, found on PATH at action time.
#[cfg(not(target_os = "macos"))]
const OPENER: &str = "xdg-open";

/// What the Enter/`o` keys ask the loop to do. A request names a
/// durable identity - provider plus session id, or the work row's
/// selection key - never a cursor index or a display label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionRequest {
    /// Enter on a [3] conversation row: select its pane when it is
    /// live, resume it when it is stopped.
    EnterConversation {
        provider: Provider,
        session_id: String,
    },
    /// Enter on a [2] work row: select a window bound to its worktree.
    EnterWork { key: String },
    /// `o` on a [2] work row: open the forge item it recorded. The
    /// verdict and URL travel with the request: the re-resolve only
    /// proves the row still exists - its scoped collect never runs the
    /// forge ask that fills those fields.
    OpenForge {
        key: String,
        /// The row's recorded forge verdict.
        item: WorkItem,
        /// The URL `gh`/`glab` recorded for an open item.
        url: Option<String>,
    },
}

/// What a resolved action came to.
#[derive(Debug)]
pub enum ActionOutcome {
    /// The action ran; the message is what the footer shows until the
    /// next key, `None` for silence.
    Done(Option<String>),
    /// A stopped conversation resumes: the loop replaces the dashboard
    /// process with this plan.
    Resume(ResumePlan),
    /// The action could not run; the reason is the footer notice.
    Failed(String),
}

/// A stopped conversation's resume, resolved while the evidence was
/// fresh: the executable, the provider's remaining argv untouched - the
/// session id included, always one element - the Work root to land in,
/// and the acknowledgement written last, so a resume that cannot launch
/// records nothing.
#[derive(Debug, Clone)]
pub struct ResumePlan {
    /// argv[0] resolved to a runnable file.
    pub executable: OsString,
    /// argv[1..]: the provider's own arguments, unchanged.
    pub argv: Vec<OsString>,
    /// The recorded Work root the resumed process starts in.
    pub cwd: PathBuf,
    /// `(conversation key, through_seq, wait_ms)` - the seen-state
    /// `space` would write, deferred until the resume can no longer
    /// fail.
    pub acknowledgement: Option<(String, u64, Option<u64>)>,
}

/// The selection key a Work row carries - what `EnterWork` and
/// `OpenForge` name. The cursor's tracking key and the fresh
/// re-resolution must mean the same row, so both live here. Name alone
/// does not pin one row - two detached checkouts can sit on the same
/// commit and a recreated branch shares its name with the gone record a
/// reference still names - so the workspace path goes in too; a
/// branch-only row's empty path stays distinct from a gone row's
/// recorded workspace.
pub fn work_key(w: &WorkRow) -> String {
    let workspace = w
        .worktree
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!(
        "{}\u{0}{}\u{0}{}\u{0}{workspace}",
        w.repo,
        w.kind.as_str(),
        w.name
    )
}

/// Resolve `request` against `snapshot` - the local evidence collected
/// fresh for the keypress - and perform it: a jump selects through
/// tmux, an open invokes the platform opener, a resume returns the plan
/// the terminal execs.
/// `store` carries the seen-state a successful action acknowledges;
/// `path` is the PATH executable lookup searches, `None` inheriting the
/// process's own.
pub fn act(
    request: &ActionRequest,
    snapshot: &Snapshot,
    store: Option<&Store>,
    path: Option<&OsStr>,
) -> ActionOutcome {
    match request {
        ActionRequest::EnterConversation {
            provider,
            session_id,
        } => conversation(snapshot, *provider, session_id, store, path),
        ActionRequest::EnterWork { key } => work(snapshot, key),
        ActionRequest::OpenForge { key, item, url } => {
            forge(snapshot, key, *item, url.as_deref(), path)
        }
    }
}

/// `enter` on a conversation: the fresh evidence decides - a live claim
/// jumps to its bound pane, a stopped conversation resumes, a vanished
/// one reports and changes nothing.
fn conversation(
    snapshot: &Snapshot,
    provider: Provider,
    session_id: &str,
    store: Option<&Store>,
    path: Option<&OsStr>,
) -> ActionOutcome {
    let Some(row) = snapshot
        .conversations
        .iter()
        .find(|c| c.provider == provider && c.session_id == session_id)
    else {
        return failed("conversation gone");
    };
    if row.running() {
        return match row.attachment.as_ref().and_then(|a| a.target.as_ref()) {
            Some(target) => jump(row, target, store),
            // A live claim that bound no pane has nowhere to send the
            // user; nothing changes.
            None => failed("live, no bound pane"),
        };
    }
    resume(row, path)
}

/// `select-window`, then `select-pane` - the acknowledgement lands only
/// after both, so a select that failed records nothing.
fn jump(row: &ConversationRow, target: &PaneTarget, store: Option<&Store>) -> ActionOutcome {
    if let Err(e) = tmux(&target.socket, "select-window", target.window.as_str())
        .and_then(|()| tmux(&target.socket, "select-pane", target.pane.as_str()))
    {
        return failed(e);
    }
    if let (Some(store), Some((key, seq, wait_ms))) = (store, acknowledgement(row))
        && let Err(e) = store.acknowledge(&key, seq, wait_ms)
    {
        let message = format!("selected {} - seen-state not saved: {e}", target.pane); // coverage: off - a seen-state write failure needs a filesystem fault
        return ActionOutcome::Done(Some(message)); // coverage: off - same
    }
    ActionOutcome::Done(Some(format!("selected {}", target.pane)))
}

/// `enter` on a stopped conversation: the provider's own argv split for
/// exec, the recorded Work root as cwd, the pending acknowledgement
/// deferred into the plan. Every gap reports and launches nothing.
fn resume(row: &ConversationRow, path: Option<&OsStr>) -> ActionOutcome {
    let Some(argv0) = row.resume_argv.first() else {
        // No verified resume invocation - the provider carries none.
        return failed("no resume");
    };
    let Some(cwd) = row.worktree.as_deref() else {
        return failed("no work root");
    };
    if !cwd.is_dir() {
        return failed("work root gone");
    }
    let Some(executable) = executable(argv0, path) else {
        return failed(format!("{argv0}: not found"));
    };
    ActionOutcome::Resume(ResumePlan {
        executable,
        argv: row.resume_argv[1..].iter().map(OsString::from).collect(),
        cwd: cwd.to_owned(),
        acknowledgement: acknowledgement(row),
    })
}

/// `enter` on a work row: `select-window` on the first bound window,
/// deterministic across servers - the targets sort on `(socket,
/// window)` and dedupe so a window is selected once. A row that lost
/// its bound panes changes nothing.
fn work(snapshot: &Snapshot, key: &str) -> ActionOutcome {
    let Some(row) = snapshot.work.iter().find(|w| work_key(w) == key) else {
        return failed("work row gone");
    };
    let mut targets: Vec<&PaneTarget> = row
        .panes
        .iter()
        .filter_map(|pane| pane.target.as_ref())
        .collect();
    targets.sort_by(|a, b| (&a.socket, &a.window).cmp(&(&b.socket, &b.window)));
    targets.dedup_by(|a, b| a.socket == b.socket && a.window == b.window);
    let Some(target) = targets.first() else {
        return failed("no bound window");
    };
    match tmux(&target.socket, "select-window", target.window.as_str()) {
        Ok(()) => ActionOutcome::Done(Some(format!("selected {}", target.window))),
        Err(e) => failed(e),
    }
}

/// `o` on a work row: the recorded URL through the platform opener,
/// exactly one argv element. Re-resolution only proves the row still
/// exists - the verdict and URL are the recorded evidence the request
/// carries, since the action's local collect leaves forge fields unset.
/// Every other forge state is only a report - no opener runs, and
/// nothing else changes.
fn forge(
    snapshot: &Snapshot,
    key: &str,
    item: WorkItem,
    url: Option<&str>,
    path: Option<&OsStr>,
) -> ActionOutcome {
    if !snapshot.work.iter().any(|w| work_key(w) == key) {
        return failed("work row gone");
    }
    match item {
        WorkItem::Open => {}
        WorkItem::NotExisting => return failed("not existing"),
        WorkItem::Closed => return failed("closed"),
        WorkItem::Unknown => return failed("?"),
    }
    let Some(url) = url else {
        return failed("unavailable");
    };
    let Some(opener) = executable(OPENER, path) else {
        return failed(format!("{OPENER}: not found"));
    };
    match Command::new(&opener).arg(url).stdin(Stdio::null()).output() {
        Ok(out) if out.status.success() => ActionOutcome::Done(Some(format!("opened {url}"))),
        Ok(out) => failed(format!("{OPENER}: {}", status_detail(&out))),
        Err(e) => failed(format!("{OPENER}: {e}")), // coverage: off - spawn needs a race between resolve and exec
    }
}

/// The resume's terminal half: enter the Work root, re-check the
/// executable, write the authored acknowledgement, then `exec` - the
/// process is replaced, so this returns only a failure. Every failure
/// path leaves the dashboard's cwd as it was; `exec` never returns on
/// success, so the guard only ever fires on failure. The
/// acknowledgement lands once preflight is passed - execve's own
/// refusal can still follow it, a race no preflight can close; the
/// report and the cwd restore are the same either way.
pub fn exec_resume(plan: &ResumePlan, store: Option<&Store>) -> Result<Infallible, String> {
    use std::os::unix::process::CommandExt;

    let original_cwd =
        std::env::current_dir().map_err(|e| format!("cannot read current directory: {e}"))?;
    let _restore = RestoreOnReturn(original_cwd);
    std::env::set_current_dir(&plan.cwd)
        .map_err(|e| format!("cannot enter {}: {e}", plan.cwd.display()))?;
    if !runnable(Path::new(&plan.executable)) {
        // The resolver's check may already be stale, and an executable
        // that cannot run must refuse before seen-state moves.
        return Err(format!(
            "{}: not executable",
            plan.executable.to_string_lossy()
        ));
    }
    if let (Some(store), Some((key, seq, wait_ms))) = (store, &plan.acknowledgement)
        && let Err(e) = store.acknowledge(key, *seq, *wait_ms)
    {
        return Err(format!("seen-state not saved: {e}"));
    }
    let mut command = Command::new(&plan.executable);
    command.args(&plan.argv);
    Err(format!(
        "{}: {}",
        plan.executable.to_string_lossy(),
        command.exec()
    ))
}

/// Put the original working directory back when `exec_resume` returns.
/// A successful `exec` never returns - it replaces the process - so the
/// guard restores on failure paths only.
struct RestoreOnReturn(PathBuf);

impl Drop for RestoreOnReturn {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

/// The seen-state tuple a deliberate action writes: the journal
/// sequence it acknowledges through and the live wait episode it names,
/// present only while something still asks for attention - the same
/// contents `space` would write.
fn acknowledgement(row: &ConversationRow) -> Option<(String, u64, Option<u64>)> {
    (row.attention_seq.is_some() || row.attention_wait_ms.is_some()).then(|| {
        (
            store::conversation_key(row.provider.as_str(), &row.session_id),
            row.attention_seq.unwrap_or(0),
            row.attention_wait_ms,
        )
    })
}

/// `argv0` resolved to a runnable file: an explicit path (one carrying
/// `/`) must itself be a regular executable; a bare name is searched on
/// `path` - the inherited PATH when `None` - first match winning.
/// Anything else resolves to nothing, and the caller reports rather
/// than guesses. Relative candidates anchor on the dashboard's cwd, not
/// the Work root - `exec` changes into that root only after the plan is
/// already built, so a relative path must name the file the resolver
/// saw.
fn executable(argv0: &str, path: Option<&OsStr>) -> Option<OsString> {
    if argv0.contains('/') {
        let candidate = absolute(Path::new(argv0));
        return runnable(&candidate).then(|| candidate.into_os_string());
    }
    let search = path
        .map(OsStr::to_owned)
        .or_else(|| std::env::var_os("PATH"))?; // coverage: off - the `?` needs an environment without PATH
    std::env::split_paths(&search)
        .map(|dir| absolute(&dir.join(argv0)))
        .find(|candidate| runnable(candidate))
        .map(|candidate| candidate.into_os_string())
}

/// `path` made absolute: absolute paths pass through, relative ones
/// anchor on the process's current directory - the cwd the resolver is
/// standing in, before any exec-time change into the Work root. The
/// components round strips the `.` a `./tool` argv0 would leave behind.
fn absolute(path: &Path) -> PathBuf {
    std::env::current_dir()
        .map(|cwd| cwd.join(path).components().collect())
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `candidate` is a regular file with an execute bit - the only
/// thing exec can run.
fn runnable(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    candidate
        .metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// `tmux -S <socket> <verb> -t <target>` - selection is the only write
/// tmux gets from this tool.
fn tmux(socket: &Path, verb: &str, target: &str) -> Result<(), String> {
    let out = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args([verb, "-t", target])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("tmux {verb}: {e}"))?; // coverage: off - needs a PATH without tmux
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("tmux {verb}: {}", status_detail(&out)))
    }
}

/// A finished spawn's reportable detail: its stderr, else the exit
/// status itself.
fn status_detail(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    if stderr.is_empty() {
        out.status.to_string()
    } else {
        stderr
    }
}

/// A one-word failure outcome.
fn failed(detail: impl std::fmt::Display) -> ActionOutcome {
    ActionOutcome::Failed(detail.to_string())
}
