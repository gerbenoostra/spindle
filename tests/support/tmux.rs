//! A throwaway `tmux -L` server: the disposable counterpart of everything
//! the runtime substrate reads. Each server owns its socket inside a
//! `TMUX_TMPDIR`-style base directory, so it never touches a socket a real
//! session could see, and it is killed on drop so a panicking test leaks
//! nothing.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::tempdir::TempDir;

/// A tmux server reachable through `-S <socket>` only.
pub struct TmuxServer {
    /// The socket path the runtime's socket enumeration produces.
    pub socket: PathBuf,
    /// The base directory tmux made its `tmux-<uid>` socket dir under.
    /// Point discovery at this and the test's servers are the whole world
    /// the inventory sees.
    dir: PathBuf,
    name: String,
    /// The directory's owner when the server made it itself.
    _guard: Option<TempDir>,
}

/// A base directory for socket paths: unix sockets cap `sun_path` near a
/// hundred bytes, which the default temp root mostly spends by itself, so
/// tmux fixtures live under `/tmp` with a short name.
pub fn base_dir() -> TempDir {
    TempDir::new_in(Path::new("/tmp"), "ts")
}

impl TmuxServer {
    /// A server under its own fresh base directory.
    pub fn new() -> TmuxServer {
        let guard = base_dir();
        Self::spawn_in(guard.path().to_path_buf(), Some(guard))
    }

    /// A server under `parent`: two servers sharing one base directory
    /// share one `tmux-<uid>` socket dir, which is what multi-server
    /// discovery is tested against. `parent` owns the cleanup.
    pub fn in_dir(parent: &TempDir) -> TmuxServer {
        Self::spawn_in(parent.path().to_path_buf(), None)
    }

    fn spawn_in(dir: PathBuf, guard: Option<TempDir>) -> TmuxServer {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "ast-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        // `-f /dev/null`: the user's tmux.conf must not decide what a test
        // sees.
        let out = Command::new("tmux")
            .env("TMUX_TMPDIR", &dir)
            .args([
                "-f",
                "/dev/null",
                "-L",
                &name,
                "new-session",
                "-d",
                "-s",
                "holder",
                "-x",
                "100",
                "-y",
                "24",
            ])
            .stdin(Stdio::null())
            .output()
            .expect("tmux is on PATH");
        assert!(
            out.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        TmuxServer {
            socket: socket_under(&dir, &name),
            dir,
            name,
            _guard: guard,
        }
    }

    /// The base directory this server's socket dir lives under.
    pub fn socket_root(&self) -> &Path {
        &self.dir
    }

    /// `tmux -S <socket> <args>` asserting success, stdout returned.
    pub fn tmux(&self, args: &[&str]) -> String {
        let out = self.try_tmux(args);
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `tmux -S <socket> <args>` without the assertion, for cases that may
    /// legitimately fail.
    pub fn try_tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output()
            .expect("tmux is on PATH")
    }

    /// A detached session running `command`, as one `new-session` call.
    pub fn new_session(&self, name: &str, command: &str) {
        self.tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "100",
            "-y",
            "24",
            command,
        ]);
    }

    /// A control-mode client attached to `session`: tmux attaches a client
    /// without a pty in control mode, which is the only headless way to
    /// make `session_attached` non-zero. Hold the returned `Child` - its
    /// piped stdin staying open is what keeps the client attached.
    pub fn attach_client(&self, session: &str) -> Child {
        Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(["-C", "attach-session", "-t", session])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("tmux is on PATH")
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        // Best-effort: the server may already be gone (a test that killed
        // its own last session), and either way it is ours alone.
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(["kill-server"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// The socket `<dir>/tmux-<uid>/<name>` tmux creates under `-L`.
fn socket_under(dir: &Path, name: &str) -> PathBuf {
    let socket_dir = dir
        .read_dir()
        .expect("the server dir exists")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("tmux-"))
        })
        .expect("tmux created its socket dir");
    socket_dir.join(name)
}
