# Codex repository reference

Crosslink is developed against the `develop` branch. For a code change, inspect the current branch and worktree, create a narrowly named branch when needed, and protect unrelated user files. Use Crosslink’s issue/session records for meaningful work when those records are active.

The user’s present request determines whether to commit, push, open a pull request, or merge. Never erase local work, skip repository safeguards, force-update history, or run a destructive Git command without explicit authorization.

Verify changed Rust with formatting, tests, and strict Clippy. Check provider assets with `python3 crosslink/scripts/sync-codex-plugin.py --check`. Run relevant dashboard, extension, documentation, hook, and cross-target checks whenever those areas change.

Useful commands:

```bash
crosslink session start
crosslink issue quick "work item" -p medium -l feature
crosslink session work <id>
crosslink issue comment <id> "result" --kind result
crosslink session end --notes "handoff"
crosslink kickoff status <agent>
crosslink sync
```

Use `crosslink --help` for the complete interface. `.crosslink/rules/*.md` files are intentionally zero bytes, and their existing prompt-hook connection remains in place.
