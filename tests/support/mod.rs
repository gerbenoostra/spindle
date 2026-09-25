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
