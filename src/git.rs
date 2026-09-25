//! Read-only Git substrate: path resolution, worktree and branch inventory,
//! and the low-level facts the state vector is built from.
//!
//! Every subprocess goes through [`git`], which pins `GIT_OPTIONAL_LOCKS=0`
//! so a background read can never contend for `.git/index.lock`, refuses
//! argv outside a read-only allowlist, and is isolated from the ambient git
//! environment and locale (see [`git_command`]). This module observes
//! repositories; it never mutates them - no fetch, no prune, no config write.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::evidence::Evidence;

/// A Git read that failed. `code` distinguishes "the answer is no" (a
/// non-zero exit on a read that can legitimately fail, like
/// `merge-base --is-ancestor`) from "could not ask" (spawn, usage errors).
#[derive(Debug)]
pub struct Error {
    pub argv: String,
    pub code: Option<i32>,
    pub detail: String,
}

impl Error {
    fn io(what: &str, path: &Path, error: std::io::Error) -> Error {
        Error {
            argv: format!("{what} {}", path.display()),
            code: None,
            detail: error.to_string(),
        }
    }

    #[rustfmt::skip]
    fn spawn(argv: &str, error: std::io::Error) -> Error { Error { argv: argv.to_owned(), code: None, detail: error.to_string() } } // coverage: off - needs a PATH without git

    #[rustfmt::skip]
    fn parse(range: &str, error: std::num::ParseIntError) -> Error { Error { argv: format!("rev-list --count {range}"), code: None, detail: format!("unparseable count: {error}") } } // coverage: off - rev-list prints a number
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "`git {}` exited {code}: {}", self.argv, self.detail),
            None => write!(f, "`git {}`: {}", self.argv, self.detail),
        }
    }
}

impl std::error::Error for Error {}

/// Environment that must never leak into a Git read. `GIT_DIR` and friends
/// override directory discovery (`-C` loses to `GIT_DIR`), so a caller
/// running inside a hook or an exported-GIT_DIR shell would silently read the
/// wrong repository. `LC_ALL=C` keeps diagnostics in one language because
/// `Repo::discover` recognizes "not a git repository" by message.
/// `GIT_TERMINAL_PROMPT=0` turns a remote that wants credentials into an
/// error instead of a collector that hangs on a prompt nobody answers; the
/// ssh transport gets the same treatment through `SSH_ASKPASS_REQUIRE` and
/// a default `ssh -oBatchMode=yes`.
///
/// `pub(crate)` so the crate's own test fixtures spawn Git through the same
/// isolation - an ambient `GIT_DIR` would defeat `-C` there exactly as it
/// does here.
pub(crate) fn git_command(global: &[OsString], args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(global).args(args);
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_QUARANTINE_PATH",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("LC_ALL", "C");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    // Filenames this tool feeds back as pathspecs are data, not patterns:
    // `:(glob)`-style magic can match nothing and fake an identical diff,
    // and glob characters widen it. Every caller passes literal paths only.
    cmd.env("GIT_LITERAL_PATHSPECS", "1");
    // GIT_TERMINAL_PROMPT covers git's own credential prompt; ssh has its
    // own - a passphrase or host-key question on an ls-remote would stall a
    // whole collection behind a prompt nobody answers. BatchMode fails
    // instead; key and agent auth still work. A caller's own
    // GIT_SSH_COMMAND (custom ports, proxies, keys) is respected.
    cmd.env("SSH_ASKPASS_REQUIRE", "never");
    cmd.env(
        "GIT_SSH_COMMAND",
        std::env::var_os("GIT_SSH_COMMAND").unwrap_or_else(|| "ssh -oBatchMode=yes".into()),
    );
    cmd
}

