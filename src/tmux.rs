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

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
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

/// A pane's socket-qualified location for an action: the server, window
/// and pane that `select-window`/`select-pane` need. Built only
/// from an inventory record - never parsed back out of a display label
/// or a provider handle, which cannot carry the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneTarget {
    /// The socket path of the server the pane lives on.
    pub socket: PathBuf,
    pub window: WindowId,
    pub pane: PaneId,
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
    /// The pane's socket-qualified action target.
    pub fn target(&self) -> PaneTarget {
        PaneTarget {
            socket: self.socket.clone(),
            window: self.window.clone(),
            pane: self.id.clone(),
        }
    }

    /// Whether this pane's evidence binds it to `worktree`: the stored
    /// admin-id edge when the window publishes one, else the derived edge -
    /// a pane cwd at or below the worktree root. Both spellings are
    /// canonicalized here, so a symlinked path binds the same no matter who
    /// built the pane record. The stored edge decides outright when
    /// present: a window whose stored id names another worktree belongs to
    /// that worktree even if a pane has since `cd`-ed into this one.
    pub fn binds_worktree(&self, admin_id: Option<&str>, worktree: &Path) -> bool {
        if let Some(decided) = self.admin_decision(admin_id) {
            return decided;
        }
        let Some(cwd) = self.cwd.as_deref() else {
            return false;
        };
        self.binds_derived(&canonical_spelling(cwd), &canonical_spelling(worktree))
    }

    /// The stored admin-id edge's verdict, when it decides outright: a
    /// matching stored id binds, a stored id naming another worktree
    /// rejects, and a stored edge is never claimed by a bare-path claim.
    /// `None` defers to the derived cwd edge - decided before any path is
    /// resolved, so a decided pane costs no filesystem read at all.
    fn admin_decision(&self, admin_id: Option<&str>) -> Option<bool> {
        match (&self.wt_adminid, admin_id) {
            (Some(stored), Some(id)) => Some(stored == id),
            (Some(_), None) => Some(false),
            _ => None,
        }
    }

    /// The derived edge against spellings resolved already - [`BindingCache`]
    /// callers hand the canonical pair in so one pass canonicalizes each
    /// distinct spelling once, however many panes, anchors and emits
    /// consult it.
    fn binds_derived(&self, cwd: &Path, root: &Path) -> bool {
        deleted_cwd_binds(cwd, root)
    }
}

#[cfg(target_os = "linux")]
fn deleted_cwd_binds(cwd: &Path, root: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    match cwd.as_os_str().as_bytes().strip_suffix(b" (deleted)") {
        Some(stripped)
            if matches!(cwd.try_exists(), Ok(false)) && matches!(root.try_exists(), Ok(false)) =>
        {
            Path::new(std::ffi::OsStr::from_bytes(stripped)).starts_with(root)
        }
        _ => cwd.starts_with(root),
    }
}

#[cfg(not(target_os = "linux"))]
fn deleted_cwd_binds(cwd: &Path, root: &Path) -> bool {
    cwd.starts_with(root)
}

