#!/usr/bin/env bash
set -e

RUSTC="$1"
shift

CMD=("$RUSTC")

if [[ -z "$NO_SCCACHE" ]] && command -v sccache >/dev/null 2>&1; then
    CMD=(sccache "$RUSTC")
fi

EXTRA_ARGS=()

# Clang detection
if command -v clang >/dev/null 2>&1; then
    EXTRA_ARGS+=("-C" "linker=clang")
else
    # Only warn once (check if we're compiling a top-level crate or just some build script)
    if [[ "$*" == *" --crate-name conduwuit "* ]]; then
        echo "warning: clang not found, falling back to default linker" >&2
    fi
fi

# Mold detection
if command -v mold >/dev/null 2>&1; then
    # Do not use mold if cross-compiling to webassembly or riscv (SP1)
    if [[ "$*" == *"wasm32"* ]] || [[ "$*" == *"riscv"* ]]; then
        : # skip mold
    else
        EXTRA_ARGS+=("-C" "link-arg=-fuse-ld=mold")
    fi
else
    if [[ "$*" == *" --crate-name conduwuit "* ]]; then
        echo "warning: mold not found, falling back to default linker" >&2
    fi
fi

exec "${CMD[@]}" "$@" "${EXTRA_ARGS[@]}"
