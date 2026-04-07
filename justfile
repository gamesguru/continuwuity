# List available commands
_help:
    @just --list

# Central metadata for C/C++ dependencies
CSV := ".github/ellis_link_deps.csv"

# --- Pre-building C/C++ Libraries ---
# Note: Building these from source avoids Cargo constantly recompiling them
# and trashing your target/ directory. After building, use the install commands
# to make them available to Cargo, RustRover, and your system.

# Initialize the global build directory
# Note: We intentionally do NOT register /usr/local/uwu/lib in the global
# /etc/ld.so.conf.d/ to prevent shadowing system libraries (like zstd/lz4).
# We expose it locally via .envrc and .cargo/config.toml instead.
init-prebuild:
    @echo "Creating /usr/local/uwu/build and assigning ownership to $USER... (Requires sudo)"
    sudo mkdir -p /usr/local/uwu/build /usr/local/uwu/lib /usr/local/uwu/include /usr/local/uwu/bin
    sudo chown -R $USER:$USER /usr/local/uwu/build /usr/local/uwu/lib /usr/local/uwu/include /usr/local/uwu/bin
    @echo "Done. You can now run prebuild commands."

# Pre-build all C/C++ dependencies
prebuild-all: init-prebuild prebuild-jemalloc prebuild-lz4 prebuild-snappy prebuild-zstd prebuild-rocksdb

# Install all pre-built C/C++ dependencies
install-all: install-jemalloc install-lz4 install-snappy install-zstd install-rocksdb

# Builds liburing
prebuild-liburing:
    #!/usr/bin/env bash
    set -e
    TAG=$(grep "^liburing," {{CSV}} | cut -d',' -f4 | tr -d '\r')
    REPO=$(grep "^liburing," {{CSV}} | cut -d',' -f3 | tr -d '\r')
    sudo mkdir -p /usr/local/uwu/build && sudo chown -R $USER:$USER /usr/local/uwu/build
    echo "Cloning and building liburing $TAG..."
    [ ! -d "/usr/local/uwu/build/liburing" ] && git clone $REPO /usr/local/uwu/build/liburing || true
    cd /usr/local/uwu/build/liburing
    git fetch --all --tags
    git checkout $TAG
    ./configure --prefix=/usr/local/uwu
    make -j$(nproc)

# Installs liburing
install-liburing:
    @echo "Installing liburing (requires sudo)..."
    cd /usr/local/uwu/build/liburing && sudo make install
    @echo "Done!"

# Builds bzip2
prebuild-bzip2:
    #!/usr/bin/env bash
    set -e
    TAG=$(grep "^bzip2," {{CSV}} | cut -d',' -f4 | tr -d '\r')
    REPO=$(grep "^bzip2," {{CSV}} | cut -d',' -f3 | tr -d '\r')
    sudo mkdir -p /usr/local/uwu/build && sudo chown -R $USER:$USER /usr/local/uwu/build
    echo "Cloning and building bzip2 $TAG..."
    [ ! -d "/usr/local/uwu/build/bzip2" ] && git clone $REPO /usr/local/uwu/build/bzip2 || true
    cd /usr/local/uwu/build/bzip2
    git fetch --all --tags
    git checkout $TAG
    make -f Makefile-libbz2_so
    make

# Installs bzip2
install-bzip2:
    @echo "Installing bzip2 (requires sudo)..."
    cd /usr/local/uwu/build/bzip2 && sudo make install PREFIX=/usr/local/uwu
    cd /usr/local/uwu/build/bzip2 && sudo cp -f libbz2.so.1.0.* /usr/local/uwu/lib/
    cd /usr/local/uwu/build/bzip2 && sudo ln -sf /usr/local/uwu/lib/libbz2.so.1.0.* /usr/local/uwu/lib/libbz2.so
    @echo "Done! Installed libbz2.so to /usr/local/uwu/lib"