/// Reject argv that would mutate a repository, run the read, return stdout.
///
/// `GIT_OPTIONAL_LOCKS=0` keeps reads from creating or waiting on lock files:
/// without it even `git status` can try to refresh the index under a lock.
fn git(global: &[OsString], args: &[&str]) -> Result<String, Error> {
    check_argv(args)?;
    let argv = global
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .chain(args.iter().map(|s| s.to_string()))
        .collect::<Vec<_>>()
        .join(" ");
    let out = git_command(global, args)
        .output()
        .map_err(|e| Error::spawn(&argv, e))?; // coverage: off - needs a PATH without git
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(Error {
            argv,
            code: out.status.code(),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    }
}

/// `args[0]` is the subcommand; the allowlist admits only read forms. A
/// mutation that somehow reaches this module fails here, loudly.
fn check_argv(args: &[&str]) -> Result<(), Error> {
    let Some((&sub, rest)) = args.split_first() else {
        return Err(Error {
            argv: String::new(),
            code: None,
            detail: "empty argv".to_owned(),
        });
    };
    let allowed = match sub {
        "rev-parse" | "status" | "rev-list" | "merge-base" | "diff" | "ls-remote"
        | "for-each-ref" | "log" | "show" | "show-ref" | "cat-file" | "ls-tree" | "ls-files"
        | "name-rev" | "describe" => true,
        "worktree" => rest == ["list"] || rest == ["list", "--porcelain"],
        "remote" => matches!(rest, [] | ["-v"] | ["--verbose"] | ["get-url", _]),
        "config" => is_read_only_config(rest),
        "symbolic-ref" => is_read_only_symbolic_ref(rest),
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(Error {
            argv: args.join(" "),
            code: None,
            detail: "argv is not a read-only Git invocation".to_owned(),
        })
    }
}

/// `git config` reads: a `--get*`/`--list` flag must be present (which rules
/// out the positional `config <key> <value>` set form) and no mutating flag
/// may appear.
fn is_read_only_config(rest: &[&str]) -> bool {
    const MUTATING: [&str; 8] = [
        "--add",
        "--unset",
        "--unset-all",
        "--replace-all",
        "--rename-section",
        "--remove-section",
        "-e",
        "--edit",
    ];
    let reads = rest
        .iter()
        .any(|a| a.starts_with("--get") || *a == "--list" || *a == "-l");
    reads && !rest.iter().any(|a| MUTATING.contains(a))
}

/// `symbolic-ref <name>` is a read; `symbolic-ref <name> <ref>` writes and
/// `-d`/`--delete` removes.
fn is_read_only_symbolic_ref(rest: &[&str]) -> bool {
    let writes = rest.iter().any(|a| *a == "-d" || *a == "--delete");
    let positionals = rest.iter().filter(|a| !a.starts_with('-')).count();
    !writes && positionals == 1
}

fn in_dir(dir: &Path, args: &[&str]) -> Result<String, Error> {
    git(&[OsString::from("-C"), dir.as_os_str().to_owned()], args)
}

/// `-C <repo_dir>` makes repo reads behave as if launched inside the
/// repository: remote names, `.` remotes and relative-path remote URLs
/// resolve against it, not against whatever directory launched the tool.
fn in_repo(repo: &Repo, args: &[&str]) -> Result<String, Error> {
    git(
        &[
            OsString::from("-C"),
            repo.repo_dir().as_os_str().to_owned(),
            OsString::from(format!("--git-dir={}", repo.common_dir.display())),
        ],
        args,
    )
}

/// What a worktree's HEAD points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// `refs/heads/<name>`; the branch is checked out here.
    Branch(String),
    /// A commit reachable from no ref; unique commits die with the worktree.
    Detached(String),
    /// The symbolic target exists but no commit does yet.
    Unborn(String),
}

/// A repository, identified by its canonical `$GIT_COMMON_DIR`. Symlinked or
/// otherwise differently-spelled paths to the same repository resolve to the
/// same `Repo`, which is what makes the id safe to key on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub(crate) common_dir: PathBuf,
}

/// One entry of `git worktree list --porcelain`, plus the admin id derived
/// from the worktree's `.git` file.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// Canonical worktree root; absent on disk only for a prunable record.
    pub path: PathBuf,
    pub head: Head,
    /// Basename of `$GIT_COMMON_DIR/worktrees/<id>`; `None` on the main
    /// worktree, which has no admin directory.
    pub admin_id: Option<String>,
    /// The repository's own checkout - the first porcelain entry - which
    /// `git worktree remove` can never remove.
    pub main: bool,
    pub bare: bool,
    pub locked: bool,
}

/// The resolved meaning of an input path.
#[derive(Debug, PartialEq)]
pub enum Resolved {
    /// A path inside a worktree: repo, checkout root and current HEAD.
    Checkout(Checkout),
    /// A repository path that is not a worktree (a `.git` dir, a bare repo).
    RepoOnly(Repo),
    /// No repository claims the path: it is a project space.
    ProjectSpace(PathBuf),
}

/// A worktree an input path resolves into.
#[derive(Debug, PartialEq)]
pub struct Checkout {
    pub repo: Repo,
    /// Canonical worktree root.
    pub root: PathBuf,
    pub admin_id: Option<String>,
    pub head: Head,
}

/// `branch.<name>.remote` plus `branch.<name>.merge`, as configured. Both keys
/// must exist before an upstream is considered configured at all; a partial
/// pair is evidence that cannot be acted on, not an absent one.
#[derive(Debug)]
pub enum UpstreamConfig {
    None,
    Partial,
    Full { remote: String, merge: String },
}

