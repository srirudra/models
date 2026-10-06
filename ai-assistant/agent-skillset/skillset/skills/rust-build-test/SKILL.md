---
name: rust-build-test
description: 'Build and test Rust workspaces. Use when selecting focused validation commands, running cargo test, diagnosing compile or clippy failures, measuring coverage, or verifying Docker builds for Rust services.'
user-invocable: false
---

# Rust Build and Test

Use this skill to choose and run the cheapest validation that can falsify the current change.

## Procedure

1. Identify the touched crate, its test targets, and the workspace layout from `Cargo.toml`.
2. Prefer focused validation before full-workspace validation.
3. For async code, confirm tests use `#[tokio::test]` and `tokio::time::pause()` where timers are involved.
4. When a command fails, fix only the relevant local defect unless the failure proves the plan is wrong.
5. Report command, result, failure summary, and remaining risk.

## Validation Ladder

- `cargo check -p <crate>` — compile the touched crate (fastest falsification).
- `cargo clippy -p <crate> --all-targets` — lints including tests and benches.
- `cargo test -p <crate> <filter>` — touched test module or name filter.
- `cargo test -p <crate>` — the nearest crate's full suite.
- `cargo test --workspace` + `cargo fmt --check` — when blast radius requires it.
- `cargo llvm-cov -p <crate>` — coverage for risk-adjacent modules (lifecycle, routing, proxy pipeline).
- `docker build -t <image> .` — for Dockerfile/compose changes (smoke: container starts, `/_health` returns 200).

## Output

Return the validation chosen, why it was cheapest, exact commands run, results, and follow-up needed.
