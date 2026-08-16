---
name: audit
description: "Reconstruct the current state of a Crosslink project when progress, ownership, or the next action is unclear."
---

# Project state audit

Collect evidence without changing repository or external state.

1. Locate the repository root and list its top-level structure.
2. Read the primary manifests and architecture documents that apply to the task.
3. Run `crosslink session status` and `crosslink session last-handoff`.
4. Inspect the active issue with `crosslink issue show <id>` when one exists.
5. Query `crosslink issue blocked`, `crosslink issue ready`, and relevant relations.
6. Run `crosslink locks list` and `crosslink agent status` for coordination state.
7. Inspect recent typed comments and interventions on the active issue.
8. Read `.crosslink/hook-config.json` and run `crosslink workflow diff`.
9. Run `git status --short`, `git branch --show-current`, `git log -n 10 --oneline`, and `git worktree list`.
10. Check background agents with `crosslink kickoff list` and, when relevant, `crosslink swarm status`.

Report the active objective, completed work, pending work, blockers, local modifications, background processes, and the single best next action. Mark missing or contradictory evidence explicitly. The empty `.crosslink/rules/*.md` compatibility files are not audit inputs.
