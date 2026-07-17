#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$repo_root/target/release-mold-full-lto}
clang=${CHATT_CLANG:-clang}
clangxx=${CHATT_CLANGXX:-clang++}
cc_wrapper=$repo_root/scripts/clang-lto-cc-wrapper.sh

for tool in cargo "$clang" "$clangxx" mold cmake ninja; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "required build tool not found: $tool" >&2
        exit 1
    fi
done

# The C/C++ flags apply to every bundled native dependency. Rust crates emit
# linker-plugin bitcode, allowing mold's LLVM plugin to optimize across the
# Rust/C language boundary at the final link.
cflags="${CFLAGS:+$CFLAGS }-flto"
cxxflags="${CXXFLAGS:+$CXXFLAGS }-flto"
rustflags="-C force-frame-pointers=yes \
-C linker-plugin-lto \
-C linker=$clang \
-C link-arg=-flto \
-C link-arg=-fuse-ld=mold \
-C link-arg=-Wl,--icf=all"

echo "Building mold+ICF+full-LTO release in $target_dir"
env \
    CARGO_TARGET_DIR="$target_dir" \
    CARGO_PROFILE_RELEASE_DEBUG="${CARGO_PROFILE_RELEASE_DEBUG:-0}" \
    CHATT_LTO_CLANG="$clang" \
    CC="$cc_wrapper" \
    CXX="$clangxx" \
    CFLAGS="$cflags" \
    CXXFLAGS="$cxxflags" \
    AWS_LC_SYS_CMAKE_BUILDER=1 \
    CMAKE_GENERATOR=Ninja \
    RUSTFLAGS="$rustflags" \
    cargo build --release --bin chatt "$@"

echo "Built $target_dir/release/chatt"
