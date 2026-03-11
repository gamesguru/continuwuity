# List available commands
default:
    @just --list

# --- Pre-building C/C++ Libraries ---
# Note: Building these from source avoids Cargo constantly recompiling them
# and trashing your target/ directory. After building, use the install commands
# to make them available to Cargo, RustRover, and your system.

# Initialize the global build directory
init-prebuild:
    @echo "Creating /usr/local/build and assigning ownership to $USER... (Requires sudo)"
    sudo mkdir -p /usr/local/build
    sudo chown -R $USER:$USER /usr/local/build
    @echo "Done. You can now run prebuild commands."

# Install all pre-built C/C++ dependencies
install-all: install-rocksdb install-jemalloc install-zstd install-lz4

# Pre-build RocksDB shared and statically
prebuild-rocksdb:
    #!/usr/bin/env bash
    set -e
    VER=$(cargo pkgid rust-librocksdb-sys | cut -d'@' -f2 | cut -d'+' -f2)
    TAG="v$VER"
    mkdir -p /usr/local/build
    if [ -d "/usr/local/build/rocksdb" ]; then
        CURRENT=$(cd /usr/local/build/rocksdb && git describe --tags --exact-match 2>/dev/null || echo "none")
        if [ "$CURRENT" != "$TAG" ]; then
            echo "RocksDB version mismatch (Current: $CURRENT, Required: $TAG). Re-cloning..."
            rm -rf /usr/local/build/rocksdb
        fi
    fi
    if [ ! -d "/usr/local/build/rocksdb" ]; then
        echo "Cloning RocksDB $TAG..."
        git clone --depth 1 --branch $TAG https://github.com/facebook/rocksdb.git /usr/local/build/rocksdb
    fi
    echo "Building RocksDB..."
    cd /usr/local/build/rocksdb && env DISABLE_JEMALLOC=1 EXTRA_CXXFLAGS="-Wno-error=unused-parameter" make shared_lib static_lib -j$(nproc)

# Install RocksDB globally (requires sudo)
install-rocksdb:
    @echo "Installing RocksDB to /usr/local... (Requires sudo)"
    cd /usr/local/build/rocksdb && sudo make install-shared INSTALL_PATH=/usr/local
    cd /usr/local/build/rocksdb && sudo make install-static INSTALL_PATH=/usr/local
    sudo ldconfig
    @echo "Remember to set ROCKSDB_LIB_DIR=/usr/local/lib if Cargo doesn't see it."

# Pre-build jemalloc
prebuild-jemalloc:
    #!/usr/bin/env bash
    set -e
    COMMIT=$(cargo pkgid tikv-jemalloc-sys | cut -d'@' -f2 | grep -o 'g[0-9a-f]*' | head -n 1 | cut -c 2-)
    mkdir -p /usr/local/build
    if [ -d "/usr/local/build/tikv-jemalloc" ]; then
        CURRENT=$(cd /usr/local/build/tikv-jemalloc && git rev-parse HEAD 2>/dev/null || echo "none")
        if [ "$CURRENT" != "$COMMIT" ] && [ "${CURRENT:0:7}" != "${COMMIT:0:7}" ]; then
            echo "jemalloc commit mismatch. Re-cloning..."
            rm -rf /usr/local/build/tikv-jemalloc
        fi
    fi
    if [ ! -d "/usr/local/build/tikv-jemalloc" ]; then
        echo "Cloning tikv-jemalloc..."
        git clone https://github.com/tikv/jemalloc.git /usr/local/build/tikv-jemalloc
        cd /usr/local/build/tikv-jemalloc && git checkout $COMMIT
    fi
    echo "Building jemalloc..."
    cd /usr/local/build/tikv-jemalloc
    [ -f Makefile ] || ./autogen.sh
    make

# Install jemalloc globally (requires sudo)
install-jemalloc:
    @echo "Installing jemalloc to /usr/local... (Requires sudo)"
    cd /usr/local/build/tikv-jemalloc && sudo make install
    sudo ldconfig

