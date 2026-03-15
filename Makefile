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
#!/bin/bash
# export ROCKSDB_INCLUDE_DIR=/usr/local/include
# export ROCKSDB_LIB_DIR=/usr/local/lib
# export LD_LIBRARY_PATH=${ROCKSDB_LIB_DIR}:${LD_LIBRARY_PATH}
# #export CPU_TARGET=skylake
# export OS_VERSION=ubuntu-24.04
# export GH_REPO=.../continuwuity
# export SKIP_CONFIRM=1
# export PROFILE=dev-quick
# # export PROFILE=release-high-perf
# # export RUSTFLAGS="-C target-cpu=native"
# # export RUSTFLAGS="-C target-cpu=skylake"
# export COMPLEMENT_DIR="/.../complement-suite"
# export CONDUWUIT_CONFIG="/<...>/etc/conduwuit/conduwuit.toml"
# export CONDUWUIT_DATABASE_PATH=/var/lib/conduwuit-v18



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
		| awk 'BEGIN {FS = ":.*?##H "}; {printf "$(STYLE_CYAN)%-20s$(STYLE_RESET) %s\n", $$1, $$2}'


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
	git fetch --all --tags --dry-run

.PHONY: cpu-info
cpu-info: ##H Print CPU info relevant to native target-cpu
	@echo "=== CPU Model ==="
	@grep -m1 'model name' /proc/cpuinfo 2>/dev/null || sysctl -n machdep.cpu.brand_string 2>/dev/null || echo "unknown"
	@echo "=== Architecture ==="
	@uname -a
	@echo "=== rustc Host Target ==="
	@rustc -vV | grep host
	@echo "=== rustc Native CPU ==="
	@rustc --print=cfg -C target-cpu=native 2>/dev/null | grep target_feature | sort
	@echo "=== CPU Flags [from /proc/cpuinfo] ==="
	@grep -m1 'flags' /proc/cpuinfo 2>/dev/null | tr ' ' '\n' | grep -E 'avx|sse|aes|bmi|fma|popcnt|lzcnt|sha|pclmul' | sort

.PHONY: vars
vars: ##H Print debug info
	@$(foreach v, $(VARS), printf "$(STYLE_CYAN)%-25s$(STYLE_RESET) %s\n" "$(v)" "$($(v))";)
	@echo "... computing version."
	@ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
		ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
		LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
		printf "$(STYLE_CYAN)%-25s$(STYLE_RESET) %s\n" "VERSION" \
		"$$(cargo run --quiet -- --version-verbose)"


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Development commands
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

PROFILE ?=
p ?=
CRATE ?=
CARGO_SCOPE ?= $(if $(p),-p $(p),$(if $(CRATE),-p $(CRATE),--workspace))
CARGO_FLAGS ?= $(if $(PROFILE),--profile $(PROFILE),)


.PHONY: cargo/lock-init
cargo/lock-init:        ##H Init or fully upgrade the lockfile (wipes it)
	ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
		ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
		LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
	ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) cargo generate-lockfile
	@echo "OK."


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
#	ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
		ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
		LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
		cargo fix $(CARGO_SCOPE) $(CARGO_FLAGS) --features default --allow-dirty --allow-no-vcs

.PHONY: lint
lint:   ##H Lint code
	@echo "Lint code? PROFILE='$(PROFILE)'"
	@$(MAKE) _confirm
	ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
		ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
		LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
		cargo clippy $(CARGO_SCOPE) --features default --locked --no-deps --profile $(PROFILE) -- -D warnings

.PHONY: test
test:   ##H Run tests
	@echo "Run tests? PROFILE='$(PROFILE)'"
	@$(MAKE) _confirm
	ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
		ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
		LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
		cargo test $(CARGO_SCOPE) --features default --locked --profile $(PROFILE) --all-targets


ROCKSDB_LIB_DIR ?= /usr/local/lib
ROCKSDB_INCLUDE_DIR ?= /usr/local/include

# Default features for standard build
FEATURES ?= standard,release_max_log_level

