PLUGIN := planeai-plugin-github
DIST := dist/$(PLUGIN)
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)

ifeq ($(UNAME_S),Darwin)
  ifeq ($(UNAME_M),arm64)
    PLATFORM := macos-arm64
  else
    $(error macOS x64 is unsupported; use Apple Silicon)
  endif
else ifeq ($(UNAME_S),Linux)
  ifeq ($(UNAME_M),aarch64)
    PLATFORM := linux-arm64
  else
    PLATFORM := linux-x64
  endif
else
  $(error Unsupported local packaging platform; use the release workflow for Windows)
endif

.PHONY: build-ui test package verify-package clean

build-ui:
	pnpm exec tsc

test: build-ui
	cargo test
	node --test build/tests/ui-entry-shortcuts.test.js
	node --test tests/release-version-injection.test.mjs

package: build-ui
	cargo build --release
	rm -rf $(DIST)
	mkdir -p $(DIST)/bin/$(PLATFORM) $(DIST)/ui
	cp planeai-plugin.json $(DIST)/
	cp build/ui/entry.js build/ui/titlebar.js $(DIST)/ui/
	cp target/release/$(PLUGIN) $(DIST)/bin/$(PLATFORM)/$(PLUGIN)
	chmod +x $(DIST)/bin/$(PLATFORM)/$(PLUGIN)
	@echo "Staged $(DIST) for $(PLATFORM)"

verify-package: package
	node scripts/verify-package-handshake.mjs $(DIST) $(PLATFORM)

clean:
	rm -rf build dist
