SHELL=/bin/bash
.DEFAULT_GOAL := help

# source .env if it exists
ifneq (,$(wildcard ./.env))
	include .env
	export
endif

.PHONY: help
help: ##H Show this help
	@grep -hE '^[a-zA-Z0-9_\/-]+:.*?##H .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?##H "}; {printf "\033[36m%-15s\033[0m %s\n", $$1, $$2}'


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Development commands
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
PROFILE ?=
CARGO_FLAGS ?= --profile $(PROFILE)

.PHONY: profiles
profiles: ##H List available cargo profiles
	@grep "^\[profile\." Cargo.toml | sed 's/\[profile\.//;s/\]//' | grep -v 'package' | grep -v 'build-override' | sort

.PHONY: version
version: ##H Print the version
	cargo run -p conduwuit_build_metadata --bin conduwuit-version --quiet


.PHONY: _profile-check
_profile-check: version
	@if [ -z "$(PROFILE)" ]; then echo "ERROR: Please set PROFILE on command line or in .env"; exit 1; fi
	@if [ -t 0 ]; then read -p "Continue with PROFILE=$(PROFILE)? Press [Enter] to continue or Ctrl+C to abort..." _; fi


.PHONY: format
format: ##H Run pre-commit hooks/formatter
	pre-commit run --all-files

.PHONY: lint
lint: _profile-check
lint:	##H Lint code
	cargo clippy --workspace --features full --locked --no-deps --profile $(PROFILE) -- -D warnings

.PHONY: test
test: _profile-check
test:	##H Run tests
	cargo test --workspace --features full --locked --profile $(PROFILE) --all-targets

.PHONY: build
build: _profile-check
build:	##H Build with PROFILE=
	cargo build $(CARGO_FLAGS)

.PHONY: _benchmark
_benchmark: _profile-check
_benchmark:
	$(MAKE) clean
	time cargo build $(CARGO_FLAGS)

.PHONY: deb
deb: _profile-check
deb:	##H Build Debian package
	cargo-deb --release


.PHONY: _clean-check
_clean-check:
	@if [ -t 0 ]; then read -p "Cleaning $(PROFILE) build directory. Press [Enter] to continue or Ctrl+C to abort..." _; fi

.PHONY: clean
clean: _profile-check _clean-check
clean:	##H Clean build directory
	cargo clean
	rm -rf target/debian

# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Deployment commands
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
CONTINUWUITY ?= conduwuit

CARGO_OUT_DIR := $(if $(filter $(PROFILE),dev test),debug,$(PROFILE))
LOCAL_BIN_DIR ?= target/$(CARGO_OUT_DIR)
LOCAL_BIN := $(LOCAL_BIN_DIR)/$(CONTINUWUITY)

REMOTE_BIN_DIR ?= /usr/local/bin
REMOTE_BIN := $(REMOTE_BIN_DIR)/$(CONTINUWUITY)

BACKUP_DIR_BASE ?= .nginx-ops/continuwuity
LOCAL_BACKUP_DIR := $(HOME)/$(BACKUP_DIR_BASE) # Local backup directory (relative to user's home)

.PHONY: _deploy-check
_deploy-check: version
	@if [ -t 0 ]; then read -p "Deploying $(CONTINUWUITY) to $(REMOTE_BIN)? Press [Enter] to continue or Ctrl+C to abort..." _; fi

.PHONY: deploy/install
deploy/install: _deploy-check
deploy/install:	##H Install (executed on VPS)
	@echo "Installing $(CONTINUWUITY) to $(REMOTE_BIN)"
	if [ ! -f "$(REMOTE_BIN)" ] || ! cmp -s "$(LOCAL_BIN)" "$(REMOTE_BIN)"; then \
		install -b -p -m 755 "$(LOCAL_BIN)" "$(REMOTE_BIN)" || sudo install -b -p -m 755 "$(LOCAL_BIN)" "$(REMOTE_BIN)"; \
	else \
		echo "Binary $(REMOTE_BIN) is identical to $(LOCAL_BIN). Skipping install."; \
	fi
	@echo "Restarting $(CONTINUWUITY)"
	systemctl restart $(CONTINUWUITY) || sudo systemctl restart $(CONTINUWUITY)
	@echo "Installation complete."
