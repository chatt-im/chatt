#!/usr/bin/env bash
set -euo pipefail

revision=42131c9959efb1d3928428259bc89853027f730d
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT

curl -fsSL \
    "https://github.com/awslabs/mls-rs/archive/${revision}.tar.gz" \
    -o "$scratch/mls-rs.tar.gz"
mkdir "$scratch/source"
tar -xzf "$scratch/mls-rs.tar.gz" \
    -C "$scratch/source" \
    --strip-components=1

for crate in mls-rs mls-rs-core mls-rs-crypto-awslc; do
    mkdir -p "$repo_root/crates/$crate/test_data"
    rsync -a --delete \
        "$scratch/source/$crate/test_data/" \
        "$repo_root/crates/$crate/test_data/"
done

echo "Restored MLS test data from awslabs/mls-rs@$revision"