/// Where the remote's default branch evidence landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteHead {
    /// `ls-remote --symref` advertised `refs/heads/<0>` as the remote HEAD.
    Advertised(String),
    /// The remote answered but carries no symbolic HEAD.
    NotAdvertised,
    /// The remote could not be asked (offline, unreachable, failure).
    Unreachable(String),
}

impl Repo {
    /// The repository claiming `path`, or `None` for a non-Git path. `path`
    /// must exist.
    pub fn discover(path: &Path) -> Result<Option<Repo>, Error> {
        let canonical = path
            .canonicalize()
            .map_err(|e| Error::io("canonicalize", path, e))?;
        let out = in_dir(
            &canonical,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        match out {
            Ok(dir) => {
                let common = PathBuf::from(dir.trim());
                let common = common
                    .canonicalize()
                    .map_err(|e| Error::io("canonicalize", &common, e))?; // coverage: off - a reported common dir exists
                Ok(Some(Repo { common_dir: common }))
            }
            Err(e) if e.detail.contains("not a git repository") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The canonical `$GIT_COMMON_DIR`; the repository's identity.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The directory repo-scoped reads run from: the worktree root for a
    /// non-bare repo (the parent of its `.git` dir), the repository dir
    /// itself for a bare one. Remote URLs spelled `.` or relative to the
    /// repository resolve against this directory, not the caller's cwd.
    fn repo_dir(&self) -> &Path {
        if self.common_dir.file_name() == Some(std::ffi::OsStr::new(".git")) {
            self.common_dir.parent().unwrap_or(&self.common_dir)
        } else {
            &self.common_dir
        }
    }

    /// Every worktree of the repository, main first, as reported by
    /// `git worktree list --porcelain`. Paths are canonicalized.
    pub fn worktrees(&self) -> Result<Vec<Worktree>, Error> {
        let text = in_repo(self, &["worktree", "list", "--porcelain"])?;
        let mut found = Vec::new();
        let mut current: Option<Worktree> = None;
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                if let Some(wt) = current.take() {
                    found.push(wt);
                }
                let path = PathBuf::from(path)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(path));
                current = Some(Worktree {
                    admin_id: admin_id(&path, &self.common_dir),
                    path,
                    head: Head::Detached(String::new()),
                    // The porcelain output lists the main worktree first.
                    main: found.is_empty(),
                    bare: false,
                    locked: false,
                });
            } else if let Some(wt) = current.as_mut() {
                if let Some(sha) = line.strip_prefix("HEAD ") {
                    wt.head = Head::Detached(sha.to_owned());
                } else if let Some(reference) = line.strip_prefix("branch ") {
                    let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
                    let sha = match &wt.head {
                        Head::Detached(sha) => sha.clone(),
                        _ => String::new(), // coverage: off - porcelain never emits this shape
                    };
                    wt.head = if sha.chars().all(|c| c == '0') && !sha.is_empty() {
                        Head::Unborn(name.to_owned())
                    } else {
                        Head::Branch(name.to_owned())
                    };
                } else if line == "detached" {
                    // The `HEAD` line already carried the sha; `detached`
                    // only confirms no branch owns it.
                } else if line == "bare" {
                    wt.bare = true;
                } else if line.starts_with("locked") {
                    wt.locked = true;
                }
            } // coverage: off - porcelain fields only follow a worktree record
        }
        found.extend(current.take());
        Ok(found)
    }

    /// Local branch names, sorted.
    pub fn local_branches(&self) -> Result<Vec<String>, Error> {
        let text = in_repo(
            self,
            &["for-each-ref", "--format=%(refname)", "refs/heads/"],
        )?;
        let mut names: Vec<String> = text
            .lines()
            .filter_map(|l| l.strip_prefix("refs/heads/").map(str::to_owned))
            .collect();
        names.sort();
        Ok(names)
    }

    /// Configured remote names, sorted.
    pub fn remotes(&self) -> Result<Vec<String>, Error> {
        let text = in_repo(self, &["remote"])?;
        let mut names: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        names.sort();
        Ok(names)
    }

    /// `remote.<name>.url`, or `None` when unset.
    pub fn remote_url(&self, name: &str) -> Result<Option<String>, Error> {
        self.config_get(&format!("remote.{name}.url"))
    }

    /// The configured upstream pair for a branch.
    pub fn upstream_config(&self, branch: &str) -> Result<UpstreamConfig, Error> {
        let remote = self.config_get(&format!("branch.{branch}.remote"))?;
        let merge = self.config_get(&format!("branch.{branch}.merge"))?; // coverage: off - a config read that fails here fails for remote too
        Ok(match (remote, merge) {
            (None, None) => UpstreamConfig::None,
            (Some(remote), Some(merge)) => UpstreamConfig::Full { remote, merge },
            _ => UpstreamConfig::Partial,
        })
    }

