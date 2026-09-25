//! Invariants about the sources themselves rather than about behaviour.
//!
//! Each task that grows a write surface adds its own assertion here; the tmux
//! scan below only covers the boundary that exists from the start.

use std::fs;
use std::path::{Path, PathBuf};

/// The exact boundary of the unit-test module every file's `#[cfg(test)]`
/// attribute is expected to sit on. Private items can only be unit-tested
/// from inside their own file (an integration test under `tests/` only sees
/// `pub` items), so scans that must look at production code only exclude
/// everything from this marker onward. A `#[cfg(test)]` anywhere else - a
/// cfg-gated helper, the attribute mentioned in a comment - would let real
/// code past it slip by unscanned, so a mismatch fails rather than being
/// ignored.
const TEST_MODULE_MARKER: &str = "#[cfg(test)]\nmod tests";

/// tmux writes this tool must never perform, as the quoted argv words that
/// would do them. Writing an option freezes or fights the user's own
/// configuration, window names belong to the user and to `wt`, and killing a
/// pane, window, session or server is not this tool's business at all. Reads
/// (`display-message`, `show-options`, `list-panes`, `list-windows`) stay
/// allowed.
///
/// The scan is deliberately blunt: the word is forbidden as a quoted string
/// literal anywhere in production code, so naming the forbidden command in
/// prose stays possible. `display-popup -E` is absent on purpose - the popup
/// launcher is an allowed optional binding that writes nothing.
const FORBIDDEN_TMUX_ARGV: [&str; 8] = [
    "\"set-option\"",
    "\"set-window-option\"",
    "\"setw\"",
    "\"rename-window\"",
    "\"kill-pane\"",
    "\"kill-window\"",
    "\"kill-session\"",
    "\"kill-server\"",
];

#[test]
fn production_sources_never_name_a_tmux_write() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_sources(&src);
    assert!(
        !files.is_empty(),
        "no sources found under {}",
        src.display()
    );
    for path in &files {
        let text = fs::read_to_string(path).expect("a source file this crate owns");
        for argv in FORBIDDEN_TMUX_ARGV {
            assert!(
                !production_part(&text, path).contains(argv),
                "{}: names the tmux write {argv}, which this tool never performs",
                path.display()
            );
        }
    }
}

/// The only modules allowed to spawn a subprocess: each external program's
/// argv lives behind one audited boundary, and adding a spawn surface means
/// editing this list where a reviewer will see it.
const SPAWN_MODULES: [&str; 2] = ["git.rs", "forge.rs"];

#[test]
fn external_programs_are_spawned_in_their_own_module() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in rust_sources(&src) {
        let text = fs::read_to_string(&path).expect("a source file this crate owns");
        let production = production_part(&text, &path);
        if production.contains("Command::new") {
            let module = path.file_name().and_then(|n| n.to_str());
            assert!(
                SPAWN_MODULES.contains(&module.unwrap_or_default()),
                "{}: spawns a subprocess outside {SPAWN_MODULES:?}",
                path.display()
            );
        }
    }
    // `GIT_OPTIONAL_LOCKS=0` is pinned exactly where the Git subprocess
    // lives, so a background read never contends for index locks.
    let git_rs = src.join("git.rs");
    let text = fs::read_to_string(&git_rs).expect("src/git.rs exists");
    assert!(
        production_part(&text, &git_rs).contains("GIT_OPTIONAL_LOCKS"),
        "src/git.rs: the Git runner must pin GIT_OPTIONAL_LOCKS=0"
    );
}

/// Git argv that mutate a repository, as the quoted argv words that would do
/// them. `git worktree remove`/`git branch -d` arrive only with confirmed
/// cleanup execution; this scan is what keeps them confined to it.
const FORBIDDEN_GIT_ARGV: [&str; 15] = [
    "\"push\"",
    "\"fetch\"",
    "\"commit\"",
    "\"merge\"",
    "\"rebase\"",
    "\"reset\"",
    "\"checkout\"",
    "\"switch\"",
    "\"restore\"",
    "\"update-ref\"",
    "\"gc\"",
    "\"prune\"",
    "\"init\"",
    "\"clone\"",
    "\"am\"",
];

#[test]
fn production_sources_never_name_a_git_write() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in rust_sources(&src) {
        let text = fs::read_to_string(&path).expect("a source file this crate owns");
        let production = production_part(&text, &path);
        for argv in FORBIDDEN_GIT_ARGV {
            assert!(
                !production.contains(argv),
                "{}: names the Git write {argv}, which the read substrate never runs",
                path.display()
            );
        }
    }
}

/// The text of `file` up to its unit-test module, after asserting the
/// `#[cfg(test)]` convention that makes that cut safe.
fn production_part<'a>(text: &'a str, path: &Path) -> &'a str {
    let cfg_test_count = text.matches("#[cfg(test)]").count();
    let test_module_count = text.matches(TEST_MODULE_MARKER).count();
    assert_eq!(
        cfg_test_count,
        test_module_count,
        "{}: a #[cfg(test)] attribute isn't on the file's `mod tests` boundary; \
         production-code scans only exclude that boundary",
        path.display()
    );
    assert!(
        test_module_count <= 1,
        "{}: more than one `{TEST_MODULE_MARKER}` block; \
         production-code scans only exclude the first one",
        path.display()
    );
    match text.find(TEST_MODULE_MARKER) {
        Some(at) => &text[..at],
        None => text,
    }
}

/// Every `.rs` file under `dir`, recursively.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).expect("the source directory") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            found.extend(rust_sources(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            found.push(path);
        }
    }
    found
}
