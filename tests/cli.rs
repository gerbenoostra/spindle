//! The command surface that exists today: `--version`, `--help`, and a usage
//! error for everything else. Subcommands arrive together with the behaviour
//! behind them; a guessed name exits 2 rather than doing nothing quietly.

mod support;

use std::process::Command;

use support::{BIN, stderr_of};

fn run(args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(args)
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
        for future in ["list", "hook", "register", "doctor"] {
            assert!(
                !stdout.contains(&format!("agent-sessions {future}")),
                "help promises `{future}`, which has not shipped: {stdout}"
            );
        }
    }
}

#[test]
fn a_bare_invocation_is_a_usage_error() {
    let out = run(&[]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = stderr_of(&out);
    assert!(stderr.contains("no command given"), "{stderr}");
    assert!(stderr.contains("usage:"), "{stderr}");
}

#[test]
fn unshipped_subcommands_are_usage_errors() {
    for args in [
        vec!["list", "--json"],
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
