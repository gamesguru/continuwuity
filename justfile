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

# Pre-build all C/C++ dependencies
prebuild-all: init-prebuild prebuild-jemalloc prebuild-lz4 prebuild-snappy prebuild-zstd prebuild-rocksdb

# Install all pre-built C/C++ dependencies
install-all: install-jemalloc install-lz4 install-snappy install-zstd install-rocksdb

# Pre-build jemalloc
prebuild-jemalloc:
    #!/usr/bin/env bash
    set -e
    TAG="5.3.0"
    mkdir -p /usr/local/build
    echo "Cloning jemalloc $TAG..."
    [ ! -d "/usr/local/build/jemalloc" ] && git clone --depth 1 --branch $TAG https://github.com/jemalloc/jemalloc.git /usr/local/build/jemalloc || true
    echo "Building jemalloc..."
    cd /usr/local/build/jemalloc
    git checkout $TAG
    [ -f configure ] || ./autogen.sh
    [ -f Makefile ] || ./configure --prefix=/usr/local
    make -j$(nproc)

# Install jemalloc globally (requires sudo)
install-jemalloc:
    @echo "Installing jemalloc to /usr/local... (Requires sudo)"
    cd /usr/local/build/jemalloc && sudo make install_lib_static install_lib_shared install_include
    sudo ldconfig

# Pre-build lz4
prebuild-lz4:
    #!/usr/bin/env bash
    set -e
    VER=$(cargo pkgid lz4-sys | cut -d'@' -f2 | grep -o 'lz4-[0-9.]*' | cut -d '-' -f2)
    TAG="v$VER"
    mkdir -p /usr/local/build
    echo "Cloning lz4 $TAG..."
    [ ! -d "/usr/local/build/lz4" ] && git clone --depth 1 --branch $TAG https://github.com/lz4/lz4.git /usr/local/build/lz4 || true
    echo "Building lz4..."
    cd /usr/local/build/lz4
    git checkout $TAG
    make lib -j$(nproc)

# Install lz4 globally (requires sudo)
install-lz4:
    @echo "Installing lz4 to /usr/local... (Requires sudo)"
    cd /usr/local/build/lz4 && sudo make install PREFIX=/usr/local
    sudo ldconfig

# Pre-build RocksDB shared and statically
prebuild-rocksdb:
    #!/usr/bin/env bash
    set -e
    TAG="continuwuity-v0.5.0"
    mkdir -p /usr/local/build
    echo "Cloning rocksdb $TAG..."
    [ ! -d "/usr/local/build/rocksdb" ] && git clone --recursive --depth 1 --branch $TAG https://forgejo.ellis.link/continuwuation/rocksdb.git /usr/local/build/rocksdb || true
    echo "Building RocksDB..."
    cd /usr/local/build/rocksdb
    git checkout $TAG
    env DISABLE_JEMALLOC=1 EXTRA_CXXFLAGS="-I/usr/local/include -Wno-error=unused-parameter" EXTRA_LDFLAGS="-L/usr/local/lib" make shared_lib static_lib -j$(nproc)

# Install RocksDB globally (requires sudo)
install-rocksdb:
    @echo "Installing RocksDB to /usr/local... (Requires sudo)"
    cd /usr/local/build/rocksdb && sudo make install-shared INSTALL_PATH=/usr/local
    cd /usr/local/build/rocksdb && sudo make install-static INSTALL_PATH=/usr/local
    sudo ldconfig
    @echo "Remember to set ROCKSDB_LIB_DIR=/usr/local/lib if Cargo doesn't see it."

# Clean RocksDB build directory
clean-rocksdb:
    @echo "Cleaning RocksDB build directory..."
    cd /usr/local/build/rocksdb && make clean
    rm -f /usr/local/build/rocksdb/make_config.mk

# Pre-build snappy
prebuild-snappy:
    #!/usr/bin/env bash
    set -e
    TAG="1.2.1"
    mkdir -p /usr/local/build
    echo "Cloning snappy $TAG..."
    [ ! -d "/usr/local/build/snappy" ] && git clone --depth 1 --branch $TAG https://github.com/google/snappy.git /usr/local/build/snappy || true
    echo "Building snappy..."
    cd /usr/local/build/snappy
    git checkout $TAG
    sed -i 's/cmake_minimum_required(VERSION 3.1)/cmake_minimum_required(VERSION 3.10)/' CMakeLists.txt
    mkdir -p build_static && cd build_static
    cmake -DBUILD_SHARED_LIBS=OFF -DSNAPPY_BUILD_TESTS=OFF -DSNAPPY_BUILD_BENCHMARKS=OFF ..
    make -j$(nproc)
    cd ..
    mkdir -p build_shared && cd build_shared
    cmake -DBUILD_SHARED_LIBS=ON -DSNAPPY_BUILD_TESTS=OFF -DSNAPPY_BUILD_BENCHMARKS=OFF ..
    make -j$(nproc)

