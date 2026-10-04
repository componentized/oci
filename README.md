# OCI Components <!-- omit in toc -->

WASM components for interacting with resources in OCI image repositories.

- [Build](#build)
  - [Components](#components)
- [Community](#community)
  - [Code of Conduct](#code-of-conduct)
  - [Communication](#communication)
  - [Contributing](#contributing)
- [Acknowledgements](#acknowledgements)
- [License](#license)


## Build

A [dev container](https://containers.dev) is available that contains the necessary tools and configuration out of the box.

Prereqs:
- a rust toolchain
- [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall), optional, to download prebuilt tools instead of building them

```sh
make components
```

The build creates each component in [`components`](./components) into `target/components`, e.g. the client at `target/components/client/client.wasm`, along with `target/components/interface.wasm`, the `componentized:oci` WIT package. Each component is also built with debug info, e.g. `target/components/client/client.debug.wasm`.

The cli tools the build uses, [`wasm-tools`](https://github.com/bytecodealliance/wasm-tools), [`wac`](https://github.com/bytecodealliance/wac), [`wasmtime`](https://github.com/bytecodealliance/wasmtime) and [`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools), are pinned in [`tools/Cargo.toml`](./tools/Cargo.toml) and installed into `target/tools/<platform>`, e.g. `target/tools/aarch64-apple-darwin`, as needed, or ahead of time with `make tools`. Dependabot bumps the pinned versions.

### Components

- [`client`](./components/client/)

## Community

### Code of Conduct

The Componentized project follow the [Contributor Covenant Code of Conduct](./CODE_OF_CONDUCT.md). In short, be kind and treat others with respect.

### Communication

General discussion and questions about the project can occur in the project's [GitHub discussions](https://github.com/orgs/componentized/discussions).

### Contributing

The Componentized project team welcomes contributions from the community. A contributor license agreement (CLA) is not required. You own full rights to your contribution and agree to license the work to the community under the Apache License v2.0, via a [Developer Certificate of Origin (DCO)](https://developercertificate.org). For more detailed information, refer to [CONTRIBUTING.md](CONTRIBUTING.md).

## Acknowledgements

This project was conceived in discussion between [Mark Fisher](https://github.com/markfisher) and [Scott Andrews](https://github.com/scothis).

## License

Apache License v2.0: see [LICENSE](./LICENSE) for details.
