//! Read-only tmux evidence: socket discovery and the merged
//! `list-panes -a` inventory across every reachable server.
//!
//! One whole-server snapshot is taken per socket per observation and the
//! results merge into a single inventory, so a pane on a non-default `-L`
//! or `-S` server resolves exactly like one on the default socket. An
//! absent or unreachable server contributes nothing and is recorded, never
//! an error that takes other evidence down with it.
//!
//! tmux is only ever *read* here. A write - an option, a window name, a
//! killed pane - would fight the user's configuration and the tools that
//! own those writes, so the only argv this module runs is `list-panes`.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A tmux id as the server prints it: `%` for panes, `@` for windows, `$`
/// for sessions. Only ever built from tmux's own output or a provider
/// handle parsed the same way, so it can be spliced into argv without
/// quoting.
macro_rules! tmux_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// `text` starting with the id's own sigil and all digits after.
            pub fn parse(text: &str) -> Option<$name> {
                let number = text.strip_prefix($prefix)?;
                (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| $name(text.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

tmux_id!(PaneId, "%");
tmux_id!(WindowId, "@");
tmux_id!(SessionId, "$");

/// A pane on a specific server: `%3` is unique within one socket and says
/// nothing across two, so a pane's identity is socket plus id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneRef {
    /// The socket path the pane was read from; the server's identity.
    pub socket: PathBuf,
    pub pane: PaneId,
}

impl fmt::Display for PaneRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.socket.display(), self.pane)
    }
}

/// One pane of the merged inventory, as `list-panes -a` reported it.
#[derive(Debug, Clone)]
pub struct Pane {
    /// The server this pane lives on.
    pub socket: PathBuf,
    pub id: PaneId,
    pub window: WindowId,
    pub session: SessionId,
    /// The session's name - what a provider's published handle carries.
    pub session_name: String,
    /// `pane_pid`: the pane's root process. Descendants resolve here.
    pub pid: u32,
    /// `pane_current_command`.
    pub command: String,
    /// `pane_current_path`, when tmux tracks one.
    pub cwd: Option<PathBuf>,
    /// The pane's pty as `/dev/...`; joins to a process's controlling tty.
    pub tty: Option<String>,
    /// The active pane of its window.
    pub active: bool,
    /// The last-active pane of its window.
    pub last: bool,
    /// The current window of its session.
    pub window_active: bool,
    /// `window_activity`: last activity epoch of the pane's window.
    pub window_activity: Option<SystemTime>,
    /// Clients attached to the pane's session; `0` is detached.
    pub session_attached: u32,
    /// The window's stored worktree binding, when the worktree tool
    /// published it. Read-only metadata; absence means nothing.
    pub wt_adminid: Option<String>,
    /// The window's stored handle, when published.
    pub wt_handle: Option<String>,
}

impl Pane {
    /// Whether this pane's evidence binds it to `worktree`: the stored
    /// admin-id edge when the window publishes one, else the derived edge -
    /// a pane cwd at or below the worktree root. The stored edge decides
    /// outright when present: a window whose stored id names another
    /// worktree belongs to that worktree even if a pane has since `cd`-ed
    /// into this one.
    pub fn binds_worktree(&self, admin_id: Option<&str>, worktree: &Path) -> bool {
        match (&self.wt_adminid, admin_id) {
            (Some(stored), Some(id)) => return stored == id,
            (Some(_), None) => return false,
            _ => {}
        }
        self.cwd.as_deref().is_some_and(|cwd| {
            let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
            cwd.starts_with(worktree)
        })
    }
}

/// A tmux read that failed. `code` is the client exit status.
#[derive(Debug)]
pub struct Error {
    pub argv: String,
    pub code: Option<i32>,
    pub detail: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "`{}` exited {code}: {}", self.argv, self.detail),
            None => write!(f, "`{}`: {}", self.argv, self.detail),
        }
    }
}

impl std::error::Error for Error {}

/// One socket's contribution to the inventory: the panes it answered with,
/// or the reason it answered nothing.
#[derive(Debug)]
pub struct Server {
    pub socket: PathBuf,
    /// Why the server contributed nothing (`None` when it answered).
    pub error: Option<String>,
}