# Install snappy globally (requires sudo)
install-snappy:
    @echo "Installing snappy to /usr/local... (Requires sudo)"
    cd /usr/local/build/snappy/build_static && sudo make install
    cd /usr/local/build/snappy/build_shared && sudo make install
    sudo ldconfig

# Pre-build zstd
prebuild-zstd:
    #!/usr/bin/env bash
    set -e
    VER=$(cargo pkgid zstd-sys | cut -d'@' -f2 | grep -o 'zstd\.[0-9.]*' | cut -d '.' -f2-)
    TAG="v$VER"
    mkdir -p /usr/local/build
    echo "Cloning zstd $TAG..."
    [ ! -d "/usr/local/build/zstd" ] && git clone --depth 1 --branch $TAG https://github.com/facebook/zstd.git /usr/local/build/zstd || true
    echo "Building zstd..."
    cd /usr/local/build/zstd
    git checkout $TAG
    make lib-release -j$(nproc)

# Install zstd globally (requires sudo)
install-zstd:
    @echo "Installing zstd to /usr/local... (Requires sudo)"
    cd /usr/local/build/zstd && sudo make install -C lib PREFIX=/usr/local
    sudo ldconfig

# --- CPU Profiling ---

# Run CPU flamegraph profiling (requires sudo for perf)
profile-runtime-cpu *args:
    cargo flamegraph --root --features local_profiling --bin conduwuit -- {{args}}
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
    jeprof --svg ./target/release/conduwuit jeprof.out.* > heap_profile.svg
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

# Extracts the workspace version from Cargo.toml
version := `grep -m1 "^version = " Cargo.toml | cut -d \" -f 2`

# Builds liburing
prebuild-liburing:
    #!/usr/bin/env bash
    set -e
    mkdir -p /usr/local/build
    echo "Cloning and building liburing (attempting to match project version {{version}})"...
    [ ! -d "/usr/local/build/liburing" ] && git clone https://github.com/axboe/liburing.git /usr/local/build/liburing || true
    cd /usr/local/build/liburing
    git checkout liburing-{{version}} || echo "Warning: Tag liburing-{{version}} not found. Building from latest master instead."
    ./configure
    make -j$(nproc)

# Installs liburing
install-liburing:
    @echo "Installing liburing (requires sudo)..."
    cd /usr/local/build/liburing && sudo make install
    @echo "Done! You might need to run 'sudo ldconfig' to update library cache."

# Builds bzip2
prebuild-bzip2:
    #!/usr/bin/env bash
    set -e
    mkdir -p /usr/local/build
    echo "Cloning and building bzip2..."
    [ ! -d "/usr/local/build/bzip2" ] && git clone git://sourceware.org/git/bzip2.git /usr/local/build/bzip2 || true
    cd /usr/local/build/bzip2
    make -f Makefile-libbz2_so
    make

# Installs bzip2
install-bzip2:
    @echo "Installing bzip2 (requires sudo)..."
    cd /usr/local/build/bzip2 && sudo make install PREFIX=/usr/local
    cd /usr/local/build/bzip2 && sudo cp -f libbz2.so.1.0.* /usr/local/lib/
    cd /usr/local/build/bzip2 && sudo ln -sf /usr/local/lib/libbz2.so.1.0.* /usr/local/lib/libbz2.so
    sudo ldconfig
    @echo "Done! Installed libbz2.so to /usr/local/lib"

# Start gdbserver for lightweight remote debugging (POC)
# Usage: just remote-debug-poc /path/to/conduwuit.toml
remote-debug-poc config="conduwuit-example.toml":
    @echo "Starting gdbserver on :1234 using config: {{config}}"
    sudo -u conduwuit gdbserver :1234 ./target/debug/conduwuit --config {{config}}

# Install the conduwuit binary to a specified directory (default: ~/.cargo/bin)
install bin_dir="~/.cargo/bin":
    @echo "Installing conduwuit to {{bin_dir}}..."
    mkdir -p {{bin_dir}}
    install -m 755 target/debug/conduwuit {{bin_dir}}/conduwuit
    @echo "Done!"
