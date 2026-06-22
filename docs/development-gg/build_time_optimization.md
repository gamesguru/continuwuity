# Proposal: Build Time Optimization

## Problem

Incremental builds take ~30-60s even for single-file changes because the workspace has a linear crate dependency chain:

```
conduwuit_core → conduwuit_database → conduwuit_service → conduwuit_api → conduwuit_admin → conduwuit_router → conduwuit
```

Cargo can't parallelize across crates when each depends on the previous. A change in `conduwuit_api` still triggers sequential recompilation of `conduwuit_admin → conduwuit_router → conduwuit`.

## Proposed Changes

### 1. Break the Dependency Chain (High Impact)

Extract service trait definitions into a thin `conduwuit_interfaces` crate containing only:

- Trait definitions for each service (rooms, users, globals, etc.)
- Shared types and error enums
- No implementation code

This lets `conduwuit_api` and `conduwuit_admin` compile against interfaces in parallel, rather than waiting for the full `conduwuit_service` implementation:

```
                    conduwuit_interfaces
                   /        |           \
    conduwuit_service   conduwuit_api   conduwuit_admin
                   \        |           /
                    conduwuit_router
                          |
                      conduwuit
```

**Estimated impact**: 2-3× faster incremental builds for API-layer changes.

**Effort**: Significant refactor — requires defining trait boundaries for every service. Could be done incrementally, one service at a time.

### 2. Use `mold` Linker (Low Effort, Medium Impact)

Linking is often the bottleneck on incremental debug builds. `mold` is 5-10× faster than the default `ld`.

```toml
# .cargo/config.toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

**Estimated impact**: 2-5s saved per build on link-heavy changes.

### 3. Cranelift Backend for Dev Builds (Low Effort, Medium Impact)

Skip LLVM entirely for debug/check builds:

```sh
cargo +nightly -Zcodegen-backend=cranelift check
```

**Estimated impact**: ~30-40% faster codegen, but nightly-only and may produce slower runtime code (dev-only).

### 4. `sccache` for Cross-Branch Caching (Low Effort, Low Impact)

Caches compiled artifacts across git branches. Switching between feature branches won't trigger full rebuilds:

```toml
# .cargo/config.toml
[build]
rustc-wrapper = "sccache"
```

**Estimated impact**: Eliminates redundant rebuilds when switching branches.

## Priority

1. **mold linker** — quick win, add to dev docs
2. **Cranelift** — easy to try, add as optional dev profile
3. **sccache** — install-and-forget
4. **Interface crate split** — plan for a future major refactor cycle
