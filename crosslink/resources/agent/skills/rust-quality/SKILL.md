---
name: rust-quality
description: "Design, implement, and review Rust with stable APIs, explicit errors, safe ownership, strict linting, and meaningful tests."
---

# Rust engineering baseline

## Before editing

Read `Cargo.toml`, crate roots, lint settings, feature flags, MSRV, callers, and existing tests. Determine whether the surface is a library, binary, async service, FFI layer, build script, or platform-specific component.

## APIs and types

Use domain types where primitives would permit invalid states. Keep public fields private unless direct data access is an intentional stable contract. Follow Rust naming and conversion conventions, implement common traits when semantics allow, and avoid leaking implementation dependencies through public signatures.

## Ownership and resources

Let ownership express lifecycle. Release files, locks, tasks, handles, and foreign resources through RAII. Avoid unnecessary cloning and allocation, but prefer clear correct code over speculative micro-optimization.

## Errors

Libraries return typed errors suitable for callers. Applications may add contextual dynamic errors at orchestration boundaries. Preserve source errors, add useful operation context, and avoid panics for recoverable input or environment failures.

## Unsafe and FFI

Minimize unsafe regions and state every required invariant in independently written safety documentation when unsafe code remains. Validate pointers, lengths, alignment, initialization, aliasing, thread constraints, and ownership transfer. Run Miri where supported.

## Async and concurrency

Do not hold synchronous locks across `.await`. Give spawned work an owner, cancellation path, and observed result. Bound queues, timeouts, retries, and concurrency. Avoid detached tasks whose errors disappear.

## Tests

Cover public behavior, boundary conditions, failure paths, regressions, serialization compatibility, and concurrency where relevant. Keep tests deterministic and avoid assertions that only repeat implementation details.

## Verification

Run `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, the relevant tests, and broader workspace checks for public or cross-crate changes. Add cross-target compilation when platform code changes. Report commands and results precisely.
