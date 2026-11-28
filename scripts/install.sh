#!/bin/sh
# Download and install the latest lvpm release for Linux x64.
#
# Resolves the release on GitHub, downloads the Linux x64 tarball over HTTPS,
# verifies its SHA-256, and installs lvpm with the VIs it ships beside it
# (relink-package.vi, build-app.vi): relink and build --app look for them
# beside lvpm, so they always go together.
#
# The download is verified against every reference available before
# anything is extracted:
#   - the SHA-256 digest GitHub records for the release asset,
#   - the release's <asset>.sha256, sha256.sum or SHA256SUMS file,
#   - a hash you pass with --sha256, from a source other than GitHub.
# If none of those is available the script stops. The only thing it runs
# from the download is `lvpm --version` at the end.
#
# Needs only curl, tar (with gzip), sed, awk and sha256sum (or shasum); no
# jq, no root. Mirrors scripts/install.ps1.
#
#   curl -fsSLO https://raw.githubusercontent.com/labview-package-manager/lvpm/main/scripts/install.sh
#   sh install.sh
#
# Options:
#   --version <tag>      release to install, e.g. v0.4.0 (default: the newest)
#   --stable             skip pre-releases when picking the newest
#   --install-dir <dir>  default: /usr/local/bin as root, else ~/.local/bin
#   --sha256 <hash>      expected SHA-256 of the tarball, obtained out of band
#   --no-path            do not add the install dir to PATH in ~/.profile
#   --repo <owner/name>  default: labview-package-manager/lvpm
#   --archive <file>     install a local release tarball (needs --sha256)
#   --build-local        cargo build --release --locked this checkout and
#                        install that, for testing; nothing is downloaded
#   -h, --help

set -eu

repo='labview-package-manager/lvpm'
repo_given=0
version=''
stable=0
install_dir=''
sha256=''
no_path=0
archive=''
build_local=0
# dist names the tarball by target. v0.3.1 and earlier upstream releases
# shipped the static musl build; the fork's releases up to v0.27.0 named it
# by version.
assets='lvpm-x86_64-unknown-linux-gnu.tar.gz lvpm-x86_64-unknown-linux-musl.tar.gz lvpm-*-linux-x64.tar.gz'
vis='relink-package.vi build-app.vi'

say() { printf '%s\n' "$*"; }
die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
usage() { sed -n '2,/^$/{s/^# \{0,1\}//;p}' "$0"; exit 0; }

while [ $# -gt 0 ]; do
    case "$1" in
        --version) version=${2:?--version needs a tag}; shift ;;
        --stable) stable=1 ;;
        --install-dir) install_dir=${2:?--install-dir needs a directory}; shift ;;
        --sha256) sha256=${2:?--sha256 needs a hash}; shift ;;
        --no-path) no_path=1 ;;
        --repo) repo=${2:?--repo needs owner/name}; repo_given=1; shift ;;
        --archive) archive=${2:?--archive needs a file}; shift ;;
        --build-local) build_local=1 ;;
        -h|--help) usage ;;
        *) die "unknown option $1 (see --help)" ;;
    esac
    shift
done

