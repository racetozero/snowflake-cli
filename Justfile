default:
    @just --list

# Recreate the pinned Python reference implementation.
upstream:
    @scripts/upstream.sh

# Regenerate the command tree from the pinned upstream.
spec:
    @target/upstream/venv/bin/python scripts/gen_cli_spec.py

# Run the workspace tests with nextest, then the documentation tests.
test *args:
    @cargo nextest run --workspace --all-targets {{args}}
    @cargo test --workspace --doc

# Format, lint and test the workspace.
check:
    @cargo fmt --all --check
    @cargo clippy --workspace --all-targets -- -D warnings
    @just test

# Run the differential parity checker against a release build.
parity *args:
    @cargo build --release -p snowflake-cli
    @python3 scripts/parity/check.py {{args}}

# Compare time and memory of the Rust and Python `snow` on the same workloads.
bench:
    @cargo build --release -p snowflake-cli
    @python3 scripts/bench_startup.py --json target/bench-startup.json

# Test the cross-platform PGO build and benchmark helpers.
pgo-test:
    @python3 scripts/test_pgo.py

# Build one native PGO release binary.
pgo-build target="":
    @python3 scripts/build_snow_pgo.py --target-dir target/snow-pgo {{ if target == "" { "" } else { "--target=" + target } }}

# Build matched ordinary and PGO binaries, then compare held-out workloads.
pgo-bench target="":
    @python3 scripts/build_snow_pgo.py --baseline-and-pgo --target-dir target/snow-pgo {{ if target == "" { "" } else { "--target=" + target } }}
    @python3 scripts/benchmark_snow_pgo.py --target-dir target/snow-pgo {{ if target == "" { "" } else { "--target=" + target } }} --json target/snow-pgo/results.json

# Run every release gate and build one native archive without publishing.
release-check version target="":
    @scripts/release.sh check "{{ version }}" "{{ target }}"

# Check, confirm, tag, and publish a release from a clean main branch.
release version target="":
    @scripts/release.sh release "{{ version }}" "{{ target }}"