/// The merged result of one snapshot pass over every reachable server.
#[derive(Debug, Default)]
pub struct PaneInventory {
    pub panes: Vec<Pane>,
    /// One entry per socket attempted, answering or not.
    pub servers: Vec<Server>,
    /// Records the server printed that this tool did not understand - kept
    /// as evidence rather than silently dropped, since an invisible pane
    /// cannot claim liveness.
    pub warnings: Vec<String>,
}

/// The `list-panes -a` format, one field per pane record. Read-only and
/// whole-server: the per-server cost of a refresh is one subprocess.
///
/// `|` is the separator because it is *printable*: tmux does not promise
/// control characters through `-F` output - one build renders a format
/// `\t` literally, another substitutes `_` - and a field value carrying a
/// `|` just costs that one record, which reports as a warning rather
/// than being misread.
const LIST_FORMAT: &str = concat!(
    "#{pane_id}|#{pane_pid}|#{pane_tty}|#{pane_current_command}",
    "|#{pane_current_path}|#{pane_active}|#{pane_last}|#{window_id}",
    "|#{window_active}|#{window_activity}|#{session_id}|#{session_name}",
    "|#{session_attached}|#{@wt_adminid}|#{@wt_handle}"
);

const FIELD_COUNT: usize = 15;

impl PaneInventory {
    /// One snapshot per socket in `sockets`, merged. Each socket is asked
    /// exactly once; unreachable ones are recorded and skipped.
    pub fn collect(sockets: &[PathBuf]) -> PaneInventory {
        let mut inventory = PaneInventory::default();
        let mut seen = HashSet::new();
        for socket in sockets {
            if !seen.insert(socket.clone()) {
                continue;
            }
            match list_panes(socket) {
                Ok(panes) => {
                    for pane in panes {
                        match pane {
                            Ok(pane) => inventory.panes.push(pane),
                            Err(line) => inventory
                                .warnings
                                .push(format!("{}: unparsed pane `{line}`", socket.display())),
                        }
                    }
                    inventory.servers.push(Server {
                        socket: socket.clone(),
                        error: None,
                    });
                }
                Err(e) => inventory.servers.push(Server {
                    socket: socket.clone(),
                    error: Some(e.to_string()),
                }),
            }
        }
        inventory
    }

    /// The inventory of every server discoverable from the environment:
    /// the sockets in the user's tmux socket dir plus the one `$TMUX`
    /// names. A machine with no tmux server reads as an empty inventory.
    #[rustfmt::skip]
    pub fn discover() -> PaneInventory { Self::discover_in(std::env::var_os("TMUX_TMPDIR").as_deref(), std::env::var_os("TMUX").as_deref()) } // coverage: off - reads the ambient environment, which tests must not touch

    /// [`discover`] with the environment passed explicitly, so tests and
    /// non-process callers can point discovery at a fixture socket dir.
    pub fn discover_in(tmux_tmpdir: Option<&OsStr>, tmux_env: Option<&OsStr>) -> PaneInventory {
        Self::collect(&sockets(tmux_tmpdir, tmux_env))
    }

    /// The pane with this id, wherever it lives. Pane ids are server-local,
    /// so more than one server can answer.
    pub fn by_pane_id(&self, id: &PaneId) -> Vec<&Pane> {
        self.panes.iter().filter(|pane| &pane.id == id).collect()
    }

    /// The pane whose root process is `pid` (`pane_pid`), anywhere.
    pub fn by_pane_pid(&self, pid: u32) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.pid == pid)
    }

    /// The pane(s) sharing a controlling terminal. In practice at most one
    /// pane owns a pty; more means the evidence is ambiguous.
    pub fn by_tty(&self, tty: &str) -> Vec<&Pane> {
        self.panes
            .iter()
            .filter(|pane| pane.tty.as_deref() == Some(tty))
            .collect()
    }

    /// The windows bound to a worktree, for counting: the stored admin-id
    /// edge decides when a window publishes it, and a window with no stored
    /// edge is bound when any of its panes reports a cwd inside the
    /// worktree. That is what keeps a hand-made window visible without
    /// ever claiming it.
    pub fn windows_bound(&self, admin_id: Option<&str>, worktree: &Path) -> usize {
        let mut windows = HashSet::new();
        for pane in &self.panes {
            if pane.binds_worktree(admin_id, worktree) {
                windows.insert((pane.socket.clone(), pane.window.clone()));
            }
        }
        windows.len()
    }

    /// The panes whose session has no client attached.
    pub fn in_detached_sessions(&self) -> Vec<&Pane> {
        self.panes
            .iter()
            .filter(|pane| pane.session_attached == 0)
            .collect()
    }
}

