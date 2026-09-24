#!/bin/sh
# agent-sessions installer
# Usage: curl -fsSL https://raw.githubusercontent.com/gerbenoostra/spindle/main/install.sh | sh
#
# x-release-please-start-version
# Environment variables:
#   AGENT_SESSIONS_VERSION       - pin a specific release (e.g. v0.0.1)
#   AGENT_SESSIONS_INSTALL_DIR   - override install directory (default: $HOME/.local/bin)
#   AGENT_SESSIONS_SKIP_CHECKSUM - set to 1 to skip checksum verification (not recommended)
# x-release-please-end
#
# This script must be executed, not sourced:
#   sh install.sh          (correct)
#   curl -fsSL ... | sh    (correct)
#   source install.sh      (wrong: exits your shell on error)

set -e

REPO="gerbenoostra/spindle"
BIN="agent-sessions"
INSTALL_DIR="${AGENT_SESSIONS_INSTALL_DIR:-$HOME/.local/bin}"
BUILD_FROM_SOURCE_URL="https://github.com/${REPO}#install"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info() { printf "${BLUE}==>${NC} %s\n" "$1"; }
ok() { printf "${GREEN}==>${NC} %s\n" "$1"; }
warn() { printf "${YELLOW}==>${NC} %s\n" "$1"; }
error() {
    printf "${RED}Error:${NC} %s\n" "$1" >&2
    exit 1
}

# x-release-please-start-version
usage() {
    cat <<'EOF'
agent-sessions installer

Usage: curl -fsSL https://raw.githubusercontent.com/gerbenoostra/spindle/main/install.sh | sh

Environment variables:
  AGENT_SESSIONS_VERSION       pin a release (e.g. v0.0.1)
  AGENT_SESSIONS_INSTALL_DIR   install directory (default: $HOME/.local/bin)
  AGENT_SESSIONS_SKIP_CHECKSUM set to 1 to skip checksum verification
EOF
}
# x-release-please-end

fetch() {
    # fetch <url> <output-file>
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 3 --retry-connrefused --connect-timeout 10 --max-time 120 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget --tries=3 --timeout=120 -q -O "$2" "$1"
    else
        error "Neither curl nor wget found. Please install one of them."
    fi
}

detect_platform() {
    case "$(uname -s)" in
        Darwin) OS=darwin ;;
        Linux) OS=linux ;;
        *) error "Unsupported operating system: $(uname -s). Build from source: ${BUILD_FROM_SOURCE_URL}" ;;
    esac

    case "$(uname -m)" in
        x86_64 | amd64) ARCH=x86_64 ;;
        arm64 | aarch64) ARCH=aarch64 ;;
        *) error "Unsupported architecture: $(uname -m). Build from source: ${BUILD_FROM_SOURCE_URL}" ;;
    esac

    # Matches the target triples built by .github/workflows/release.yml.
    case "$OS" in
        darwin) TARGET="${ARCH}-apple-darwin" ;;
        linux)
            # The gnu-libc builds hard-code the glibc dynamic loader, which
            # musl (Alpine, Void, Chimera) and NixOS systems lack.
            case "$ARCH" in
                x86_64) loader=/lib64/ld-linux-x86-64.so.2 ;;
                aarch64) loader=/lib/ld-linux-aarch64.so.1 ;;
            esac
            [ -e "$loader" ] \
                || error "No glibc dynamic loader at ${loader} (musl or NixOS system?) - the prebuilt binary would not run. Build from source: ${BUILD_FROM_SOURCE_URL}"
            TARGET="${ARCH}-unknown-linux-gnu"
            ;;
    esac
}

latest_version() {
    VERSION=""
    version_error="Failed to determine the latest version (there may be no releases yet, or the GitHub API is rate-limited; set AGENT_SESSIONS_VERSION=vX.Y.Z to pin)"

    # Try the /releases/latest redirect first: it costs one request and does
    # not count against the GitHub API's anonymous rate limit.
    if command -v curl >/dev/null 2>&1; then
        VERSION=$(curl -fsSI "https://github.com/${REPO}/releases/latest" 2>/dev/null \
            | grep -i '^location:' \
            | head -n 1 \
            | sed -n -E 's#.*/tag/([^[:space:]]+).*#\1#p' \
            | tr -d '\r')
    fi

    if [ -z "$VERSION" ]; then
        if command -v curl >/dev/null 2>&1; then
            warn "Redirect lookup failed, falling back to the GitHub API..."
        fi
        api_json=$(mktemp)
        trap 'rm -f "$api_json"' EXIT
        fetch "https://api.github.com/repos/${REPO}/releases/latest" "$api_json" \
            || error "$version_error"
        VERSION=$(grep '"tag_name"' "$api_json" | head -n 1 \
            | sed -n -E 's/.*"tag_name": *"([^"]+)".*/\1/p')
        rm -f "$api_json"
        trap - EXIT
    fi

    [ -n "$VERSION" ] || error "$version_error"
}

