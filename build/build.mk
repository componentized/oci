# The build shared by the componentized repositories, synced from https://github.com/componentized/build
# at the version in build/VERSION. Nothing in build/ is edited here, see build/README.md.
#
# The repository's Makefile sets its settings, includes this file, then adds its own targets and
# prerequisites, e.g.
#
#   COMPONENT_TARGET := wasm32-wasip3
#   include build/build.mk
#   test-integration: test-valkey

SHELL := /bin/bash

export RUST_BACKTRACE ?= 1
export WASMTIME_BACKTRACE_DETAILS ?= 1
export WKG_CONFIG_FILE := $(abspath .config/wasm-pkg/config.toml)

# `make` without a goal runs all, otherwise the first target of an included file would run
.DEFAULT_GOAL := all

# the rust target components are built for, `wasm32-wasip3` builds components that run std threads on
# component model cooperative threads, see build/components.mk
COMPONENT_TARGET ?= wasm32-unknown-unknown

# the cli built from this repository, e.g. `constants`, released with an archive for each platform by
# the CI workflow. Empty for a repository without a cli.
CLI_BIN ?=
# the crates published to crates.io when a release is published, e.g. `componentized-constants
# componentized-constants-cli`, checked by the CI workflow. Empty for a repository without crates.
PUBLISH_CRATES ?=

# tools first, the others use the variables it defines for each tool when their rules are read
include build/tools.mk
include build/wit.mk
include build/components.mk
include build/publish.mk
include build/sync.mk


.PHONY: all ## Install the tools, generate, build the components and test them, `make` without a target
all: tools generate components test

# the help of a target follows `## ` on its .PHONY line, e.g. `.PHONY: tools ## Install the tools`, or
# on the line of its rule, e.g. `run: cli.wasm ## Run the cli`. A variable's line, with an `=`, is not
# a target.
.PHONY: help ## Display this help screen
help:
	@awk '/^[^#[:space:]][^=]*:[^=]*## / { \
		names = substr($$0, 1, index($$0, "## ") - 1); \
		if (sub(/^\.PHONY:/, "", names) == 0) sub(/:.*$$/, "", names); \
		text = substr($$0, index($$0, "## ") + 3); \
		count = split(names, list, /[[:space:]]+/); \
		for (i = 1; i <= count; i++) if (list[i] != "" && !(list[i] in seen)) { seen[list[i]] = 1; printf "\033[36m%-20s\033[0m %s\n", list[i], text } \
	}' $(MAKEFILE_LIST) | sort

# the repository adds what else it generates and commits as prerequisites, e.g. `generate: config`
.PHONY: generate ## Generate the files committed with the sources, the CI workflow fails when they drift
generate: wit
	@:

.PHONY: clean ## Remove the built components and the fetched wit dependencies
clean: clean-components clean-wit
	@:

.PHONY: clean-all ## Remove everything built, the installed tools and cargo's target directory too
clean-all: clean-components clean-tools clean-wit
	cargo clean

.PHONY: test ## Build the components and run the tests of the workspace
test: components
	cargo test --workspace

# the repository adds the tests that need more than cargo as prerequisites, e.g. a server in a
# container, `test-integration: test-valkey`
.PHONY: test-integration ## Run the tests that need more than cargo, run by the CI workflow after `make test`
test-integration:
	@:

# prints the value of a variable, e.g. `make -s print-CLI_BIN`, for the workflows
print-%:
	$(info $($*))
	@:
