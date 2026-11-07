#!/bin/sh
# Deterministic offline test suite for `install.sh` and `contrib/` service definitions.

set -eu

REPO_ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
INSTALL_SH="${REPO_ROOT}/install.sh"

fail() {
    printf 'test-install.sh: FAIL: %s\n' "$*" >&2
    exit 1
}

compute_sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        openssl dgst -sha256 -r "$1" | awk '{print $1}'
    fi
}

make_fixture_archive() {
    releases_dir="$1"
    version="$2"
    target="$3"

    ver_dir="${releases_dir}/${version}"
    build_dir="${releases_dir}/build-${version}-${target}"
    mkdir -p "$ver_dir" "$build_dir"

    cat >"${build_dir}/rings" <<EOF
#!/bin/sh
printf 'rings %s (%s)\n' "${version}" "${target}"
EOF
    chmod 0755 "${build_dir}/rings"

    archive="${ver_dir}/rings-${version}-${target}.zip"
    (cd "$build_dir" && zip -q -j "$archive" ./rings)
    digest="$(compute_sha256 "$archive")"
    printf '%s  rings-%s-%s.zip\n' "$digest" "$version" "$target" >"${archive}.sha256"
}

tmp_root="$(mktemp -d 2>/dev/null || mktemp -d -t rings-install-test)"
trap 'rm -rf "$tmp_root"' EXIT INT TERM

releases_dir="${tmp_root}/releases"
mkdir -p "$releases_dir"

for target in \
    aarch64-apple-darwin \
    x86_64-apple-darwin \
    x86_64-unknown-linux-musl \
    aarch64-unknown-linux-gnu
do
    make_fixture_archive "$releases_dir" "v0.37.0" "$target"
    make_fixture_archive "$releases_dir" "v0.38.1" "$target"
done

latest_json="${tmp_root}/latest.json"
cat >"$latest_json" <<'EOF'
{
  "tag_name": "v0.38.1",
  "name": "v0.38.1"
}
EOF

BASE_URL="file://${releases_dir}"
LATEST_URL="file://${latest_json}"

# 1. Help and invalid arguments
help_out="$(sh "$INSTALL_SH" --help)"
case "$help_out" in
    *"Usage:"*) ;;
    *) fail "--help output did not include Usage:" ;;
esac

if sh "$INSTALL_SH" --unknown-flag >/dev/null 2>&1; then
    fail "expected unknown flag to fail"
fi

if sh "$INSTALL_SH" --version >/dev/null 2>&1; then
    fail "expected missing --version value to fail"
fi

if sh "$INSTALL_SH" --install-dir >/dev/null 2>&1; then
    fail "expected missing --install-dir value to fail"
fi

# 2. Unsupported OS and architecture fail with clear messages
if err_out="$(RINGS_OS=FreeBSD RINGS_ARCH=x86_64 sh "$INSTALL_SH" 2>&1)"; then
    fail "expected unsupported OS FreeBSD to fail"
fi
case "$err_out" in
    *"unsupported operating system 'FreeBSD'"*) ;;
    *) fail "unexpected error for unsupported OS: ${err_out}" ;;
esac

if err_out="$(RINGS_OS=Linux RINGS_ARCH=riscv64 sh "$INSTALL_SH" 2>&1)"; then
    fail "expected unsupported arch riscv64 to fail"
fi
case "$err_out" in
    *"unsupported architecture 'riscv64'"*) ;;
    *) fail "unexpected error for unsupported arch: ${err_out}" ;;
esac

# 3. OS/arch matrix + default ~/.local/bin fallback when system bin is not writable
fake_home="${tmp_root}/home"
missing_sys_bin="${tmp_root}/nonexistent-system-bin"
mkdir -p "$fake_home"

for pair in \
    "Darwin:arm64:aarch64-apple-darwin" \
    "Darwin:x86_64:x86_64-apple-darwin" \
    "Linux:amd64:x86_64-unknown-linux-musl" \
    "Linux:aarch64:aarch64-unknown-linux-gnu"
do
    os_name="${pair%%:*}"
    rest="${pair#*:}"
    arch_name="${rest%%:*}"
    expected_target="${rest#*:}"

    rm -rf "${fake_home}/.local/bin"
    HOME="$fake_home" \
        RINGS_SYSTEM_BIN_DIR="$missing_sys_bin" \
        RINGS_OS="$os_name" \
        RINGS_ARCH="$arch_name" \
        RINGS_RELEASE_BASE_URL="$BASE_URL" \
        RINGS_LATEST_RELEASE_URL="$LATEST_URL" \
        sh "$INSTALL_SH" >/dev/null

    installed_bin="${fake_home}/.local/bin/rings"
    [ -x "$installed_bin" ] || fail "expected executable ${installed_bin} for ${pair}"
    version_out="$("$installed_bin")"
    [ "$version_out" = "rings v0.38.1 (${expected_target})" ] || \
        fail "unexpected binary output '${version_out}' for ${pair}"
done

