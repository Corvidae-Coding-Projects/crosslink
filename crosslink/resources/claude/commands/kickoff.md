---
allowed-tools: Bash(crosslink *), Bash(which *), Bash(tmux *), Bash(docker *), Bash(podman *), Read, Skill
description: Launch and monitor a background implementation agent
argument-hint: <task> [--issue <id>] [--verify local|ci|thorough] [--container docker|podman]
---

Use the `kickoff` skill with $ARGUMENTS. Check provider readiness, launch once, capture the returned identifiers, and continue monitoring until a verified terminal state.
