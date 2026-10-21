# sign published components with cosign, `SIGN=false` to push without signing, e.g. to a local registry
SIGN ?= true
# show what would be published without pushing or signing anything, `DRY_RUN=true`
DRY_RUN ?= false
# append each published file, its size in bytes and its image to this file, e.g. `gate.wasm 1234567 ghcr.io/componentized/http/gate:0.1.0@sha256:...`
PUBLISH_LOG ?=

# the files that can be published, e.g. gate.wasm, published from target/components/gate/gate.wasm.
# Components are not published when they are pulled from elsewhere, `dep-`, built here for use by
# other components, `internal-`, or tests, `test-`.
PUBLISH_FILES := interface.wasm $(foreach component,$(filter-out dep-% internal-% test-%,$(COMPONENTS)),$(component).wasm $(component).debug.wasm)

.PHONY: publish ## Publish each component in the target/components directory, `make publish-<file>` publishes one, e.g. publish-gate.wasm
publish: $(addprefix publish-,$(PUBLISH_FILES))

.PHONY: $(addprefix publish-,$(PUBLISH_FILES))
$(addprefix publish-,$(PUBLISH_FILES)): publish-%: | $(call tool,wkg)
	@VERSION="$(VERSION)" REPOSITORY="$(REPOSITORY)" COMPONENTS_DIR="$(COMPONENTS_DIR)" SIGN="$(SIGN)" PUBLISH_LOG="$(PUBLISH_LOG)" DRY_RUN="$(DRY_RUN)" \
		build/scripts/publish.sh $*
