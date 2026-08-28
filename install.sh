#!/bin/sh
# install.sh — install the `q` binary on an arm64 macOS machine.
#
# Both of Ivan's machines (laptop + workstation) are arm64 macOS, so this
# fetches the latest release's aarch64-apple-darwin asset and drops it at
# ~/.local/bin/q. If no release/asset is available it builds from source with
# `cargo install --path .`. Re-runnable: it overwrites in place.
#
# POSIX sh; no bashisms.

set -eu

REPO="ilucin/quest"
ASSET="q-aarch64-apple-darwin"
BIN_DIR="${HOME}/.local/bin"
BIN_PATH="${BIN_DIR}/q"

usage() {
    cat <<EOF
install.sh — install the q binary (arm64 macOS)

Usage:
  sh install.sh [--help]

What it does:
  1. Downloads the latest ${REPO} release asset "${ASSET}"
     (via gh if available, otherwise curl) and installs it to
     ${BIN_PATH} (chmod +x).
  2. If no release or asset is found, falls back to building from source
     with 'cargo install --path .' (installs to ~/.cargo/bin/q).

The script is idempotent — run it again any time to update.
After install, make sure ${BIN_DIR} is on your PATH.
EOF
}

case "${1:-}" in
    -h | --help)
        usage
        exit 0
        ;;
    "") : ;;
    *)
        echo "install.sh: unknown argument '$1'" >&2
        usage >&2
        exit 2
        ;;
esac

# `command -v` is POSIX; use it instead of `which`.
have() { command -v "$1" >/dev/null 2>&1; }

install_binary() {
    # Downloads $ASSET to a temp file and moves it into place. Returns non-zero
    # (without aborting the script, thanks to the caller's `if`) when no asset
    # can be fetched, so main() can fall through to the source build.
    tmp="$(mktemp)"
    # mktemp's file is cleaned up on any exit from here on.
    trap 'rm -f "${tmp}"' EXIT INT TERM

    if have gh; then
        echo "Fetching latest release asset via gh…"
        if ! gh release download --repo "${REPO}" \
            --pattern "${ASSET}" --output "${tmp}" --clobber; then
            return 1
        fi
    elif have curl; then
        url="https://github.com/${REPO}/releases/latest/download/${ASSET}"
        echo "Fetching ${url} via curl…"
        # -f: fail on HTTP error (e.g. no release) rather than saving the error
        # page; -L: follow the redirect GitHub serves for /latest/download.
        if ! curl -fsSL "${url}" -o "${tmp}"; then
            return 1
        fi
    else
        echo "Neither gh nor curl found." >&2
        return 1
    fi

    # A missing asset can still leave an empty file behind; treat that as failure.
    if [ ! -s "${tmp}" ]; then
        return 1
    fi

    mkdir -p "${BIN_DIR}"
    chmod +x "${tmp}"
    mv "${tmp}" "${BIN_PATH}"
    trap - EXIT INT TERM
    return 0
}

install_from_source() {
    if ! have cargo; then
        echo "No release asset and cargo is not installed — cannot build from source." >&2
        echo "Install Rust (https://rustup.rs) or publish a release, then re-run." >&2
        exit 1
    fi
    # Run from the script's own directory so `--path .` finds Cargo.toml no
    # matter where the script was invoked from.
    script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
    echo "No release asset found — building from source with cargo…"
    cargo install --path "${script_dir}" --locked
    echo
    echo "Installed q to $(cargo_bin)/q via cargo."
    path_hint "$(cargo_bin)"
}

cargo_bin() {
    echo "${CARGO_HOME:-${HOME}/.cargo}/bin"
}

path_hint() {
    dir="$1"
    case ":${PATH}:" in
        *":${dir}:"*)
            : # already on PATH
            ;;
        *)
            echo
            echo "NOTE: ${dir} is not on your PATH. Add it, e.g.:"
            echo "  echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.zshrc"
            ;;
    esac
}

main() {
    if install_binary; then
        echo
        echo "Installed q to ${BIN_PATH}"
        "${BIN_PATH}" --version 2>/dev/null || true
        path_hint "${BIN_DIR}"
    else
        install_from_source
    fi
    echo
    echo "Tip: enable shell completions with, e.g.:"
    echo "  eval \"\$(q completions zsh)\""
}

main
