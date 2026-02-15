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

.PHONY: _profile-check
_profile-check:
	@if [ -z "$(PROFILE)" ]; then echo "ERROR: Please set PROFILE on command line or in .env"; exit 1; fi
	read -p "Continue with PROFILE=$(PROFILE)? [Y/n] " ans && [ "$${ans:-Y}" != "n" ] && [ "$${ans:-Y}" != "N" ]


.PHONY: format
format: _profile-check	##H Format changed code blocks
	cargo +nightly fmt

.PHONY: lint
lint: _profile-check	##H Lint code
	cargo clippy --workspace --features full --locked --no-deps --profile $(PROFILE) -- -D warnings

.PHONY: test
test: _profile-check	##H Run tests
	cargo test --workspace --features full --locked --profile $(PROFILE) --all-targets

.PHONY: build
build: _profile-check	##H Build with PROFILE=
	cargo build $(CARGO_FLAGS)


.PHONY: _clean-check
_clean-check:
	@read -p "Are you sure you want to clean the $(PROFILE) build directory? [y/N] " ans && [ $${ans:-N} = y ]

.PHONY: clean
clean: _profile-check _clean-check	##H Clean build directory
	cargo clean


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# DevOps commands
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
CONTINUWUITY ?= conduwuit

LOCAL_BIN_DIR ?= target/$(PROFILE)
LOCAL_BIN := $(LOCAL_BIN_DIR)/$(CONTINUWUITY)

REMOTE_BIN_DIR ?= /usr/local/bin
REMOTE_BIN := $(REMOTE_BIN_DIR)/$(CONTINUWUITY)

BACKUP_DIR_BASE ?= .nginx-ops/continuwuity
LOCAL_BACKUP_DIR := $(HOME)/$(BACKUP_DIR_BASE) # Local backup directory (relative to user's home)

.PHONY: vps/install
vps/install: ##H Install (executed on VPS)
	@echo "Installing $(CONTINUWUITY) to $(REMOTE_BIN)"
	if [ ! -f $(LOCAL_BIN) ]; then echo "Error: $(LOCAL_BIN) not found. Run 'cargo build $(CARGO_FLAGS)' first."; exit 1; fi
	if [ -f "$(REMOTE_BIN)" ] && cmp -s "$(LOCAL_BIN)" "$(REMOTE_BIN)"; then \
		echo "Binary $(REMOTE_BIN) is identical to $(LOCAL_BIN). Skipping install."; \
		exit 2; \
	fi
	@echo "Binary differs or check failed. Proceeding with install..."

	mkdir -p "$(LOCAL_BACKUP_DIR)"
	if [ -f "$(REMOTE_BIN)" ]; then \
		CURRENT_VER=$$("$(REMOTE_BIN)" --version | awk '{print $$2 "-" $$3}' | tr -d '()'); \
		echo "Backing up existing binary to $(LOCAL_BACKUP_DIR)/$(CONTINUWUITY)-$$CURRENT_VER"; \
		cp -p "$(REMOTE_BIN)" "$(LOCAL_BACKUP_DIR)/$(CONTINUWUITY)-$$CURRENT_VER"; \
	fi

	cp -p "$(LOCAL_BIN)" "$(REMOTE_BIN)" || sudo cp -p "$(LOCAL_BIN)" "$(REMOTE_BIN)"
	@echo "Restarting $(CONTINUWUITY)"
	systemctl restart $(CONTINUWUITY) || sudo systemctl restart $(CONTINUWUITY)
	@echo "Installation complete."
