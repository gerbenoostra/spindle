# The Linux side of `just ci`: an ubuntu-latest runner reduced to what ci.yml
# installs. Built and run by `just ci-linux`; CI itself does not use it.
FROM ubuntu:24.04

# CI installs the latest of everything on every run. A new value, which
# `just ci-linux` sets to the ISO week, rebuilds every layer below, so the
# image trails CI by a week at most.
ARG IMAGE_WEEK

# Everything the jobs install with apt, plus what a GitHub runner ships with.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl git jq procps shellcheck \
        tmux xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Determinate Nix, as nix-installer-action installs it, without a daemon: a
# container has no init. The store is handed to the runner user below so the
# jobs run unprivileged, as they do on GitHub. Without the privileges the
# sandbox needs, Nix quietly builds unsandboxed; failing instead keeps a
# missing build input from passing here and failing in CI.
RUN curl -fsSL https://install.determinate.systems/nix \
        | sh -s -- install linux --init none --no-confirm \
    && echo 'sandbox-fallback = false' >> /etc/nix/nix.custom.conf

# Not root: a root runner reads files a test made unreadable.
RUN useradd --create-home --shell /bin/bash runner \
    && chown -R runner /nix
USER runner
WORKDIR /home/runner
ENV PATH=/home/runner/.cargo/bin:/nix/var/nix/profiles/default/bin:$PATH

# The stable toolchain the jobs install, and the MSRV the msrv job builds with.
ARG MSRV
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path \
        --profile minimal --default-toolchain stable \
        --component clippy,rustfmt,llvm-tools-preview \
    && rustup toolchain install "$MSRV" --profile minimal

# The latest just, as setup-just installs it; Ubuntu's lags behind.
RUN curl -fsSL https://just.systems/install.sh | bash -s -- --to /home/runner/.cargo/bin

# The cargo-llvm-cov release the coverage job pins.
ARG LLVM_COV_VERSION
RUN curl -fsSL "https://github.com/taiki-e/cargo-llvm-cov/releases/download/v${LLVM_COV_VERSION}/cargo-llvm-cov-$(uname -m)-unknown-linux-gnu.tar.gz" \
        | tar -xzf - -C /home/runner/.cargo/bin

# Mount points for the volumes `just ci-linux` keeps between runs, created
# here so a fresh volume inherits the runner's ownership.
RUN mkdir -p /home/runner/ci /home/runner/.cargo/registry
