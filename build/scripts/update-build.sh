#!/usr/bin/env bash

# Update the build synced from componentized/build to one of its releases.
#
#   build/scripts/update-build.sh [<version>]
#
# Downloads the release, removes the files synced from the previous version, listed in
# build/MANIFEST, copies in the files of the release, listed in its SYNC file, and records the
# version in build/VERSION and a checksum of each file in build/MANIFEST, checked by
# build/scripts/check-build.sh. The current version the repository keeps in a synced file, the
# default of the bump-version workflow, is carried forward. A tool pinned in tools/Cargo.toml to a version older than the
# release's build/tool-versions requires is raised to that version.
#
# <version>         the release to update to, e.g. v0.2.0, the latest release by default
# BUILD_REPOSITORY  the GitHub repository of the build, defaults to componentized/build
# BUILD_SOURCE      a checkout of the build repository to update from in place of a release, e.g. to
#                   try a change before releasing it, recorded as its commit in build/VERSION
# GH_TOKEN          a GitHub token for the API, optional, raises its rate limit

set -euo pipefail

# the update replaces this script, bash reads a script while it runs it, so the whole script is in a
# block bash reads before running any of it
{

cd "$(dirname "$0")/../.."

BUILD_REPOSITORY="${BUILD_REPOSITORY:-componentized/build}"
BUILD_SOURCE="${BUILD_SOURCE:-}"
version="${1:-}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

if [[ -n "$BUILD_SOURCE" ]]; then
    source=$(cd "$BUILD_SOURCE" && pwd)
    if [[ -z "$version" ]]; then
        version="$(git -C "$source" rev-parse --verify --quiet HEAD || echo local)"
        [[ -n "$(git -C "$source" status --porcelain 2> /dev/null)" ]] && version="${version}+dirty"
    fi
else
    auth=()
    [[ -n "${GH_TOKEN:-}" ]] && auth=(--header "Authorization: Bearer ${GH_TOKEN}")
    if [[ -z "$version" ]]; then
        version=$(curl -fsSL ${auth[@]+"${auth[@]}"} "https://api.github.com/repos/${BUILD_REPOSITORY}/releases/latest" | jq -r .tag_name)
    fi
    if [[ ! "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
        echo "the version '${version}' is not a release of ${BUILD_REPOSITORY}, e.g. v0.2.0" >&2
        exit 1
    fi
    source="$work/source"
    mkdir -p "$source"
    echo "downloading ${BUILD_REPOSITORY} ${version}"
    curl -fsSL "https://github.com/${BUILD_REPOSITORY}/archive/refs/tags/${version}.tar.gz" | tar -xz --strip-components 1 -C "$source"
fi

if [[ ! -f "$source/SYNC" ]]; then
    echo "unable to find the SYNC file of ${BUILD_REPOSITORY} ${version}" >&2
    exit 1
fi

# the line of a synced file the repository keeps current, the default version of the bump-version
# workflow, e.g. `default: "0.1.0-dev" # the current version, kept current by
# build/scripts/bump-version.sh`. Its value is the repository's own, left out of the checksum.
CURRENT_VERSION_RE='"[^"]*"( # the current version, kept current by build/scripts/bump-version\.sh)$'
# the checksum of a synced file, without the value the repository keeps current
checksum() {
    sed -E "s|${CURRENT_VERSION_RE}|\"\"\1|" "$1" | shasum -a 256 | cut -d ' ' -f 1
}

# the current version kept in a file, e.g. `0.1.0-dev`, also from a file of the build before it moved
# into build/, kept current by scripts/bump-version.sh
current_version() {
    [[ -f "$1" ]] && sed -nE 's|.*"([^"]*)" # the current version, kept current by (build/)?scripts/bump-version\.sh$|\1|p' "$1" | head -n 1
}

# the repository's version, the version of its interface package, e.g. `package componentized:http@0.1.0-dev;`
interface_version=$(sed -n 's/^package [^@]*@\(.*\);$/\1/p' wit/worlds.wit 2> /dev/null | head -n 1 || true)

# a path in the repository, relative and inside it, e.g. build/build.mk
check_path() {
    if [[ "$1" == /* || "/$1/" == */../* ]]; then
        echo "refusing to sync '$1', outside the repository" >&2
        exit 1
    fi
}

# the files of the release, each path in SYNC is a file or a directory of files
synced="$work/synced"
: > "$synced"
while read -r path; do
    [[ -z "$path" || "$path" == \#* ]] && continue
    check_path "$path"
    if [[ ! -e "$source/$path" ]]; then
        echo "unable to find ${path}, listed in the SYNC file of ${BUILD_REPOSITORY} ${version}" >&2
        exit 1
    fi
    # a checkout syncs only its tracked and unignored files
    if git -C "$source" rev-parse --is-inside-work-tree > /dev/null 2>&1; then
        git -C "$source" ls-files --cached --others --exclude-standard -- "$path" >> "$synced"
    else
        (cd "$source" && find "$path" -type f) >> "$synced"
    fi
done < "$source/SYNC"
sort -u -o "$synced" "$synced"

# the files synced from the previous version, a file the release no longer has is removed
if [[ -f build/MANIFEST ]]; then
    while read -r _ path; do
        check_path "$path"
        if ! grep -qxF "$path" "$synced"; then
            rm -f "$path"
            echo "removed ${path}"
        fi
    done < build/MANIFEST
fi

while read -r path; do
    mkdir -p "$(dirname "$path")"
    cp "$source/$path" "$path.tmp"
    # the repository's current version rather than the build's, or its interface version for a file
    # it didn't have yet
    current=$(current_version "$path" || true)
    current="${current:-$interface_version}"
    if [[ -n "$current" ]]; then
        sed -E "s|${CURRENT_VERSION_RE}|\"${current}\"\1|" "$path.tmp" > "$path.tmp.current"
        mv "$path.tmp.current" "$path.tmp"
    fi
    if cmp -s "$path.tmp" "$path"; then
        rm "$path.tmp"
    else
        # moved into place rather than overwritten, e.g. this script while it runs
        mv "$path.tmp" "$path"
        echo "updated ${path}"
    fi
    chmod "$( [[ -x "$source/$path" ]] && echo 755 || echo 644 )" "$path"
done < "$synced"

echo "$version" > build/VERSION
while read -r path; do
    echo "$(checksum "$path")  ${path}"
done < "$synced" > build/MANIFEST

# raise the tools pinned to a version older than the build requires, e.g. `wac-cli = "=0.11.0"`
while read -r tool required; do
    [[ -z "$tool" || "$tool" == \#* ]] && continue
    pinned=$(sed -n "s/^${tool} = \"=\(.*\)\"$/\1/p" tools/Cargo.toml 2> /dev/null || true)
    [[ -z "$pinned" ]] && continue
    if [[ "$pinned" != "$required" && "$(printf '%s\n%s\n' "$pinned" "$required" | sort -V | head -n 1)" == "$pinned" ]]; then
        perl -pi -e 's/^(\Q'"${tool}"'\E = "=)[^"]*(")/${1}'"${required}"'${2}/' tools/Cargo.toml
        echo "updated ${tool} from ${pinned} to ${required} in tools/Cargo.toml, required by the build"
    fi
done < build/tool-versions

echo "updated the build to ${BUILD_REPOSITORY} ${version}"
exit
}
