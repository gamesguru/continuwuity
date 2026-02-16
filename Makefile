SHELL=/bin/bash
.DEFAULT_GOAL=help

# [CONFIG] Suppresses annoying "make[1]: Entering directory" messages
MAKEFLAGS += --no-print-directory

# [CONFIG] source .env if it exists
ifneq (,$(wildcard ./.env))
	include .env
	export
endif

# [CONFIG] Auto-discover custom vars
_BUILTIN_VARS := $(.VARIABLES)
VARS := $(sort $(filter-out $(_BUILTIN_VARS) _BUILTIN_VARS VARS, $(.VARIABLES)))

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

.PHONY: profiles
profiles: ##H List available cargo profiles
	# NOTE: not authoritative — see Cargo.toml for definitive profiles.
	@grep "^\[profile\." Cargo.toml \
		| sed 's/\[profile\.//;s/\]//' \
		| grep -v 'package' \
		| grep -v 'build-override' \
		| sort

.PHONY: vars
vars: ##H Print debug info
	@$(foreach v, $(VARS), printf "$(STYLE_CYAN)%-25s$(STYLE_RESET) %s\n" "$(v)" "$($(v))";)
	@printf "$(STYLE_CYAN)%-25s$(STYLE_RESET) %s\n" "VERSION" \
		"$(shell cargo run -p conduwuit_build_metadata --bin conduwuit-version --quiet)"


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Development commands
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

PROFILE ?=
CRATE ?=
CARGO_SCOPE ?= $(if $(CRATE),-p $(CRATE),--workspace)
CARGO_FLAGS ?= --profile $(PROFILE)

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
	@echo "Build this profile? PROFILE='$(PROFILE)'"
	@$(MAKE) _confirm
	cargo build $(CARGO_FLAGS)
	@echo "Build finished! Linking '$(PROFILE)' to target/latest"
	ln -sfn $(if $(filter $(PROFILE),dev test),debug,$(PROFILE)) target/latest

.PHONY: nohup
nohup:	##H Build with nohup
	nohup $(MAKE) build SKIP_CONFIRM=1 > build.log 2>&1 &
	tail -n +1 -f build.log


.PHONY: deb
deb:	##H Build Debian package
	@echo "Build Debian package?"
	@$(MAKE) _confirm
	cargo-deb --release


.PHONY: clean
clean:	##H Clean build directory for current profile
	@echo "Clean the '$(PROFILE)' build directory?"
	@$(MAKE) _confirm
	cargo clean --profile $(PROFILE)
	@echo "Also remove debian build?"
	@$(MAKE) _confirm
	rm -rf target/debian


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Deployment commands
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

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
	install -b -p -m 755 "$(LOCAL_BIN)" "$(REMOTE_BIN)" || sudo install -b -p -m 755 "$(LOCAL_BIN)" "$(REMOTE_BIN)"
	@echo "Installation complete."
# 	@echo "Restarting $(CONTINUWUITY)"
# 	systemctl restart $(CONTINUWUITY) || sudo systemctl restart $(CONTINUWUITY)
