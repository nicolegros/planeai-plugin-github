PLUGIN := planeai-plugin-github
DIST := dist/$(PLUGIN)
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)

ifeq ($(UNAME_S),Darwin)
  ifeq ($(UNAME_M),arm64)
    PLATFORM := macos-arm64
  else
    PLATFORM := macos-x64
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

.PHONY: test package clean

test:
	cargo test

package:
	cargo build --release
	rm -rf $(DIST)
	mkdir -p $(DIST)/bin/$(PLATFORM) $(DIST)/ui
	cp planeai-plugin.json $(DIST)/
	cp ui/entry.js $(DIST)/ui/
	cp target/release/$(PLUGIN) $(DIST)/bin/$(PLATFORM)/$(PLUGIN)
	chmod +x $(DIST)/bin/$(PLATFORM)/$(PLUGIN)
	@echo "Staged $(DIST) for $(PLATFORM)"

clean:
	rm -rf dist