# Pre-build zstd
prebuild-zstd:
    #!/usr/bin/env bash
    set -e
    VER=$(cargo pkgid zstd-sys | cut -d'@' -f2 | grep -o 'zstd\.[0-9.]*' | cut -d '.' -f2-)
    TAG="v$VER"
    mkdir -p /usr/local/build
    if [ -d "/usr/local/build/zstd" ]; then
        CURRENT=$(cd /usr/local/build/zstd && git describe --tags --exact-match 2>/dev/null || echo "none")
        if [ "$CURRENT" != "$TAG" ]; then
            echo "zstd version mismatch. Re-cloning..."
            rm -rf /usr/local/build/zstd
        fi
    fi
    if [ ! -d "/usr/local/build/zstd" ]; then
        echo "Cloning zstd $TAG..."
        git clone --depth 1 --branch $TAG https://github.com/facebook/zstd.git /usr/local/build/zstd
    fi
    echo "Building zstd..."
    cd /usr/local/build/zstd && make lib-release -j$(nproc)

# Install zstd globally (requires sudo)
install-zstd:
    @echo "Installing zstd to /usr/local... (Requires sudo)"
    cd /usr/local/build/zstd && sudo make install -C lib PREFIX=/usr/local
    sudo ldconfig

# Pre-build lz4
prebuild-lz4:
    #!/usr/bin/env bash
    set -e
    VER=$(cargo pkgid lz4-sys | cut -d'@' -f2 | grep -o 'lz4-[0-9.]*' | cut -d '-' -f2)
    TAG="v$VER"
    mkdir -p /usr/local/build
    if [ -d "/usr/local/build/lz4" ]; then
        CURRENT=$(cd /usr/local/build/lz4 && git describe --tags --exact-match 2>/dev/null || echo "none")
        if [ "$CURRENT" != "$TAG" ]; then
            echo "lz4 version mismatch. Re-cloning..."
            rm -rf /usr/local/build/lz4
        fi
    fi
    if [ ! -d "/usr/local/build/lz4" ]; then
        echo "Cloning lz4 $TAG..."
        git clone --depth 1 --branch $TAG https://github.com/lz4/lz4.git /usr/local/build/lz4
    fi
    echo "Building lz4..."
    cd /usr/local/build/lz4 && make lib -j$(nproc)

# Install lz4 globally (requires sudo)
install-lz4:
    @echo "Installing lz4 to /usr/local... (Requires sudo)"
    cd /usr/local/build/lz4 && sudo make install PREFIX=/usr/local
    sudo ldconfig

# --- CPU Profiling ---

# Run CPU flamegraph profiling (requires sudo for perf)
profile-cpu *args:
    cargo flamegraph --root --features local_profiling --bin conduwuit -- {{args}}
    @echo "Flamegraph saved to flamegraph.svg"

# --- Async & I/O Profiling ---

# Run with tokio-console instrumentation active
profile-async *args:
    @echo "Run 'tokio-console' in a separate terminal"
    env RUSTFLAGS="--cfg tokio_unstable ${RUSTFLAGS:-}" cargo run --features local_profiling --bin conduwuit -- {{args}}

# --- Memory Profiling (jemalloc) ---

# Run release build and dump jemalloc heap profiles
profile-mem *args:
    cargo build --release --features local_profiling --bin conduwuit
    @echo "Starting with jemalloc profiling..."
    env MALLOC_CONF="prof:true,lg_prof_interval:24,prof_prefix:jeprof.out" ./target/release/conduwuit {{args}}

# Generate heap_profile.svg from collected jemalloc dumps
profile-mem-analyze:
    jeprof --svg ./target/release/conduwuit jeprof.out.* > heap_profile.svg
    @echo "Saved heap_profile.svg"

# Clean up jemalloc dump files
profile-mem-clean:
    rm -f jeprof.out.* heap_profile.svg
