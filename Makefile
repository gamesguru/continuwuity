VPS_USER ?= gg
VPS_HOST ?= dev.nutra.tk
VPS := $(VPS_USER)@$(VPS_HOST)
LOCAL_BIN_NAME := conduwuit
REMOTE_BIN_NAME := conduwuit
LOCAL_BIN := target/release/$(LOCAL_BIN_NAME)
REMOTE_BIN := /usr/bin/$(REMOTE_BIN_NAME)

.PHONY: build
build:
	cargo build --release

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
	sudo mv $(LOCAL_BIN) $(REMOTE_BIN)
	@echo "Restarting $(REMOTE_BIN_NAME) service..."
	sudo systemctl restart $(REMOTE_BIN_NAME)
	@echo "Installation complete."