case "$sha256" in
    '') ;;
    *[!0-9a-fA-F]*) die "--sha256 must be 64 hex digits" ;;
    *) [ ${#sha256} -eq 64 ] || die "--sha256 must be 64 hex digits" ;;
esac
case "$repo" in
    */*) ;;
    *) die "--repo must be owner/name" ;;
esac
if [ -n "$archive" ] || [ "$build_local" -eq 1 ]; then
    [ -z "$version" ] && [ "$stable" -eq 0 ] && [ "$repo_given" -eq 0 ] ||
        die "--archive and --build-local cannot be combined with --version, --stable or --repo"
fi
if [ "$build_local" -eq 1 ] && [ -n "$archive$sha256" ]; then
    die "--build-local cannot be combined with --archive or --sha256"
fi
if [ -n "$archive" ] && [ -z "$sha256" ]; then die "--archive requires --sha256"; fi

[ "$(uname -s)" = Linux ] || die "this installs the Linux build; on Windows use scripts/install.ps1"
[ "$build_local" -eq 1 ] || [ "$(uname -m)" = x86_64 ] ||
    die "releases are built for x86_64 only, not $(uname -m); build from source (--build-local)"

if [ -z "$install_dir" ]; then
    if [ "$(id -u)" -eq 0 ]; then install_dir=/usr/local/bin; else install_dir="$HOME/.local/bin"; fi
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/lvpm-install.XXXXXX")
# The cleanup on any exit; an interrupt also ends the script, rather than
# going on without $work.
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# Put lvpm and the VIs beside it in directory $1 into $install_dir; READMEs
# and licenses stay out of a bin directory. Each lands under a temporary name
# and is renamed over the old one, so a running lvpm keeps its file and
# nothing ever sees half of one. lvpm goes last.
install_from() {
    [ -f "$1/lvpm" ] || die "no lvpm in $1"
    mkdir -p "$install_dir"
    for f in "$1"/*.vi; do
        [ -f "$f" ] || continue
        name=${f##*/}
        cp "$f" "$install_dir/.$name.$$" && chmod 644 "$install_dir/.$name.$$"
        mv -f "$install_dir/.$name.$$" "$install_dir/$name"
    done
    cp "$1/lvpm" "$install_dir/.lvpm.$$" && chmod 755 "$install_dir/.lvpm.$$"
    mv -f "$install_dir/.lvpm.$$" "$install_dir/lvpm"
    say "install: $install_dir"
}

finish() {
    case ":$PATH:" in
        *":$install_dir:"*) say "path:    $install_dir is on PATH" ;;
        *)
            line="export PATH=\"$install_dir:\$PATH\""
            if [ "$no_path" -eq 1 ]; then
                say "path:    $install_dir is not on PATH; add it, e.g. in ~/.profile:"
                say "         $line"
            elif grep -qxF "$line" "$HOME/.profile" 2>/dev/null; then
                say "path:    already in ~/.profile (new login shells will see it)"
            else
                printf '\n# lvpm\n%s\n' "$line" >> "$HOME/.profile"
                say "path:    added to ~/.profile (new login shells will see it)"
            fi ;;
    esac
    say ''
    # Assigned first: set -e does not see a failure inside an argument.
    installed=$("$install_dir/lvpm" --version) || die "the installed lvpm does not run: $install_dir/lvpm --version failed"
    say "$installed installed"
}

# ---------------------------------------------------------- build-local ----

if [ "$build_local" -eq 1 ]; then
    root=$(cd "$(dirname "$0")/.." && pwd)
    cargo build --release --locked --manifest-path "$root/Cargo.toml"
    target=$(cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" |
        sed 's/.*"target_directory":"\([^"]*\)".*/\1/')
    mkdir "$work/payload"
    cp "$target/release/lvpm" "$work/payload/"
    for v in $vis; do cp "$root/crates/lvpm-core/src/lv-src/$v" "$work/payload/"; done
    install_from "$work/payload"
    finish
    exit 0
fi

# ------------------------------------------------------ resolve release ----

api="https://api.github.com/repos/$repo"
# -L: GitHub answers a renamed repository with a redirect.
fetch() { curl -fsSL --proto '=https' --tlsv1.2 -H 'User-Agent: lvpm-install.sh' "$@"; }
# One JSON key per line, whatever GitHub's formatting, so sed and awk can read
# the few keys needed without jq.
api_get() {
    # Through a file, not a pipe, so a failed request fails the call.
    fetch -H 'Accept: application/vnd.github+json' -H 'X-GitHub-Api-Version: 2022-11-28' \
        -o "$work/api.json" "$api/$1" || return 1
    tr ',{}[]' '\n\n\n\n\n' < "$work/api.json"
}
value() { sed -n "s/^ *\"$1\": *\"\{0,1\}\([^\"]*\)\"\{0,1\} *$/\1/p"; }

