SHELL := /bin/bash

export RUST_BACKTRACE ?= 1
export WASMTIME_BACKTRACE_DETAILS ?= 1
WKG_CONFIG_FILE ?= $(dir $(abspath $(lastword $(MAKEFILE_LIST)))).config/wasm-pkg/config.toml

COMPONENTS = $(sort $(notdir $(patsubst %/,%,$(dir $(wildcard  components/*/Cargo.toml)))))

.PHONY: all
all: components test

.PHONY: clean
clean:
	cargo clean
	rm -rf lib/*.wasm
	rm -rf lib/*.wasm.md

.PHONY: test
test: wit
	cargo test

.PHONY: components
components: lib/interface.wasm $(foreach component,$(COMPONENTS),lib/$(component).wasm lib/$(component).debug.wasm)

define BUILD_COMPONENT

.PHONY: components/$1
components/$1: lib/$1.wasm lib/$1.debug.wasm

lib/$1.wasm: Cargo.toml Cargo.lock components/wit/deps $(shell find components/$1 -type f)
	cargo build -p $1 --target wasm32-unknown-unknown --release
	wasm-tools component new target/wasm32-unknown-unknown/release/$(subst -,_,$1).wasm -o lib/$1.wasm
	cp components/$1/README.md lib/$1.wasm.md

lib/$1.debug.wasm: Cargo.toml Cargo.lock components/wit/deps $(shell find components/$1 -type f)
	cargo build -p $1 --target wasm32-unknown-unknown
	wasm-tools component new target/wasm32-unknown-unknown/debug/$(subst -,_,$1).wasm -o lib/$1.debug.wasm
	cp components/$1/README.md lib/$1.debug.wasm.md

endef

$(foreach component,$(COMPONENTS),$(eval $(call BUILD_COMPONENT,$(component))))

lib/interface.wasm: wit/deps README.md
	wkg build -o lib/interface.wasm
	cp README.md lib/interface.wasm.md

.PHONY: wit
wit: wit/deps components/wit/deps

wit/deps: wkg.toml $(WKG_CONFIG_FILE) $(shell find wit -type f -name "*.wit" -not -path "deps")
	wkg fetch --config $(WKG_CONFIG_FILE)

components/wit/deps: wit/deps components/wkg.toml $(WKG_CONFIG_FILE) $(shell find components/wit -type f -name "*.wit" -not -path "deps")
	( cd components && wkg fetch --config $(WKG_CONFIG_FILE) )

.PHONY: publish ## Publish each component in the lib directory
publish: $(shell find lib -maxdepth 1 -type f -name "*.wasm" | sed -e 's:^lib/:publish-:g')

.PHONY: publish-%
publish-%:
ifndef VERSION
	$(error VERSION is undefined)
endif
ifndef REPOSITORY
	$(error REPOSITORY is undefined)
endif
	@$(eval FILE := $(@:publish-%=%))
	@$(eval COMPONENT := $(if $(filter %.debug.wasm,$(FILE)),$(FILE:%.debug.wasm=%),$(FILE:%.wasm=%)))
	@$(eval TITLE := $(if $(filter %.debug.wasm,$(FILE)),$(COMPONENT) (debug),$(COMPONENT)))
	@$(eval DESCRIPTION := $(shell head -n 3 "lib/${FILE}.md" | tail -n 1))
	@$(eval REVISION := $(shell git rev-parse HEAD)$(shell git diff --quiet HEAD && echo "+dirty"))
	@$(eval COMPONENT_VERSION := $(if $(filter %.debug.wasm,$(FILE)),${VERSION}+debug,${VERSION}))
	@$(eval TAG := $(patsubst v%,%,$(subst +,_,$(COMPONENT_VERSION))))
	@$(eval IMAGE := $(if $(filter interface.wasm,$(FILE)),${REPOSITORY}:${TAG},${REPOSITORY}/${COMPONENT}:${TAG}))

	@echo "::group::${FILE} -> ${IMAGE}"
	@DIGEST=$$( \
		wkg oci push \
			--annotation "org.opencontainers.image.title=${TITLE}" \
			--annotation "org.opencontainers.image.description=${DESCRIPTION}" \
			--annotation "org.opencontainers.image.version=${COMPONENT_VERSION}" \
			--annotation "org.opencontainers.image.source=https://github.com/${GITHUB_REPOSITORY}.git" \
			--annotation "org.opencontainers.image.revision=${REVISION}" \
			--annotation "org.opencontainers.image.licenses=Apache-2.0" \
			"${IMAGE}" \
			"lib/${FILE}" \
			2>&1 \
			| tee /dev/stderr \
			| grep -o 'sha256:[a-f0-9]\{64\}' \
	) ; \
	cosign sign --yes "${IMAGE}@$${DIGEST}"
	@echo "::endgroup::"
