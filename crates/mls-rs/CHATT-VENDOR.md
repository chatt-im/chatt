# Vendored for Chatt

These MLS crates come from `awslabs/mls-rs` commit
`42131c9959efb1d3928428259bc89853027f730d`. Chatt vendors only its production
dependency closure under `crates/`.

Large upstream test vectors are deliberately omitted. Restore them before
running the vendored crates' own test suites:

```console
./scripts/fetch-mls-test-data.sh
```

The Chatt workspace tests do not require those vectors.
