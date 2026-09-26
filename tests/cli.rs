//! The command surface that exists today: the TUI on a bare invocation,
//! `list --json`, `--version`, `--help`, and a usage error for everything
//! else. Subcommands arrive together with the behaviour behind them; a
//! guessed name exits 2 rather than doing nothing quietly.

mod support;

use std::process::Command;

use support::tempdir::TempDir;
use support::{BIN, stderr_of};

fn run(args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("the binary runs")
}

/// The binary over an empty machine: a temp HOME and tmux socket dir, so a
/// scan finds no providers and no panes rather than the host's real state.
fn run_isolated(args: &[&str], home: &TempDir) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .env("HOME", home.path())
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("TMUX_TMPDIR", home.join("tmux"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("the binary runs")
}

#[test]
fn version_prints_version_and_the_resolved_executable() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("agent-sessions "), "{stdout}");
    assert!(
        stdout.contains("running from"),
        "the resolved path is the whole point of printing it: {stdout}"
    );
    assert!(stderr_of(&out).is_empty());
}

#[test]
fn help_lists_only_what_is_shipped() {
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("--version"), "{stdout}");
        // No stubbed subcommands: help names nothing the binary cannot do.
        for future in ["hook", "register", "doctor"] {
            assert!(
                !stdout.contains(&format!("agent-sessions {future}")),
                "help promises `{future}`, which has not shipped: {stdout}"
            );
        }
    }
}

/// The `script` invocation that runs `bin` under a pty on this platform.
/// Two dialects exist: BSD takes the command as trailing positional
/// arguments, while util-linux wants `-c` and rejects extra positionals.
/// Probing a trivial command picks the local dialect; neither working means
/// there is no usable `script` here.
fn script_pty(bin: &std::ffi::OsStr) -> Option<Vec<std::ffi::OsString>> {
    let dialects: [&[&str]; 2] = [
        // BSD: the command is trailing positional arguments.
        &["-q", "/dev/null", "/usr/bin/true"],
        // util-linux: the command is `-c`'s argument; extra positionals are
        // a usage error.
        &["-q", "-c", "/usr/bin/true", "/dev/null"],
    ];
    for probe in dialects {
        let ok = Command::new("script")
            .args(probe)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return Some(
                probe
                    .iter()
                    .map(|a| {
                        if *a == "/usr/bin/true" {
                            bin.to_os_string()
                        } else {
                            std::ffi::OsString::from(a)
                        }
                    })
                    .collect(),
            );
        }
    }
    None
}

#[test]
fn a_bare_invocation_under_a_terminal_quits_on_q() {
    // `script` runs the binary behind a pty, so the TUI path - terminal
    // setup, event poll, draw loop - executes for real. One `q` ends it.
    let Some(argv) = script_pty(std::ffi::OsStr::new(BIN)) else {
        return; // no usable script(1) on this platform
    };
    let home = TempDir::new("cli");
    let mut child = Command::new("script")
        .args(&argv)
        .env("HOME", home.path())
        .env("TMUX_TMPDIR", home.join("tmux"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("script spawns");
    use std::io::Write;
    // Give the event loop one idle tick first: an expired poll returns no
    // event, so the quiet path runs too.
    std::thread::sleep(std::time::Duration::from_millis(300));
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"q")
        .expect("q writes to the pty");
    let out = child.wait_with_output().expect("the TUI exits");
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_bare_invocation_without_a_terminal_is_an_operational_error() {
    // `agent-sessions` is the dashboard; a piped stdout is not a terminal, so
    // it declines rather than drawing escape codes into a file.
    let home = TempDir::new("cli");
    let out = run_isolated(&[], &home);
    assert_eq!(out.status.code(), Some(1));
    let stderr = stderr_of(&out);
    assert!(stderr.contains("needs a terminal"), "{stderr}");
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn list_json_prints_the_complete_snapshot() {
    let home = TempDir::new("cli");
    // No CLAUDE_CONFIG_DIR here: the default `$HOME/.claude` resolves.
    let out = Command::new(BIN)
        .args(["list", "--json"])
        .env("HOME", home.path())
        .env("TMUX_TMPDIR", home.join("tmux"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("the binary runs");
    assert!(out.status.success(), "{}", stderr_of(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON output");
    assert_eq!(json["schema_version"], 1, "{stdout}");
    assert!(json["observed_at"].as_u64().unwrap_or(0) > 0);
    assert!(json["repos"].is_array() && json["work"].is_array());
    assert!(json["conversations"].is_array() && json["errors"].is_array());
    assert!(stderr_of(&out).is_empty(), "{out:?}");
}

#[test]
fn list_json_without_a_home_is_an_operational_error() {
    let home = TempDir::new("cli");
    let out = Command::new(BIN)
        .args(["list", "--json"])
        .env_remove("HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("TMUX_TMPDIR", home.join("tmux"))
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("HOME is not set"), "{out:?}");
}

#[test]
fn list_without_json_is_a_usage_error() {
    let home = TempDir::new("cli");
    let out = run_isolated(&["list"], &home);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("list"), "{out:?}");
}

#[test]
fn unshipped_subcommands_are_usage_errors() {
    for args in [
        vec!["hook", "claude", "stop"],
        vec!["register"],
        vec!["doctor"],
        vec!["frobnicate"],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let stderr = stderr_of(&out);
        assert!(
            stderr.contains("unexpected arguments:"),
            "{args:?}: {stderr}"
        );
        assert!(stderr.contains(&args[0].to_string()), "{args:?}: {stderr}");
        assert!(out.stdout.is_empty(), "{args:?} wrote to stdout");
    }
}

#[test]
fn version_and_help_short_circuit_other_arguments() {
    // Deliberate: a guessed word beside --version or --help still answers the
    // flag. The usage-error contract is about silence, not about precedence.
    for flag in ["--version", "--help"] {
        let out = run(&["frobnicate", flag]);
        assert!(out.status.success(), "{flag}: {out:?}");
        assert!(stderr_of(&out).is_empty(), "{flag}: {out:?}");
    }
}

#[test]
fn an_unknown_flag_is_a_usage_error() {
    let out = run(&["--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("--bogus"));
}

#[test]
fn a_non_utf8_argument_is_a_usage_error() {
    use std::os::unix::ffi::OsStrExt;
    let out = Command::new(BIN)
        .arg(std::ffi::OsStr::from_bytes(b"\xff"))
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr_of(&out).contains("not valid UTF-8"));
}
