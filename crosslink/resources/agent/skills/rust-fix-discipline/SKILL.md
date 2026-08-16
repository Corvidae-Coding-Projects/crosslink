---
name: rust-fix-discipline
description: "Resolve Rust audit, review, lint, and test findings at their actual cause with explicit verification."
---

# Rust remediation discipline

## Understand the finding

Read the complete report, surrounding implementation, callers, sibling patterns, relevant tests, workspace lint policy, and minimum Rust version. Reproduce the failure when possible. Correct a mistaken severity or premise before changing code.

## Select the repair shape

Classify each item as:

- Root-cause repair: change the shared invariant or abstraction producing the defect.
- Local correctness repair: fix a genuinely isolated site without pretending it solves a wider pattern.
- Explanatory annotation: document a sound but non-obvious safety, lifetime, platform, or lint condition.

If a public API, feature policy, dependency, MSRV, or workspace contract must change, inspect all consumers and surface the coordination impact. Do not hide the need behind a local workaround.

## Forbidden shortcuts

Do not blanket-allow a lint, replace a failure with a silent fallback, weaken a test, introduce a stub, change observable behavior accidentally, duplicate the defective pattern, or report success from a narrower command than the changed interface requires.

## Verify

Run focused regression tests first, then formatting, strict Clippy, the affected crate tests, and the workspace build or test set required by public changes. Probe platform or feature configurations touched by the fix. Search again for sibling instances.

## Report each repair

State the finding, chosen repair category, cause, files changed, behavior before and after, checks run, and remaining limitation. Use exact language: `verified`, `not reproduced`, `not run`, or `failed`. Never translate an unavailable check into a pass.
