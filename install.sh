#!/bin/sh
# Install or upgrade the prebuilt `rings` binary from GitHub Releases.
#
# Usage:
#   curl -fsSL https://rings.rs/install.sh | sh
#   curl -fsSL https://rings.rs/install.sh | sh -s -- --version v0.39.0
#
# Environment overrides:
#   RINGS_VERSION            Release tag to install (e.g. v0.39.0); defaults to latest release.
#   RINGS_INSTALL_DIR        Destination directory; defaults to /usr/local/bin when writable,
#                            otherwise ~/.local/bin.
#   RINGS_RELEASE_BASE_URL   Base URL for release assets
#                            (default: https://github.com/RingsNetwork/rings/releases/download).
#   RINGS_LATEST_RELEASE_URL GitHub API URL returning the latest release JSON
#                            (default: https://api.github.com/repos/RingsNetwork/rings/releases/latest).
#   RINGS_OS                 Override `uname -s` for testing.
#   RINGS_ARCH               Override `uname -m` for testing.
#   RINGS_SYSTEM_BIN_DIR     Override the default system bin path (/usr/local/bin) for testing.

set -eu

REPO="RingsNetwork/rings"
DEFAULT_RELEASE_BASE_URL="https://github.com/${REPO}/releases/download"
DEFAULT_LATEST_RELEASE_URL="https://api.github.com/repos/${REPO}/releases/latest"

usage() {
    cat <<'EOF'
Install or upgrade the prebuilt `rings` node binary.

Usage:
  install.sh [OPTIONS]

Options:
  -v, --version <TAG>       Install a specific release tag (e.g. v0.39.0)
  -d, --install-dir <DIR>   Install `rings` into <DIR>
  -h, --help                Print this help message
EOF
}

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

normalize_version() {
    raw_version="$1"
    if [ -z "$raw_version" ]; then
        err "version tag must not be empty"
    fi
    case "$raw_version" in
        v* | prerelease)
            printf '%s\n' "$raw_version"
            ;;
        [0-9]*)
            printf 'v%s\n' "$raw_version"
            ;;
        *)
            printf '%s\n' "$raw_version"
            ;;
    esac
}

detect_target() {
    os_name="${RINGS_OS:-$(uname -s)}"
    arch_name="${RINGS_ARCH:-$(uname -m)}"

    case "$os_name" in
        Darwin)
            os_part="apple-darwin"
            ;;
        Linux)
            os_part="unknown-linux"
            ;;
        *)
            err "unsupported operating system '${os_name}' (supported: Darwin, Linux)"
            ;;
    esac

    case "$arch_name" in
        x86_64 | amd64)
            arch_part="x86_64"
            ;;
        arm64 | aarch64)
            arch_part="aarch64"
            ;;
        *)
            err "unsupported architecture '${arch_name}' on ${os_name} (supported: x86_64/amd64, arm64/aarch64)"
            ;;
    esac

    case "${os_part}:${arch_part}" in
        apple-darwin:x86_64)
            printf 'x86_64-apple-darwin\n'
            ;;
        apple-darwin:aarch64)
            printf 'aarch64-apple-darwin\n'
            ;;
        unknown-linux:x86_64)
            printf 'x86_64-unknown-linux-musl\n'
            ;;
        unknown-linux:aarch64)
            printf 'aarch64-unknown-linux-gnu\n'
            ;;
        *)
            err "unsupported target '${os_name}/${arch_name}'"
            ;;
    esac
}

download_to() {
    url="$1"
    dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$dest" "$url" || err "failed to download ${url}"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$dest" "$url" || err "failed to download ${url}"
    else
        err "neither 'curl' nor 'wget' was found in PATH"
    fi
}

compute_sha256() {
    file_path="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file_path" | awk '{print $1}' | tr 'A-F' 'a-f'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file_path" | awk '{print $1}' | tr 'A-F' 'a-f'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 -r "$file_path" | awk '{print $1}' | tr 'A-F' 'a-f'
    else
        err "no SHA-256 tool found (need 'sha256sum', 'shasum', or 'openssl')"
    fi
}

verify_sha256() {
    archive_path="$1"
    checksum_path="$2"

    expected="$(awk 'NF {print $1; exit}' "$checksum_path" | tr 'A-F' 'a-f')"
    case "$expected" in
        [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f])
            if [ "${#expected}" -ne 64 ]; then
                err "malformed SHA-256 digest in ${checksum_path}"
            fi
            ;;
        *)
            err "malformed SHA-256 digest in ${checksum_path}"
            ;;
    esac

    actual="$(compute_sha256 "$archive_path")"
    if [ "$actual" != "$expected" ]; then
        err "SHA-256 checksum mismatch for $(basename "$archive_path"): expected ${expected}, got ${actual}"
    fi
}

