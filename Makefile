VPS_USER ?= gg
VPS_HOST ?= dev.nutra.tk
VPS ?= $(VPS_USER)@$(VPS_HOST)
LOCAL_BIN_NAME ?= conduwuit

PROFILE ?= release
CARGO_FLAGS = --release
BIN_DIR = target/release

ifeq ($(PROFILE),debug)
	CARGO_FLAGS =
	BIN_DIR = target/debug
endif

ifeq ($(PROFILE),release-fast)
	CARGO_FLAGS = --profile release-fast
	BIN_DIR = target/release-fast
endif

# Local backup directory (relative to user's home)
LOCAL_BACKUP_DIR = $(HOME)/$(BACKUP_DIR_BASE)

.PHONY: build
build:
	cargo build $(CARGO_FLAGS)

.PHONY: deploy
deploy: build
	@echo "Deploying $(LOCAL_BIN_NAME) to $(VPS) as $(REMOTE_BIN_NAME)..."
	scp $(LOCAL_BIN) $(VPS):/tmp/$(REMOTE_BIN_NAME)
	ssh -t $(VPS) "sudo mv /tmp/$(REMOTE_BIN_NAME) $(REMOTE_BIN) && sudo systemctl restart $(REMOTE_BIN_NAME)"
	@echo "Deployment complete."

.PHONY: install
install:
	@echo "Installing $(LOCAL_BIN_NAME) to $(REMOTE_BIN)..."
	@if [ ! -f $(LOCAL_BIN) ]; then echo "Error: $(LOCAL_BIN) not found. Run 'cargo build --release' first."; exit 1; fi
	@echo "Checking systemd service..."
	@if ! cmp -s pkg/conduwuit.service /etc/systemd/system/conduwuit.service; then \
		echo "Error: pkg/conduwuit.service differs from installed version."; \
		echo "Automatic update not possible. Please update /etc/systemd/system/conduwuit.service manually."; \
		exit 1; \
	else \
		echo "Service file unchanged."; \
	fi
	@echo "Creating backup directory $(LOCAL_BACKUP_DIR)..."
	@mkdir -p $(LOCAL_BACKUP_DIR)
	@if [ -f $(REMOTE_BIN) ]; then \
		CURRENT_VER=$$($(REMOTE_BIN) --version | awk '{print $$2}'); \
		echo "Backing up existing binary to $(LOCAL_BACKUP_DIR)/$(LOCAL_BIN_NAME)-$$CURRENT_VER..."; \
		cp -p $(REMOTE_BIN) $(LOCAL_BACKUP_DIR)/$(LOCAL_BIN_NAME)-$$CURRENT_VER; \
	fi
	@echo "Moving new binary..."
	sudo mv $(LOCAL_BIN) $(REMOTE_BIN)
	@echo "Restarting $(LOCAL_BIN_NAME)..."
	sudo systemctl restart $(LOCAL_BIN_NAME)
	@echo "Installation complete."

.PHONY: git/hooks
# Helper to update the git hook on the server (run locally)
git/hooks:
	@echo "Deploying post-receive hook to git@$(VPS_HOST)..."
	scp -p scripts/post-receive git@$(VPS_HOST):repos/home.shane.repos.continuwuity.git/hooks/post-receive
	ssh git@$(VPS_HOST) "chmod +x repos/home.shane.repos.continuwuity.git/hooks/post-receive"
	@echo "Hook deployed."
	@echo "Copying pre-push hook to local .git/hooks..."
	cp scripts/pre-push .git/hooks/pre-push
	chmod +x .git/hooks/pre-push
	@echo "Hook copied."
