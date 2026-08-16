---
name: dev-release
description: "Run a traceable release workflow from version selection through validation, pull request, tag, publication, and branch reconciliation."
---

# Development release

Inputs may include a version, source branch, target branch, `--skip-docs`, or `--skip-tests`. Discover repository conventions before choosing defaults.

1. Confirm the requested semantic version and inspect the current branch, worktree, tags, and remote state.
2. Create or select the release branch from the agreed source.
3. Update every authoritative version location and regenerate lockfiles or derived manifests.
4. Draft the changelog from commits and completed work since the previous release. Keep entries user-facing and verifiable.
5. Review documentation, installation examples, compatibility notes, and provider assets unless explicitly skipped.
6. Run formatting, linting, tests, packaging, and platform checks appropriate to the release. Record any deliberately unavailable check.
7. Review the complete diff and commit the release changes.
8. Push and open the pull request only when authorized. Target the agreed release branch.
9. Monitor required CI and report failures with their logs.
10. After the release change is merged and the user authorizes publication, create the signed or annotated tag required by project policy and publish it.
11. Create the GitHub release with changelog-derived notes and attach required artifacts.
12. Reconcile release changes back to the development branch when the repository flow requires it.

Keep a release-state record containing the version, source, target, checks, PR, tag, and publication URL. Never claim a tag, merge, artifact, or release exists without verifying it. Skipping documentation or tests must remain visible in the final report.