/// `path`'s canonical spelling, or the raw one when it cannot be resolved -
/// a gone path still binds literally, matching how pane bindings read.
fn canonical_spelling(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

/// The pass-local canonical binding cache: one collect pass resolves every
/// spelling through this so each distinct path canonicalizes once per pass,
/// not once per pane per anchor per emit. Pane cwds freeze at construction
/// since the pass's single inventory read never re-runs, while worktree
/// roots resolve lazily on first encounter, so an anchor the pass discovers
/// only in stage 2, or a gone row's vanished path, still binds. The roots
/// map sits behind a mutex because `runtime_facts` resolves inside fan-out
/// workers; the lock guards the resolution itself - a miss canonicalizes
/// under it, which is what makes once-per-spelling safe across workers -
/// and is never held across a pane comparison. Dropped with the pass that
/// built it: the next pass resolves against the filesystem as it stands
/// then.
pub(crate) struct BindingCache {
    /// Canonical form per distinct pane-cwd spelling the inventory carried.
    cwds: HashMap<PathBuf, PathBuf>,
    /// Canonical form per distinct root spelling, memoized on encounter.
    roots: Mutex<HashMap<PathBuf, PathBuf>>,
}

impl BindingCache {
    /// Freeze `panes`' cwd spellings: each distinct one canonicalizes once
    /// here and the pass's comparisons never canonicalize it again.
    pub(crate) fn new(panes: &PaneInventory) -> BindingCache {
        Self::new_with(panes, canonical_spelling)
    }

    /// `new` with the canonicalizer injected, so tests can count actual
    /// resolutions instead of the cache carrying instrumentation.
    fn new_with(panes: &PaneInventory, mut resolve: impl FnMut(&Path) -> PathBuf) -> BindingCache {
        let mut cwds = HashMap::new();
        for cwd in panes.panes.iter().filter_map(|p| p.cwd.as_deref()) {
            if let std::collections::hash_map::Entry::Vacant(entry) = cwds.entry(cwd.to_owned()) {
                entry.insert(resolve(cwd));
            }
        }
        BindingCache {
            cwds,
            roots: Mutex::new(HashMap::new()),
        }
    }

    /// `cwd`'s canonical spelling: borrowed from the frozen map on a hit,
    /// so the per-pane path never allocates. A spelling the inventory
    /// never carried resolves fresh, uncached - no pane appears mid-pass,
    /// so a miss is a defensive fallback, not a second look.
    fn cwd<'a>(&'a self, cwd: &'a Path) -> Cow<'a, Path> {
        match self.cwds.get(cwd) {
            Some(resolved) => Cow::Borrowed(resolved.as_path()),
            None => Cow::Owned(canonical_spelling(cwd)), // coverage: off - every pass caller binds panes the inventory itself reported
        }
    }

    /// `root`'s canonical spelling, resolved once per distinct spelling
    /// however many panes, anchors or emits consult it.
    pub(crate) fn root(&self, root: &Path) -> PathBuf {
        self.root_with(root, canonical_spelling)
    }

    /// `root` with the canonicalizer injected, like [`Self::new_with`]: a
    /// miss resolves under the lock so exactly one resolution runs per
    /// distinct spelling even with fan-out workers racing it.
    fn root_with(&self, root: &Path, resolve: impl FnOnce(&Path) -> PathBuf) -> PathBuf {
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        roots
            .entry(root.to_owned())
            .or_insert_with(|| resolve(root))
            .clone()
    }

    /// `binds` against an already-resolved `root` - the per-anchor shape:
    /// resolve the anchor's root once, then compare each pane's frozen cwd
    /// purely, canonicalizing nothing per pane. The stored admin-id edge
    /// still decides before the cwd is consulted, and the Linux
    /// `(deleted)` comparison can still stat its pair - only the
    /// canonicalization is cached.
    pub(crate) fn binds_at(&self, pane: &Pane, admin_id: Option<&str>, root: &Path) -> bool {
        if let Some(decided) = pane.admin_decision(admin_id) {
            return decided;
        }
        let Some(cwd) = pane.cwd.as_deref() else {
            return false;
        };
        pane.binds_derived(&self.cwd(cwd), root)
    }

    /// `PaneInventory::windows_bound` through the cache: the same bound
    /// windows, with `worktree` resolved once for the whole inventory.
    pub(crate) fn windows_bound(
        &self,
        panes: &PaneInventory,
        admin_id: Option<&str>,
        worktree: &Path,
    ) -> usize {
        let root = self.root(worktree);
        let mut windows = HashSet::new();
        for pane in &panes.panes {
            if self.binds_at(pane, admin_id, &root) {
                windows.insert((&pane.socket, &pane.window));
            }
        }
        windows.len()
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
    /// Sockets with no server behind them: tmux never unlinks its socket
    /// file, so a dead server leaves one that refuses every connection.
    /// Counted, never asked.
    pub stale_sockets: usize,
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
    /// One snapshot per socket in `sockets`, merged. Each live socket is
    /// asked exactly once; unreachable ones are recorded and skipped, and
    /// ones no server listens on are only counted.
    pub fn collect(sockets: &[PathBuf]) -> PaneInventory {
        let mut inventory = PaneInventory::default();
        let mut seen = HashSet::new();
        for socket in sockets {
            // Two spellings can name one server - a symlink in the socket
            // dir, or a `$TMUX` path that resolves differently - and
            // querying both would return every pane twice. Dedup on the
            // canonical path while keeping the spelling that was given.
            let identity = socket.canonicalize().unwrap_or_else(|_| socket.clone());
            if !seen.insert(identity) {
                continue;
            }
            if is_stale(socket) {
                inventory.stale_sockets += 1;
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

    /// The record a socket-qualified pane reference names, when the
    /// inventory still holds it.
    pub fn get(&self, pref: &PaneRef) -> Option<&Pane> {
        self.panes
            .iter()
            .find(|pane| pane.id == pref.pane && pane.socket == pref.socket)
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
                windows.insert((&pane.socket, &pane.window));
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
    if let Some(socket) = tmux_socket(tmux_env) {
        found.push(socket);
    }
    found.sort();
    found.dedup();
    found
}

/// The socket `$TMUX` names: the value is `<socket path>,<server pid>,
/// <session id>` and only the socket matters. `None` when the value is
/// absent, non-UTF-8 or not in that shape.
fn tmux_socket(tmux_env: Option<&OsStr>) -> Option<PathBuf> {
    let (socket, _) = tmux_env?.to_str()?.split_once(',')?;
    (!socket.is_empty()).then(|| PathBuf::from(socket))
}

/// The pane this process occupies, socket-qualified: `$TMUX` names the
/// server, `$TMUX_PANE` the pane. `None` when either is absent, non-UTF-8
/// or invalid - a bare pane id is ambiguous across servers and never
/// guesses at one.
pub fn pane_ref_from_env(tmux: Option<&OsStr>, pane: Option<&str>) -> Option<PaneRef> {
    Some(PaneRef {
        socket: tmux_socket(tmux)?,
        pane: PaneId::parse(pane?)?,
    })
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

/// Whether no server listens on `socket`: the path is gone, or a connect
/// is refused. A connect costs microseconds where a `tmux` spawn costs
/// milliseconds, and a machine can hold tens of thousands of dead sockets.
/// Any other failure is left for `tmux` itself to report.
fn is_stale(socket: &Path) -> bool {
    match std::os::unix::net::UnixStream::connect(socket) {
        Ok(_) => false,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ),
    }
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
            stale_sockets: 0,
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

    impl BindingCache {
        /// Whether `pane` binds to `worktree`: resolve the root, then the
        /// same pure per-pane compare production runs per anchor.
        fn binds(&self, pane: &Pane, admin_id: Option<&str>, worktree: &Path) -> bool {
            let root = self.root(worktree);
            self.binds_at(pane, admin_id, &root)
        }
    }

    /// A unique scratch dir per test, removed on drop. Collision-proof by
    /// construction: `create_dir` on an existing path retries with a fresh
    /// counter, never deletes a stranger's tree. The path is canonicalized
    /// once so assertions compare macOS `/tmp` and `/private/tmp`
    /// spellings alike.
    struct ScratchDir(PathBuf);
    impl ScratchDir {
        fn new(name: &str) -> ScratchDir {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            loop {
                let dir = std::env::temp_dir().join(format!(
                    "agent-sessions-{name}-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                match std::fs::create_dir(&dir) {
                    Ok(()) => return ScratchDir(dir.canonicalize().unwrap()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue, // coverage: off - needs a name collision with a live sibling test
                    Err(e) => panic!("create_dir {}: {e}", dir.display()), // coverage: off - needs an unwritable temp root
                }
            }
        }
        fn join(&self, name: impl AsRef<Path>) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_binding_cache_decides_exactly_what_binds_worktree_did() {
        let base = ScratchDir::new("bind-semantics");
        let real = base.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The stored admin-id edge decides outright: it binds with no cwd
        // at all and against a path that could never match.
        let mut p = pane("%1", "@1", "$0");
        p.wt_adminid = Some("adm".to_owned());
        let cache = BindingCache::new(&inventory(vec![p.clone()]));
        assert!(cache.binds(&p, Some("adm"), &PathBuf::from("/no/such/root")));
        // A mismatching stored id rejects even a cwd that sits inside.
        p.cwd = Some(link.join("sub"));
        let cache = BindingCache::new(&inventory(vec![p.clone()]));
        assert!(!cache.binds(&p, Some("other"), &real));
        assert!(cache.binds(&p, Some("adm"), &real));
        // A stored edge is never claimed by a bare-path claim.
        assert!(!cache.binds(&p, None, &real));

        // No stored edge: symlinked spellings on either side canonicalize
        // to the same containment.
        p.wt_adminid = None;
        let cache = BindingCache::new(&inventory(vec![p.clone()]));
        assert!(cache.binds(&p, None, &real));
        assert!(cache.binds(&p, None, &link));
        // Containment is by component, never by prefix: a sibling whose
        // name extends the root's is outside it.
        let sibling = base.join("real-not");
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(!cache.binds(&p, None, &sibling));

        // Gone paths fall back to their literal spelling: a pane whose
        // cwd names a deleted root's child still binds that root.
        let gone = base.join("gone-root");
        p.cwd = Some(gone.join("sub"));
        let cache = BindingCache::new(&inventory(vec![p.clone()]));
        assert!(cache.binds(&p, None, &gone));
        assert!(!cache.binds(&p, None, &real));
        // A cwd under a different root, and no cwd at all, both reject.
        p.cwd = Some(real.join("sub"));
        let cache = BindingCache::new(&inventory(vec![p.clone()]));
        assert!(!cache.binds(&p, None, &gone));
        p.cwd = None;
        let cache = BindingCache::new(&inventory(vec![p.clone()]));
        assert!(!cache.binds(&p, None, &real));
    }

    /// The Linux deleted-cwd regression through the cache: a
    /// `cwd (deleted)` spelling binds only while both it and the root are
    /// truly gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_binding_cache_keeps_the_deleted_cwd_semantics() {
        let base = ScratchDir::new("bind-deleted");
        let gone = base.join("gone");
        let mut p = pane("%1", "@1", "$0");
        p.cwd = Some(PathBuf::from(format!("{} (deleted)", gone.display())));
        let cache = BindingCache::new(&inventory(vec![p.clone()]));
        assert!(cache.binds(&p, None, &gone));
        // The root reappearing mid-pass stays unbound: the cache froze
        // the root spelling at first lookup, and both-gone no longer
        // holds for a fresh cache either.
        std::fs::create_dir(&gone).unwrap();
        let fresh = BindingCache::new(&inventory(vec![p.clone()]));
        assert!(!fresh.binds(&p, None, &gone));
    }

    #[test]
    fn a_binding_cache_freezes_a_root_for_its_pass_only() {
        let base = ScratchDir::new("bind-lifetime");
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();

        let cache = BindingCache::new(&inventory(vec![]));
        assert_eq!(cache.root(&link), a.canonicalize().unwrap());
        // Repointing the symlink does not move a resolution this pass
        // already made.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        assert_eq!(cache.root(&link), a.canonicalize().unwrap());
        // The next pass's own cache resolves what the filesystem says now.
        let fresh = BindingCache::new(&inventory(vec![]));
        assert_eq!(fresh.root(&link), b.canonicalize().unwrap());
    }

    /// A panic while the roots map is locked poisons the mutex: the next
    /// resolution recovers through `into_inner`, no binding is lost.
    #[test]
    fn a_poisoned_roots_lock_recovers() {
        let base = ScratchDir::new("bind-poison");
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let mut p = pane("%1", "@1", "$0");
        p.cwd = Some(real.clone());
        let inv = inventory(vec![p]);
        let cache = BindingCache::new(&inv);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = cache.roots.lock().unwrap(); // coverage: off - the panic edge is the point of the test
            panic!("poison"); // coverage: off - unwinds by construction
        }));
        assert!(cache.roots.lock().is_err());
        assert!(cache.binds(&inv.panes[0], None, &real));
    }

    /// The counting proof: 12 distinct roots bound against 29 distinct
    /// pane-cwd spellings across 27 emit-equivalent rebuilds resolves each
    /// distinct spelling exactly once - 12 root plus 29 cwd - not
    /// 27 x 12 x 29 x 2. A second pane repeating a spelling adds no
    /// resolution either.
    #[test]
    fn a_binding_cache_resolves_each_spelling_once_per_pass() {
        let base = ScratchDir::new("bind-counting");
        let roots: Vec<PathBuf> = (0..12)
            .map(|i| {
                let root = base.join(format!("r{i:02}"));
                std::fs::create_dir_all(&root).unwrap();
                root
            })
            .collect();
        // 29 distinct cwd spellings inside those roots, plus a duplicate
        // pane repeating one - it must not cost a second resolution.
        let mut panes: Vec<Pane> = (0..29)
            .map(|i| {
                let cwd = roots[i % 12].join(format!("sub{i:02}"));
                std::fs::create_dir_all(&cwd).unwrap();
                let mut p = pane(&format!("%{}", i + 1), &format!("@{}", i + 1), "$0");
                p.cwd = Some(cwd);
                p
            })
            .collect();
        let mut dup = pane("%99", "@99", "$0");
        dup.cwd = panes[0].cwd.clone();
        panes.push(dup);
        let inv = inventory(panes);
        // Count the resolver's actual invocations: no instrumentation on
        // the cache, just the injected seam observing itself.
        let cwd_calls = std::cell::Cell::new(0usize);
        let cache = BindingCache::new_with(&inv, |p| {
            cwd_calls.set(cwd_calls.get() + 1);
            canonical_spelling(p)
        });
        assert_eq!(cwd_calls.get(), 29);
        let root_calls = std::cell::Cell::new(0usize);
        for _emit in 0..27 {
            for root in &roots {
                let resolved = cache.root_with(root, |p| {
                    root_calls.set(root_calls.get() + 1);
                    canonical_spelling(p)
                });
                let bound = inv
                    .panes
                    .iter()
                    .filter(|p| cache.binds_at(p, None, &resolved))
                    .count();
                assert!(bound >= 2, "every root holds its panes: {bound}");
            }
        }
        assert_eq!(root_calls.get(), 12);
    }

    /// An anchor spelling first seen mid-pass - a worktree root the pass
    /// discovers only in stage 2 - still binds the panes the inventory
    /// froze, and binding stays stable across repeated emits of the same
    /// pass.
    #[test]
    fn a_binding_cache_binds_anchors_created_mid_pass() {
        let base = ScratchDir::new("bind-midpass");
        let real = base.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        // The pane's cwd is frozen evidence from before the anchor's
        // spelling existed.
        let mut p = pane("%1", "@1", "$0");
        p.cwd = Some(real.join("sub"));
        let inv = inventory(vec![p]);
        let cache = BindingCache::new(&inv);
        // The anchor's own spelling appears mid-pass: a symlink to the
        // root that did not exist when the cache was built.
        let late = base.join("late");
        std::os::unix::fs::symlink(&real, &late).unwrap();
        let pane = &inv.panes[0];
        // First and second emit of the pass bind alike.
        assert!(cache.binds(pane, None, &late));
        assert!(cache.binds(pane, None, &late));
        assert_eq!(cache.windows_bound(&inv, None, &late), 1);
    }

    /// The frozen cwd map memoizes a symlinked spelling too: repointing
    /// it mid-pass does not move this pass's binding, and the next pass's
    /// cache resolves what the filesystem says then.
    #[test]
    fn a_binding_cache_freezes_a_cwd_symlink_for_its_pass_only() {
        let base = ScratchDir::new("bind-cwd-symlink");
        let a = base.join("a");
        let b = base.join("b");
        // `inside` must exist on both targets: canonicalize resolves every
        // component, and a missing tail leaves the raw spelling.
        std::fs::create_dir_all(a.join("inside")).unwrap();
        std::fs::create_dir_all(b.join("inside")).unwrap();
        let link = base.join("cwd");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let mut p = pane("%1", "@1", "$0");
        p.cwd = Some(link.join("inside"));
        let inv = inventory(vec![p]);
        let cache = BindingCache::new(&inv);
        assert!(cache.binds(&inv.panes[0], None, &a));
        // Repointing the symlink keeps this pass's frozen resolution.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        assert!(cache.binds(&inv.panes[0], None, &a));
        assert!(!cache.binds(&inv.panes[0], None, &b));
        // A fresh pass resolves the spelling the filesystem reports now.
        let fresh = BindingCache::new(&inv);
        assert!(!fresh.binds(&inv.panes[0], None, &a));
        assert!(fresh.binds(&inv.panes[0], None, &b));
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
        // The same for a worktree that does not exist, and for a
        // worktree passed under a symlinked spelling of its real path:
        // binds_worktree canonicalizes both spellings itself (where
        // temp_dir has no symlink the two are one path and the check
        // still runs).
        p.cwd = Some(worktree.join("inside"));
        assert!(!p.binds_worktree(None, Path::new("/no/such/worktree/here")));
        let raw = std::env::temp_dir();
        let canon = raw.canonicalize().unwrap();
        p.cwd = Some(canon.join("inside"));
        assert!(p.binds_worktree(None, &raw));
        // A pane tmux reports no cwd for binds by the stored edge alone.
        p.cwd = None;
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
    fn pane_ref_from_env_needs_a_socket_and_a_pane() {
        use std::os::unix::ffi::OsStrExt;
        let pref = pane_ref_from_env(Some(OsStr::new("/tmp/tmux-501/a,42,0")), Some("%12"))
            .expect("a socket and a pane parse");
        assert_eq!(pref.socket, PathBuf::from("/tmp/tmux-501/a"));
        assert_eq!(pref.pane.as_str(), "%12");
        assert_eq!(pref.to_string(), "/tmp/tmux-501/a:%12");
        // Either part absent, malformed or non-UTF-8 is no pane at all -
        // a bare `%12` could name a pane on every server.
        assert!(pane_ref_from_env(None, Some("%12")).is_none());
        assert!(pane_ref_from_env(Some(OsStr::new("/tmp/s,1,0")), None).is_none());
        assert!(pane_ref_from_env(Some(OsStr::new(",1,0")), Some("%12")).is_none());
        assert!(pane_ref_from_env(Some(OsStr::new("/tmp/no-comma")), Some("%12")).is_none());
        assert!(pane_ref_from_env(Some(OsStr::new("/tmp/s,1,0")), Some("junk")).is_none());
        assert!(pane_ref_from_env(Some(OsStr::new("/tmp/s,1,0")), Some("%")).is_none());
        assert!(pane_ref_from_env(Some(OsStr::from_bytes(&[0xff])), Some("%12")).is_none());
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
