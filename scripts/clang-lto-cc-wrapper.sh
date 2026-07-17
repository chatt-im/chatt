#!/usr/bin/env bash
set -euo pipefail

clang=${CHATT_LTO_CLANG:-clang}
args=()

# aws-lc-sys 0.42 adds this GNU-as-only spelling even when Clang uses its
# integrated assembler. Clang rejects it before compiling AWS-LC as bitcode.
# The mapping only affects paths in debug information, which distribution
# builds disable; retain every other compiler argument verbatim.
for arg in "$@"; do
    case "$arg" in
        -Wa,--debug-prefix-map=*) ;;
        *) args+=("$arg") ;;
    esac
done

exec "$clang" "${args[@]}"
