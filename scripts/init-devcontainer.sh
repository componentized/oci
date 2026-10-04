#!/usr/bin/env bash

# Setup script run once for new devcontainers to init the environment.

set -euo pipefail

curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash

cargo check

echo "export \"PATH=$(make -s tools-path):${PATH}\"" >> ~/.bashrc
make tools