resolve_install_dir() {
    explicit_dir="$1"
    if [ -n "$explicit_dir" ]; then
        printf '%s\n' "$explicit_dir"
        return
    fi

    system_bin="${RINGS_SYSTEM_BIN_DIR:-/usr/local/bin}"
    if [ -d "$system_bin" ] && [ -w "$system_bin" ]; then
        printf '%s\n' "$system_bin"
        return
    fi

    if [ -z "${HOME:-}" ]; then
        err "HOME is not set and ${system_bin} is not writable; pass --install-dir <DIR>"
    fi
    printf '%s/.local/bin\n' "$HOME"
}

resolve_version() {
    pinned="$1"
    tmp_dir="$2"
    if [ -n "$pinned" ]; then
        normalize_version "$pinned"
        return
    fi

    latest_url="${RINGS_LATEST_RELEASE_URL:-$DEFAULT_LATEST_RELEASE_URL}"
    latest_json="${tmp_dir}/latest.json"
    download_to "$latest_url" "$latest_json"
    tag="$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$latest_json" | head -n 1)"
    if [ -z "$tag" ]; then
        err "could not determine latest release tag from ${latest_url}"
    fi
    normalize_version "$tag"
}

main() {
    version="${RINGS_VERSION:-}"
    install_dir="${RINGS_INSTALL_DIR:-}"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            -v | --version)
                if [ "$#" -lt 2 ]; then
                    err "missing value for $1"
                fi
                version="$2"
                shift 2
                ;;
            --version=*)
                version="${1#--version=}"
                shift
                ;;
            -d | --install-dir)
                if [ "$#" -lt 2 ]; then
                    err "missing value for $1"
                fi
                install_dir="$2"
                shift 2
                ;;
            --install-dir=*)
                install_dir="${1#--install-dir=}"
                shift
                ;;
            -h | --help)
                usage
                exit 0
                ;;
            *)
                err "unknown argument '$1' (run with --help for usage)"
                ;;
        esac
    done

    if ! command -v unzip >/dev/null 2>&1; then
        err "'unzip' is required to extract release archives"
    fi

    target="$(detect_target)"
    tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t rings-install)"
    trap 'rm -rf "$tmp_dir"' EXIT INT TERM

    resolved_version="$(resolve_version "$version" "$tmp_dir")"
    dest_dir="$(resolve_install_dir "$install_dir")"

    base_url="${RINGS_RELEASE_BASE_URL:-$DEFAULT_RELEASE_BASE_URL}"
    archive_name="rings-${resolved_version}-${target}.zip"
    archive_url="${base_url}/${resolved_version}/${archive_name}"
    checksum_url="${archive_url}.sha256"

    archive_path="${tmp_dir}/${archive_name}"
    checksum_path="${tmp_dir}/${archive_name}.sha256"

    printf 'Downloading %s (%s)...\n' "$archive_name" "$resolved_version"
    download_to "$archive_url" "$archive_path"
    download_to "$checksum_url" "$checksum_path"

    verify_sha256 "$archive_path" "$checksum_path"
    printf 'Verified SHA-256 checksum for %s\n' "$archive_name"

    extract_dir="${tmp_dir}/extract"
    mkdir -p "$extract_dir"
    unzip -q "$archive_path" -d "$extract_dir"

    extracted_bin="${extract_dir}/rings"
    if [ ! -f "$extracted_bin" ]; then
        err "archive ${archive_name} did not contain a 'rings' binary"
    fi

    mkdir -p "$dest_dir"
    if [ ! -w "$dest_dir" ]; then
        err "install directory '${dest_dir}' is not writable"
    fi

    staged_bin="${dest_dir}/.rings.tmp.$$"
    cp "$extracted_bin" "$staged_bin"
    chmod 0755 "$staged_bin"
    mv -f "$staged_bin" "${dest_dir}/rings"

    printf 'Installed rings %s (%s) to %s/rings\n' "$resolved_version" "$target" "$dest_dir"
    case ":${PATH:-}:" in
        *":${dest_dir}:"*) ;;
        *)
            printf 'Note: %s is not on your PATH. Add it with:\n  export PATH="%s:$PATH"\n' "$dest_dir" "$dest_dir"
            ;;
    esac
}

main "$@"