# Pre-build jemalloc
prebuild-jemalloc:
    #!/usr/bin/env bash
    set -e
    TAG=$(grep "^jemalloc," {{CSV}} | cut -d',' -f4 | tr -d '\r')
    REPO=$(grep "^jemalloc," {{CSV}} | cut -d',' -f3 | tr -d '\r')
    sudo mkdir -p /usr/local/uwu/build && sudo chown -R $USER:$USER /usr/local/uwu/build
    echo "Cloning jemalloc $TAG..."
    [ ! -d "/usr/local/uwu/build/jemalloc" ] && git clone $REPO /usr/local/uwu/build/jemalloc || true
    echo "Building jemalloc..."
    cd /usr/local/uwu/build/jemalloc
    git fetch --all --tags
    git checkout $TAG
    [ -f configure ] || ./autogen.sh --prefix=/usr/local/uwu
    grep -q "prefix = /usr/local/uwu" Makefile 2>/dev/null || ./configure --prefix=/usr/local/uwu
    make -j$(nproc)

# Install jemalloc globally (requires sudo)
install-jemalloc:
    @echo "Installing jemalloc to /usr/local/uwu... (Requires sudo)"
    cd /usr/local/uwu/build/jemalloc && sudo make install_lib_static install_lib_shared install_include

# Pre-build lz4
prebuild-lz4:
    #!/usr/bin/env bash
    set -e
    TAG=$(grep "^lz4," {{CSV}} | cut -d',' -f4 | tr -d '\r')
    REPO=$(grep "^lz4," {{CSV}} | cut -d',' -f3 | tr -d '\r')
    sudo mkdir -p /usr/local/uwu/build && sudo chown -R $USER:$USER /usr/local/uwu/build
    echo "Cloning lz4 $TAG..."
    [ ! -d "/usr/local/uwu/build/lz4" ] && git clone $REPO /usr/local/uwu/build/lz4 || true
    echo "Building lz4..."
    cd /usr/local/uwu/build/lz4
    git fetch --all --tags
    git checkout $TAG
    make lib -j$(nproc)

# Install lz4 globally (requires sudo)
install-lz4:
    @echo "Installing lz4 to /usr/local/uwu... (Requires sudo)"
    cd /usr/local/uwu/build/lz4 && sudo make install PREFIX=/usr/local/uwu

# Pre-build RocksDB shared and statically
prebuild-rocksdb:
    #!/usr/bin/env bash
    set -e
    # satisfy build_detect_platform if hostname is missing
    if ! command -v hostname >/dev/null 2>&1; then
        hostname() { uname -n; }
        export -f hostname
    fi
    TAG=$(grep "^rocksdb," {{CSV}} | cut -d',' -f4 | tr -d '\r' || true)
    if [ -z "$TAG" ]; then
        TAG="continuwuity-v0.5.0"
    fi
    REPO=$(grep "^rocksdb," {{CSV}} | cut -d',' -f3 | tr -d '\r' || true)
    if [ -z "$REPO" ]; then
        REPO="https://forgejo.ellis.link/continuwuation/rocksdb.git"
    fi
    sudo mkdir -p /usr/local/uwu/build && sudo chown -R $USER:$USER /usr/local/uwu/build
    echo "Cloning rocksdb $TAG..."
    if [ ! -d "/usr/local/uwu/build/rocksdb" ]; then
        git clone --recursive "$REPO" /usr/local/uwu/build/rocksdb
    fi
    echo "Building RocksDB..."
    cd /usr/local/uwu/build/rocksdb

    # Use --all --tags to support arbitrary commit hashes from the CSV
    git fetch --all --tags
    git checkout "$TAG"

    # Disable ccache auto-detection ONLY if we are already using sccache
    if [[ "$CC" == *"sccache"* ]]; then
        export USE_CCACHE=0
    fi

    # Build core libraries explicitly WITHOUT RTTI
    env ROCKSDB_NO_FBCODE=1 DISABLE_JEMALLOC=1 EXTRA_CXXFLAGS="${EXTRA_CXXFLAGS:-} -I/usr/local/uwu/include -Wno-error=unused-parameter" EXTRA_LDFLAGS="-L/usr/local/uwu/lib" PORTABLE=0 USE_RTTI=1 make shared_lib static_lib -j$(nproc)

    # Build ldb
    # env DISABLE_WARNING_AS_ERROR=1 DEBUG_LEVEL=0 USE_RTTI=1 DISABLE_SNAPPY=1 make ldb
    env DISABLE_WARNING_AS_ERROR=1 DEBUG_LEVEL=0 USE_RTTI=1 make ldb

