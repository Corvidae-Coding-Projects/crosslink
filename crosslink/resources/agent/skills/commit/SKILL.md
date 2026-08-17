---
name: commit
description: "Create an intentional Git commit and record the delivered result on the active Crosslink issue."
---

# Commit a completed work unit

1. Inspect staged and unstaged changes, including untracked paths.
2. Determine the exact files that belong to the requested work and leave unrelated files untouched.
3. Review the selected diff and confirm relevant verification has passed.
4. Stage explicit paths rather than using an unbounded add operation.
5. Write a concise conventional-style subject that states the outcome. Add a useful body when the reason is not obvious.
6. Create the commit with repository hooks enabled.
7. Run `crosslink session status`. If an active issue exists, add a `result` comment containing the commit identifier, subject, and verification summary.
8. Show the final commit, files included, remaining worktree state, and whether anything was intentionally excluded.

Do not amend, bypass signing or verification, include secrets, discard changes, push, or merge unless the user’s request includes that action.
