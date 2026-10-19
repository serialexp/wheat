#!/bin/sh
# Installs the `wheat` and `wheat-server` binaries from a GitHub Release.
#
#   curl -fsSL https://raw.githubusercontent.com/serialexp/wheat/main/install.sh | sh
#
# Settings, all optional:
#   WHEAT_VERSION        release tag to install, e.g. v0.2.0 (default: the latest)
#   WHEAT_INSTALL_DIR    where the binaries go (default: ~/.local/bin)
#   WHEAT_REPO           GitHub repository (default: serialexp/wheat)
#   WHEAT_DOWNLOAD_BASE  fetch archives from this URL instead of GitHub
#                        (expects <base>/<archive> and <base>/SHA256SUMS)
#   GH_TOKEN, GITHUB_TOKEN  token for a private repository; otherwise a logged-in
#                        `gh` is used when present, else an anonymous download
#
# The wheat CLI runs only on Linux (x86_64 and aarch64, glibc 2.35 or newer).

set -eu

repo=${WHEAT_REPO:-serialexp/wheat}
version=${WHEAT_VERSION:-latest}
install_dir=${WHEAT_INSTALL_DIR:-$HOME/.local/bin}
token=${GH_TOKEN:-${GITHUB_TOKEN:-}}

say() { printf 'wheat-install: %s\n' "$*" >&2; }
fail() { say "error: $*"; exit 1; }
have() { command -v "$1" > /dev/null 2>&1; }

case $(uname -s) in
    Linux) ;;
    *) fail "the wheat CLI runs only on Linux for now (this is $(uname -s))" ;;
esac
case $(uname -m) in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) fail "no wheat build for $(uname -m); build from source with \`just install\`" ;;
esac
glibc=$(getconf GNU_LIBC_VERSION 2> /dev/null || true)
case $glibc in
    "glibc "*) ;;
    *) fail "the release binaries need glibc; build from source with \`just install\`" ;;
esac
glibc_minor=$(printf '%s\n' "${glibc#glibc }" | cut -d. -f2)
if [ "${glibc#glibc 2.}" = "$glibc" ] || [ "$glibc_minor" -lt 35 ]; then
    fail "the release binaries need glibc 2.35 or newer (this is ${glibc#glibc }); build from source with \`just install\`"
fi

target=$arch-unknown-linux-gnu
archive=wheat-$target.tar.gz

if have curl; then
    fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
    fetch_api() { curl -fsSL --retry 3 -H "Authorization: Bearer $token" -H "Accept: $3" -o "$2" "$1"; }
elif have wget; then
    fetch() { wget -q -O "$2" "$1"; }
    fetch_api() { wget -q --header="Authorization: Bearer $token" --header="Accept: $3" -O "$2" "$1"; }
else
    fail "needs curl or wget"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

# The API URL of release asset $1, from the release JSON in $2. GitHub lists each
# asset's "url" before its "name". Splitting at JSON punctuation puts every field on a
# line of its own, however the JSON is formatted.
asset_url() {
    tr ',{}[]' '\n' < "$2" | awk -v name="$1" '
        {
            line = $0
            gsub(/^[ \t]+|[ \t]+$/, "", line)
            gsub(/": +/, "\":", line)
        }
        line ~ /^"url":"https:\/\/api\.github\.com\/repos\/.*\/releases\/assets\/[0-9]+"$/ {
            url = line
            sub(/^"url":"/, "", url)
            sub(/"$/, "", url)
        }
        line == "\"name\":\"" name "\"" { print url; exit }
    '
}

download() {
    if [ -n "${WHEAT_DOWNLOAD_BASE:-}" ]; then
        say "downloading $archive from $WHEAT_DOWNLOAD_BASE"
        fetch "$WHEAT_DOWNLOAD_BASE/$archive" "$tmp/$archive" ||
            fail "could not download $WHEAT_DOWNLOAD_BASE/$archive"
        fetch "$WHEAT_DOWNLOAD_BASE/SHA256SUMS" "$tmp/SHA256SUMS" ||
            fail "could not download $WHEAT_DOWNLOAD_BASE/SHA256SUMS"
    elif [ -n "$token" ]; then
        if [ "$version" = latest ]; then
            release_url=https://api.github.com/repos/$repo/releases/latest
        else
            release_url=https://api.github.com/repos/$repo/releases/tags/$version
        fi
        say "downloading $archive ($version) from $repo with a token"
        fetch_api "$release_url" "$tmp/release.json" application/vnd.github+json ||
            fail "no release $version in $repo, or the token cannot read it"
        for file in "$archive" SHA256SUMS; do
            url=$(asset_url "$file" "$tmp/release.json")
            [ -n "$url" ] || fail "release $version has no $file"
            fetch_api "$url" "$tmp/$file" application/octet-stream
        done
    elif have gh && gh auth status > /dev/null 2>&1; then
        say "downloading $archive ($version) from $repo with gh"
        if [ "$version" = latest ]; then
            set --
        else
            set -- "$version"
        fi
        gh release download "$@" --repo "$repo" --dir "$tmp" \
            --pattern "$archive" --pattern SHA256SUMS ||
            fail "gh could not download release $version of $repo"
    else
        if [ "$version" = latest ]; then
            base=https://github.com/$repo/releases/latest/download
        else
            base=https://github.com/$repo/releases/download/$version
        fi
        say "downloading $archive ($version) from $repo"
        fetch "$base/$archive" "$tmp/$archive" ||
            fail "download failed; for a private repository set GH_TOKEN or log in with \`gh auth login\`"
        fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "could not download SHA256SUMS"
    fi
}

download

expected=$(awk -v name="$archive" '$2 == name || $2 == "*" name { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "SHA256SUMS does not list $archive"
if have sha256sum; then
    actual=$(sha256sum "$tmp/$archive" | cut -d' ' -f1)
elif have shasum; then
    actual=$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)
else
    fail "needs sha256sum or shasum to check the download"
fi
[ "$actual" = "$expected" ] || fail "checksum mismatch for $archive"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$install_dir"
for binary in wheat wheat-server; do
    # Copy beside the destination, then rename over it, so a running binary is never
    # half written.
    cp "$tmp/wheat-$target/$binary" "$install_dir/.$binary.new"
    chmod 0755 "$install_dir/.$binary.new"
    mv -f "$install_dir/.$binary.new" "$install_dir/$binary"
done

say "installed $("$install_dir/wheat" --version) to $install_dir"
case :$PATH: in
    *:"$install_dir":*) ;;
    *) say "$install_dir is not on your PATH; add it, e.g. export PATH=\"$install_dir:\$PATH\"" ;;
esac
