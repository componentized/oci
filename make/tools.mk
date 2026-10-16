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

# cargo binstall downloads prebuilt binaries, without it the tools are built with cargo install
CARGO_INSTALL := $(if $(shell command -v cargo-binstall 2> /dev/null),cargo binstall --no-confirm --disable-telemetry,cargo install)

.PHONY: clean-tools
clean-tools:
	rm -rf ${TOOLS_DIR}

# each tool is a dependency of the tools package, the binaries it installs are listed in
# [package.metadata.bins]
$(TOOLS_MK): tools/Cargo.toml make/pinned.jq
	@mkdir -p $(@D)
	@set -o pipefail; cargo metadata --manifest-path tools/Cargo.toml --no-deps --format-version 1 | jq -r -f make/pinned.jq > $@.tmp
	@mv $@.tmp $@

tool_version = $(patsubst $(1)@%,%,$(filter $(1)@%,$(TOOLS_PINNED)))
# a stamp naming the version of a tool installed in $(TOOLS_DIR)/bin, e.g. `wkg@0.16.1`, the binary
# does not say which version it is. Bumping the pinned version names a stamp that does not exist yet,
# so the tool is installed again.
tool = $(TOOLS_DIR)/.installed/$(1)@$(call tool_version,$(1))

.PHONY: tools ## Install the cli tools pinned in tools/Cargo.toml
tools: $(foreach name,$(TOOLS),$(call tool,$(name)))

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

$(foreach name,$(TOOLS),$(eval $(call INSTALL_TOOL,$(name))))