/// Every socket worth asking: the entries of the user's tmux socket
/// directory plus the socket `$TMUX` names, deduplicated by path.
fn sockets(tmux_tmpdir: Option<&OsStr>, tmux_env: Option<&OsStr>) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    if let Some(dir) = socket_dir(tmux_tmpdir)
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            // tmux leaves only sockets in this directory; a stray file is
            // not worth a subprocess that would fail anyway.
            if entry
                .file_type()
                .is_ok_and(|t| t.is_socket() || t.is_symlink())
            {
                found.push(path);
            }
        }
    }
    // `$TMUX` is `<socket path>,<server pid>,<session id>`; only the socket
    // matters, and it is the only way a server outside the default dir is
    // found.
    if let Some(tmux) = tmux_env.and_then(|t| t.to_str())
        && let Some(socket) = tmux.split(',').next()
        && !socket.is_empty()
    {
        found.push(PathBuf::from(socket));
    }
    found.sort();
    found.dedup();
    found
}

/// The user's tmux socket directory: tmux always places it at
/// `<base>/tmux-<uid>`, where `base` is `$TMUX_TMPDIR` or `/tmp`.
/// `None` when no uid can be learned.
fn socket_dir(tmux_tmpdir: Option<&OsStr>) -> Option<PathBuf> {
    let base = match tmux_tmpdir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("/tmp"),
    };
    uid().map(|uid| base.join(format!("tmux-{uid}")))
}

/// The real uid via `id -u`: the last part of the default socket path.
/// `None` when `id` cannot run - discovery then relies on `$TMUX` alone.
fn uid() -> Option<u32> {
    let out = Command::new("id").arg("-u").output().ok()?; // coverage: off - needs a PATH without id
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
        .flatten()
}

/// One `tmux -S <socket> list-panes -a` read. The socket is always a path,
/// which handles `-L` and `-S` servers identically; `LC_ALL=C` keeps the
/// output stable.
fn list_panes(socket: &Path) -> Result<Vec<Result<Pane, String>>, Error> {
    #[rustfmt::skip]
    let out = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(["list-panes", "-a", "-F", LIST_FORMAT])
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| Error { argv: "tmux list-panes".to_owned(), code: None, detail: e.to_string() })?; // coverage: off - needs a PATH without tmux
    if !out.status.success() {
        return Err(Error {
            argv: format!("tmux -S {} list-panes -a", socket.display()),
            code: out.status.code(),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| parse_pane(socket, line))
        .collect())
}

/// One `list-panes` record into a [`Pane`]; `Err(line)` when the record is
/// not what was asked for. Every field was requested explicitly, so a
/// wrong count is a tmux that did not answer the question asked - a
/// record worth reporting, not guessing at.
fn parse_pane(socket: &Path, line: &str) -> Result<Pane, String> {
    let fields: Vec<&str> = line.split('|').collect();
    if fields.len() != FIELD_COUNT {
        return Err(line.to_owned());
    }
    let parse = |text: &str, what: &str| -> Result<_, String> {
        text.parse::<u32>()
            .map_err(|_| format!("{line} ({what} `{text}`)"))
    };
    let epoch = |text: &str| -> Option<SystemTime> {
        text.parse::<u64>()
            .ok()
            .filter(|s| *s > 0)
            .map(|s| UNIX_EPOCH + Duration::from_secs(s))
    };
    let opt = |text: &str| (!text.is_empty()).then(|| text.to_owned());
    let non_empty = |text: &str| (!text.is_empty()).then(|| PathBuf::from(text));

    let id = PaneId::parse(fields[0]).ok_or_else(|| line.to_owned())?;
    let pid = parse(fields[1], "pane_pid")?;
    let window = WindowId::parse(fields[7]).ok_or_else(|| line.to_owned())?;
    let session = SessionId::parse(fields[10]).ok_or_else(|| line.to_owned())?;
    Ok(Pane {
        socket: socket.to_owned(),
        id,
        window,
        session,
        session_name: fields[11].to_owned(),
        pid,
        command: fields[3].to_owned(),
        cwd: non_empty(fields[4]),
        tty: opt(fields[2]).map(|t| {
            if t.starts_with("/dev/") {
                t.to_owned()
            } else {
                format!("/dev/{t}")
            }
        }),
        active: fields[5] == "1",
        last: fields[6] == "1",
        window_active: fields[8] == "1",
        window_activity: epoch(fields[9]),
        session_attached: parse(fields[12], "session_attached")?,
        wt_adminid: opt(fields[13]),
        wt_handle: opt(fields[14]),
    })
}

