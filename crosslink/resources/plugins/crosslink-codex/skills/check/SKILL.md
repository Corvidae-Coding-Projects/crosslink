---
name: check
description: "Inspect and monitor Crosslink kickoff agents running in tmux or containers, including completion sentinels and recent output."
---

# Background agent check

If the user names an agent, inspect that agent. Otherwise enumerate the current repository’s kickoff runs with `crosslink kickoff list`.

For each run:

1. Resolve its worktree, provider, branch, issue, process type, and launch time.
2. Read `.kickoff-status`, `.kickoff-metadata.json`, and `.kickoff-session` when present.
3. Use `crosslink kickoff status <agent>` as the primary status interface.
4. Capture bounded recent output with `crosslink kickoff logs <agent> --lines 80`.
5. For tmux, confirm the session exists and capture its pane without attaching.
6. For containers, inspect state and exit code without restarting or deleting anything.
7. Inspect the worktree’s Git status and recent commits.

Classify the run as working, waiting for input, idle, complete, failed, or missing. Explain the evidence behind the classification. Offer the exact status, log, attach, stop, or cleanup command that fits the state. Do not send input, stop a process, remove a worktree, or clean up a run unless the user requests it.
