# List available commands
default:
    @just --list

# Run CPU flamegraph profiling (requires sudo for perf)
profile-cpu *args:
    cargo flamegraph --root --features local_profiling --bin conduwuit -- {{args}}
    @echo "Flamegraph saved to flamegraph.svg"

# Run with tokio-console instrumentation active
profile-async *args:
    @echo "Run 'tokio-console' in a separate terminal"
    env RUSTFLAGS="--cfg tokio_unstable ${RUSTFLAGS:-}" cargo run --features local_profiling --bin conduwuit -- {{args}}

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
