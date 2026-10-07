#!/usr/bin/env bash

# Bump the version of the wit interface package.
#
#   scripts/bump-version.sh <new-version>
#
# Updates the package declaration and every reference to the package in tracked files, then
# refreshes the generated wit dependencies. Items whose `@since` names an unreleased (prerelease)
# version move to the new version, since they were never published under the old one. Items
# released under the old version keep their `@since`.
#
#   0.1.0-dev -> 0.1.0      releases 0.1.0, `@since(version = 0.1.0-dev)` becomes 0.1.0
#   0.1.0     -> 0.2.0-dev  starts 0.2.0, `@since(version = 0.1.0)` is unchanged

set -euo pipefail

cd "$(dirname "$0")/.."

PACKAGE="${PACKAGE:-componentized:$(basename $(git rev-parse --show-toplevel))}"
SEMVER='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'

new="${1:-}"
if [[ ! "$new" =~ $SEMVER ]]; then
    echo "usage: $0 <new-version>, e.g. 0.1.0 or 0.2.0-dev" >&2
    exit 1
fi

old=$(sed -n "s/^package ${PACKAGE}@\(.*\);$/\1/p" wit/worlds.wit)
if [[ -z "$old" ]]; then
    echo "unable to find the ${PACKAGE} package declaration in wit/worlds.wit" >&2
    exit 1
fi

# succeeds when version $1 is lower than version $2, a prerelease is lower than its release
version_lt() {
    local a_core="${1%%-*}" b_core="${2%%-*}"
    local a_pre="" b_pre=""
    [[ "$1" == *-* ]] && a_pre="${1#*-}"
    [[ "$2" == *-* ]] && b_pre="${2#*-}"
    if [[ "$a_core" != "$b_core" ]]; then
        local IFS=.
        local -a a=($a_core) b=($b_core)
        for i in 0 1 2; do
            (( a[i] < b[i] )) && return 0
            (( a[i] > b[i] )) && return 1
        done
    fi
    [[ -n "$a_pre" && -z "$b_pre" ]] && return 0
    [[ -z "$a_pre" && -n "$b_pre" ]] && return 1
    [[ -n "$a_pre" && "$a_pre" < "$b_pre" ]]
}

if ! version_lt "$old" "$new"; then
    echo "the new version ${new} must be greater than the current version ${old}" >&2
    exit 1
fi

# the bump-version workflow offers the current version as the default for the next bump, checked
# before changing anything
workflow=.github/workflows/bump-version.yaml
workflow_default="default: \"${old}\" # the current version, kept current by scripts/bump-version.sh"
if ! grep -qF "$workflow_default" "$workflow"; then
    echo "unable to find the current version as the default in ${workflow}, expected: ${workflow_default}" >&2
    exit 1
fi

old_re="${old//./\\.}"
# references to the package or one of its interfaces, an interface named for a keyword is escaped
# with `%`, e.g. `componentized:oci/client@0.1.0`
ref_re="${PACKAGE}(/%?[a-z0-9-]+)?"
# the fetched wit dependencies and the wkg.lock files are left to `make wit`, wkg replaces the
# dependencies and updates the locks for the new version
files=$(git grep --untracked -l -E "${ref_re}@${old_re}" -- ':(exclude,glob)**/wit/deps/**' ':(exclude,glob)**/wkg.lock' || true)
for file in $files; do
    sed -i.bak -E "s#(${ref_re})@${old_re}#\1@${new}#g" "$file"
    rm "$file.bak"
    echo "updated ${file}"
done

if [[ "$old" == *-* ]]; then
    files=$(git grep --untracked -l -F "@since(version = ${old})" -- 'wit/*.wit' || true)
    for file in $files; do
        sed -i.bak "s/@since(version = ${old_re})/@since(version = ${new})/g" "$file"
        rm "$file.bak"
        echo "updated @since in ${file}"
    done
fi

perl -pi -e 's{^(\s+)\Q'"${workflow_default}"'\E$}{${1}'"${workflow_default/\"${old}\"/\"${new}\"}"'}' "$workflow"
echo "updated the default version in ${workflow}"

# regenerate the wit dependencies for the new version
make wit components test

echo "bumped ${PACKAGE} from ${old} to ${new}"
