---
name: maintain
description: "Assess dependency, build, test, lint, documentation, issue, and generated-asset health without silently expanding scope."
---

# Maintenance pass

Inspect first; modify only when the user requests remediation.

## Dependencies

Read manifests and lockfiles, then use ecosystem-native tools to identify outdated, vulnerable, duplicated, or unused packages. Distinguish confirmed findings from network-unavailable checks.

## Formatting and lint

Run check-only formatters and configured linters for each maintained component. Capture exact failures and avoid blanket suppression.

## Tests and builds

Run the normal test suite, then relevant feature, integration, platform, packaging, or documentation builds. Investigate hangs and timeouts as failures with causes, not as missing results.

## Source health

Search for dead paths, obsolete compatibility code, stubs, debug residue, duplicate logic, stale feature flags, and generated files that differ from their source. Confirm candidates through references and build behavior before recommending removal.

## Documentation and tracking

Compare public behavior with README, architecture, command reference, changelog, provider assets, and examples. Review open, blocked, orphaned, and completed Crosslink issues for inconsistent state.

## Artifacts

Measure build caches and generated outputs. Never delete them unless the user authorizes cleanup and the target is exact.

Report checks run, findings by severity, evidence, safe fixes, unavailable checks, and suggested order. Do not update dependencies, delete artifacts, close issues, or rewrite documentation during a read-only maintenance request.