    /// A single `git config --local --get`: absent key is `Ok(None)`, not an
    /// error. `--local` matters twice over: without it a broken repository
    /// silently reads global config, and a key a user set globally would leak
    /// into repository evidence.
    fn config_get(&self, key: &str) -> Result<Option<String>, Error> {
        match in_repo(self, &["config", "--local", "--get", key]) {
            Ok(value) => Ok(Some(value.trim().to_owned())),
            // Exit 1 with no output is how config reports a missing key.
            Err(e) if e.code == Some(1) && e.detail.is_empty() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The local `refs/remotes/<remote>/HEAD` symref target's branch name, if
    /// the symref exists. This is what the last fetch recorded; it can be
    /// stale but it is never a guess.
    pub fn local_remote_head(&self, remote: &str) -> Result<Option<String>, Error> {
        let reference = format!("refs/remotes/{remote}/HEAD");
        match in_repo(self, &["symbolic-ref", "-q", &reference]) {
            Ok(target) => {
                let prefix = format!("refs/remotes/{remote}/");
                Ok(target.trim().strip_prefix(&prefix).map(str::to_owned))
            }
            Err(e) if e.code == Some(1) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `ls-remote --symref <remote> HEAD`: what the remote advertises as its
    /// default branch, read-only and without fetching.
    pub fn remote_head(&self, remote: &str) -> RemoteHead {
        match in_repo(self, &["ls-remote", "--symref", remote, "HEAD"]) {
            Ok(text) => {
                for line in text.lines() {
                    if let Some(rest) = line.strip_prefix("ref: refs/heads/")
                        && let Some((branch, "HEAD")) = rest.split_once('\t')
                    {
                        return RemoteHead::Advertised(branch.to_owned());
                    }
                }
                RemoteHead::NotAdvertised
            }
            Err(e) => RemoteHead::Unreachable(format!("ls-remote {remote} HEAD: {e}")),
        }
    }

    /// Every refname the remote advertises, proven by one `ls-remote`
    /// listing (no fetch, no local mutation). Membership checks against the
    /// listing answer any per-ref question without asking again. `Unknown`
    /// when the remote could not be asked.
    pub fn remote_refs(&self, remote: &str) -> Evidence<Vec<String>> {
        match in_repo(self, &["ls-remote", remote]) {
            Ok(text) => Evidence::Known(
                text.lines()
                    .filter_map(|line| line.rsplit('\t').next())
                    .map(str::to_owned)
                    .collect(),
            ),
            Err(e) => Evidence::Unknown(format!("ls-remote {remote}: {e}")),
        }
    }

    /// Whether `refname` resolves locally (`rev-parse --verify -q`).
    pub fn has_ref(&self, refname: &str) -> bool {
        in_repo(self, &["rev-parse", "--verify", "-q", refname])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }

    /// `rev-list --count <a>..<b>`.
    pub fn rev_list_count(&self, from_exclusive: &str, to: &str) -> Result<u64, Error> {
        let range = format!("{from_exclusive}..{to}");
        let text = in_repo(self, &["rev-list", "--count", &range])?;
        text.trim()
            .parse::<u64>()
            .map_err(|e| Error::parse(&range, e)) // coverage: off - rev-list prints a number
    }

    /// `rev-list --count <head> --not --glob=refs/*`: commits reachable
    /// from `head` that no ref reaches - branch, tag, remote-tracking,
    /// stash, anything under `refs/`. `--not --all` does not work here:
    /// `--all` includes every worktree's per-worktree HEAD, so a detached
    /// worktree's own commits would always read as reachable. Commits a
    /// *different* worktree's HEAD alone keeps pinned therefore count as
    /// unique - literally reachable from no ref - and fail closed.
    pub fn unreachable_commits(&self, head: &str) -> Result<u64, Error> {
        let text = in_repo(
            self,
            &["rev-list", "--count", head, "--not", "--glob=refs/*"],
        )?;
        text.trim()
            .parse::<u64>()
            .map_err(|e| Error::parse(&format!("{head} --not --glob=refs/*"), e)) // coverage: off - rev-list prints a number
    }

    /// `merge-base --is-ancestor`: three-valued because the comparison itself
    /// can fail, and a failed comparison is not "no".
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Evidence<bool> {
        match in_repo(self, &["merge-base", "--is-ancestor", ancestor, descendant]) {
            Ok(_) => Evidence::Known(true),
            Err(e) if e.code == Some(1) => Evidence::Known(false),
            Err(e) => Evidence::Unknown(format!("merge-base --is-ancestor: {e}")),
        }
    }

    /// `merge-base`; `Ok(None)` when the histories share no ancestor.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>, Error> {
        match in_repo(self, &["merge-base", a, b]) {
            Ok(sha) => Ok(Some(sha.trim().to_owned())),
            Err(e) if e.code == Some(1) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Paths whose content differs between `from` and `to`
    /// (`diff --name-only -z`).
    pub fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>, Error> {
        let text = in_repo(self, &["diff", "--name-only", "-z", from, to])?;
        Ok(text
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Whether every one of `paths` has identical content at `a` and `b`
    /// (`diff --quiet a b -- <paths>`). Unknown when the comparison fails.
    pub fn paths_match(&self, a: &str, b: &str, paths: &[String]) -> Evidence<bool> {
        // argv length is finite; compare in bounded batches.
        for chunk in paths.chunks(200) {
            let mut args = vec!["diff", "--quiet", a, b, "--"];
            args.extend(chunk.iter().map(String::as_str));
            match in_repo(self, &args) {
                Ok(_) => {}
                Err(e) if e.code == Some(1) => return Evidence::Known(false),
                Err(e) => return Evidence::Unknown(format!("diff --quiet: {e}")),
            }
        }
        Evidence::Known(true)
    }

    /// `status --porcelain --untracked-files=all`: dirty means tracked *or*
    /// untracked changes, so configuration hiding untracked files cannot make
    /// a dirty tree look disposable.
    pub fn dirty(&self, checkout: &Path) -> Evidence<bool> {
        match in_dir(
            checkout,
            &["status", "--porcelain", "--untracked-files=all"],
        ) {
            Ok(text) => Evidence::Known(!text.trim().is_empty()),
            Err(e) => Evidence::Unknown(format!("git status: {e}")),
        }
    }

    /// Last entry time of a worktree's HEAD reflog, or of a branch reflog,
    /// taken with the log file's mtime: the newest of the two is the activity
    /// signal.
    pub fn reflog_activity(&self, log: &Path) -> Option<SystemTime> {
        let path = self.common_dir.join(log);
        let text = fs::read_to_string(&path).ok()?;
        let last = text.lines().rev().find(|l| !l.trim().is_empty())?;
        // `<old> <new> <ident> <epoch> <tz>\t<msg>`: the epoch is the second
        // token before the tab when read from the right, which is robust
        // against spaces inside the identity.
        let fields = last.split('\t').next().unwrap_or(last);
        let epoch: u64 = fields.split_whitespace().nth_back(1)?.parse().ok()?;
        let entry = UNIX_EPOCH + Duration::from_secs(epoch);
        let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
        Some(mtime.map_or(entry, |m| m.max(entry)))
    }
}

/// The admin id of a worktree: the basename of its
/// `$GIT_COMMON_DIR/worktrees/<id>` directory, read from the `.git` file. The
/// main worktree has no admin directory and no id.
fn admin_id(path: &Path, common_dir: &Path) -> Option<String> {
    let dotgit = path.join(".git");
    if !dotgit.is_file() {
        return None;
    }
    let text = fs::read_to_string(&dotgit).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    let admin = Path::new(target).canonicalize().ok()?;
    if admin.parent() == Some(&common_dir.join("worktrees")) {
        admin.file_name().map(|n| n.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Resolve `path` to the repository, worktree and HEAD it falls under; a
/// non-Git path is a project space, not an error.
pub fn resolve(path: &Path) -> Result<Resolved, Error> {
    let Some(repo) = Repo::discover(path)? else {
        let canonical = path
            .canonicalize()
            .map_err(|e| Error::io("canonicalize", path, e))?; // coverage: off - the path exists; discover just read it
        return Ok(Resolved::ProjectSpace(canonical));
    };
    let canonical = path
        .canonicalize()
        .map_err(|e| Error::io("canonicalize", path, e))?; // coverage: off - discover() already canonicalized this path
    let toplevel = in_dir(&canonical, &["rev-parse", "--show-toplevel"]);
    let Ok(toplevel) = toplevel else {
        // Inside `.git` or a bare repository: a repo with no checkout.
        return Ok(Resolved::RepoOnly(repo));
    };
    let root = PathBuf::from(toplevel.trim())
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(toplevel.trim())); // coverage: off - a reported toplevel is canonical
    let head = head_at(&canonical)?; // coverage: off - a HEAD symbolic-ref fails after rev-parse only on a corrupt repo
    Ok(Resolved::Checkout(Checkout {
        admin_id: admin_id(&root, repo.common_dir()),
        repo,
        root,
        head,
    }))
}

/// The HEAD of the worktree containing `dir`: symbolic branch, detached sha,
/// or the unborn target of a symbolic ref no commit exists on yet.
fn head_at(dir: &Path) -> Result<Head, Error> {
    match in_dir(dir, &["symbolic-ref", "-q", "--short", "HEAD"]) {
        Ok(name) => {
            let exists = in_dir(dir, &["rev-parse", "--verify", "-q", "HEAD"])
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            Ok(if exists {
                Head::Branch(name.trim().to_owned())
            } else {
                Head::Unborn(name.trim().to_owned())
            })
        }
        Err(e) if e.code == Some(1) => {
            let sha = in_dir(dir, &["rev-parse", "HEAD"])?; // coverage: off - rev-parse worked in resolve() moments earlier
            Ok(Head::Detached(sha.trim().to_owned()))
        }
        Err(e) => Err(e), // coverage: off - a repo this broken fails --show-toplevel first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway dir, removed on drop.
    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Temp {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "agent-sessions-git-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("temp dir");
            Temp(path.canonicalize().expect("canonical"))
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn is_unreachable(head: &RemoteHead) -> bool {
        matches!(head, RemoteHead::Unreachable(_))
    }

    /// A `Repo` whose common dir is a plain file: every subprocess read
    /// fails with a real error, which is how the error edges get exercised.
    /// (A merely absent gitdir fails differently: `config --get` reports it
    /// as an absent key, indistinguishable and therefore not an error.)
    fn bogus_repo() -> Repo {
        let temp = Temp::new();
        let file = temp.0.join("not-a-repo");
        fs::write(&file, "x").unwrap();
        Repo { common_dir: file }
    }

    #[test]
    fn the_runner_strips_ambient_git_environment() {
        let cmd = git_command(&[OsString::from("-C"), OsString::from("/tmp")], &["status"]);
        let envs: std::collections::HashMap<&std::ffi::OsStr, Option<&std::ffi::OsStr>> =
            cmd.get_envs().collect();
        // An ambient GIT_DIR would override -C discovery and attribute every
        // path to the wrong repository; these must be explicit removals.
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_QUARANTINE_PATH",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
        ] {
            assert_eq!(envs.get(std::ffi::OsStr::new(var)), Some(&None), "{var}");
        }
        for (var, value) in [
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("LC_ALL", "C"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("SSH_ASKPASS_REQUIRE", "never"),
            ("GIT_LITERAL_PATHSPECS", "1"),
        ] {
            assert_eq!(
                envs.get(std::ffi::OsStr::new(var)),
                Some(&Some(std::ffi::OsStr::new(value))),
                "{var}"
            );
        }
        // BatchMode is the default only; a caller-set GIT_SSH_COMMAND wins.
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            assert_eq!(
                envs.get(std::ffi::OsStr::new("GIT_SSH_COMMAND")),
                Some(&Some(std::ffi::OsStr::new("ssh -oBatchMode=yes")))
            );
        } // coverage: off - the ambient-set arm needs a shell exporting GIT_SSH_COMMAND
    }

    #[test]
    fn the_allowlist_admits_only_read_only_git() {
        for allowed in [
            vec!["rev-parse", "HEAD"],
            vec!["status", "--porcelain", "--untracked-files=all"],
            vec!["rev-list", "--count", "a..b"],
            vec!["merge-base", "--is-ancestor", "a", "b"],
            vec!["diff", "--name-only", "-z", "a", "b"],
            vec!["ls-remote", "--symref", "origin", "HEAD"],
            vec!["for-each-ref", "--format=%(refname)", "refs/heads/"],
            vec!["show-ref", "--verify", "refs/heads/main"],
            vec!["worktree", "list"],
            vec!["worktree", "list", "--porcelain"],
            vec!["remote"],
            vec!["remote", "-v"],
            vec!["remote", "--verbose"],
            vec!["remote", "get-url", "origin"],
            vec!["config", "--get", "branch.main.remote"],
            vec!["config", "--worktree", "--get", "wt.handle"],
            vec!["config", "--get-regexp", "^remote\\."],
            vec!["symbolic-ref", "-q", "--short", "HEAD"],
        ] {
            assert!(check_argv(&allowed).is_ok(), "{allowed:?}");
        }
        for denied in [
            vec![],
            vec!["push", "origin", "main"],
            vec!["fetch", "origin"],
            vec!["checkout", "main"],
            vec!["worktree", "remove", "x"],
            vec!["worktree", "add", "x"],
            vec!["worktree", "prune"],
            vec!["remote", "add", "x", "y"],
            vec!["remote", "prune", "origin"],
            // The positional set form carries no read flag.
            vec!["config", "user.name", "x"],
            vec!["config", "--add", "k", "v"],
            vec!["config", "--unset", "k"],
            // A read flag does not launder a mutating one.
            vec!["config", "--get", "k", "--add", "l", "v"],
            vec!["symbolic-ref", "HEAD", "refs/heads/x"],
            vec!["symbolic-ref", "-d", "HEAD"],
            vec!["tag", "v1"],
            vec!["update-ref", "HEAD", "x"],
        ] {
            assert!(check_argv(&denied).is_err(), "{denied:?}");
        }
        // The refusal is the subprocess boundary, not just the checker.
        assert!(git(&[], &["push"]).is_err());
        assert!(git(&[], &[]).is_err());
    }

    #[test]
    fn errors_name_the_argv_and_the_failure() {
        let repo = bogus_repo();
        let err = repo.worktrees().expect_err("a bogus repo fails");
        assert_eq!(err.code, Some(128));
        assert!(err.to_string().contains("--git-dir="), "{err}");

        let io = Error::io(
            "canonicalize",
            Path::new("/no/such"),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        assert!(io.to_string().contains("canonicalize /no/such"));
        assert!(io.to_string().contains("/no/such"));
    }

    #[test]
    fn a_broken_repo_fails_closed_everywhere() {
        let repo = bogus_repo();
        assert!(repo.worktrees().is_err());
        assert!(repo.local_branches().is_err());
        assert!(repo.remotes().is_err());
        assert!(repo.remote_url("origin").is_err());
        assert!(repo.upstream_config("main").is_err());
        assert!(repo.local_remote_head("origin").is_err());
        assert!(is_unreachable(&repo.remote_head("origin")));
        assert!(!is_unreachable(&RemoteHead::Advertised("x".to_owned())));
        assert!(!repo.remote_refs("origin").is_known());
        assert!(!repo.has_ref("refs/heads/main"));
        assert!(repo.rev_list_count("a", "b").is_err());
        assert!(repo.unreachable_commits("a").is_err());
        assert!(!repo.is_ancestor("a", "b").is_known());
        assert!(repo.merge_base("a", "b").is_err());
        assert!(repo.changed_paths("a", "b").is_err());
        assert!(!repo.paths_match("a", "b", &["x".to_owned()]).is_known());
        assert!(!repo.dirty(Path::new("/also/not/here")).is_known());
    }

    #[test]
    fn discover_distinguishes_repo_worktree_and_project_space() {
        let temp = Temp::new();
        // A plain file is not a path git can read a repo from.
        let file = temp.0.join("file");
        fs::write(&file, "x").unwrap();
        assert!(Repo::discover(&file).is_err());
        assert!(Repo::discover(&temp.0.join("missing")).is_err());
        assert!(resolve(&file).is_err());

        // A non-git directory is a project space.
        assert!(Repo::discover(&temp.0).unwrap().is_none());
        assert_eq!(
            resolve(&temp.0).unwrap(),
            Resolved::ProjectSpace(temp.0.clone())
        );

        // A bare repository is a repo with no checkout.
        let bare = temp.0.join("bare.git");
        assert!(
            git_command(&[], &["init", "--bare", bare.to_str().unwrap()])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap()
                .status
                .success()
        );
        let repo = Repo::discover(&bare).unwrap().expect("bare is a repo");
        assert_eq!(resolve(&bare).unwrap(), Resolved::RepoOnly(repo.clone()));
        assert!(repo.worktrees().unwrap()[0].bare);
    }

    #[test]
    fn a_corrupt_head_turns_the_repo_into_a_project_space() {
        // Garbage in `.git/HEAD` makes git itself deny the repository, which
        // resolve() reports as an ordinary non-repo path rather than a crash.
        let temp = Temp::new();
        let dir = temp.0.join("repo");
        assert!(
            git_command(&[], &["init", dir.to_str().unwrap()])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap()
                .status
                .success()
        );
        fs::write(dir.join(".git/HEAD"), "garbage\n").unwrap();
        assert_eq!(resolve(&dir).unwrap(), Resolved::ProjectSpace(dir.clone()));
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let out = git_command(&[OsString::from("-C"), dir.as_os_str().to_owned()], args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@invalid")
            .env("GIT_COMMITTER_NAME", "fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@invalid")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {:?}", out.stderr);
    }

    #[test]
    fn resolve_reads_unborn_and_detached_heads() {
        let temp = Temp::new();
        let dir = temp.0.join("repo");
        git_ok(&temp.0, &["init", dir.to_str().unwrap()]);

        // A fresh repository has an unborn branch.
        let Resolved::Checkout(checkout) = resolve(&dir).unwrap() else {
            panic!("a worktree resolves to a checkout") // coverage: off - the panic edge is the failure path
        };
        assert!(matches!(checkout.head, Head::Unborn(_))); // coverage: off - miss edge is the assert failing

        // Committing, then detaching, resolves to a bare sha.
        fs::write(dir.join("f.txt"), "x").unwrap();
        git_ok(&dir, &["add", "f.txt"]);
        git_ok(&dir, &["commit", "-qm", "c"]);
        git_ok(&dir, &["checkout", "--detach", "HEAD"]);
        let Resolved::Checkout(checkout) = resolve(&dir).unwrap() else {
            panic!("a worktree resolves to a checkout") // coverage: off - the panic edge is the failure path
        };
        assert!(matches!(checkout.head, Head::Detached(_))); // coverage: off - miss edge is the assert failing
    }

    #[test]
    fn the_admin_id_comes_from_the_git_file() {
        let temp = Temp::new();
        let common = temp.0.join("repo").join(".git");
        let admin = common.join("worktrees").join("wt1");
        fs::create_dir_all(&admin).unwrap();

        // A directory `.git` means the main worktree: no admin id.
        let main_wt = temp.0.join("repo");
        fs::create_dir_all(main_wt.join(".git")).unwrap();
        assert_eq!(admin_id(&main_wt, &common), None);

        // A `.git` file pointing into worktrees/<id> yields the id.
        let linked = temp.0.join("linked");
        fs::create_dir_all(&linked).unwrap();
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        assert_eq!(admin_id(&linked, &common).as_deref(), Some("wt1"));

        // A malformed `.git` file, a target outside worktrees/, and a target
        // that no longer exists each resolve to no id rather than a guess.
        let other = temp.0.join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join(".git"), "garbage").unwrap();
        assert_eq!(admin_id(&other, &common), None);

        // An existing target outside `worktrees/` is not an admin dir.
        let outside = temp.0.join("outside-target");
        fs::create_dir_all(&outside).unwrap();
        let elsewhere = temp.0.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(
            elsewhere.join(".git"),
            format!("gitdir: {}\n", outside.display()),
        )
        .unwrap();
        assert_eq!(admin_id(&elsewhere, &common), None);

        let dangling = temp.0.join("dangling");
        fs::create_dir_all(&dangling).unwrap();
        fs::write(
            dangling.join(".git"),
            format!("gitdir: {}\n", common.join("worktrees/gone").display()),
        )
        .unwrap();
        assert_eq!(admin_id(&dangling, &common), None);

        // An unreadable `.git` file is no id either.
        let unreadable = temp.0.join("unreadable");
        fs::create_dir_all(&unreadable).unwrap();
        let dotgit = unreadable.join(".git");
        fs::write(&dotgit, "gitdir: /x\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dotgit, fs::Permissions::from_mode(0o000)).unwrap();
            assert_eq!(admin_id(&unreadable, &common), None);
            fs::set_permissions(&dotgit, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    #[test]
    fn reflog_activity_reads_the_last_entry_and_mtime() {
        let temp = Temp::new();
        let repo = Repo {
            common_dir: temp.0.clone(),
        };
        // No log at all is no evidence.
        assert_eq!(repo.reflog_activity(Path::new("logs/HEAD")), None);

        let log_dir = temp.0.join("logs");
        fs::create_dir_all(&log_dir).unwrap();
        // A log of only blank lines has no entry to read.
        fs::write(log_dir.join("HEAD"), "  \n\n").unwrap();
        assert_eq!(repo.reflog_activity(Path::new("logs/HEAD")), None);
        // Garbage parses to nothing rather than to a guessed time: a line
        // without an epoch field, and a line whose epoch is not a number.
        fs::write(log_dir.join("HEAD"), "onefield\n").unwrap();
        assert_eq!(repo.reflog_activity(Path::new("logs/HEAD")), None);
        fs::write(log_dir.join("HEAD"), "not a reflog line\n").unwrap();
        assert_eq!(repo.reflog_activity(Path::new("logs/HEAD")), None);

        fs::write(
            log_dir.join("HEAD"),
            "0000 1111 A Name <a@b> 1700000000 +0200\tcommit: x\n",
        )
        .unwrap();
        let at = repo
            .reflog_activity(Path::new("logs/HEAD"))
            .expect("a parseable log has a time");
        assert!(at >= UNIX_EPOCH + Duration::from_secs(1700000000));
    }
}
