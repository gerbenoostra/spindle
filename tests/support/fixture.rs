//! A disposable Git fixture: a bare "remote" (a local path, so no network is
//! ever involved) plus a clone, with helpers that build the branch/worktree
//! scenarios the evidence and verdict tables are asserted against.
//!
//! Fixture construction may mutate - it *is* the state under test - but every
//! command pins an isolated config and identity so nothing touches the
//! developer's real Git setup.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::tempdir::TempDir;

pub struct FixtureRepo {
    pub dir: TempDir,
    /// The bare remote.
    pub remote: PathBuf,
    /// The clone holding the main worktree.
    pub main: PathBuf,
    /// The remote's name in the clone (`origin`, `upstream`, ...).
    pub remote_name: String,
}

impl FixtureRepo {
    /// A remote named `remote_name` plus a clone of it at `main/`, with one
    /// commit on `main` already pushed.
    pub fn new(remote_name: &str) -> FixtureRepo {
        let dir = TempDir::new("fixture-repo");
        let remote = dir.join("remote.git");
        let main = dir.join("main");
        run(
            None,
            &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
        );
        run(
            None,
            &[
                "clone",
                "-o",
                remote_name,
                remote.to_str().unwrap(),
                main.to_str().unwrap(),
            ],
        );
        let f = FixtureRepo {
            dir,
            remote,
            main,
            remote_name: remote_name.to_owned(),
        };
        f.commit(f.main.as_path(), "seed.txt", "seed", "seed");
        f.git(f.main.as_path(), &["push", "-u", remote_name, "main"]);
        // The clone happened while the remote was empty, so it recorded no
        // `refs/remotes/<r>/HEAD`; ask for it now that main exists.
        f.git(f.main.as_path(), &["remote", "set-head", remote_name, "-a"]);
        f
    }

    /// `git` in `dir`, with a hermetic identity and config, asserting success.
    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        run(Some(dir), args)
    }

    /// Commit `name` containing `content` on the current checkout of `dir`.
    pub fn commit(&self, dir: &Path, name: &str, content: &str, message: &str) {
        std::fs::write(dir.join(name), content).expect("write");
        self.git(dir, &["add", name]);
        self.git(dir, &["commit", "-m", message]);
    }

    /// Create `branch` with `commits` extra commits in a scratch checkout,
    /// push it and hand back nothing; the branch is left un-merged.
    pub fn branch_with_commits(&self, branch: &str, commits: usize, push: bool) {
        let names: Vec<String> = (0..commits).map(|i| format!("{branch}-{i}.txt")).collect();
        let commits: Vec<(&str, Option<&str>)> = names
            .iter()
            .map(|name| (name.as_str(), Some("x")))
            .collect();
        self.branch_with_files(branch, &commits, push);
    }

    /// As `branch_with_commits`, with caller-chosen commits: a `None` content
    /// deletes the file again, which is how a net-zero delta is built.
    pub fn branch_with_files(&self, branch: &str, commits: &[(&str, Option<&str>)], push: bool) {
        let scratch = self.dir.join(format!("scratch-{branch}"));
        self.git(
            self.main.as_path(),
            &["worktree", "add", "-b", branch, scratch.to_str().unwrap()],
        );
        for (i, (name, content)) in commits.iter().enumerate() {
            match content {
                Some(content) => {
                    std::fs::write(scratch.join(name), content).expect("write");
                    self.git(&scratch, &["add", name]);
                }
                None => {
                    self.git(&scratch, &["rm", "-q", name]);
                }
            }
            self.git(&scratch, &["commit", "-qm", &format!("{branch} {i}")]);
        }
        if push {
            self.git(&scratch, &["push", "-u", &self.remote_name.clone(), branch]);
        }
        self.git(
            self.main.as_path(),
            &["worktree", "remove", "--force", scratch.to_str().unwrap()],
        );
    }

    /// A branch on a history that shares no ancestor with `main` at all.
    pub fn orphan_branch(&self, branch: &str, push: bool) {
        let scratch = self.dir.join(format!("scratch-{branch}"));
        self.git(
            self.main.as_path(),
            &["worktree", "add", "--detach", scratch.to_str().unwrap()],
        );
        // `switch --orphan` leaves an empty index and worktree.
        self.git(&scratch, &["switch", "--orphan", branch]);
        self.commit(&scratch, &format!("{branch}.txt"), "orphan", "orphan root");
        if push {
            self.git(&scratch, &["push", "-u", &self.remote_name.clone(), branch]);
        }
        self.git(
            self.main.as_path(),
            &["worktree", "remove", "--force", scratch.to_str().unwrap()],
        );
    }

    /// Merge `branch` into `main` on the remote-visible history. `how` is the
    /// merge shape a real forge would produce.
    pub fn land(&self, branch: &str, how: Landing) {
        let main = self.main.clone();
        self.git(&main, &["checkout", "main"]);
        match how {
            Landing::Merge => {
                self.git(&main, &["merge", "--no-ff", "-m", "merge", branch]);
            }
            Landing::Squash => {
                self.git(&main, &["merge", "--squash", branch]);
                self.git(&main, &["commit", "-m", "squash"]);
            }
            Landing::CherryPick => {
                // Move main first: a cherry-pick onto the same parent can
                // produce the identical sha and make the branch look
                // ancestor-merged, which is precisely the case under test.
                self.commit(&main, "main-moved.txt", "moved", "main moved on");
                let shas = self.git(
                    &main,
                    &["rev-list", "--reverse", &format!("main..{branch}")],
                );
                for sha in shas.lines().filter(|l| !l.is_empty()) {
                    self.git(&main, &["cherry-pick", sha]);
                }
            }
        }
        self.git(&main, &["push", &self.remote_name.clone(), "main"]);
    }

    /// `git worktree add <dir>/wt-<branch> <branch>` (or `--detach` at HEAD).
    pub fn add_worktree(&self, name: &str, branch: Option<&str>) -> PathBuf {
        let path = self.dir.join(format!("wt-{name}"));
        let mut args = vec!["worktree", "add", path.to_str().unwrap()];
        match branch {
            Some(b) => args.push(b),
            None => args.push("--detach"),
        }
        self.git(self.main.as_path(), &args);
        path
    }
}

/// The merge shape a landing takes.
pub enum Landing {
    /// A true merge: the branch tip becomes an ancestor of the base.
    Merge,
    /// A squash merge: content lands, commit identity does not.
    Squash,
    /// Cherry-picks: content lands commit by commit, rebased onto the base.
    CherryPick,
}

/// The command a fixture operation runs: ambient Git environment removed
/// (an exported `GIT_DIR` or `GIT_WORK_TREE` defeats `-C` discovery, so the
/// fixture's mutations would land in the live repository they name), and a
/// hermetic identity and config pinned so nothing touches the developer's
/// real Git setup. Mirrors the isolation `src/git.rs` applies to its own
/// subprocesses; keep the variable lists in sync.
pub fn command(dir: Option<&Path>, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
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
    cmd.args(args)
        // Fixture paths are literal filenames: `x[0].txt` and `:(exclude)x`
        // are data the tests create on purpose, not pathspec patterns.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    cmd
}

fn run(dir: Option<&Path>, args: &[&str]) -> String {
    let out = command(dir, args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {:?} in {:?} failed: {}",
        args,
        dir,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
