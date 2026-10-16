SHELL := /bin/bash

export RUST_BACKTRACE ?= 1
export WASMTIME_BACKTRACE_DETAILS ?= 1
export WKG_CONFIG_FILE := $(abspath .config/wasm-pkg/config.toml)

# `make` without a goal runs all, otherwise the first target of an included file would run
.DEFAULT_GOAL := all

# tools first, the others use the variables it defines for each tool when their rules are read
include make/tools.mk
include make/wit.mk
include make/components.mk
include make/publish.mk


.PHONY: all
all: tools wit components test

.PHONY: clean
clean: clean-components clean-wit 
	@:

.PHONY: clean-all
clean-all: clean-components clean-tools clean-wit
	cargo clean

.PHONY: test
test: components
	cargo test --workspace
