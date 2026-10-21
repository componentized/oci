# the release of componentized/build to update to, e.g. v0.2.0, the latest release by default
BUILD_VERSION ?=

.PHONY: update-build ## Update the build to a release of componentized/build, e.g. `make update-build BUILD_VERSION=v0.2.0`
update-build:
	build/scripts/update-build.sh $(BUILD_VERSION)

.PHONY: check-build ## Check the files synced from componentized/build are unchanged since `make update-build`
check-build:
	build/scripts/check-build.sh
