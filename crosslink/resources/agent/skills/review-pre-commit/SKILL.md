---
name: review-pre-commit
description: "Apply a final evidence-based gate to the selected working diff before it is committed."
---

# Pre-commit review

1. Inspect `git status --short`, the full unstaged diff, the staged diff, and untracked files.
2. Confirm every selected change belongs to the requested work and no user-owned change is being absorbed.
3. Trace changed logic for correctness, failure handling, compatibility, and security.
4. Search changed paths for unfinished branches, placeholder returns, disabled tests, debug output, hardcoded credentials, and accidental generated artifacts.
5. Run check-only formatting and linting, then the smallest complete test and build set for the affected surface.
6. Verify generated assets against their canonical inputs.
7. Inspect Crosslink session and issue state when tracking is active; record missing result evidence before committing.

Print a checklist with diff review, formatting, lint, tests, build, generated assets, security scan, and tracking state. Mark each item `pass`, `fail`, or `not applicable` with evidence. A failure blocks the commit until fixed or explicitly accepted by the user.