# Install RocksDB globally (requires sudo)
install-rocksdb:
    @echo "Installing RocksDB to /usr/local/uwu... (Requires sudo)"
    cd /usr/local/uwu/build/rocksdb && sudo make install-shared PREFIX=/usr/local/uwu
    cd /usr/local/uwu/build/rocksdb && sudo make install-static PREFIX=/usr/local/uwu
    sudo install -m 755 /usr/local/uwu/build/rocksdb/ldb /usr/local/uwu/bin/ldb
    sudo ldconfig
    @echo "Remember to set ROCKSDB_LIB_DIR=/usr/local/uwu/lib if Cargo doesn't see it."

# Pre-build snappy
prebuild-snappy:
    #!/usr/bin/env bash
    set -e
    TAG=$(grep "^snappy," {{CSV}} | cut -d',' -f4 | tr -d '\r')
    REPO=$(grep "^snappy," {{CSV}} | cut -d',' -f3 | tr -d '\r')
    sudo mkdir -p /usr/local/uwu/build && sudo chown -R $USER:$USER /usr/local/uwu/build
    echo "Cloning snappy $TAG..."
    if [ ! -d "/usr/local/uwu/build/snappy" ]; then
        git clone $REPO /usr/local/uwu/build/snappy
    fi
    echo "Building snappy..."
    cd /usr/local/uwu/build/snappy
    git fetch origin
    git checkout $TAG
    sed -i 's/cmake_minimum_required(VERSION 3.1)/cmake_minimum_required(VERSION 3.10)/' CMakeLists.txt
    mkdir -p build_static && cd build_static
    cmake -DCMAKE_INSTALL_PREFIX=/usr/local/uwu -DBUILD_SHARED_LIBS=OFF -DSNAPPY_BUILD_TESTS=OFF -DSNAPPY_BUILD_BENCHMARKS=OFF ..
    make -j$(nproc)
    cd ..
    mkdir -p build_shared && cd build_shared
    cmake -DCMAKE_INSTALL_PREFIX=/usr/local/uwu -DBUILD_SHARED_LIBS=ON -DSNAPPY_BUILD_TESTS=OFF -DSNAPPY_BUILD_BENCHMARKS=OFF ..
    make -j$(nproc)

# Install snappy globally (requires sudo)
install-snappy:
    @echo "Installing snappy to /usr/local/uwu... (Requires sudo)"
    cd /usr/local/uwu/build/snappy/build_static && sudo make install
    cd /usr/local/uwu/build/snappy/build_shared && sudo make install
    sudo ldconfig

# Pre-build zstd
prebuild-zstd:
    #!/usr/bin/env bash
    set -e
    TAG=$(grep "^zstd," {{CSV}} | cut -d',' -f4 | tr -d '\r')
    REPO=$(grep "^zstd," {{CSV}} | cut -d',' -f3 | tr -d '\r')
    sudo mkdir -p /usr/local/uwu/build && sudo chown -R $USER:$USER /usr/local/uwu/build
    echo "Cloning zstd $TAG..."
    [ ! -d "/usr/local/uwu/build/zstd" ] && git clone $REPO /usr/local/uwu/build/zstd || true
    echo "Building zstd..."
    cd /usr/local/uwu/build/zstd
    git fetch --all --tags
    git checkout $TAG
    make lib-release -j$(nproc)

# Install zstd globally (requires sudo)
install-zstd:
    @echo "Installing zstd to /usr/local/uwu... (Requires sudo)"
    cd /usr/local/uwu/build/zstd && sudo make install -C lib PREFIX=/usr/local/uwu
    sudo ldconfig

