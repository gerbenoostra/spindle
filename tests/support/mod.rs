//! Shared helpers for integration tests.
//!
//! Each integration test binary imports only the pieces it needs; suppress
//! dead-code warnings because the whole module is compiled for every test.

#![allow(dead_code)]

pub mod fixture;
pub mod markdown;
pub mod tempdir;
pub mod tmux;

pub const BIN: &str = env!("CARGO_BIN_EXE_agent-sessions");

/// What the binary itself wrote to stderr.
///
/// Under `cargo llvm-cov` the profiling runtime shares this stream with the
/// process it instruments, and writes `LLVM Profile Error: ...` on it when a
/// `.profraw` cannot be written. That is a fact about the coverage run, not
/// about the binary, and left in it turns every "writes nothing to stderr"
/// assertion into an assertion about the profiler's health as well - so
/// `just coverage` fails in tests that have nothing to do with what is being
/// measured, while `cargo test` passes. Verified: an instrumented binary whose
/// profile path cannot be written prints exactly that prefix and nothing else
/// changes about it.
pub fn stderr_of(out: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr);
    // Only reshaped when the profiler actually interfered. Splitting into
    // lines and joining them back loses a trailing newline, which would let a
    // binary that wrote nothing but a blank line pass an `is_empty` check - so
    // the usual case keeps the bytes exactly as they came.
    match text.contains("LLVM Profile") {
        false => text.into_owned(),
        true => text
            .lines()
            .filter(|line| !line.starts_with("LLVM Profile"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// `name` resolved on `PATH`: fixtures must not assume `/bin`, which the
/// Linux nix build sandbox reduces to `sh` alone.
pub fn on_path(name: &str) -> std::path::PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| panic!("{name} is on PATH"))
}

/// Whether there is a tmux to test against.
///
/// tmux is installed everywhere this suite runs and the CI job installs it
/// before running the tests; a machine without one skips rather than fails,
/// which is the same promise the tool itself makes.
pub fn tmux_or_skip() -> bool {
    let found = std::process::Command::new("tmux")
        .arg("-V")
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        eprintln!("no tmux on PATH: skipping");
    }
    found
}

/// One Claude transcript user record for `id` at `cwd`, newline-terminated,
/// shaped like the records Claude writes: `timestamp` is now, and
/// `gitBranch` is the branch checked out in `cwd` right now (`HEAD` when
/// detached), omitted outside a Git checkout - `cwd` is the project.
pub fn claude_turn(id: &str, cwd: &std::path::Path, text: &str) -> String {
    claude_record(id, cwd, cwd, text)
}

/// The same record run from `cwd` inside a session whose project is
/// `project`: Claude's `gitBranch` names the project directory's branch,
/// wherever the record itself ran.
pub fn claude_record(
    id: &str,
    cwd: &std::path::Path,
    project: &std::path::Path,
    text: &str,
) -> String {
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(project)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let git_branch = branch
        .map(|b| format!(",\"gitBranch\":\"{b}\""))
        .unwrap_or_default();
    format!(
        "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"{}\"{git_branch},\"timestamp\":\"{}\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n",
        cwd.display(),
        iso_now()
    )
}

/// Now as `YYYY-MM-DDTHH:MM:SS.mmmZ`, the form Claude stamps records with.
pub fn iso_now() -> String {
    iso(std::time::SystemTime::now())
}

/// `at` as `YYYY-MM-DDTHH:MM:SS.mmmZ`, the form Claude stamps records with.
pub fn iso(at: std::time::SystemTime) -> String {
    let since = at
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch");
    let secs = since.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60,
        since.subsec_millis()
    )
}

/// A `claude` process that is really `bash`: a symlink, not a copy - macOS
/// kills a relocated copy of a signed system binary, while `comm` still
/// reports the invoked name. Spawns `sleep` under the `claude` name.
pub fn live_claude(
    home: &tempdir::TempDir,
    id: &str,
    status: &str,
    worktree: &std::path::Path,
) -> std::process::Child {
    let exe = home.join("claude");
    // One link serves every agent the home runs.
    if !exe.exists() {
        std::os::unix::fs::symlink(on_path("bash"), &exe).expect("bash links");
    }
    let child = std::process::Command::new(&exe)
        .arg("-c")
        .arg("sleep 300; exit")
        // Killing the guard leaves `sleep` orphaned: it must not hold the
        // test harness's output pipes open for its remaining lifetime.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the agent spawns");
    let sessions = home.join(".claude/sessions");
    std::fs::create_dir_all(&sessions).expect("mkdir");
    std::fs::write(
        sessions.join(format!("{}.json", child.id())),
        format!(
            "{{\"pid\":{},\"sessionId\":\"{id}\",\"status\":\"{status}\",\"updatedAt\":1788621019906,\"statusUpdatedAt\":1788621019906,\"cwd\":\"{}\",\"procStart\":\"{}\"}}",
            child.id(),
            worktree.display(),
            proc_start(child.id()),
        ),
    )
    .expect("session file writes");
    child
}

/// The live process's start as Claude's `procStart` ctime (UTC), read from
/// the kernel through `ps -o lstart` so the `(pid, pid_start)` pair
/// validates as that instance.
pub fn proc_start(pid: u32) -> String {
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC0")
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}