/// A provider-published tmux handle: `session:@window.%pane` in full, or
/// any suffix of it down to a bare `%pane`. Only the ids a handle carries
/// constrain matching - a session name is corroboration, never identity,
/// because names are not unique across servers.
#[derive(Debug, Default)]
pub struct PublishedHandle {
    pub pane: Option<PaneId>,
    pub window: Option<WindowId>,
    /// Session name or `$id`, for corroboration only.
    pub session: Option<String>,
}

impl PublishedHandle {
    /// Parse `text` into a handle. `session:window.pane` pieces split on
    /// `:` and `.`; components that are not `%N`, `@N` or a leading session
    /// word do not disqualify the handle - they just do not constrain it.
    /// `None` only when nothing usable was found at all.
    pub fn parse(text: &str) -> Option<PublishedHandle> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let (session, rest) = match text.split_once(':') {
            Some((session, rest)) if !session.is_empty() => (Some(session.to_owned()), rest),
            _ => (None, text),
        };
        let mut handle = PublishedHandle {
            pane: None,
            window: None,
            session,
        };
        for part in rest.split('.') {
            if part.starts_with('%') {
                handle.pane = PaneId::parse(part);
            } else if part.starts_with('@') {
                handle.window = WindowId::parse(part);
            }
        }
        (handle.pane.is_some() || handle.window.is_some() || handle.session.is_some())
            .then_some(handle)
    }
}

