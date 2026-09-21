FROM mcr.microsoft.com/devcontainers/rust:2-1-trixie
RUN sudo apt update && sudo apt upgrade -y
RUN curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
RUN cargo binstall -y wasm-tools
RUN cargo binstall -y wkg
RUN cargo binstall -y wac-cli
RUN cargo binstall -y wasmtime-cli
