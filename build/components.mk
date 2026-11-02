COMPONENTS_DIR := target/components

# the WIT packages of components/wit/deps, encoded where `wac compose` looks for a package a
# composition names without instantiating it, e.g. `import stderr: wasi:cli/stderr@0.3.0;`
WAC_DEPS_DIR := target/wac-deps

COMPONENTS = $(sort $(foreach file,$(wildcard $(addprefix components/*/,wit/*.constants.wit *.properties *.wac *.wkg Cargo.toml)),$(word 2,$(subst /, ,$(file)))))

# the cargo profiles components are built with, `component` and `component-debug` when Cargo.toml
# defines them, e.g. to build smaller components than the release profile a cli is built with
COMPONENT_PROFILE ?= $(if $(shell grep -s '^\[profile\.component\]' Cargo.toml),component,release)
COMPONENT_DEBUG_PROFILE ?= $(if $(shell grep -s '^\[profile\.component-debug\]' Cargo.toml),component-debug,dev)

# the core module cargo built for a component with a profile, e.g. `$(call cargo_wasm,gate,dev)` for
# target/wasm32-unknown-unknown/debug/gate.wasm, `$(call cargo_wasm,gate,release,.opt)` for the
# module wasm-opt optimized
cargo_wasm = target/$(call component_target,$(1))/$(if $(filter dev,$(2)),debug,$(2))/$(subst -,_,$(1))$(3).wasm

# the files the components cargo builds need besides the sources, e.g. a component the library
# embeds, `CARGO_COMPONENT_DEPS := crates/componentized-static-config/constants-config.wasm`
CARGO_COMPONENT_DEPS ?=

ifneq ($(filter wasm32-wasip3,$(COMPONENT_TARGETS)),)
# wasm32-wasip3 runs std threads on component model cooperative threads, which needs a wasi-libc built
# for them from wasi-sdk 34+'s experimental sysroot, installed with the tools or from WASI_SDK_PATH, or
# built from WASI_LIBC_VERSION, see build/tools.mk, and a rust toolchain with cooperative thread
# support, 1.100+. `make components COMPONENT_TARGET=wasm32-unknown-unknown` builds without.
WASI_SYSROOT_LIB := $(WASI_SYSROOT)/lib/wasm32-wasip3
# linked with wasm-component-ld, like the target's default, against the coop threads wasi-libc in
# place of the libc vendored with std. Linked by clang, a cdylib is a shared library, not a component.
# It links a core module, like wasm32-unknown-unknown, wasm-opt removes the code the release build
# doesn't reach, and with it the imports only that code uses, before it's made a component.
export CARGO_TARGET_WASM32_WASIP3_LINKER := $(WASI_SDK_PATH)/bin/wasm-component-ld
export CARGO_TARGET_WASM32_WASIP3_RUSTFLAGS := -Clink-self-contained=n -Clink-arg=-L$(WASI_SYSROOT_LIB) -Clink-arg=$(WASI_SYSROOT_LIB)/crt1-reactor.o -Clink-arg=--export=cabi_realloc -Clink-arg=--skip-wit-component
endif

# turn the core module cargo built into a component
componentize = $(WASM_TOOLS) component new $(1) -o $(2)

# the release build of a component is optimized again for size with wasm-opt when it is pinned in
# tools/Cargo.toml, `COMPONENT_OPTIMIZE=` builds without.
COMPONENT_OPTIMIZE ?= $(filter wasm-opt,$(TOOLS))

# the wasm features rust enables for wasm32-unknown-unknown and wasm32-wasip3, `rustc --print cfg
# --target wasm32-unknown-unknown`. The stripped modules don't say which features they use, wasm-opt
# is limited to these so it doesn't introduce instructions rust wouldn't emit.
WASM_OPT_FEATURES ?= --enable-bulk-memory --enable-multivalue --enable-mutable-globals --enable-nontrapping-float-to-int --enable-reference-types --enable-sign-ext

.PHONY: clean-components ## Remove the built components
clean-components: clean-wit
	rm -rf ${COMPONENTS_DIR} ${WAC_DEPS_DIR}

.PHONY: components ## Build the interface and each component into target/components, `make components/<name>` builds one
components: ${COMPONENTS_DIR}/interface.wasm $(foreach component,$(COMPONENTS),${COMPONENTS_DIR}/$(component)/$(component).wasm ${COMPONENTS_DIR}/$(component)/$(component).debug.wasm)

.PHONY: check-components ## Check the release and debug build of each component have the same WIT, `make check-components/<name>` checks one
check-components: $(foreach component,$(COMPONENTS),check-components/$(component))

.PHONY: validate-components ## Validate the release and debug build of each component, `make validate-components/<name>` checks one
validate-components: $(foreach component,$(COMPONENTS),validate-components/$(component))

define BUILD_COMPONENT

.PHONY: components/$1
components/$1: ${COMPONENTS_DIR}/$1/$1.wasm ${COMPONENTS_DIR}/$1/$1.debug.wasm

# the same imports and exports, e.g. a debug build that isn't optimized may keep imports the release
# build doesn't use, the order of the WIT may differ, see build/scripts/wit-canonical.sh
.PHONY: check-components/$1
check-components/$1: ${COMPONENTS_DIR}/$1/$1.wasm ${COMPONENTS_DIR}/$1/$1.debug.wasm build/scripts/wit-canonical.sh | $(call tool,wasm-tools)
	@diff -u --label $1.wasm --label $1.debug.wasm \
		<(WASM_TOOLS=$(WASM_TOOLS) build/scripts/wit-canonical.sh ${COMPONENTS_DIR}/$1/$1.wasm) \
		<(WASM_TOOLS=$(WASM_TOOLS) build/scripts/wit-canonical.sh ${COMPONENTS_DIR}/$1/$1.debug.wasm) \
		|| { echo "the WIT of the release and debug build of $1 differ" >&2; exit 1; }

.PHONY: validate-components/$1
validate-components/$1: ${COMPONENTS_DIR}/$1/$1.wasm | $(call tool,wasm-tools)
	@$(WASM_TOOLS) validate --features all ${COMPONENTS_DIR}/$1/$1.wasm || { echo "$1.wasm is not a valid component"; exit 1; }
	@$(WASM_TOOLS) validate --features all ${COMPONENTS_DIR}/$1/$1.debug.wasm || { echo "$1.debug.wasm is not a valid component"; exit 1; }

ifneq ($(wildcard components/$1/wit/$1.constants.wit),)

${COMPONENTS_DIR}/$1/$1.wasm: components/$1/wit/deps ${COMPONENTS_DIR}/$1/README.md $(call tool_prerequisite,componentized-constants-cli)
	$(CONSTANTS) --wit components/$1/wit -o ${COMPONENTS_DIR}/$1/$1.wasm

${COMPONENTS_DIR}/$1/$1.debug.wasm: components/$1/wit/deps ${COMPONENTS_DIR}/$1/README.md $(call tool_prerequisite,componentized-constants-cli)
	$(CONSTANTS) --wit components/$1/wit -o ${COMPONENTS_DIR}/$1/$1.debug.wasm

else ifneq ($(wildcard components/$1/$1.properties),)

${COMPONENTS_DIR}/$1/$1.wasm: components/$1/$1.properties ${COMPONENTS_DIR}/$1/README.md $(call tool_prerequisite,componentized-static-config-cli)
	$(STATIC_CONFIG) -f components/$1/$1.properties -o ${COMPONENTS_DIR}/$1/$1.wasm

${COMPONENTS_DIR}/$1/$1.debug.wasm: components/$1/$1.properties ${COMPONENTS_DIR}/$1/README.md $(call tool_prerequisite,componentized-static-config-cli)
	$(STATIC_CONFIG) -f components/$1/$1.properties -o ${COMPONENTS_DIR}/$1/$1.debug.wasm

else ifneq ($(wildcard components/$1/$1.wac),)

# the local packages the composition instantiates, e.g. `new local:latch-n2 { ... }`
WAC_DEPS_$1 := $$(shell grep -v '^\s*//' components/$1/$1.wac | grep -oE 'local:[a-z0-9-]+' | sed 's/^local://' | sort -u)

${COMPONENTS_DIR}/$1/$1.wasm: components/$1/$1.wac $$(foreach component,$$(WAC_DEPS_$1),$${COMPONENTS_DIR}/$$(component)/$$(component).wasm) ${COMPONENTS_DIR}/$1/README.md ${WAC_DEPS_DIR} | $(call tool,wac-cli)
	$(WAC) compose --deps-dir ${WAC_DEPS_DIR} $$(foreach component,$$(WAC_DEPS_$1),-d local:$$(component)=$${COMPONENTS_DIR}/$$(component)/$$(component).wasm) -o ${COMPONENTS_DIR}/$1/$1.wasm components/$1/$1.wac

${COMPONENTS_DIR}/$1/$1.debug.wasm: components/$1/$1.wac $$(foreach component,$$(WAC_DEPS_$1),$${COMPONENTS_DIR}/$$(component)/$$(component).debug.wasm) ${COMPONENTS_DIR}/$1/README.md ${WAC_DEPS_DIR} | $(call tool,wac-cli)
	$(WAC) compose --deps-dir ${WAC_DEPS_DIR} $$(foreach component,$$(WAC_DEPS_$1),-d local:$$(component)=$${COMPONENTS_DIR}/$$(component)/$$(component).debug.wasm) -o ${COMPONENTS_DIR}/$1/$1.debug.wasm components/$1/$1.wac

else ifneq ($(wildcard components/$1/$1.wkg),)

${COMPONENTS_DIR}/$1/$1.wasm: components/$1/$1.wkg ${COMPONENTS_DIR}/$1/README.md | $(call tool,wkg)
	$(WKG) oci pull $(shell cat components/$1/$1.wkg 2> /dev/null | head -1) -o ${COMPONENTS_DIR}/$1/$1.wasm

${COMPONENTS_DIR}/$1/$1.debug.wasm: components/$1/$1.wkg ${COMPONENTS_DIR}/$1/README.md | $(call tool,wkg)
	$(WKG) oci pull $(shell cat components/$1/$1.wkg  2> /dev/null | tail -1 2> /dev/null) -o ${COMPONENTS_DIR}/$1/$1.debug.wasm

# cargo is checked last, other strategies may have a Cargo.toml for tests of non-rust sources
else ifneq ($(wildcard components/$1/Cargo.toml),)

${COMPONENTS_DIR}/$1/$1.wasm: $(CARGO_COMPONENT_DEPS) Cargo.toml Cargo.lock components/wit/deps $(shell find components/$1 -type f) $(shell find crates -type f 2> /dev/null) ${COMPONENTS_DIR}/$1/README.md | $(call tool,wasm-tools) $(if $(COMPONENT_OPTIMIZE),$(call tool,wasm-opt)) $(if $(filter wasm32-wasip3,$(call component_target,$1)),$(WASI_SDK) $(WASI_LIBC))
	cargo build -p $1 --target $(call component_target,$1) --profile $(COMPONENT_PROFILE)
ifneq ($(COMPONENT_OPTIMIZE),)
	$(WASM_OPT) -Oz $(WASM_OPT_FEATURES) $(call cargo_wasm,$1,$(COMPONENT_PROFILE)) -o $(call cargo_wasm,$1,$(COMPONENT_PROFILE),.opt)
	$(call componentize,$(call cargo_wasm,$1,$(COMPONENT_PROFILE),.opt),${COMPONENTS_DIR}/$1/$1.wasm)
else
	$(call componentize,$(call cargo_wasm,$1,$(COMPONENT_PROFILE)),${COMPONENTS_DIR}/$1/$1.wasm)
endif

${COMPONENTS_DIR}/$1/$1.debug.wasm: $(CARGO_COMPONENT_DEPS) Cargo.toml Cargo.lock components/wit/deps $(shell find components/$1 -type f) $(shell find crates -type f 2> /dev/null) ${COMPONENTS_DIR}/$1/README.md | $(call tool,wasm-tools) $(if $(filter wasm32-wasip3,$(call component_target,$1)),$(WASI_SDK) $(WASI_LIBC))
	cargo build -p $1 --target $(call component_target,$1) --profile $(COMPONENT_DEBUG_PROFILE)
	$(call componentize,$(call cargo_wasm,$1,$(COMPONENT_DEBUG_PROFILE)),${COMPONENTS_DIR}/$1/$1.debug.wasm)

endif

${COMPONENTS_DIR}/$1/README.md: components/$1/README.md
	@mkdir -p ${COMPONENTS_DIR}/$1
	@cp components/$1/README.md ${COMPONENTS_DIR}/$1/README.md

endef

$(foreach component,$(COMPONENTS),$(eval $(call BUILD_COMPONENT,$(component))))

${WAC_DEPS_DIR}: components/wit/deps build/scripts/wac-deps.sh | $(call tool,wasm-tools)
	WASM_TOOLS=$(WASM_TOOLS) build/scripts/wac-deps.sh components/wit/deps ${WAC_DEPS_DIR}

${COMPONENTS_DIR}/interface.wasm: wit/deps README.md | $(call tool,wkg)
	@mkdir -p ${COMPONENTS_DIR}
	$(call wkg,.,build -o $(abspath ${COMPONENTS_DIR})/interface.wasm)
	@cp README.md ${COMPONENTS_DIR}/README.md
