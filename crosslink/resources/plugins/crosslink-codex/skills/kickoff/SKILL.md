---
name: kickoff
description: "Launch a monitored background coding agent in an isolated Crosslink worktree."
---

# Kickoff agent

Build the command from the task plus any issue, design document, provider model, effort, timeout, verification, container, branch, or permission options supplied by the user.

1. Confirm the repository is initialized and run `crosslink doctor` when provider readiness is uncertain.
2. Preview consequential settings with `crosslink kickoff run "<task>" --dry-run` when useful.
3. Launch with `crosslink kickoff run "<task>"` and the selected options.
4. Capture the returned agent, issue, branch, worktree, tmux session or container, provider, and timeout.
5. Monitor with `crosslink kickoff status <agent>` and bounded logs. Keep monitoring until the requested outcome reaches a terminal state.
6. On completion, verify the sentinel, process exit, Git diff and commits, required tests, and requested artifact contents.
7. On failure or waiting state, diagnose the actual output before proposing a restart or code change.

Do not fabricate completion, launch duplicate work for the same branch, attach interactively without need, or stop and clean a run without authorization.
