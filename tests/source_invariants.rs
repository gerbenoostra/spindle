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

/// tmux options owned by the agent-status tool, as bare substrings. The
/// dashboard must never read them: they are a lossy projection of the
/// primary evidence this tool collects itself, and depending on them would
/// couple the tools. Unlike the argv words above, an option name appears
/// inside a `#{...}` format string, so the scan is on the name itself -
/// production sources may not mention them at all.
const FORBIDDEN_STATUS_OPTIONS: [&str; 2] = ["@agent_status", "@agent_pane_status"];

#[test]
fn production_sources_never_read_an_agent_status_option() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in rust_sources(&src) {
        let text = fs::read_to_string(&path).expect("a source file this crate owns");
        let production = production_part(&text, &path);
        for option in FORBIDDEN_STATUS_OPTIONS {
            assert!(
                !production.contains(option),
                "{}: names the agent-status option {option}, which this tool never reads",
                path.display()
            );
        }
    }
}

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
const SPAWN_MODULES: [&str; 5] = ["action.rs", "git.rs", "forge.rs", "process.rs", "tmux.rs"];

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

/// File-writing calls, as substrings. Every write the tool performs lands
/// inside its own store under `$XDG_STATE_HOME/agent-sessions/` and goes
/// through `src/store.rs` - the journal, the checkpoint, the seen-state and
/// the not-busy marks. Agent files, transcripts, tmux options, Git config
/// and window names are not ours to write, so no other module may hold a
/// write primitive.
const WRITE_CALLS: [&str; 8] = [
    "fs::write",
    "File::create",
    "create_dir_all",
    "OpenOptions",
    "fs::rename",
    "remove_file",
    "set_permissions",
    "fs::copy",
];

#[test]
fn only_the_store_module_writes_files() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in rust_sources(&src) {
        let text = fs::read_to_string(&path).expect("a source file this crate owns");
        let production = production_part(&text, &path);
        let module = path.file_name().and_then(|n| n.to_str());
        if module == Some("store.rs") {
            continue;
        }
        for call in WRITE_CALLS {
            assert!(
                !production.contains(call),
                "{}: writes through {call}, which belongs in src/store.rs",
                path.display()
            );
        }
    }
}

/// argv words the action module must never build, as quoted string
/// literals: a shell - the resume is exec'd argv, never a `-c` line -
/// tmux topology creation, renames and option writes, and any URL
/// literal, since forge URLs arrive from the snapshot only. Selection
/// argv (`select-window`, `select-pane`, `-t`) stays allowed.
const FORBIDDEN_ACTION_ARGV: [&str; 15] = [
    "\"sh\"",
    "\"bash\"",
    "\"zsh\"",
    "\"fish\"",
    "\"-c\"",
    "\"new-session\"",
    "\"new-window\"",
    "\"split-window\"",
    "\"rename-window\"",
    "\"rename-session\"",
    "\"set-option\"",
    "\"set-window-option\"",
    "\"setw\"",
    "\"http://",
    "\"https://",
];

#[test]
fn action_code_spawns_no_shell_topology_or_urls() {
    let action = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/action.rs");
    let text = fs::read_to_string(&action).expect("src/action.rs exists");
    let production = production_part(&text, &action);
    for argv in FORBIDDEN_ACTION_ARGV {
        assert!(
            !production.contains(argv),
            "src/action.rs: names {argv} - resumes are exec'd argv, jumps only select, URLs come from the snapshot"
        );
    }
}

/// Style names the render path must not hold outside the palette: a span
/// takes a `Role`, and only `src/tui/palette.rs` may build a `Style` or
/// turn a role into a `Color` or `Modifier` - the next style change
/// audits one file, not twenty.
#[test]
fn only_the_palette_names_a_style() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let palette = src.join("tui/palette.rs");
    assert!(
        palette.is_file(),
        "src/tui/palette.rs must exist - it is the one file allowed to name styles"
    );
    for path in rust_sources(&src) {
        if path == palette {
            continue;
        }
        let text = fs::read_to_string(&path).expect("a source file this crate owns");
        let named = style_names(production_part(&text, &path));
        assert!(
            named.is_empty(),
            "{}: names {named:?} outside src/tui/palette.rs; take a Role instead",
            path.display()
        );
    }
}

/// The style names `code` holds. Besides the `Style`, `Color` and
/// `Modifier` paths, an import from `ratatui::style` - braced, aliased or
/// through ratatui's `prelude` - is what brings in `Stylize` shorthand
/// (`.cyan()`, `.bold()`), which names no type at the call site. Each
/// needle must start a word, so `OrdinalStyle::` is not `Style::`; a
/// `prelude::` counts only within a statement that names `ratatui`, so
/// `std::io::prelude` passes.
fn style_names(code: &str) -> Vec<&'static str> {
    const NEEDLES: [&str; 6] = [
        "Style::",
        "Color::",
        "Modifier::",
        "Stylize",
        "style::",
        "prelude::",
    ];
    NEEDLES
        .into_iter()
        .filter(|needle| {
            code.match_indices(needle).any(|(at, _)| {
                let before = &code[..at];
                let starts_word = !before
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                let statement = &before[before.rfind(';').map_or(0, |end| end + 1)..];
                starts_word && (*needle != "prelude::" || statement.contains("ratatui"))
            })
        })
        .collect()
}

#[test]
fn the_style_guard_sees_every_way_to_name_a_style() {
    for (code, named) in [
        ("Style::default()", vec!["Style::"]),
        ("Style::reset().fg(Color::Red)", vec!["Style::", "Color::"]),
        ("x.add_modifier(Modifier::BOLD)", vec!["Modifier::"]),
        ("use ratatui::style::Stylize;", vec!["Stylize", "style::"]),
        ("use ratatui::style::Color as C;", vec!["style::"]),
        ("use ratatui::{style::Color, text::Line};", vec!["style::"]),
        ("use ratatui::prelude::*;", vec!["prelude::"]),
        ("use ratatui::{prelude::*, Frame};", vec!["prelude::"]),
        ("use ratatui::text::Line;\nuse std::io::prelude::*;", vec![]),
        ("OrdinalStyle::Suffix", vec![]),
        ("Role::Focus.style()", vec![]),
        ("Block::default().border_style(s)", vec![]),
    ] {
        assert_eq!(style_names(code), named, "{code}");
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
