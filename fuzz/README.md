# Fuzzing (design §6.5)

Requires nightly + cargo-fuzz (`cargo install cargo-fuzz`):

```sh
cd fuzz
cargo +nightly fuzz run parse_roundtrip -- -max_total_time=300
cargo +nightly fuzz run trace_ingest   -- -max_total_time=300
```

The same properties run continuously in the stable test suite with a
seeded mutator (`crates/cmakedb-db/tests/robustness.rs`); these targets
add coverage-guided depth. CI runs them weekly (`fuzz.yml`).
