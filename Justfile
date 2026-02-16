set shell := ["bash", "-c"]
set dotenv-load

# Default goal
default:
    @just --list

# Variables
export PROFILE := env_var_or_default("PROFILE", "")
export CONTINUWUITY := env_var_or_default("CONTINUWUITY", "conduwuit")
export CARGO_FLAGS := env_var_or_default("CARGO_FLAGS", if PROFILE != "" { "--profile " + PROFILE } else { "" })

# Logic for bin dirs
local_bin_dir := if PROFILE == "dev" || PROFILE == "test" { "target/debug" } else { "target/" + PROFILE }
local_bin := local_bin_dir / CONTINUWUITY

remote_bin_dir := "/usr/local/bin"
remote_bin := remote_bin_dir / CONTINUWUITY

# Helper to check profile
[private]
_profile_check:
    @just version
    @if [ -z "$PROFILE" ]; then echo "ERROR: Please set PROFILE on command line or in .env"; exit 1; fi
    @if [ -t 0 ]; then read -p "Continue with PROFILE={{PROFILE}}? Press [Enter] to continue or Ctrl+C to abort..." _; fi

# List available cargo profiles
profiles:
    @grep "^\[profile\." Cargo.toml | sed 's/\[profile\.//;s/\]//' | grep -v 'package' | grep -v 'build-override' | sort

# Print the version
version:
    cargo run -p conduwuit_build_metadata --bin conduwuit-version --quiet

# Run pre-commit hooks/formatter
format:
    pre-commit run --all-files

# Lint code
lint: _profile_check
    cargo clippy --workspace --features full --locked --no-deps --profile $PROFILE -- -D warnings

# Run tests
test: _profile_check
    cargo test --workspace --features full --locked --profile $PROFILE --all-targets

# Build with PROFILE=
build: _profile_check
    cargo build $CARGO_FLAGS

# Clean build directory
clean: _profile_check
    @if [ -t 0 ]; then read -p "Cleaning $PROFILE build directory. Press [Enter] to continue or Ctrl+C to abort..." _; fi
    cargo clean
    rm -rf target/debian

# Deploy/Install
deploy-install: version
    cargo run -p conduwuit_build_metadata --bin conduwuit-deploy