install_from_release() {
    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT

    name="${BIN}-${VERSION}-${TARGET}"
    archive="${name}.tar.gz"
    checksum="${archive}.sha256"
    base_url="https://github.com/${REPO}/releases/download/${VERSION}"

    info "Downloading ${archive}..."
    fetch "${base_url}/${archive}" "${tmp_dir}/${archive}" \
        || error "Download failed. The release may not have a prebuilt binary for your platform (${TARGET})."

    if [ "${AGENT_SESSIONS_SKIP_CHECKSUM:-0}" = "1" ]; then
        warn "AGENT_SESSIONS_SKIP_CHECKSUM=1 set - skipping checksum verification (not recommended)"
    else
        info "Verifying checksum..."
        fetch "${base_url}/${checksum}" "${tmp_dir}/${checksum}" \
            || error "Failed to download ${checksum} - refusing to install an unverified binary (set AGENT_SESSIONS_SKIP_CHECKSUM=1 to bypass at your own risk)"

        if command -v sha256sum >/dev/null 2>&1; then
            (cd "$tmp_dir" && sha256sum -c "$checksum") >/dev/null \
                || error "Checksum verification failed - the download may be corrupted or tampered with"
        elif command -v shasum >/dev/null 2>&1; then
            (cd "$tmp_dir" && shasum -a 256 -c "$checksum") >/dev/null \
                || error "Checksum verification failed - the download may be corrupted or tampered with"
        else
            error "Neither sha256sum nor shasum found - refusing to install an unverified binary (set AGENT_SESSIONS_SKIP_CHECKSUM=1 to bypass at your own risk)"
        fi
    fi

    # Reject archive entries with an absolute path or a ".." component
    # before extracting (CWE-22 path traversal).
    info "Verifying archive contents..."
    tar -tzf "${tmp_dir}/${archive}" > "${tmp_dir}/listing" \
        || error "Could not list the archive contents - the download may be corrupt"
    if grep -qE '^/|(^|/)\.\.(/|$)' "${tmp_dir}/listing"; then
        error "Archive contains unsafe paths (absolute or directory traversal) - refusing to extract"
    fi

    info "Extracting..."
    tar -xzf "${tmp_dir}/${archive}" -C "$tmp_dir"

    mkdir -p "$INSTALL_DIR"
    tmp_binary="${INSTALL_DIR}/${BIN}.tmp.$$"
    cp "${tmp_dir}/${name}/${BIN}" "$tmp_binary"
    chmod 755 "$tmp_binary"
    mv -f "$tmp_binary" "${INSTALL_DIR}/${BIN}"

    # Remove the macOS quarantine attribute so Gatekeeper does not block a
    # freshly downloaded, unsigned binary on first run.
    if [ "$OS" = darwin ] && command -v xattr >/dev/null 2>&1; then
        xattr -d com.apple.quarantine "${INSTALL_DIR}/${BIN}" 2>/dev/null || true
    fi

    ok "${BIN} ${VERSION} installed to ${INSTALL_DIR}/${BIN}"
}

verify_installation() {
    [ -x "${INSTALL_DIR}/${BIN}" ] || error "${BIN} binary not found or not executable at ${INSTALL_DIR}/${BIN}"

    resolved=$(command -v "$BIN" 2>/dev/null || true)
    if [ -z "$resolved" ]; then
        warn "${INSTALL_DIR} does not appear to be on your PATH"
        echo ""
        echo "Add this to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
        echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
        echo ""
    elif [ "$resolved" != "${INSTALL_DIR}/${BIN}" ]; then
        warn "${resolved} is ahead on your PATH and will run instead of ${INSTALL_DIR}/${BIN}"
        echo ""
    fi

    ok "${BIN} is installed and ready!"
    echo ""
    echo "Documentation: https://github.com/${REPO}"
    echo ""
}

main() {
    case "${1:-}" in
        "") ;;
        -h | --help) usage; exit 0 ;;
        *) error "Unknown argument: $1 (run with --help for usage)" ;;
    esac

    if [ -z "${HOME:-}" ] && [ -z "${AGENT_SESSIONS_INSTALL_DIR:-}" ]; then
        error "HOME is not set - set AGENT_SESSIONS_INSTALL_DIR to choose an install directory"
    fi

    echo ""
    echo "${BIN} installer"
    echo ""

    detect_platform
    info "Platform: ${OS}/${ARCH} (${TARGET})"

    if [ -n "${AGENT_SESSIONS_VERSION:-}" ]; then
        VERSION="$AGENT_SESSIONS_VERSION"
        case "$VERSION" in
            v*) ;;
            *)
                VERSION="v${VERSION}"
                info "Releases are tagged vX.Y.Z - using ${VERSION}"
                ;;
        esac
        info "Using pinned version: ${VERSION}"
    else
        info "Fetching the latest release..."
        latest_version
    fi
    info "Installing version: ${VERSION}"

    install_from_release
    verify_installation
}

main "$@"
