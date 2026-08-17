---
name: feature
description: "Create a feature branch from a chosen base and register a matching Crosslink issue."
---

# Feature branch

Derive a readable lowercase slug from the user’s description. The target is `feature/<slug>`.

Before changing Git state, inspect the worktree, current branch, requested base, and existing local or remote branches. If unrelated modifications make branch creation unsafe, explain the exact conflict instead of hiding them.

Create the branch from the resolved base. Register the work with:

```bash
crosslink issue create "<original description>" -p medium -l feature
```

Use the user’s priority or labels when provided. Report the branch, base commit, and issue identifier. Do not push, force, delete, stash, or switch away from user work without authorization.
