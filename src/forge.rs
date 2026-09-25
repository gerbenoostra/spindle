//! Optional pull/merge-request enrichment through the host's own CLI.
//!
//! The remote URL's host selects the tool: `gh` for GitHub hosts, `glab` for
//! GitLab hosts, nothing for anything else. Forge access is never required -
//! absence, authentication failure and command failure all map to
//! `WorkItem::Unknown` and are informational rather than blocking - and
//! network facts live behind [`ForgeCache`] so no query ever runs on a render
//! path. Landing itself is Git evidence; this is only the work-item overlay.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// State of the pull request or merge request a branch feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkItem {
    /// Not asked, not asked successfully, or no CLI can ask this host.
    Unknown,
    /// The forge answered: no work item exists for the branch.
    NotExisting,
    Open,
    /// Merged or closed; the work item is no longer open either way.
    Closed,
}

/// Pipeline state of an open work item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pipeline {
    Busy,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeStatus {
    pub item: WorkItem,
    /// Only meaningful while `item` is `Open`; `Unknown` otherwise.
    pub pipeline: Pipeline,
    /// `PR #191`, `MR !7` - a short human label for reasons.
    pub label: Option<String>,
    pub url: Option<String>,
    /// Why `item` is `Unknown`, when it is.
    pub reason: Option<String>,
}

impl ForgeStatus {
    fn unknown(reason: impl Into<String>) -> ForgeStatus {
        ForgeStatus {
            item: WorkItem::Unknown,
            pipeline: Pipeline::Unknown,
            label: None,
            url: None,
            reason: Some(reason.into()),
        }
    }

    fn not_existing() -> ForgeStatus {
        ForgeStatus {
            item: WorkItem::NotExisting,
            pipeline: Pipeline::Unknown,
            label: None,
            url: None,
            reason: None,
        }
    }
}

/// Which CLI serves a remote host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cli {
    Gh,
    Glab,
}

/// The `gh`/`glab` probe. `path` is the executable search path for the child
/// processes: production uses the inherited `PATH`, tests point it at a
/// directory of stubs so no real CLI (and no network) is ever involved.
pub struct Forge {
    path: OsString,
}

impl Default for Forge {
    fn default() -> Self {
        Self::from_env()
    }
}

impl Forge {
    pub fn from_env() -> Forge {
        Forge {
            path: std::env::var_os("PATH").unwrap_or_default(),
        }
    }

    /// A forge whose CLIs are searched for and run under `path` only.
    pub fn with_path(path: OsString) -> Forge {
        Forge { path }
    }

    /// The work-item state of `branch` on `remote_url`. Every failure mode -
    /// no CLI for the host, executable absent, non-zero exit, unparseable
    /// output - is `Unknown`, never a guess.
    pub fn status(&self, remote_url: &str, branch: &str) -> ForgeStatus {
        let Some((host, path)) = parse_remote(remote_url) else {
            return ForgeStatus::unknown(format!("not a forge remote: {remote_url}"));
        };
        let Some(cli) = cli_for(&host) else {
            return ForgeStatus::unknown(format!("no forge CLI known for host {host}"));
        };
        let program = match cli {
            Cli::Gh => "gh",
            Cli::Glab => "glab",
        };
        let Some(exe) = self.locate(program) else {
            return ForgeStatus::unknown(format!("{program} is not on PATH"));
        };
        match cli {
            Cli::Gh => self.gh(&exe, &host, &path, branch),
            Cli::Glab => self.glab(&exe, remote_url, branch),
        }
    }