# --- CPU Profiling ---

# Run CPU flamegraph profiling on release build (requires sudo for perf)
profile-runtime-cpu *args:
    cargo flamegraph --root --features local_profiling --bin conduwuit -- {{args}}
    @echo "Flamegraph saved to flamegraph.svg"

# Run CPU flamegraph profiling on dev build (requires sudo for perf)
profile-runtime-cpu-dev *args:
    cargo flamegraph --root --dev --features local_profiling --bin conduwuit -- {{args}}
    @echo "Flamegraph saved to flamegraph.svg"

# --- Async & I/O Profiling ---

# Run with tokio-console instrumentation active
profile-runtime-async *args:
    @echo "Run 'tokio-console' in a separate terminal"
    env RUSTFLAGS="--cfg tokio_unstable ${RUSTFLAGS:-}" cargo run --features local_profiling --bin conduwuit -- {{args}}

# --- Memory Profiling (jemalloc) ---

# Run release build and dump jemalloc heap profiles
profile-runtime-mem *args:
    cargo build --release --features local_profiling --bin conduwuit
    @echo "Starting with jemalloc profiling..."
    env MALLOC_CONF="prof:true,lg_prof_interval:24,prof_prefix:jeprof.out" ./target/release/conduwuit {{args}}

# Generate heap_profile.svg from collected jemalloc dumps
profile-runtime-mem-analyze:
    jeprof --svg ./target/release/conduwuit jeprof.out.*
    @echo "Saved heap_profile.svg"

# Clean up jemalloc dump files
profile-runtime-mem-clean:
    rm -f jeprof.out.* heap_profile.svg

# --- Compile-time Profiling ---

# Profile cargo build times
profile-build-times:
    cargo build --profile ${PROFILE:-release} --timings
    @echo "Report saved to target/cargo-timings/"

# Analyze binary size by crates
profile-build-bloat-crates:
    cargo bloat --profile ${PROFILE:-release} -p conduwuit --crates

# Analyze binary size by functions
profile-build-bloat-functions:
    cargo bloat --profile ${PROFILE:-release} -p conduwuit --bin conduwuit -n 50

# Analyze generic instantiation (Monomorphization)
profile-build-llvm-lines:
    cargo llvm-lines --profile ${PROFILE:-release} -p conduwuit --lib

# --- Build targets ---

# Build dev (default,console,url_preview)
build-dev:
    cargo build --profile dev --features default,console,url_preview

# --- Cross Compilation ---

# Cross-compile using cargo-zigbuild for specific glibc versions
# Usage: just build-cross-compile <target-glibc-version> <cpu-arch>
# Example: just build-cross-compile 2.36 skylake
build-cross-compile glibc_version="2.36" cpu_arch="skylake":
    @echo "Building for glibc {{glibc_version}} with CPU target {{cpu_arch}} using cargo-zigbuild..."
    @if ! command -v cargo-zigbuild >/dev/null 2>&1; then \
        echo "Error: cargo-zigbuild is not installed. Run: cargo install cargo-zigbuild"; \
        exit 1; \
    fi
    @if ! command -v zig >/dev/null 2>&1; then \
        echo "Error: zig is not installed. Run: sudo pacman -S zig (or your package manager's equivalent)"; \
        exit 1; \
    fi
    rustup target add x86_64-unknown-linux-gnu
    env RUSTFLAGS="-C target-cpu={{cpu_arch}}" cargo zigbuild --release --target x86_64-unknown-linux-gnu.{{glibc_version}}

# Extracts the workspace version from Cargo.toml
version := "$(grep -m1 '^version = ' Cargo.toml | cut -d \" -f 2)"

# Start gdbserver for lightweight remote debugging (POC)
# Usage: just remote-debug-poc /path/to/conduwuit.toml
remote-debug-poc config="conduwuit-example.toml":
    @echo "Starting gdbserver on :1234 using config: {{config}}"
    sudo -u conduwuit gdbserver :1234 ./target/debug/continuwuity --config {{config}}

