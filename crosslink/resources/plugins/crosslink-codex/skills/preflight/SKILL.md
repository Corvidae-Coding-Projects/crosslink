---
name: preflight
description: "Ground implementation work in current repository structure, manifests, interfaces, tests, session state, and user constraints."
---

# Implementation preflight

Before a nontrivial edit:

1. Restate the requested outcome and explicit exclusions.
2. Inspect the repository root, current branch, worktree state, and applicable provider instruction file.
3. Read the primary manifests to determine languages, versions, features, tooling, and workspace boundaries.
4. Locate the implementation, its callers, related tests, stored formats, and generated projections.
5. Run `crosslink session status` and inspect the active issue when Crosslink tracking is in use.
6. Check existing knowledge and design documents for the same subsystem.
7. Identify the smallest complete verification set, including platform or provider checks where relevant.

Print a compact summary containing the objective, affected paths, current architecture, dependency facts, active work item, risks, and planned checks. Confirm `.crosslink/rules/*.md` remain zero bytes without disconnecting their prompt-hook loader.