impl PaneInventory {
    /// Panes matching every component the published handle carries. An
    /// empty match is a stale handle; more than one is ambiguous.
    pub fn matching(&self, handle: &PublishedHandle) -> Vec<&Pane> {
        self.panes
            .iter()
            .filter(|pane| {
                let pane_ok = match &handle.pane {
                    Some(id) => &pane.id == id,
                    None => true,
                };
                let window_ok = match &handle.window {
                    Some(id) => &pane.window == id,
                    None => true,
                };
                let session_ok = match &handle.session {
                    Some(name) => {
                        &pane.session_name == name || pane.session.as_str() == name.as_str()
                    }
                    None => true,
                };
                pane_ok && window_ok && session_ok
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal pane for tests that exercise matching and binding
    /// rather than parsing.
    fn pane(id: &str, window: &str, session: &str) -> Pane {
        Pane {
            socket: PathBuf::from("/sock/a"),
            id: PaneId::parse(id).unwrap(),
            window: WindowId::parse(window).unwrap(),
            session: SessionId::parse(session).unwrap(),
            session_name: "s".to_owned(),
            pid: 100,
            command: "sleep".to_owned(),
            cwd: None,
            tty: None,
            active: true,
            last: true,
            window_active: true,
            window_activity: None,
            session_attached: 0,
            wt_adminid: None,
            wt_handle: None,
        }
    }

    /// A one-pane inventory.
    fn inventory(panes: Vec<Pane>) -> PaneInventory {
        PaneInventory {
            panes,
            servers: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn pane_ids_lookup_panes_and_display() {
        let mut a = pane("%3", "@1", "$0");
        a.tty = Some("/dev/ttys001".to_owned());
        let b = pane("%3", "@2", "$0");
        let inv = inventory(vec![a, b]);
        // The same `%3` exists on both sockets; lookup returns all of
        // them, and a PaneRef pins the socket.
        assert_eq!(inv.by_pane_id(&PaneId::parse("%3").unwrap()).len(), 2);
        assert_eq!(inv.by_pane_id(&PaneId::parse("%4").unwrap()).len(), 0);
        assert_eq!(inv.panes[0].id.to_string(), "%3");
        assert_eq!(inv.panes[0].window.to_string(), "@1");
        assert_eq!(inv.panes[0].session.to_string(), "$0");
    }

    #[test]
    fn a_stored_worktree_edge_decides_the_binding() {
        let worktree = std::env::temp_dir().canonicalize().unwrap();
        // Stored id matching the claim binds even with no cwd.
        let mut p = pane("%1", "@1", "$0");
        p.wt_adminid = Some("adm".to_owned());
        assert!(p.binds_worktree(Some("adm"), &worktree));
        assert!(!p.binds_worktree(Some("other"), &worktree));
        assert!(!p.binds_worktree(None, &worktree));
        // No stored edge: a cwd inside the worktree binds, outside does
        // not, and a cwd that cannot be canonicalized binds raw.
        p.wt_adminid = None;
        p.cwd = Some(worktree.join("inside"));
        assert!(p.binds_worktree(None, &worktree));
        p.cwd = Some(PathBuf::from("/no/such/dir/here"));
        assert!(!p.binds_worktree(None, &worktree));
    }

    #[test]
    fn matching_uses_every_component_the_handle_carries() {
        let mut a = pane("%3", "@1", "$0");
        a.session_name = "workmux".to_owned();
        let b = pane("%3", "@9", "$0");
        let inv = inventory(vec![a, b]);

        // Pane id alone matches both sockets.
        let handle = PublishedHandle::parse("%3").unwrap();
        assert_eq!(inv.matching(&handle).len(), 2);
        // A window alone narrows to one.
        let handle = PublishedHandle::parse("@1").unwrap();
        assert_eq!(inv.matching(&handle).len(), 1);
        // A session name corroborates down to one.
        let handle = PublishedHandle::parse("workmux:@1.%3").unwrap();
        assert_eq!(inv.matching(&handle).len(), 1);
        // A `$` session id also corroborates.
        let handle = PublishedHandle::parse("$0:@1.%3").unwrap();
        assert_eq!(inv.matching(&handle).len(), 1);
        // A wrong window id matches nothing.
        let handle = PublishedHandle::parse("workmux:@8.%3").unwrap();
        assert!(inv.matching(&handle).is_empty());
    }

    #[test]
    fn errors_describe_their_argv() {
        let err = Error {
            argv: "tmux -S x list-panes".to_owned(),
            code: Some(1),
            detail: "no server".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "`tmux -S x list-panes` exited 1: no server"
        );
        let err = Error {
            argv: "tmux list-panes".to_owned(),
            code: None,
            detail: "spawn failed".to_owned(),
        };
        assert_eq!(err.to_string(), "`tmux list-panes`: spawn failed");
    }

    #[test]
    fn ids_validate_their_sigil() {
        assert!(PaneId::parse("%12").is_some());
        assert!(PaneId::parse("%").is_none());
        assert!(PaneId::parse("%a").is_none());
        assert!(PaneId::parse("@12").is_none());
        assert!(WindowId::parse("@0").is_some());
        assert!(SessionId::parse("$3").is_some());
    }

    #[test]
    fn published_handles_parse_every_shape() {
        let full = PublishedHandle::parse("workmux:@149.%162").unwrap();
        assert_eq!(full.pane.as_ref().map(|p| p.as_str()), Some("%162"));
        assert_eq!(full.window.as_ref().map(|w| w.as_str()), Some("@149"));
        assert_eq!(full.session.as_deref(), Some("workmux"));

        let bare = PublishedHandle::parse("%5").unwrap();
        assert_eq!(bare.pane.as_ref().map(|p| p.as_str()), Some("%5"));
        assert_eq!(bare.window, None);
        assert_eq!(bare.session, None);

        // A window+pane without a session.
        let wp = PublishedHandle::parse("@3.%7").unwrap();
        assert_eq!(wp.window.as_ref().map(|w| w.as_str()), Some("@3"));
        assert_eq!(wp.pane.as_ref().map(|p| p.as_str()), Some("%7"));

        // A session plus pane without a window.
        let sp = PublishedHandle::parse("s:.%4").unwrap();
        assert_eq!(sp.pane.as_ref().map(|p| p.as_str()), Some("%4"));

        // Nothing tmux-shaped at all.
        assert!(PublishedHandle::parse("").is_none());
        assert!(PublishedHandle::parse("not-a-handle").is_none());
        // A word before a colon is a session name even alone.
        let sess = PublishedHandle::parse("mysess:").unwrap();
        assert_eq!(sess.session.as_deref(), Some("mysess"));
        assert_eq!(sess.pane, None);
    }

    #[test]
    fn pane_records_parse_and_reject() {
        let socket = Path::new("/tmp/tmux-501/default");
        let line = concat!(
            "%3|80255|/dev/ttys010|sleep|/wt/repo|1|0|@0|1|1790348629",
            "|$0|main|0|admin-1|handle-x"
        );
        let pane = parse_pane(socket, line).expect("a pane record");
        assert_eq!(pane.id.as_str(), "%3");
        assert_eq!(pane.pid, 80255);
        assert_eq!(pane.tty.as_deref(), Some("/dev/ttys010"));
        assert_eq!(pane.command, "sleep");
        assert_eq!(pane.cwd.as_deref(), Some(Path::new("/wt/repo")));
        assert!(pane.active);
        assert!(!pane.last);
        assert!(pane.window_active);
        assert_eq!(pane.window.as_str(), "@0");
        assert_eq!(pane.session.as_str(), "$0");
        assert_eq!(pane.session_name, "main");
        assert_eq!(pane.session_attached, 0);
        assert_eq!(
            pane.window_activity,
            Some(UNIX_EPOCH + Duration::from_secs(1_790_348_629))
        );
        assert_eq!(pane.wt_adminid.as_deref(), Some("admin-1"));
        assert_eq!(pane.wt_handle.as_deref(), Some("handle-x"));

        // Empty option and cwd fields read as absent.
        let sparse = parse_pane(socket, "%0|1|ttys001|sh||0|1|@2|0|0|$1|s|1||").unwrap();
        assert_eq!(sparse.tty.as_deref(), Some("/dev/ttys001"));
        assert_eq!(sparse.cwd, None);
        assert_eq!(sparse.window_activity, None);
        assert_eq!(sparse.wt_adminid, None);
        assert_eq!(sparse.session_attached, 1);

        // Wrong field count and bad ids are records we cannot use.
        assert!(parse_pane(socket, "%0|1").is_err());
        assert!(parse_pane(socket, "x|1|/dev/ttys1|sh|/p|0|0|@0|0|0|$0|s|0||").is_err());
        assert!(parse_pane(socket, "%0|x|/dev/ttys1|sh|/p|0|0|@0|0|0|$0|s|0||").is_err());
        // A bad window id, session id or attachment count is the same.
        assert!(parse_pane(socket, "%0|1|/dev/ttys1|sh|/p|0|0|w0|0|0|$0|s|0||").is_err());
        assert!(parse_pane(socket, "%0|1|/dev/ttys1|sh|/p|0|0|@0|0|0|s0|s|0||").is_err());
        assert!(parse_pane(socket, "%0|1|/dev/ttys1|sh|/p|0|0|@0|0|0|$0|s|x||").is_err());
    }

    #[test]
    fn socket_discovery_lists_the_dir_and_tmux_env() {
        // tmux places sockets under `<base>/tmux-<uid>`; a $TMUX_TMPDIR
        // base therefore has its sockets one directory down.
        let base = std::env::temp_dir().join(format!("agent-sessions-sock-{}", std::process::id()));
        let dir = base.join(format!("tmux-{}", uid().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        // A socket file, a plain file (skipped) and a symlink.
        let sock = dir.join("server-a");
        std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::write(dir.join("not-a-socket"), "x").unwrap();
        let link = dir.join("linked");
        std::os::unix::fs::symlink(&sock, &link).unwrap();

        let found = sockets(Some(base.as_os_str()), None);
        assert_eq!(found, vec![link.clone(), sock.clone()]);

        // $TMUX adds its socket and deduplicates a path already listed.
        let tmux = std::ffi::OsString::from(format!("{},1,0", sock.display()));
        let found = sockets(Some(base.as_os_str()), Some(&tmux));
        assert_eq!(found, vec![link, sock.clone()]);
        let other = std::ffi::OsString::from("/elsewhere/sock,1,0");
        let found = sockets(Some(base.as_os_str()), Some(&other));
        assert!(found.contains(&PathBuf::from("/elsewhere/sock")));

        // No dir and no $TMUX is no sockets.
        assert!(sockets(Some(OsStr::new("/no/such/dir")), None).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_default_socket_dir_uses_the_uid() {
        // tmux always names it `tmux-<uid>` under the base dir.
        assert_eq!(
            socket_dir(Some(OsStr::new("/custom"))),
            Some(PathBuf::from(format!("/custom/tmux-{}", uid().unwrap())))
        );
        // `/tmp` is the base when $TMUX_TMPDIR is unset.
        let dir = socket_dir(None).expect("uid can be learned");
        let expected = PathBuf::from(format!("/tmp/tmux-{}", uid().unwrap()));
        assert_eq!(dir, expected);
    }
}
