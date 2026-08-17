---
name: qa
description: "Perform an evidence-based architecture, correctness, security, and maintainability review of a code change."
---

# Quality and architecture review

Review the requested scope without editing it unless remediation is also requested.

## Establish context

Read the change, its surrounding implementation, public callers, tests, manifests, and architecture records. Identify the intended behavior and trust boundaries.

## Examine the design

Check dependency direction, module ownership, cohesion, duplication, API stability, state transitions, storage compatibility, concurrency, cleanup, and operational observability. Prefer simple interfaces that preserve invariants. Treat numeric complexity thresholds as signals requiring explanation, not automatic defects.

## Examine correctness

Trace success, empty input, invalid input, boundary values, partial failure, retry, cancellation, timeout, and recovery paths. Confirm errors are propagated with useful context and resources are released.

## Examine security

Review input validation, authorization, injection surfaces, command construction, path handling, secret exposure, unsafe code, dependency risk, and external-content provenance using the project’s actual threat model.

## Verify

Run format, lint, tests, builds, and focused reproductions appropriate to the changed surface. Search for each suspected pattern across sibling code before calling it systemic.

## Report

List findings from highest to lowest impact. Each finding must contain a precise location, observed behavior, user or system consequence, evidence, and a concrete corrective direction. Separate confirmed defects, risks, and optional improvements. End with checks run, checks unavailable, and an overall `pass`, `pass with risks`, or `changes required` assessment.
