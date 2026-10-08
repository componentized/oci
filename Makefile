SHELL := /bin/bash

export RUST_BACKTRACE ?= 1
export WASMTIME_BACKTRACE_DETAILS ?= 1

COMPONENTS_DIR := target/components
TOOLS_DIR := target/tools/$(shell rustc --print host-tuple)
# absolute, tools also run from other directories, e.g. `cd components && wkg fetch`
export PATH := $(abspath $(TOOLS_DIR))/bin:$(PATH)

# cargo binstall downloads prebuilt binaries, without it the tools are built with cargo install
CARGO_INSTALL := $(if $(shell command -v cargo-binstall 2> /dev/null),cargo binstall --no-confirm --disable-telemetry,cargo install)

COMPONENTS = $(sort $(foreach file,$(wildcard $(addprefix components/*/,wit/*.constants.wit *.properties *.wac *.wkg Cargo.toml)),$(word 2,$(subst /, ,$(file)))))
TOOLS := componentized-constants-cli static-config wac-cli wasm-tools wasmtime-cli wkg

export WKG_CONFIG_FILE := $(abspath .config/wasm-pkg/config.toml)

# a path relative to the root of the repository, e.g. `wit` for `components/../wit`
relpath = $(if $(filter $(CURDIR),$(abspath $(1))),.,$(patsubst $(CURDIR)/%,%,$(abspath $(1))))


.PHONY: all
all: components

.PHONY: clean
clean: clean-wit
	cargo clean

.PHONY: clean-components
clean-components: clean-wit
	rm -rf ${COMPONENTS_DIR}

.PHONY: clean-wit ## Remove the fetched wit dependencies, fetched again by `make wit`
clean-wit:
	rm -rf $(WIT_DEPS)

.PHONY: test
test: components
	cargo test --workspace


tool_version = $(shell sed -n 's/^$(1) = "=\(.*\)"$$/\1/p' tools/Cargo.toml)
# a stamp naming the version of a tool installed in $(TOOLS_DIR)/bin, e.g. `wkg@0.16.1`, the binary
# does not say which version it is. Bumping the pinned version names a stamp that does not exist yet,
# so the tool is installed again.
tool = $(TOOLS_DIR)/.installed/$(1)@$(call tool_version,$(1))

.PHONY: tools ## Install the cli tools pinned in tools/Cargo.toml
tools: $(foreach name,$(TOOLS),$(call tool,$(name)))

.PHONY: tools-path ## Print the directory of the installed tools for this platform, to add to the PATH
tools-path:
	@echo $(abspath $(TOOLS_DIR))/bin

define INSTALL_TOOL

$(call tool,$1):
	$(CARGO_INSTALL) --locked --root $(TOOLS_DIR) --version $(call tool_version,$1) $1
	@mkdir -p $$(@D)
	@# only the installed version has a stamp, so going back to a previous version installs it again
	@rm -f $$(@D)/$1@*
	@touch $$@

endef

$(foreach name,$(TOOLS),$(eval $(call INSTALL_TOOL,$(name))))

.PHONY: components
components: ${COMPONENTS_DIR}/interface.wasm $(foreach component,$(COMPONENTS),${COMPONENTS_DIR}/$(component)/$(component).wasm ${COMPONENTS_DIR}/$(component)/$(component).debug.wasm)

define BUILD_COMPONENT

.PHONY: components/$1
components/$1: ${COMPONENTS_DIR}/$1/$1.wasm ${COMPONENTS_DIR}/$1/$1.debug.wasm

ifneq ($(wildcard components/$1/wit/$1.constants.wit),)

${COMPONENTS_DIR}/$1/$1.wasm: components/$1/wit/deps ${COMPONENTS_DIR}/$1/README.md | $(call tool,componentized-constants-cli)
	constants --wit components/$1/wit -o ${COMPONENTS_DIR}/$1/$1.wasm

${COMPONENTS_DIR}/$1/$1.debug.wasm: components/$1/wit/deps ${COMPONENTS_DIR}/$1/README.md | $(call tool,componentized-constants-cli)
	constants --wit components/$1/wit -o ${COMPONENTS_DIR}/$1/$1.debug.wasm

else ifneq ($(wildcard components/$1/$1.properties),)

${COMPONENTS_DIR}/$1/$1.wasm: components/$1/$1.properties ${COMPONENTS_DIR}/$1/README.md | $(call tool,static-config)
	static-config -f components/$1/$1.properties -o ${COMPONENTS_DIR}/$1/$1.wasm

${COMPONENTS_DIR}/$1/$1.debug.wasm: components/$1/$1.properties ${COMPONENTS_DIR}/$1/README.md | $(call tool,static-config)
	static-config -f components/$1/$1.properties -o ${COMPONENTS_DIR}/$1/$1.debug.wasm

else ifneq ($(wildcard components/$1/$1.wac),)

# the local packages the composition instantiates, e.g. `new local:latch-n2 { ... }`
WAC_DEPS_$1 := $$(shell grep -v '^\s*//' components/$1/$1.wac | grep -oE 'local:[a-z0-9-]+' | sed 's/^local://' | sort -u)

${COMPONENTS_DIR}/$1/$1.wasm: components/$1/$1.wac $$(foreach component,$$(WAC_DEPS_$1),$${COMPONENTS_DIR}/$$(component)/$$(component).wasm) ${COMPONENTS_DIR}/$1/README.md | $(call tool,wac-cli)
	wac compose $$(foreach component,$$(WAC_DEPS_$1),-d local:$$(component)=$${COMPONENTS_DIR}/$$(component)/$$(component).wasm) -o ${COMPONENTS_DIR}/$1/$1.wasm components/$1/$1.wac

${COMPONENTS_DIR}/$1/$1.debug.wasm: components/$1/$1.wac $$(foreach component,$$(WAC_DEPS_$1),$${COMPONENTS_DIR}/$$(component)/$$(component).debug.wasm) ${COMPONENTS_DIR}/$1/README.md | $(call tool,wac-cli)
	wac compose $$(foreach component,$$(WAC_DEPS_$1),-d local:$$(component)=$${COMPONENTS_DIR}/$$(component)/$$(component).debug.wasm) -o ${COMPONENTS_DIR}/$1/$1.debug.wasm components/$1/$1.wac

else ifneq ($(wildcard components/$1/$1.wkg),)

${COMPONENTS_DIR}/$1/$1.wasm: components/$1/$1.wkg ${COMPONENTS_DIR}/$1/README.md | $(call tool,wkg)
	wkg oci pull $(shell cat components/$1/$1.wkg 2> /dev/null | head -1) -o ${COMPONENTS_DIR}/$1/$1.wasm

${COMPONENTS_DIR}/$1/$1.debug.wasm: components/$1/$1.wkg ${COMPONENTS_DIR}/$1/README.md | $(call tool,wkg)
	wkg oci pull $(shell cat components/$1/$1.wkg  2> /dev/null | tail -1 2> /dev/null) -o ${COMPONENTS_DIR}/$1/$1.debug.wasm

# cargo is checked last, other strategies may have a Cargo.toml for tests of non-rust sources
else ifneq ($(wildcard components/$1/Cargo.toml),)

${COMPONENTS_DIR}/$1/$1.wasm: Cargo.toml Cargo.lock components/wit/deps $(shell find components/$1 -type f) $(shell find crates -type f 2> /dev/null) ${COMPONENTS_DIR}/$1/README.md | $(call tool,wasm-tools)
	cargo build -p $1 --target wasm32-unknown-unknown --release
	wasm-tools component new target/wasm32-unknown-unknown/release/$(subst -,_,$1).wasm -o ${COMPONENTS_DIR}/$1/$1.wasm

${COMPONENTS_DIR}/$1/$1.debug.wasm: Cargo.toml Cargo.lock components/wit/deps $(shell find components/$1 -type f) $(shell find crates -type f 2> /dev/null) ${COMPONENTS_DIR}/$1/README.md | $(call tool,wasm-tools)
	cargo build --target wasm32-unknown-unknown -p $1
	wasm-tools component new target/wasm32-unknown-unknown/debug/$(subst -,_,$1).wasm -o ${COMPONENTS_DIR}/$1/$1.debug.wasm

endif

${COMPONENTS_DIR}/$1/README.md: components/$1/README.md
	@mkdir -p ${COMPONENTS_DIR}/$1
	@cp components/$1/README.md ${COMPONENTS_DIR}/$1/README.md

endef

$(foreach component,$(COMPONENTS),$(eval $(call BUILD_COMPONENT,$(component))))

${COMPONENTS_DIR}/interface.wasm: wit/deps README.md | $(call tool,wkg)
	@mkdir -p ${COMPONENTS_DIR}
	wkg build -o ${COMPONENTS_DIR}/interface.wasm
	@cp README.md ${COMPONENTS_DIR}/README.md

# directories with a wkg.toml, each fetches the dependencies of its wit directory into wit/deps,
# e.g. `.` and `components`
WKG_DIRS := $(sort $(patsubst ./%,%,$(patsubst %/,%,$(dir $(shell find . -name wkg.toml -not -path './target/*' -not -path '*/deps/*')))))

# the wit/deps directory of a directory with a wkg.toml, e.g. `wit/deps` for `.`
wit_deps = $(patsubst ./%,%,$(1)/wit/deps)

WIT_DEPS := $(foreach dir,$(WKG_DIRS),$(call wit_deps,$(dir)))

.PHONY: wit
wit: $(WIT_DEPS)

define FETCH_WIT

# a package overridden with a local path, e.g. `{ path = "../wit" }`, has its dependencies fetched first
$(call wit_deps,$1): $1/wkg.toml $1/wkg.lock $(shell find $1/wit -type f -name "*.wit" -not -path "*/deps/*") $(foreach path,$(shell sed -n 's/.*path *= *"\(.*\)".*/\1/p' $1/wkg.toml),$(call relpath,$1/$(path))/deps) | $(call tool,wkg)
	$(if $(filter .,$1),,cd $1 && )wkg fetch

endef

$(foreach dir,$(WKG_DIRS),$(eval $(call FETCH_WIT,$(dir))))

# sign published components with cosign, `SIGN=false` to push without signing, e.g. to a local registry
SIGN ?= true
# append each published file and its image to this file, e.g. `client.wasm ghcr.io/componentized/oci/client:0.1.0@sha256:...`
PUBLISH_LOG ?=

# the files that can be published, e.g. client.wasm, published from target/components/client/client.wasm
PUBLISH_FILES := interface.wasm $(foreach component,$(filter-out dep-% internal-% test-%,$(COMPONENTS)),$(component).wasm $(component).debug.wasm)

.PHONY: publish ## Publish each component in the target/components directory
publish: $(addprefix publish-,$(PUBLISH_FILES))

.PHONY: $(addprefix publish-,$(PUBLISH_FILES))
$(addprefix publish-,$(PUBLISH_FILES)): publish-%: | $(call tool,wkg)
	@VERSION="$(VERSION)" REPOSITORY="$(REPOSITORY)" COMPONENTS_DIR="$(COMPONENTS_DIR)" SIGN="$(SIGN)" PUBLISH_LOG="$(PUBLISH_LOG)" \
		scripts/publish.sh $*
