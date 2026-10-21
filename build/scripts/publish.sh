#!/usr/bin/env bash

# Publish a built component to an OCI registry, and sign it with cosign.
#
#   VERSION=<version> REPOSITORY=<repository> build/scripts/publish.sh <file>
#
# The file is one of the files built into the components directory, components are in a directory
# of their own, the interface is not, e.g. gate.wasm from target/components/gate/gate.wasm and
# interface.wasm from target/components/interface.wasm. The interface is published to the
# repository, each component to a repository of its own under it, a debug build is tagged with a
# `_debug` suffix. A debug build identical to the release build adds nothing, e.g. components
# without debug info, and is not published.
#
#   interface.wasm   -> ${REPOSITORY}:0.1.0
#   gate.wasm        -> ${REPOSITORY}/gate:0.1.0
#   gate.debug.wasm  -> ${REPOSITORY}/gate:0.1.0_debug
#
# VERSION            the version to publish, a leading `v` is dropped from the tag, e.g. v0.1.0
# REPOSITORY         the repository to publish to, e.g. ghcr.io/componentized/http
# GITHUB_REPOSITORY  the GitHub repository the components are built from, e.g. componentized/http
# COMPONENTS_DIR     the directory the components are built into, defaults to target/components
# SIGN               sign the published components with cosign, `false` to push without signing,
#                    e.g. to a local registry
# PUBLISH_LOG        append the published file, its size in bytes and its image to this file, e.g.
#                    `gate.wasm 1234567 ghcr.io/componentized/http/gate:0.1.0@sha256:...`
# DRY_RUN            `true` to show what would be published without pushing or signing anything,
#                    the image is logged with a `dry-run` digest

set -euo pipefail

cd "$(dirname "$0")/../.."

file="${1:-}"
if [[ -z "$file" ]]; then
    echo "usage: $0 <file>, e.g. interface.wasm or gate.wasm" >&2
    exit 1
fi
: "${VERSION:?VERSION is undefined}"
: "${REPOSITORY:?REPOSITORY is undefined}"
GITHUB_REPOSITORY="${GITHUB_REPOSITORY:-componentized/$(basename "$(git rev-parse --show-toplevel)")}"
COMPONENTS_DIR="${COMPONENTS_DIR:-target/components}"
SIGN="${SIGN:-true}"
PUBLISH_LOG="${PUBLISH_LOG:-}"
DRY_RUN="${DRY_RUN:-false}"

component="${file%.wasm}"
component="${component%.debug}"
debug=""
[[ "$file" == *.debug.wasm ]] && debug=true

if [[ "$file" == interface.wasm ]]; then
    component_file="$file"
    title="${GITHUB_REPOSITORY/\//:}"
else
    component_file="${component}/${file}"
    title="${GITHUB_REPOSITORY/\//:}-${component}"
fi
[[ -n "$debug" ]] && title="${title} (debug)"

if [[ -n "$debug" ]] && cmp -s "${COMPONENTS_DIR}/${component_file}" "${COMPONENTS_DIR}/${component}/${component}.wasm"; then
    echo "Not publishing ${file} as it is identical to ${component}.wasm"
    exit 0
fi

# the description is the line following the title in the readme
readme="${COMPONENTS_DIR}/$(dirname "$component_file")/README.md"
description=$(sed -n 3p "$readme")

commit=$(git rev-parse HEAD)
revision="$commit"
git diff --quiet HEAD || revision="${commit}+dirty"
url="https://github.com/${GITHUB_REPOSITORY}/tree/${commit}"
[[ -f "components/${component}/README.md" ]] && url="${url}/components/${component}"

component_version="$VERSION"
[[ -n "$debug" ]] && component_version="${VERSION}+debug"
tag="${component_version#v}"
tag="${tag//+/_}"

if [[ "$file" == interface.wasm ]]; then
    image="${REPOSITORY}:${tag}"
else
    image="${REPOSITORY}/${component}:${tag}"
fi

echo "::group::${file} -> ${image}"
# close the group when the push or signing fails, so the error isn't hidden in it
trap 'echo "::endgroup::"' EXIT

annotations=(
    --annotation="org.opencontainers.image.title=${title}"
    --annotation="org.opencontainers.image.description=${description}"
    --annotation="org.opencontainers.image.version=${component_version}"
    --annotation="org.opencontainers.image.url=${url}"
    --annotation="org.opencontainers.image.source=https://github.com/${GITHUB_REPOSITORY}.git"
    --annotation="org.opencontainers.image.revision=${revision}"
    --annotation="org.opencontainers.image.licenses=Apache-2.0"
)

if [[ "$DRY_RUN" == true ]]; then
    echo "Not pushing ${COMPONENTS_DIR}/${component_file} to ${image}, DRY_RUN=${DRY_RUN}"
    printf '  %s\n' "${annotations[@]}"
    digest=dry-run
else
    digest=$(
        wkg oci push \
            "${annotations[@]}" \
            "$image" \
            "${COMPONENTS_DIR}/${component_file}" \
            2>&1 \
            | tee /dev/stderr \
            | grep -o 'sha256:[a-f0-9]\{64\}'
    )
fi

if [[ -n "$PUBLISH_LOG" ]]; then
    size=$(wc -c < "${COMPONENTS_DIR}/${component_file}" | tr -d ' ')
    echo "${file} ${size} ${image}@${digest}" >> "$PUBLISH_LOG"
fi

if [[ "$DRY_RUN" == true ]]; then
    echo "Not signing ${image}@${digest}, DRY_RUN=${DRY_RUN}"
elif [[ "$SIGN" == true ]]; then
    cosign sign --yes "${image}@${digest}"
else
    echo "Not signing ${image}@${digest}, SIGN=${SIGN}"
fi