.PHONY: build
build:  ##H Build with selected profile (standard dynamic)
	# NOTE: for a build that works best and ONLY for your CPU: export RUSTFLAGS=-C target-cpu=native
	@echo "Build this profile? PROFILE='$(PROFILE)' FEATURES='$(FEATURES)'"
	@$(MAKE) _confirm
	ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
		ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
		LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
		LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LIBRARY_PATH \
		ROCKSDB_STATIC=$(ROCKSDB_STATIC) \
		ROCKSDB_LIB_STATIC=$(ROCKSDB_LIB_STATIC) \
		RUSTFLAGS="-L $(ROCKSDB_LIB_DIR) -l snappy -l z -l bz2 -l lz4 -l zstd -l numa -l tbb -l uring -l stdc++ $$RUSTFLAGS" \
		cargo build --features $(FEATURES) --locked $(CARGO_FLAGS)
	@echo "Build finished! Hard-linking '$(PROFILE)' binary to target/latest/"
	mkdir -p target/latest target/debug
	-ln -f target/$(if $(filter $(PROFILE),dev test),debug,$(PROFILE))/conduwuit target/latest/conduwuit
	-ln -f target/$(if $(filter $(PROFILE),dev test),debug,$(PROFILE))/conduwuit target/debug/conduwuit


.PHONY: build-bundled
build-bundled: ##H Build a bundled binary (Static RocksDB)
	ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
	ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
	LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
	LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LIBRARY_PATH \
	ROCKSDB_STATIC=1 \
	ROCKSDB_LIB_STATIC=1 \
	$(MAKE) build FEATURES="$(FEATURES),bindgen-static"


.PHONY: build-dynamic
build-dynamic: ##H Build with shared library (requires librocksdb.so at runtime)
	ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
	ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
	LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
	$(MAKE) build FEATURES="$(FEATURES)"


.PHONY: release
release: ##H Build a production-ready bundled binary (High-performance, Static RocksDB)
	ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
	ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
	LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
	LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LIBRARY_PATH \
	ROCKSDB_STATIC=1 \
	ROCKSDB_LIB_STATIC=1 \
	$(MAKE) build PROFILE=release-max-perf FEATURES="$(FEATURES),bindgen-static"


.PHONY: clean
clean:  ##H Clean build directory
	@echo "Clean everything?"
	@$(MAKE) _confirm
	cargo clean
	-rm -rf target/latest target/debug
# Old logic, wipes it out too much, results in slow builds
#       cargo clean --features default --profile $(PROFILE)
#       @echo "Also remove debian build?"
#       @$(MAKE) _confirm
#       rm -rf target/debian


.PHONY: build-docs
build-docs:     ##H Regenerate docs (admin commands, etc.)
	@echo "Regenerate docs with PROFILE='$(PROFILE)'?"
	@$(MAKE) _confirm
	ROCKSDB_INCLUDE_DIR=$(ROCKSDB_INCLUDE_DIR) \
		ROCKSDB_LIB_DIR=$(ROCKSDB_LIB_DIR) \
		LD_LIBRARY_PATH=$(ROCKSDB_LIB_DIR):$$LD_LIBRARY_PATH \
		cargo run -p xtask --profile $(PROFILE) -- generate-docs


# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# Complement (build docker, stats, run... also used by CI)
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

COMPLEMENT_DIR ?=
COMPLEMENT_IMAGE ?= continuwuity:complement
COMPLEMENT_BASE_IMAGE ?= ubuntu:latest

.PHONY: complement/build
complement/build: ##H Build conduwuit w direct_tls
	@echo "Building conduwuit binary with direct_tls feature for Complement..."
	@$(MAKE) _confirm
	$(MAKE) build PROFILE=$(PROFILE) CARGO_FLAGS="--profile $(PROFILE) --features direct_tls"

