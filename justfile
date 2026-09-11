# Vocoder task runner — https://just.systems

# Re-extract spec/ from the pinned dsh/ submodule.
update-spec:
    tools/spec-extractor/update-spec.sh

# CI gate: committed spec/ must equal regeneration.
spec-check:
    tools/spec-extractor/update-spec.sh
    git diff --exit-code -- spec/

# Run the conformance suite against HOST=dsh (control) or HOST=vocoderd.
conformance HOST="vocoderd":
    harness/runners/run-conformance.sh {{HOST}}

# Regenerate Rust bindings from spec/ into rust/crates/vocoder-spec-api.
codegen:
    cargo run -p vocoder-codegen

# Emit docs/spec-coverage.md: spec endpoints x passing tests per host.
coverage-report:
    cargo run -p vocoder-codegen -- coverage-report

# Rust housekeeping.
build:
    cargo build --workspace

test:
    cargo test --workspace