# The newest release, from api_get's output on stdin, that is not a draft (a
# failed release run leaves one) and, with $1 = 1, not a pre-release.
# tag_name, draft and prerelease are release-level keys; assets carry none of
# them. ci/lint/install-sh-drafts.sh tests it.
select_release() {
    awk -v stable="$1" '
        function ok() { return !draft && !(stable && pre) }
        /^ *"tag_name":/ { if (tag != "" && ok()) { print tag; exit } ; tag = $0; sub(/^ *"tag_name": *"/, "", tag); sub(/".*/, "", tag); draft = pre = 0 }
        /^ *"draft":/ { draft = $0 ~ /true/ }
        /^ *"prerelease":/ { pre = $0 ~ /true/ }
        END { if (tag != "" && ok()) print tag }' | head -n 1
}

if [ -n "$archive" ]; then
    [ -f "$archive" ] || die "$archive is not a file"
    asset=${archive##*/}
    cp "$archive" "$work/$asset"
else
    if [ -z "$version" ]; then
        version=$(api_get 'releases?per_page=30' | select_release "$stable")
        [ -n "$version" ] || die "no release found in $repo$( [ "$stable" -eq 1 ] && printf ' (every release is a pre-release; drop --stable)')"
    fi
    tag=$version
    release=$(api_get "releases/tags/$tag") || die "no release $tag in $repo"
    names=$(printf '%s\n' "$release" | value name)
    asset=''
    set -f # $assets holds globs to match names with, not to expand here
    for pattern in $assets; do
        for name in $names; do
            # shellcheck disable=SC2254 # $pattern is a glob on purpose
            case "$name" in $pattern) asset=$name; break 2 ;; esac
        done
    done
    set +f
    [ -n "$asset" ] || die "release $tag has no Linux tarball (none of: $assets)"
    pre=$(printf '%s\n' "$release" | value prerelease | head -n 1)
    say "release: $tag$( [ "$pre" = true ] && printf ' (pre-release)')"
    say "asset:   $asset"
    fetch -o "$work/$asset" "https://github.com/$repo/releases/download/$tag/$asset"
fi

# --------------------------------------------------------------- verify ----

actual=$(sha256_of "$work/$asset")
checked=0
check() { # source, expected hash
    expected=$(printf '%s' "$2" | tr 'A-F' 'a-f')
    [ "$expected" = "$actual" ] ||
        die "SHA-256 mismatch against $1: expected $expected, got $actual. Nothing was installed."
    say "sha256:  ok ($1)"
    checked=$((checked + 1))
}

[ -n "$sha256" ] && check '--sha256' "$sha256"
if [ -z "$archive" ]; then
    # The digest GitHub records: it follows the asset's name in its entry.
    digest=$(printf '%s\n' "$release" | awk -v a="$asset" '
        /^ *"name":/ { mine = ($0 ~ "\"" a "\"") }
        mine && /^ *"digest": *"sha256:/ { sub(/.*sha256:/, ""); sub(/".*/, ""); print; exit }')
    [ -n "$digest" ] && check 'GitHub asset digest' "$digest"
    for sums in "$asset.sha256" sha256.sum SHA256SUMS; do
        if printf '%s\n' "$names" | grep -qxF "$sums" &&
            fetch -o "$work/$sums" "https://github.com/$repo/releases/download/$tag/$sums"; then
            hash=$(awk -v a="$asset" '{ n = $2; sub(/^\*/, "", n) } n == a { print $1; exit }' "$work/$sums")
            [ -n "$hash" ] && check "$sums" "$hash"
        fi
    done
fi
[ "$checked" -gt 0 ] ||
    die "nothing to verify $asset against: no digest and no checksum file. Pass --sha256 <hash> from a source you trust, or stop here."

# -------------------------------------------------------------- install ----

mkdir "$work/unpacked"
tar -xzf "$work/$asset" -C "$work/unpacked"
bin=$(find "$work/unpacked" -type f -name lvpm | head -n 1)
[ -n "$bin" ] || die "$asset contains no lvpm"
dir=$(dirname "$bin")
for v in $vis; do [ -f "$dir/$v" ] || say "warning: no $v beside lvpm in $asset"; done
install_from "$dir"
finish
