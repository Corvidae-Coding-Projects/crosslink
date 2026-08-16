---
name: featree
description: "Create an isolated feature branch and linked worktree, then initialize Crosslink for the selected providers."
---

# Feature worktree

1. Convert the requested feature name to a safe lowercase slug and resolve the agreed base ref.
2. Inspect `git status`, existing branches, and `git worktree list`. Stop on a name collision or unclear local modifications.
3. Create `feature/<slug>` without rewriting another branch.
4. Ensure `<repo>/.worktrees/` is ignored, then add the linked worktree at `<repo>/.worktrees/<slug>`.
5. In the new worktree, run `crosslink init --defaults --agent-integration both` unless the user selected a provider override.
6. Initialize a unique agent identity if init did not already provide one, then run `crosslink sync`.
7. Verify the branch, worktree path, provider integrations, agent identity, and issue visibility.

Report the absolute worktree path and branch. Do not stash, delete a branch, remove another worktree, or overwrite unrelated files to make the operation succeed.
