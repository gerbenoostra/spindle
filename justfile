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

# The fast subset of CI: format, lints and tests.
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

# ci.yml's jobs per runner OS, as recipes; tests/ci_jobs.rs holds the two equal.
ci_linux_jobs := "fmt-check lint lint-sh test coverage msrv nix-verify package-verify"
ci_macos_jobs := "test nix-verify"

# Run ci.yml's macOS and Linux jobs locally, against the committed HEAD.
ci: ci-macos ci-linux

# Run ci.yml's macOS jobs on this Mac, against the committed HEAD.
ci-macos:
    #!/usr/bin/env bash
    set -euo pipefail
    [[ "$(uname -s)" == Darwin ]] || { echo "ci-macos runs on macOS." >&2; exit 1; }
    just _ci-snapshot "{{justfile_directory()}}" "{{justfile_directory()}}/target/ci/macos" macos

# Run ci.yml's Linux jobs in a Docker container, against the committed HEAD.
ci-linux:
    #!/usr/bin/env bash
    set -euo pipefail
    # The container runs the host's architecture, so on Apple silicon this is
    # aarch64 Linux where CI's ubuntu-latest is x86_64. It runs privileged
    # because the Nix build sandbox needs namespaces.
    root="{{justfile_directory()}}"
    # The image, like the jobs, comes from HEAD rather than the working tree.
    at_head() { git -C "$root" show "HEAD:$1"; }
    msrv="$(at_head Cargo.toml | sed -n 's/^rust-version = "\(.*\)"$/\1/p')"
    llvm_cov="$(at_head .github/workflows/ci.yml | sed -n 's/.*tool: cargo-llvm-cov@//p')"
    week="$(date +%G-W%V)"
    # Without a provenance attestation, which carries a build timestamp, the
    # image ID changes only when the image does.
    image="$(at_head ci/linux.Dockerfile | docker build --quiet --pull --provenance=false \
        --tag spindle-ci-linux --build-arg "IMAGE_WEEK=$week" \
        --build-arg "MSRV=$msrv" --build-arg "LLVM_COV_VERSION=$llvm_cov" -)"
    # The Nix store is kept per image: a fresh volume starts as a copy of the
    # image's /nix, and a rebuilt image's Nix never meets an older store.
    # Every image is rebuilt weekly, so stores of earlier weeks are dead and
    # go (unless a running build still holds one); this week's stay, as
    # checkouts whose HEADs build different images each need theirs.
    nix_volume="spindle-ci-linux-nix-$week-$(printf '%s' "${image#sha256:}" | cut -c1-12)"
    docker volume ls --quiet --filter name='^spindle-ci-linux-nix-' \
        | { grep -v -- "^spindle-ci-linux-nix-$week-" || true; } \
        | while IFS= read -r stale; do docker volume rm "$stale" >/dev/null 2>&1 || true; done
    # Mounted at their host paths: a linked worktree's .git names its common
    # git directory by absolute path, and that may lie outside the checkout.
    mounts=(--volume "$root:$root:ro")
    common="$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)"
    [[ "$common" == "$root"/* ]] || mounts+=(--volume "$common:$common:ro")
    # One snapshot per checkout, so runs from two worktrees cannot reset each
    # other's tree. The cargo home is shared whole: cargo keeps its package
    # cache locks at its root, not in registry/, and they hold across
    # containers on one volume.
    checkout="spindle-ci-linux-src-$(printf '%s' "$root" | git hash-object --stdin | cut -c1-12)"
    # Each snapshot is labelled with its checkout, so the snapshots of removed
    # checkouts, such as deleted worktrees, go (unless a running build still
    # holds one). Created before the run, as `docker run` cannot label one.
    label=spindle-ci-linux.checkout
    docker volume create --label "$label=$root" "$checkout" >/dev/null
    docker volume ls --filter label="$label" --format "{{{{.Name}}\t{{{{.Label \"$label\"}}" \
        | while IFS=$'\t' read -r volume path; do
            [[ -e "$path" ]] || docker volume rm "$volume" >/dev/null 2>&1 || true
        done
    # No --tty: a GitHub runner has no terminal, and colour is forced below.
    # Without a terminal, Ctrl-C reaches only the container's PID 1, which a
    # bash there ignores; tini as PID 1, told to signal the whole process
    # group, stops the jobs rather than leaving them running unseen.
    docker run --rm --privileged --init --env TINI_KILL_PROCESS_GROUP=1 "${mounts[@]}" \
        --volume "$checkout:/home/runner/ci" \
        --volume spindle-ci-linux-cargo-home:/home/runner/cargo-home \
        --volume "$nix_volume:/nix" \
        --env CARGO_TERM_COLOR=always \
        "$image" \
        bash -c 'set -euo pipefail
            git config --global --add safe.directory "*"
            # CI installs the stable of the day, not the one the image baked.
            rustup update stable --no-self-update >/dev/null
            # Set after rustup, whose proxies stay in ~/.cargo from the image.
            # Only the cache moves: the tools stay on PATH in ~/.cargo/bin,
            # and anything cargo installs here would land off PATH.
            export CARGO_HOME=/home/runner/cargo-home
            just --justfile "$1/justfile" _ci-snapshot "$1" /home/runner/ci linux' \
        _ "$root"

# Drop the Linux CI image and the volumes that cache its builds, for every checkout.
ci-linux-clean:
    #!/usr/bin/env bash
    set -euo pipefail
    volumes=()
    while IFS= read -r v; do volumes+=("$v"); done \
        < <(docker volume ls --quiet --filter name='^spindle-ci-linux-')
    (( ${#volumes[@]} == 0 )) || docker volume rm "${volumes[@]}"
    docker image rm --force spindle-ci-linux 2>/dev/null || true

# Run CI's jobs for `os` in a clean checkout of the repo's HEAD, kept under
# `dir` so builds stay incremental between runs. The job list, like the job
# recipes, is read from that checkout; only this plumbing is the working tree's.
_ci-snapshot repo dir os:
    #!/usr/bin/env bash
    set -euo pipefail
    src="{{dir}}/src"
    [[ -d "$src/.git" ]] || git init --quiet "$src"
    git -C "$src" fetch --quiet --no-tags "{{repo}}" HEAD
    git -C "$src" checkout --quiet --force --detach FETCH_HEAD
    git -C "$src" clean --quiet -ffdx
    echo "ci: $(git -C "$src" log -1 --format='%h %s') on $(uname -s)" >&2
    export CARGO_TARGET_DIR="{{dir}}/target"
    cd "$src"
    jobs="$(just --evaluate "ci_{{os}}_jobs")"
    # Coverage profiles from the previous run's HEAD pollute this one's.
    if [[ " $jobs " == *" coverage "* ]]; then
        cargo llvm-cov clean --workspace
    fi
    # Word-split on purpose: the list is recipe names.
    # shellcheck disable=SC2086
    just $jobs

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