# Run Complement tests (requires complement-src)
# Usage: just complement TestName
complement args=".":
    env COMPLEMENT_ALWAYS_PRINT_SERVER_LOGS=1 COMPLEMENT_RUN="{{args}}" ./bin/complement ./complement-src

# -----------------------------------------------------------------------------
# Complement CI
# -----------------------------------------------------------------------------

COMPLEMENT_IMAGE := env_var_or_default("COMPLEMENT_IMAGE", "continuwuity:complement")
COMPLEMENT_BASE_IMAGE := env_var_or_default("COMPLEMENT_BASE_IMAGE", "ubuntu:latest")
PROFILE := env_var_or_default("PROFILE", "release")

# Build docker image from existing binary
ci-complement-docker:
    #!/usr/bin/env bash
    set -euo pipefail

    echo "Copying dynamically linked libraries to target/{{PROFILE}}/lib/..."
    mkdir -p target/{{PROFILE}}/lib && rm -f target/{{PROFILE}}/lib/*

    LD_LIBRARY_PATH="${ROCKSDB_LIB_DIR:-}:$(echo ${LD_LIBRARY_PATH:-})" \
        ldd target/latest/conduwuit | awk '/=> \// {print $3}' \
        | grep -vE 'libc\.so|libm\.so|libgcc_s\.so|libstdc\+\+\.so|ld-linux|libdl\.so|libpthread\.so|librt\.so' \
        | xargs -I {} cp "{}" target/{{PROFILE}}/lib/ || true

    rm -rf target/latest/lib
    ln -sfn ../{{PROFILE}}/lib target/latest/lib

    echo "Building Complement Docker image using base image: {{COMPLEMENT_BASE_IMAGE}}..."
    DOCKER_BUILDKIT=1 docker buildx build \
            --build-arg BASE_IMAGE={{COMPLEMENT_BASE_IMAGE}} \
            --build-arg BINARY_PATH=target/latest/conduwuit \
            --build-arg LIB_PATH=target/{{PROFILE}}/lib \
            --build-arg UID="$(id -u)" \
            --build-arg GID="$(id -g)" \
            -t {{COMPLEMENT_IMAGE}} \
            -f ./docker/complement.Dockerfile \
            --load .

# Aggregates test results generated by complement
ci-complement-stats:
    #!/usr/bin/env bash
    set -euo pipefail

    RESULTS="tests/test_results/complement/test_results.jsonl"
    if [ ! -f "$RESULTS" ]; then
        echo "ERROR: $RESULTS does not exist"
        exit 1
    fi

    echo "Parsing Complement test results..."
    PASS=$(jq -s '[.[] | select(.Action == "pass")] | length' "$RESULTS")
    FAIL=$(jq -s '[.[] | select(.Action == "fail")] | length' "$RESULTS")
    SKIP=$(jq -s '[.[] | select(.Action == "skip")] | length' "$RESULTS")
    TOTAL=$((PASS + FAIL + SKIP))

    echo ""
    if [ "$FAIL" -gt 0 ] && [ "${VERBOSE:-0}" = "1" ]; then
        echo "Failed Tests:"
        jq -r 'select(.Action == "fail") | .Test' "$RESULTS" | sort -u
        echo ""
    fi

    echo "=== Complement Test Stats ==="
    echo "✓ Passed:  $PASS"
    echo "✗ Failed:  $FAIL"
    echo "⚠ Skipped: $SKIP"
    echo "Overall:   $TOTAL tests"

    echo ""
    echo "Last modified by:"
    git log -5 --format="%an (%ad) %H" origin/main -- tests/test_results/complement/test_results.jsonl

# -----------------------------------------------------------------------------
# CI Database Queries
# -----------------------------------------------------------------------------

# Query the CI run regressions view via DB shell.
# Usage:
#   just ci-query-failures limit=100 order=run_date asc like=branch_name baseline=123
ci-query-failures +args="":
    #!/usr/bin/env bash
    ./.github/actions/postgres/ci-query-failures.py {{args}}