    fn locate(&self, program: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.path)
            .map(|dir| dir.join(program))
            .find(|candidate| is_executable(candidate.as_path()))
    }

    fn run(&self, exe: &PathBuf, args: &[String]) -> Result<String, String> {
        // execve refuses a file still open for writing with ETXTBSY; the
        // hold lasts microseconds. Tests script a CLI and run it from
        // sibling threads, and a real `gh` could be mid-upgrade, so a
        // bounded retry beats reporting a transient Unknown.
        let mut retries = 5;
        let out = loop {
            let result = Command::new(exe)
                .args(args)
                .env("PATH", &self.path)
                .output();
            let busy = matches!(&result, Err(e) if e.kind() == ErrorKind::ExecutableFileBusy); // coverage: off - the true arm needs ETXTBSY
            if retries == 0 || !busy {
                break result;
            } // coverage: off - the fallthrough is the unreachable ETXTBSY retry
            retries -= 1; // coverage: off - ETXTBSY needs a writer racing the exec
            std::thread::sleep(Duration::from_millis(2)); // coverage: off - same retry arm
        }
        .map_err(|e| format!("{}: {e}", exe.display()))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(format!(
                "{} exited {:?}: {}",
                exe.display(),
                out.status.code(),
                stderr.trim()
            ))
        }
    }

    /// `gh pr list --head` answers both existence and state, without the
    /// exit-code ambiguity `gh pr view` has for branches with no PR.
    fn gh(&self, exe: &PathBuf, host: &str, path: &str, branch: &str) -> ForgeStatus {
        let args = vec![
            "pr".to_owned(),
            "list".to_owned(),
            "--repo".to_owned(),
            format!("{host}/{path}"),
            "--head".to_owned(),
            branch.to_owned(),
            "--state".to_owned(),
            "all".to_owned(),
            "--json".to_owned(),
            "number,state,url,statusCheckRollup".to_owned(),
            "--limit".to_owned(),
            "20".to_owned(),
        ];
        let text = match self.run(exe, &args) {
            Ok(text) => text,
            Err(reason) => return ForgeStatus::unknown(reason),
        };
        let items: serde_json::Value = match serde_json::from_str(&text) {
            Ok(items) => items,
            Err(e) => return ForgeStatus::unknown(format!("unparseable gh output: {e}")),
        };
        let Some(items) = items.as_array() else {
            return ForgeStatus::unknown("gh output is not a list".to_owned());
        };
        // An open item wins over any number of closed predecessors.
        let item = items
            .iter()
            .find(|pr| pr["state"] == "OPEN")
            .or_else(|| items.first());
        let Some(item) = item else {
            return ForgeStatus::not_existing();
        };
        let state = item["state"].as_str().unwrap_or_default();
        let mut status = ForgeStatus {
            item: match state {
                "OPEN" => WorkItem::Open,
                "CLOSED" | "MERGED" => WorkItem::Closed,
                _ => WorkItem::Unknown,
            },
            pipeline: Pipeline::Unknown,
            label: item["number"].as_u64().map(|n| format!("PR #{n}")),
            url: item["url"].as_str().map(str::to_owned),
            reason: (state != "OPEN" && state != "CLOSED" && state != "MERGED")
                .then(|| format!("gh reports PR state {state:?}")),
        };
        if status.item == WorkItem::Open {
            status.pipeline = gh_pipeline(&item["statusCheckRollup"]);
        }
        status
    }

    /// `glab mr list --source-branch --all -F json` answers existence and
    /// state; `head_pipeline.status` carries the pipeline.
    fn glab(&self, exe: &PathBuf, remote_url: &str, branch: &str) -> ForgeStatus {
        let args = vec![
            "mr".to_owned(),
            "list".to_owned(),
            "--repo".to_owned(),
            remote_url.to_owned(),
            "--source-branch".to_owned(),
            branch.to_owned(),
            "--all".to_owned(),
            "--output".to_owned(),
            "json".to_owned(),
            "--per-page".to_owned(),
            "20".to_owned(),
        ];
        let text = match self.run(exe, &args) {
            Ok(text) => text,
            Err(reason) => return ForgeStatus::unknown(reason),
        };
        let items: serde_json::Value = match serde_json::from_str(&text) {
            Ok(items) => items,
            Err(e) => return ForgeStatus::unknown(format!("unparseable glab output: {e}")),
        };
        let Some(items) = items.as_array() else {
            return ForgeStatus::unknown("glab output is not a list".to_owned());
        };
        let item = items
            .iter()
            .find(|mr| mr["state"] == "opened")
            .or_else(|| items.first());
        let Some(item) = item else {
            return ForgeStatus::not_existing();
        };
        let state = item["state"].as_str().unwrap_or_default();
        let mut status = ForgeStatus {
            item: match state {
                "opened" => WorkItem::Open,
                "closed" | "merged" | "locked" => WorkItem::Closed,
                _ => WorkItem::Unknown,
            },
            pipeline: Pipeline::Unknown,
            label: item["iid"].as_u64().map(|n| format!("MR !{n}")),
            url: item["web_url"].as_str().map(str::to_owned),
            reason: (!matches!(state, "opened" | "closed" | "merged" | "locked"))
                .then(|| format!("glab reports MR state {state:?}")),
        };
        if status.item == WorkItem::Open {
            status.pipeline = glab_pipeline(&item["head_pipeline"]);
        }
        status
    }
}

/// A host containing "github" is served by `gh`, one containing "gitlab" by
/// `glab` - which also routes enterprise deployments. Anything else has no
/// CLI we can drive and stays `Unknown`.
fn cli_for(host: &str) -> Option<Cli> {
    let host = host.to_ascii_lowercase();
    if host.contains("github") {
        Some(Cli::Gh)
    } else if host.contains("gitlab") {
        Some(Cli::Glab)
    } else {
        None
    }
}

/// `(host, owner/repo-path)` from a remote URL, or `None` for a local path:
/// ssh (`git@host:path`), `ssh://` and `https://` are remotes a forge can
/// serve; a bare path is not.
fn parse_remote(url: &str) -> Option<(String, String)> {
    let strip = |path: &str| {
        path.trim_end_matches('/')
            .trim_end_matches(".git")
            .to_owned()
    };
    if let Some(rest) = url.split_once("://") {
        let rest = rest.1;
        let rest = rest.rsplit('@').next().unwrap_or(rest);
        let (hostport, path) = rest.split_once('/')?;
        let host = hostport.split(':').next().unwrap_or(hostport);
        Some((host.to_owned(), strip(path)))
    } else {
        // scp syntax: [user@]host:path. No `@`/`:` means a local path.
        let (head, path) = url.split_once(':')?;
        if path.is_empty() || head.contains('/') {
            return None;
        }
        let host = head.rsplit('@').next().unwrap_or(head);
        if host.is_empty() {
            return None;
        }
        Some((host.to_owned(), strip(path)))
    }
}