# 4. Writable system bin directory is preferred by default
writable_sys_bin="${tmp_root}/usr-local-bin"
mkdir -p "$writable_sys_bin"
HOME="$fake_home" \
    RINGS_SYSTEM_BIN_DIR="$writable_sys_bin" \
    RINGS_OS=Darwin \
    RINGS_ARCH=aarch64 \
    RINGS_RELEASE_BASE_URL="$BASE_URL" \
    RINGS_LATEST_RELEASE_URL="$LATEST_URL" \
    sh "$INSTALL_SH" >/dev/null
[ -x "${writable_sys_bin}/rings" ] || fail "expected install into writable system bin dir"

# 5. Pinned version via --version (with and without leading 'v') and idempotent upgrade in place
custom_dir="${tmp_root}/custom-bin"
RINGS_OS=Linux \
    RINGS_ARCH=x86_64 \
    RINGS_RELEASE_BASE_URL="$BASE_URL" \
    sh "$INSTALL_SH" --version 0.37.0 --install-dir "$custom_dir" >/dev/null

[ "$("${custom_dir}/rings")" = "rings v0.37.0 (x86_64-unknown-linux-musl)" ] || \
    fail "expected pinned v0.37.0 binary"

RINGS_OS=Linux \
    RINGS_ARCH=x86_64 \
    RINGS_RELEASE_BASE_URL="$BASE_URL" \
    RINGS_LATEST_RELEASE_URL="$LATEST_URL" \
    sh "$INSTALL_SH" --install-dir="$custom_dir" >/dev/null

[ "$("${custom_dir}/rings")" = "rings v0.38.1 (x86_64-unknown-linux-musl)" ] || \
    fail "expected idempotent upgrade in place to v0.38.1"

# 6. Checksum mismatch and malformed checksum are rejected without modifying target
bad_releases="${tmp_root}/bad-releases"
cp -R "$releases_dir" "$bad_releases"
printf '0000000000000000000000000000000000000000000000000000000000000000  rings-v0.37.0-x86_64-unknown-linux-musl.zip\n' \
    >"${bad_releases}/v0.37.0/rings-v0.37.0-x86_64-unknown-linux-musl.zip.sha256"

if err_out="$(
    RINGS_OS=Linux \
        RINGS_ARCH=x86_64 \
        RINGS_RELEASE_BASE_URL="file://${bad_releases}" \
        sh "$INSTALL_SH" --version v0.37.0 --install-dir "$custom_dir" 2>&1
)"; then
    fail "expected corrupted SHA-256 checksum to fail"
fi
case "$err_out" in
    *"SHA-256 checksum mismatch"*) ;;
    *) fail "expected checksum mismatch error, got: ${err_out}" ;;
esac
[ "$("${custom_dir}/rings")" = "rings v0.38.1 (x86_64-unknown-linux-musl)" ] || \
    fail "failed checksum install must leave existing binary untouched"

printf 'not-a-valid-sha256\n' \
    >"${bad_releases}/v0.37.0/rings-v0.37.0-x86_64-unknown-linux-musl.zip.sha256"
if err_out="$(
    RINGS_OS=Linux \
        RINGS_ARCH=x86_64 \
        RINGS_RELEASE_BASE_URL="file://${bad_releases}" \
        sh "$INSTALL_SH" --version v0.37.0 --install-dir "$custom_dir" 2>&1
)"; then
    fail "expected malformed SHA-256 digest to fail"
fi
case "$err_out" in
    *"malformed SHA-256 digest"*) ;;
    *) fail "expected malformed digest error, got: ${err_out}" ;;
esac

# 7. Verify contrib/ service definitions and documentation
systemd_unit="${REPO_ROOT}/contrib/systemd/rings.service"
launchd_plist="${REPO_ROOT}/contrib/launchd/rs.rings.node.plist"
contrib_readme="${REPO_ROOT}/contrib/README.md"

[ -f "$systemd_unit" ] || fail "missing ${systemd_unit}"
[ -f "$launchd_plist" ] || fail "missing ${launchd_plist}"
[ -f "$contrib_readme" ] || fail "missing ${contrib_readme}"

grep -q '^\[Unit\]' "$systemd_unit" || fail "systemd unit missing [Unit]"
grep -q '^\[Service\]' "$systemd_unit" || fail "systemd unit missing [Service]"
grep -q '^\[Install\]' "$systemd_unit" || fail "systemd unit missing [Install]"
grep -q '^ExecStart=.*rings run' "$systemd_unit" || fail "systemd unit missing ExecStart"

grep -q '<string>rs.rings.node</string>' "$launchd_plist" || fail "launchd plist missing Label"
if command -v plutil >/dev/null 2>&1; then
    plutil -lint "$launchd_plist" >/dev/null || fail "plutil -lint failed on ${launchd_plist}"
fi

grep -q 'systemctl --user enable --now rings.service' "$contrib_readme" || \
    fail "contrib/README.md missing systemd enable command"
grep -q 'systemctl --user disable --now rings.service' "$contrib_readme" || \
    fail "contrib/README.md missing systemd disable command"
grep -q 'launchctl bootstrap' "$contrib_readme" || \
    fail "contrib/README.md missing launchctl bootstrap command"
grep -q 'launchctl bootout' "$contrib_readme" || \
    fail "contrib/README.md missing launchctl bootout command"

printf 'test-install.sh: ok\n'
