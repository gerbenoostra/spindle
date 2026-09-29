# The recipes CI runs are the recipes a human types; that is the whole point of this file.

# List the recipes.
default:
    @just --list

# Format the sources.
fmt:
    cargo fmt

# Fail if the sources are not formatted.
fmt-check:
    cargo fmt --check

# Lint, warnings are errors.
lint:
    cargo clippy --all-targets -- -D warnings

# Lint the shell installer.
lint-sh:
    shellcheck -s sh install.sh

# Run the test suite.
test:
    cargo test

# Run the test suite and hold every region of `src/` to covered.
#
# The bar is read from the exported segments rather than from
# `--fail-under-regions`, because the two do not agree. `src/` is compiled
# twice - once with `cfg(test)` for the lib's own test binary, once as the rlib
# the integration tests link - and `llvm-cov report` leaves a handful of spans
# unmerged between the two, so its summary counts regions as missed that
# `llvm-cov show` renders as covered. The segments are the view `show` renders,
# and they answer one question consistently: is there a region nothing reached?
# Full region coverage implies full line coverage, so that one bar is enough.
#
# A line that cannot be reached says so for itself, with a trailing
# `// coverage: off` and the reason it is unreachable. The marker is matched
# against the whole line, so it exempts every region on that line and not only
# the one that is uncovered today: keep it to lines that carry nothing else, or
# say in the comment what else it covers.
coverage:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v jq >/dev/null || { echo "the coverage gate needs jq." >&2; exit 1; }
    cargo llvm-cov --no-report
    cargo llvm-cov report --summary-only
    echo
    echo "The misses above are counted per compilation, not per region; the bar"
    echo "below is the merged region view. See the comment on this recipe."
    report="$(mktemp)"
    regions="$(mktemp)"
    trap 'rm -f "$report" "$regions"' EXIT
    cargo llvm-cov report --json --output-path "$report"
    # A segment carries [line, column, count, has-count, region-entry, gap].
    # One with a count of zero that is not a gap is a region nothing reached.
    # Written to a file rather than piped into the loop: a gate that cannot
    # fail is worse than no gate, and `set -e` does not reach into a process
    # substitution, so a `jq` that dies there would read as "nothing to report".
    jq -r '.data[].files[] | .filename as $file
           | (.segments // [])[]
           | select(.[3] and .[2] == 0 and (.[5] | not))
           | "\($file):\(.[0])"' "$report" | sort -u > "$regions"
    # The same argument: a report naming no file at all is a broken run, not a
    # clean one.
    files="$(jq -r '[.data[].files[].filename] | length' "$report")"
    if (( files == 0 )); then
        echo "the coverage report names no files; nothing was measured." >&2
        exit 1
    fi
    uncovered=0
    while IFS= read -r region; do
        [[ -n "$region" ]] || continue
        file="${region%:*}"
        line="${region##*:}"
        if [[ "$(sed -n "${line}p" "$file")" == *'// coverage: off'* ]]; then
            continue
        fi
        echo "uncovered region: $file:$line" >&2
        uncovered=$((uncovered + 1))
    done < "$regions"
    if (( uncovered )); then
        echo "$uncovered uncovered region(s) in $files file(s); the bar is all of them." >&2
        exit 1
    fi
    echo "Every region of src/ was reached, across $files files."

# What CI runs.
check: fmt-check lint lint-sh test

# Build with the minimum supported Rust version from Cargo.toml.
msrv:
    #!/usr/bin/env bash
    set -euo pipefail
    msrv="$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' Cargo.toml)"
    [[ -n "$msrv" ]] || { echo "Cargo.toml names no rust-version." >&2; exit 1; }
    rustup toolchain install "$msrv" --profile minimal --no-self-update
    rustup run "$msrv" cargo build --locked --all-targets

# Check the flake, build the package and run what came out of it.
nix-verify:
    #!/usr/bin/env bash
    set -euo pipefail
    nix flake check
    nix build .#agent-sessions
    # The packaging path is only proven by running what came out of it.
    ./result/bin/agent-sessions --version
    # share/ joins the package when it exists; today there is nothing to check.
    [ -d share ] || exit 0
    missing=0
    while IFS= read -r f; do
        rel="${f#share/}"
        if [[ ! -e "result/share/$rel" ]]; then
            echo "missing in Nix output: $rel" >&2
            missing=1
        fi
    done < <(find share -type f -o -type l)
    exit "$missing"

# Build the release tarball and check its share tree.
package-verify:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release --locked
    target="$(rustc -vV | sed -n 's|host: ||p')"
    dist="$(mktemp -d)"
    trap 'rm -rf "$dist"' EXIT
    name="agent-sessions-ci-${target}"
    mkdir -p "$dist/$name"
    cp "${CARGO_TARGET_DIR:-target}/release/agent-sessions" "$dist/$name/"
    cp README.md LICENSE "$dist/$name/"
    # -L, not -R alone: share/ may carry symlinks into trees the
    # tarball does not ship. share/ joins the tarball when it exists.
    if [ -d share ]; then
        cp -RL share "$dist/$name/"
    fi
    tar -C "$dist" -czf "$dist/$name.tar.gz" "$name"
    [ -d share ] || exit 0
    find share -type f -o -type l | sed 's|^share/||' | sort > "$dist/expected"
    tar -tzf "$dist/$name.tar.gz" \
        | grep '/share/.' \
        | sed 's|^[^/]*/share/||' \
        | grep -v '/$' \
        | sort -u > "$dist/actual"
    if ! diff -u "$dist/expected" "$dist/actual"; then
        echo "release tarball share files do not match share/" >&2
        exit 1
    fi

# Build the release binary.
build:
    cargo build --release

# Shadow the installed binary with this checkout's release build.
link:
    #!/usr/bin/env bash
    set -euo pipefail
    bin_dir="${AGENT_SESSIONS_BIN_DIR:-$HOME/.local/bin}"
    mkdir -p "$bin_dir"
    ln -sf "{{justfile_directory()}}/target/release/agent-sessions" "$bin_dir/agent-sessions"
    echo "$bin_dir/agent-sessions now shadows any installed agent-sessions." >&2
    echo "The shadow is invisible: 'agent-sessions --version' prints the resolved path." >&2
    echo "'just unlink' removes it." >&2

# Remove the dev shadow.
unlink:
    #!/usr/bin/env bash
    set -euo pipefail
    bin_dir="${AGENT_SESSIONS_BIN_DIR:-$HOME/.local/bin}"
    rm -f "$bin_dir/agent-sessions"

# Build the nix package from this checkout.
nix-build:
    nix build .#agent-sessions