/// Fold a GitHub `statusCheckRollup` into one pipeline state: any failure
/// fails it, else anything still running keeps it busy, else the checks
/// succeeded. An empty rollup is no evidence at all.
fn gh_pipeline(rollup: &serde_json::Value) -> Pipeline {
    let Some(entries) = rollup.as_array() else {
        return Pipeline::Unknown;
    };
    let mut saw_pending = false;
    let mut saw_success = false;
    let mut saw_unknown = false;
    for entry in entries {
        // CheckRun entries carry status+conclusion; StatusContext entries
        // carry state. A shape neither matches counts as unknown, not as
        // success.
        let pending = matches!(
            entry["status"].as_str(),
            Some("QUEUED" | "IN_PROGRESS" | "WAITING" | "PENDING" | "REQUESTED")
        ) || matches!(entry["state"].as_str(), Some("PENDING" | "EXPECTED"));
        let failed = matches!(
            entry["conclusion"].as_str(),
            Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE")
        ) || matches!(entry["state"].as_str(), Some("FAILURE" | "ERROR"));
        let succeeded = (entry["status"].as_str() == Some("COMPLETED")
            && matches!(
                entry["conclusion"].as_str(),
                Some("SUCCESS" | "NEUTRAL" | "SKIPPED")
            ))
            || entry["state"].as_str() == Some("SUCCESS");
        if failed {
            return Pipeline::Failed;
        }
        if pending {
            saw_pending = true;
        } else if succeeded {
            saw_success = true;
        } else {
            saw_unknown = true;
        }
    }
    if saw_pending {
        Pipeline::Busy
    } else if saw_unknown {
        Pipeline::Unknown
    } else if saw_success {
        Pipeline::Succeeded
    } else {
        Pipeline::Unknown
    }
}

fn glab_pipeline(head_pipeline: &serde_json::Value) -> Pipeline {
    match head_pipeline["status"].as_str() {
        Some("success") => Pipeline::Succeeded,
        Some("created" | "waiting_for_resource" | "preparing" | "pending" | "running") => {
            Pipeline::Busy
        }
        Some("failed") => Pipeline::Failed,
        _ => Pipeline::Unknown,
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Keeps network facts off the render path: a cached answer is served until
/// `ttl` old, and only an explicit collection asks the CLI again.
pub struct ForgeCache {
    ttl: Duration,
    entries: HashMap<(String, String), (Instant, ForgeStatus)>,
}

impl ForgeCache {
    pub fn new(ttl: Duration) -> ForgeCache {
        ForgeCache {
            ttl,
            entries: HashMap::new(),
        }
    }

    /// `now` is a parameter so tests drive expiry without sleeping.
    pub fn status(
        &mut self,
        forge: &Forge,
        remote_url: &str,
        branch: &str,
        now: Instant,
    ) -> ForgeStatus {
        let key = (remote_url.to_owned(), branch.to_owned());
        if let Some((at, status)) = self.entries.get(&key)
            && now.duration_since(*at) < self.ttl
        {
            return status.clone();
        }
        let status = forge.status(remote_url, branch);
        self.entries.insert(key, (now, status.clone()));
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_urls_parse_to_host_and_path_or_nothing() {
        for (url, expected) in [
            ("git@github.com:o/r.git", Some(("github.com", "o/r"))),
            (
                "git@gitlab.com:group/sub/r.git",
                Some(("gitlab.com", "group/sub/r")),
            ),
            ("ssh://git@gitlab.com/o/r.git", Some(("gitlab.com", "o/r"))),
            ("https://github.com/o/r.git", Some(("github.com", "o/r"))),
            (
                "https://ghe.acme.example:8443/o/r",
                Some(("ghe.acme.example", "o/r")),
            ),
            ("https://github.com/o/r/", Some(("github.com", "o/r"))),
        ] {
            assert_eq!(
                parse_remote(url).map(|(h, p)| (h.as_str().to_owned(), p.as_str().to_owned())),
                expected.map(|(h, p)| (h.to_owned(), p.to_owned())),
                "{url}"
            );
        }
        for url in [
            "/abs/path.git",
            "relative/path",
            "https://host",
            "host:",
            ":path",
            "a/b:c",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
        }
    }

    #[test]
    fn a_missing_executable_is_an_error_not_a_panic() {
        let forge = Forge::from_env();
        let missing = PathBuf::from("/definitely/not/an/exe");
        assert!(forge.run(&missing, &[]).is_err());
        // The production constructor resolves the inherited PATH.
        assert_eq!(
            Forge::default().path,
            std::env::var_os("PATH").unwrap_or_default()
        );
    }
}
