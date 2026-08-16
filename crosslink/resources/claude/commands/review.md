---
allowed-tools: Bash(crosslink *), Bash(git *), Bash(cargo *), Bash(npm *), Bash(npx *), Bash(uv *), Bash(ruff *), Bash(go *), Bash(mix *), Read, Grep, Glob, Skill
description: Run the final pre-commit quality gate
argument-hint: [scope]
---

Use the `review-pre-commit` skill for $ARGUMENTS. Inspect the full intended diff, run the relevant checks, verify generated assets and tracking evidence, and print a pass/fail checklist.
