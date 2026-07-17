#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$repo_root/target/release-mold-icf}
clang=${CHATT_CLANG:-clang}

for tool in cargo "$clang" mold; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "required build tool not found: $tool" >&2
        exit 1
    fi
done

# Setting RUSTFLAGS replaces .cargo/config.toml's build flags, so retain the
# repository's frame-pointer setting explicitly.
rustflags="-C force-frame-pointers=yes \
-C linker=$clang \
-C link-arg=-fuse-ld=mold \
-C link-arg=-Wl,--icf=all"

echo "Building mold+ICF release in $target_dir"
env \
    CARGO_TARGET_DIR="$target_dir" \
    CARGO_PROFILE_RELEASE_DEBUG="${CARGO_PROFILE_RELEASE_DEBUG:-0}" \
    RUSTFLAGS="$rustflags" \
    cargo build --release --bin chatt "$@"

echo "Built $target_dir/release/chatt"