.PHONY: complement/docker
complement/docker: ##H Build docker image from existing binary
	@echo "Copying dynamically linked libraries to target/$(if $(filter $(PROFILE),dev test),debug,$(PROFILE))/lib/..."
	@mkdir -p target/$(if $(filter $(PROFILE),dev test),debug,$(PROFILE))/lib && rm -f target/$(if $(filter $(PROFILE),dev test),debug,$(PROFILE))/lib/*
	@LD_LIBRARY_PATH="$(ROCKSDB_LIB_DIR):$(LD_LIBRARY_PATH)" \
		ldd target/latest/conduwuit | awk '/=> \// {print $$3}' \
		| grep -vE 'libc\.so|libm\.so|libgcc_s\.so|libstdc\+\+\.so|ld-linux|libdl\.so|libpthread\.so|librt\.so' \
		| xargs -I {} cp "{}" target/$(if $(filter $(PROFILE),dev test),debug,$(PROFILE))/lib/ || true
	@rm -rf target/latest/lib
	@ln -sfn ../$(if $(filter $(PROFILE),dev test),debug,$(PROFILE))/lib target/latest/lib
	@echo "Building Complement Docker image using base image: $(COMPLEMENT_BASE_IMAGE)..."
	DOCKER_BUILDKIT=1 docker buildx build \
		--build-arg BASE_IMAGE=$(COMPLEMENT_BASE_IMAGE) \
		--build-arg BINARY_PATH=target/latest/conduwuit \
		--build-arg LIB_PATH=target/$(if $(filter $(PROFILE),dev test),debug,$(PROFILE))/lib \
		--build-arg UID=$(shell id -u) \
		--build-arg GID=$(shell id -g) \
		-t $(COMPLEMENT_IMAGE) \
		-f ./docker/complement.Dockerfile \
		--load .

.PHONY: complement/run
complement/run: ##H Run Complement docker tests locally (requires COMPLEMENT_DIR)
	@test -d "$(COMPLEMENT_DIR)" || (echo "ERROR: COMPLEMENT_DIR ($(COMPLEMENT_DIR)) does not exist" && exit 1)
	@echo "Running Complement tests from $(COMPLEMENT_DIR)..."
	./bin/complement $(COMPLEMENT_DIR)


.PHONY: complement/stats
complement/stats: ##H Check local test stats
	@test -f "tests/test_results/complement/test_results.jsonl" || (echo "ERROR: tests/test_results/complement/test_results.jsonl does not exist" && exit 1)
	@echo "Parsing Complement test results..."
	@PASS=$$(jq -s '[.[] | select(.Action == "pass")] | length' tests/test_results/complement/test_results.jsonl); \
	FAIL=$$(jq -s '[.[] | select(.Action == "fail")] | length' tests/test_results/complement/test_results.jsonl); \
	SKIP=$$(jq -s '[.[] | select(.Action == "skip")] | length' tests/test_results/complement/test_results.jsonl); \
	TOTAL=$$((PASS + FAIL + SKIP)); \
	echo ""; \
	if [ "$$FAIL" -gt 0 ] && [ "$$VERBOSE" = "1" ]; then \
		echo "Failed Tests:"; \
		jq -r 'select(.Action == "fail") | .Test' tests/test_results/complement/test_results.jsonl | sort -u; \
		echo ""; \
	fi; \
	echo "=== Complement Test Stats ==="; \
	echo "✓ Passed:  $$PASS"; \
	echo "✗ Failed:  $$FAIL"; \
	echo "○ Skipped: $$SKIP"; \
	echo "---------------------------"; \
	echo "Total:     $$TOTAL"; \
	echo ""; \
	echo "JSON file (on main) last modified by: "; \
	git log -1 --format="%an (%ad) %H" origin/main -- tests/test_results/complement/test_results.jsonl

.PHONY: complement/logs
complement/logs: ##H Tail logs for all running and future Complement containers
	@echo "Tailing logs for Complement containers (Press Ctrl+C to stop)..."
	@docker ps -q --filter "name=complement_" | xargs -r -n 1 docker logs -f & \
	docker events --filter 'event=start' --format '{{.Actor.Attributes.name}}' | grep --line-buffered "^complement_" | xargs -r -I {} sh -c 'echo "--- Tailing {} ---"; docker logs -f {} &' ; wait



# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
# GitHub CI/build related targets
# ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

GH_REPO ?=

GH_CACHE_KEY ?=

.PHONY: download/clear-cache
download/clear-cache: ##H Delete GitHub Actions caches
	# Testing you have explicitly set GH_CACHE_KEY
	@if [ -z "$(GH_CACHE_KEY)" ]; then \
		gh cache list -R "$(GH_REPO)" --limit 100; \
		echo ""; \
		echo "Hint: Use GH_CACHE_KEY=prefix to delete caches."; \
	else \
		echo "Deleting GitHub Actions caches matching prefix: $(GH_CACHE_KEY)..."; \
		gh cache list -R "$(GH_REPO)" --key "$(GH_CACHE_KEY)" --limit 1000 | awk '{print $$1}' \
			| grep -v 'ID' | grep -v 'KEYS' \
			| xargs -r -I {} sh -c 'echo "Deleting cache: {}" && gh cache delete -R "$(GH_REPO)" {}'; \
	fi


CPU_TARGET ?=
OS_VERSION ?=

GH_ARTIFACT_NAME ?= conduwuit
GH_ARTIFACT_SUFFIX ?= $(CPU_TARGET)-$(OS_VERSION)
ARTIFACT ?= $(if $(and $(CPU_TARGET),$(OS_VERSION)),$(GH_ARTIFACT_NAME)$(GH_ARTIFACT_SUFFIX),)

RUN ?=
N_RUNS ?= 6

.PHONY: download
download:	##H Download CI binary (set RUN to a specific RunID)
	# Testing whether GH_REPO is set
	test "$(GH_REPO)"
	# Testing whether ARTIFACT related vars are set
	(test "$(OS_VERSION)" && test "$(CPU_TARGET)" ) || test "$(ARTIFACT)"
	@mkdir -p target/ci
	# Checking version of old binary, if it exists
	@-./target/ci/conduwuit -V
	@rm -f target/ci/conduwuit
	gh run download $(RUN) -R $(GH_REPO) -n $(ARTIFACT) -D target/ci
	@chmod +x target/ci/conduwuit
	@echo "Downloaded to target/ci/conduwuit"
	@./target/ci/conduwuit -V
	@ln -sfn ci target/latest

.PHONY: download/list
download/list:	##H List recent CI runs
	@test "$(GH_REPO)" || (echo "ERROR: GH_REPO is not set. Add GH_REPO=owner/repo to .env" && exit 1)
	mkdir -p .tmp && echo '**' > .tmp/.gitignore
	# gh run list -R $(GH_REPO) --limit $(N_RUNS)
	@echo "Fetching latest $(N_RUNS) runs and their artifacts in parallel..."
	@echo ""
	@gh run list -R "$(GH_REPO)" --limit $(N_RUNS) --json databaseId,workflowName,headBranch,headSha,event,conclusion,status > .tmp/runs.json
	@bash -c ' \
	for ID in $$(jq -r ".[].databaseId" .tmp/runs.json); do \
		gh api "repos/$(GH_REPO)/actions/runs/$$ID/artifacts" --jq ".artifacts[].name" > ".tmp/artifacts_$$ID.txt" & \
	done; \
	wait; \
	jq -c ".[]" .tmp/runs.json | while read -r run; do \
		ID=$$(echo "$$run" | jq -r ".databaseId"); \
		STATUS=$$(echo "$$run" | jq -r "if .status == \"completed\" then .conclusion else .status end"); \
		WORKFLOW=$$(echo "$$run" | jq -r ".workflowName"); \
		BRANCH=$$(echo "$$run" | jq -r ".headBranch"); \
		SHA=$$(echo "$$run" | jq -r ".headSha" | cut -c 1-7); \
		ICON="-"; \
		if [ "$$STATUS" = "success" ]; then ICON="✓"; fi; \
		if [ "$$STATUS" = "failure" ]; then ICON="X"; fi; \
		if [ "$$STATUS" = "in_progress" ]; then ICON="*"; fi; \
		printf "%-2s %-20s %-30s %-15s (ID: %s)\n" "$$ICON" "$$WORKFLOW" "$$BRANCH ($$SHA)" "$$STATUS" "$$ID"; \
		if [ -s ".tmp/artifacts_$$ID.txt" ]; then \
			while read -r artifact; do \
				echo "    └─ $$artifact"; \
			done < ".tmp/artifacts_$$ID.txt"; \
		else \
			echo "    └─ (No artifacts)"; \
		fi; \
		echo ""; \
		rm -f ".tmp/artifacts_$$ID.txt"; \
	done'
	@rm -f .tmp/runs.json

# Binary name
CONTINUWUITY ?= conduwuit
# systemctl service name
C10Y_SERV ?= conduwuit.service

# Configure these in .env if alternate path(s) are desired
BUILD_BIN_DIR ?= target/latest
DEPLOY_BIN_DIR ?= /usr/local/bin

BUILD_BIN ?= $(BUILD_BIN_DIR)/$(CONTINUWUITY)
DEPLOY_BIN ?= $(DEPLOY_BIN_DIR)/$(CONTINUWUITY)

.PHONY: install
install:	##H Install (executed on VPS)
	@echo "Install $(CONTINUWUITY) to $(DEPLOY_BIN)?"
	@$(MAKE) _confirm
	# You may need to run with sudo or adjust REMOTE_BIN_DIR if this fails
	install -b -p -m 755 $(BUILD_BIN) $(DEPLOY_BIN)
	@echo "Installation complete."


.PHONY: restart
restart:    ##H Restart service (using systemctl)
	sudo systemctl restart $(C10Y_SERV)
