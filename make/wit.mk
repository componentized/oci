# a path relative to the root of the repository, e.g. `wit` for `components/../wit`
relpath = $(if $(filter $(CURDIR),$(abspath $(1))),.,$(patsubst $(CURDIR)/%,%,$(abspath $(1))))

# directories with a wkg.toml, each fetches the dependencies of its wit directory into wit/deps,
# e.g. `.` and `components`
WKG_DIRS := $(sort $(patsubst ./%,%,$(patsubst %/,%,$(dir $(shell find . -name wkg.toml -not -path './target/*' -not -path '*/deps/*')))))

# the wit/deps directory of a directory with a wkg.toml, e.g. `wit/deps` for `.`
wit_deps = $(patsubst ./%,%,$(1)/wit/deps)

WIT_DEPS := $(foreach dir,$(WKG_DIRS),$(call wit_deps,$(dir)))

# runs wkg with the arguments in the directory with a wkg.toml, e.g. `$(call wkg,components,fetch)`.
# wkg rewrites wkg.lock even when nothing changed, which would leave everything made from the lock
# looking out of date, so the lock is put back as it was, with its time, when its content is unchanged
wkg = cp -p $(1)/wkg.lock $(1)/wkg.lock.orig && \
	(cd $(1) && $(WKG) $(2)); status=$$?; \
	if cmp -s $(1)/wkg.lock.orig $(1)/wkg.lock; then mv $(1)/wkg.lock.orig $(1)/wkg.lock; else rm $(1)/wkg.lock.orig; fi; \
	exit $$status

.PHONY: clean-wit ## Remove the fetched wit dependencies, fetched again by `make wit`
clean-wit:
	rm -rf $(WIT_DEPS)

.PHONY: wit
wit: $(WIT_DEPS)

define FETCH_WIT

# the dependencies are touched after fetching, newer than the lock they were fetched with even when
# the fetch changed nothing in them
#
# a package overridden with a local path, e.g. `{ path = "../wit" }`, has its dependencies fetched first,
# an override without dependencies of its own is skipped, e.g. `{ path = "./wit/overrides/wasi-muppets" }`
$(call wit_deps,$1): $1/wkg.toml $1/wkg.lock $(shell find $1/wit -type f -name "*.wit" -not -path "*/deps/*") $(filter $(WIT_DEPS),$(foreach path,$(shell sed -n 's/.*path *= *"\(.*\)".*/\1/p' $1/wkg.toml),$(call relpath,$1/$(path))/deps)) | $(call tool,wkg)
	$$(call wkg,$1,fetch)
	@touch $(call wit_deps,$1)

endef

$(foreach dir,$(WKG_DIRS),$(eval $(call FETCH_WIT,$(dir))))
