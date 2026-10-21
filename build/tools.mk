TOOLS_DIR := target/tools/$(shell rustc --print host-tuple)
# absolute, tools also run from other directories, e.g. `cd components && wkg fetch`
TOOLS_BIN := $(abspath $(TOOLS_DIR))/bin
# for the scripts and tests the recipes run, they look up the tools on the PATH
export PATH := $(TOOLS_BIN):$(PATH)

# the tools pinned in tools/Cargo.toml, generated when it changes, make restarts after generating it.
# Defines TOOLS, TOOLS_PINNED with the version of each tool, e.g. `wkg@0.16.1`, and a variable for
# each binary a tool installs, e.g. `WKG := $(TOOLS_BIN)/wkg`. Recipes run the pinned tools by path,
# make 3.81 runs a simple command without a shell and looks it up on its own PATH, before the export
# above, which would find a tool already on the system.
TOOLS_MK := target/tools/pinned.mk
include $(TOOLS_MK)

# the tools built from this repository rather than pinned in tools/Cargo.toml, e.g. a cli the
# components are built with, `LOCAL_TOOLS := componentized-constants-cli`. Each is a package of the
# workspace, installed with the pinned tools, and installed again when its sources change, its own or
# those of the workspace packages it depends on. A local tool replaces a pinned tool of the same name.
# Generated when the workspace changes, defines LOCAL_TOOL_PATH_<package>, LOCAL_TOOL_SOURCES_<package>
# and a variable for each binary the package installs, like the pinned tools, e.g.
# `CONSTANTS := $(TOOLS_BIN)/constants`.
LOCAL_TOOLS ?=
LOCAL_TOOLS_MK := target/tools/local.mk
ifneq ($(strip $(LOCAL_TOOLS)),)
# quietly, the first time make generates it before reading it
-include $(LOCAL_TOOLS_MK)
endif

# the toolchain pinned in rust-toolchain.toml, named for `cargo install`, otherwise rustup warns that
# the toolchain file overrides the default
RUST_TOOLCHAIN := $(shell sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml 2> /dev/null)

# cargo binstall downloads prebuilt binaries, without it the tools are built with cargo install
CARGO_INSTALL := $(if $(shell command -v cargo-binstall 2> /dev/null),cargo binstall --no-confirm --disable-telemetry,cargo install)

.PHONY: clean-tools ## Remove the installed tools, installed again by `make tools`
clean-tools:
	rm -rf ${TOOLS_DIR}

# each tool is a dependency of the tools package, the binaries it installs are listed in
# [package.metadata.bins]
$(TOOLS_MK): tools/Cargo.toml build/pinned.jq
	@mkdir -p $(@D)
	@set -o pipefail; cargo metadata --manifest-path tools/Cargo.toml --no-deps --format-version 1 | jq -r -f build/pinned.jq > $@.tmp
	@mv $@.tmp $@

# the packages of the workspace, generated again when LOCAL_TOOLS changes. The workspace's members may
# change without changing Cargo.toml or Cargo.lock, `make clean-tools` generates it again.
$(LOCAL_TOOLS_MK): Cargo.toml Cargo.lock $(filter-out $(TOOLS_MK) $(LOCAL_TOOLS_MK),$(MAKEFILE_LIST)) build/local.jq
	@mkdir -p $(@D)
	@set -o pipefail; cargo metadata --no-deps --format-version 1 | jq -r --arg tools "$(LOCAL_TOOLS)" -f build/local.jq > $@.tmp
	@mv $@.tmp $@

tool_version = $(if $(filter $(1),$(LOCAL_TOOLS)),local,$(patsubst $(1)@%,%,$(filter $(1)@%,$(TOOLS_PINNED))))
# a stamp naming the version of a tool installed in $(TOOLS_DIR)/bin, e.g. `wkg@0.16.1`, the binary
# does not say which version it is. Bumping the pinned version names a stamp that does not exist yet,
# so the tool is installed again. A local tool's stamp is `<package>@local`.
tool = $(TOOLS_DIR)/.installed/$(1)@$(call tool_version,$(1))

# a tool a recipe runs: a local tool is a prerequisite, what's made with it is made again when it
# changes, a pinned tool only has to be installed first, e.g. `$(call tool_prerequisite,wkg)`. Last
# among the prerequisites, it may be order-only.
tool_prerequisite = $(if $(filter $(1),$(LOCAL_TOOLS)),$(call tool,$(1)),| $(call tool,$(1)))

.PHONY: tools ## Install the cli tools pinned in tools/Cargo.toml, and those built from the workspace
tools: $(foreach name,$(sort $(TOOLS) $(LOCAL_TOOLS)),$(call tool,$(name)))

.PHONY: tools-path ## Print the directory of the installed tools for this platform, to add to the PATH
tools-path:
	@echo $(TOOLS_BIN)

define INSTALL_TOOL

$(call tool,$1):
	$(CARGO_INSTALL) --locked --root $(TOOLS_DIR) --version $(call tool_version,$1) $1
	@mkdir -p $$(@D)
	@# only the installed version has a stamp, so going back to a previous version installs it again
	@rm -f $$(@D)/$1@*
	@touch $$@

endef

$(foreach name,$(filter-out $(LOCAL_TOOLS),$(TOOLS)),$(eval $(call INSTALL_TOOL,$(name))))

define INSTALL_LOCAL_TOOL

$(call tool,$1): Cargo.toml Cargo.lock $(wildcard rust-toolchain.toml) $(if $(LOCAL_TOOL_SOURCES_$1),$(shell find $(LOCAL_TOOL_SOURCES_$1) -type f))
	@# forced, make only installs when the sources change, and the binary may belong to another package
	cargo $(if $(RUST_TOOLCHAIN),+$(RUST_TOOLCHAIN)) install --force --locked --root $(TOOLS_DIR) --path $(LOCAL_TOOL_PATH_$1)
	@mkdir -p $$(@D)
	@# going back to the pinned version installs it again
	@rm -f $$(@D)/$1@*
	@touch $$@

endef

$(foreach name,$(LOCAL_TOOLS),$(eval $(call INSTALL_LOCAL_TOOL,$(name))))

ifeq ($(COMPONENT_TARGET),wasm32-wasip3)

# wasi-sdk, for its experimental sysroot with a wasi-libc built for component model cooperative
# threads, see build/components.mk. Installed with the tools, set WASI_SDK_PATH to use an existing install.
WASI_SDK_VERSION := 34.0
WASI_SDK_HOST := $(or $(word 2,$(subst :, ,$(filter $(shell rustc --print host-tuple):%,aarch64-apple-darwin:arm64-macos x86_64-apple-darwin:x86_64-macos aarch64-unknown-linux-gnu:arm64-linux x86_64-unknown-linux-gnu:x86_64-linux))),unsupported-host)
WASI_SDK_DIR := $(abspath $(TOOLS_DIR))/wasi-sdk-$(WASI_SDK_VERSION)
WASI_SDK_PATH ?= $(WASI_SDK_DIR)
WASI_SDK := $(TOOLS_DIR)/.installed/wasi-sdk@$(WASI_SDK_VERSION)

tools: $(WASI_SDK)

$(WASI_SDK):
	@mkdir -p $(WASI_SDK_DIR) $(@D)
	@set -o pipefail; curl -fsSL https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-$(basename $(WASI_SDK_VERSION))/wasi-sdk-$(WASI_SDK_VERSION)-$(WASI_SDK_HOST).tar.gz | tar xz --strip-components 1 -C $(WASI_SDK_DIR)
	@rm -f $(@D)/wasi-sdk@*
	@touch $@

endif
