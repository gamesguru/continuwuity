SHELL=/bin/bash
.DEFAULT_GOAL=help

# [CONFIG] Suppresses annoying "make[1]: Entering directory" messages
MAKEFLAGS += --no-print-directory

# [CONFIG] source .env if it exists
ifneq (,$(wildcard ./.env))
	include .env
	export
	# Strip double quotes from .env values (annoying disagreement between direnv, dotenv)
	RUSTFLAGS := $(subst ",,$(RUSTFLAGS))
endif

# Example .env:
# #!/bin/bash
# export OS_VERSION=ubuntu-24.04
# export GH_REPO=gamesguru/continuwuity
# export SKIP_CONFIRM=1
# export PROFILE=dev-quick
# export GIT_DESCRIBE_OPTIONAL_BRANCH=
# export RUSTFLAGS="-C target-cpu=native"


# [CONFIG] Auto-discover vars defined in Makefiles (not env-inherited)
VARS = $(sort $(foreach v,$(.VARIABLES),$(if $(filter file override command,$(origin $v)),$v)))

# [ENUM] Styling / Colors
STYLE_CYAN := $(shell tput setaf 6 2>/dev/null || echo -e "\033[36m")
STYLE_RESET := $(shell tput sgr0 2>/dev/null || echo -e "\033[0m")


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Meta/help commands
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.PHONY: help
help: ##H Show this help, list available targets
	@grep -hE '^[a-zA-Z0-9_\/-]+:.*?##H .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?##H "}; {printf "$(STYLE_CYAN)%-15s$(STYLE_RESET) %s\n", $$1, $$2}'


.PHONY: doctor
doctor: ##H Output version info for required tools
	@echo "Sanity check — not comprehensive. Requirements may be missing or out of date."
	@echo "See rust-toolchain.toml for authoritative versions."
	cargo --version
	rustup --version
	cargo +nightly fmt --version
	cargo fmt --version
	cargo +nightly clippy --version
	cargo clippy --version
	pre-commit --version
	pkg-config --version
	pkg-config --libs --cflags liburing
	@echo "OK."
	@echo "Checking for newer tags [DRY RUN]..."
	git fetch --all --dry-run --tags

.PHONY: cpu-info
cpu-info: ##H Print CPU info relevant to target-cpu=native
	@echo "=== CPU Model ==="
	@grep -m1 'model name' /proc/cpuinfo 2>/dev/null || sysctl -n machdep.cpu.brand_string 2>/dev/null || echo "unknown"
	@echo "=== Architecture ==="
	@uname -a
	@echo "=== rustc Host Target ==="
	@rustc -vV | grep host
	@echo "=== rustc Native CPU ==="
	@rustc --print=cfg -C target-cpu=native 2>/dev/null | grep target_feature | sort
	@echo "=== CPU Flags (from /proc/cpuinfo) ==="
	@grep -m1 'flags' /proc/cpuinfo 2>/dev/null | tr ' ' '\n' | grep -E 'avx|sse|aes|bmi|fma|popcnt|lzcnt|sha|pclmul' | sort

.PHONY: vars
vars: ##H Print debug info
	@$(foreach v, $(VARS), printf "$(STYLE_CYAN)%-25s$(STYLE_RESET) %s\n" "$(v)" "$($(v))";)
	@echo "... computing version."
	@printf "$(STYLE_CYAN)%-25s$(STYLE_RESET) %s\n" "VERSION" \
		"$$(cargo run -p conduwuit_build_metadata --bin conduwuit-version --quiet)"


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Development commands
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

.PHONY: profiles
profiles: ##H List available cargo profiles
	# NOTE: not authoritative — see Cargo.toml for definitive profiles.
	@grep "^\[profile\." Cargo.toml \
		| sed 's/\[profile\.//;s/\]//' \
		| grep -v 'package' \
		| grep -v 'build-override' \
		| sort

PROFILE ?=
CRATE ?=
CARGO_SCOPE ?= $(if $(CRATE),-p $(CRATE),--workspace)
CARGO_FLAGS ?= --profile $(PROFILE)

# For native, highly-optimized builds that work only for you cpu: -C target-cpu=native
RUSTFLAGS ?=

# Display crate compilation progress [X/Y] in nohup or no-tty environment.
# Override or unset in .env to disable.
export CARGO_TERM_PROGRESS_WHEN ?= auto
export CARGO_TERM_PROGRESS_WIDTH ?= 80

# To suppress the confirmation prompt, add to your .env: SKIP_CONFIRM=1
SKIP_CONFIRM ?=

# Target to prompt for confirmation before proceeding (slow tasks, cleaning builds, etc)
.PHONY: _confirm
_confirm:
	# Verifying required variables are set...
	@test "$(PROFILE)" || (echo "ERROR: PROFILE is not set" && exit 1)
	# Confirming you wish to proceed...
	@if [ -t 0 ] && [ -z "$(SKIP_CONFIRM)" ]; then read -p "Press [Enter] to continue or Ctrl+C to cancel..." _; fi


.PHONY: format
format: ##H Run pre-commit hooks/formatters
	pre-commit run --all-files

.PHONY: lint
lint:	##H Lint code
	@echo "Lint code? PROFILE='$(PROFILE)'"
	@$(MAKE) _confirm
	cargo clippy $(CARGO_SCOPE) --features full --locked --no-deps --profile $(PROFILE) -- -D warnings

.PHONY: test
test:	##H Run tests
	@echo "Run tests? PROFILE='$(PROFILE)'"
	@$(MAKE) _confirm
	cargo test $(CARGO_SCOPE) --features full --locked --profile $(PROFILE) --all-targets


.PHONY: build
build:	##H Build with selected profile
	# NOTE: for a build that works best and ONLY for your CPU: export RUSTFLAGS=-C target-cpu=native
	@echo "Build this profile? PROFILE='$(PROFILE)'"
	@$(MAKE) _confirm
	cargo build $(CARGO_FLAGS)
	@echo "Build finished! Linking '$(PROFILE)' to target/latest"
	ln -sfn $(if $(filter $(PROFILE),dev test),debug,$(PROFILE)) target/latest


.PHONY: clean
clean:	##H Clean build directory for current profile
	@echo "Clean the '$(PROFILE)' build directory?"
	@$(MAKE) _confirm
	cargo clean --profile $(PROFILE)
	@echo "Also remove debian build?"
	@$(MAKE) _confirm
	rm -rf target/debian


.PHONY: docs
docs:	##H Regenerate docs (admin commands, etc.)
	cargo run -p xtask --profile $(PROFILE) -- generate-docs


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Deployment commands
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

# CI artifact OS target. Override with: make download OS_VERSION=ubuntu-22.04
OS_VERSION ?=
GH_REPO ?=
RUN_ID ?=

.PHONY: download
download:	##H Download CI binary (use RUN_ID=... to pick a specific run)
	# Testing whether OS_VERSION and GH_REPO are set...
	@test "$(OS_VERSION)"
	@test "$(GH_REPO)"
	@mkdir -p target/ci
	# Checking version of old binary, if it exists
	@-./target/ci/conduwuit -V
	@rm -f target/ci/conduwuit
	gh run download $(RUN_ID) -R $(GH_REPO) -n conduwuit-$(OS_VERSION) -D target/ci
	@chmod +x target/ci/conduwuit
	@echo "Downloaded to target/ci/conduwuit"
	@./target/ci/conduwuit -V
	@ln -sfn ci target/latest

.PHONY: download-list
download-list:	##H List recent CI runs
	@test "$(GH_REPO)" || (echo "ERROR: GH_REPO is not set. Add GH_REPO=owner/repo to .env" && exit 1)
	gh run list -R $(GH_REPO) --limit 15


# Binary name
CONTINUWUITY ?= conduwuit

# Configure these in .env if alternate path(s) are desired
LOCAL_BIN_DIR ?= target/latest
REMOTE_BIN_DIR ?= /usr/local/bin

LOCAL_BIN ?= $(LOCAL_BIN_DIR)/$(CONTINUWUITY)
REMOTE_BIN ?= $(REMOTE_BIN_DIR)/$(CONTINUWUITY)

.PHONY: install
install:	##H Install (executed on VPS)
	@echo "Install $(CONTINUWUITY) to $(REMOTE_BIN)?"
	@$(MAKE) _confirm
	# You may need to run with sudo or adjust REMOTE_BIN_DIR if this fails
	install -b -p -m 755 $(LOCAL_BIN) $(REMOTE_BIN)
	@echo "Installation complete."
# 	@echo "Restarting $(CONTINUWUITY)"
# 	systemctl restart $(CONTINUWUITY) || sudo systemctl restart $(CONTINUWUITY)
