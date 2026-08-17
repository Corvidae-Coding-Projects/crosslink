---
name: workflow
description: "Review Crosslink tracking, provider hooks, generated integrations, and repository workflow configuration."
---

# Crosslink workflow review

1. Run `crosslink workflow diff`, `crosslink config show`, and `crosslink doctor`.
2. Inspect `.crosslink/hook-config.json`, provider selection, binary overrides, tracking mode, allowed commands, and configured remote.
3. Verify Claude assets under `.claude/` and Codex assets under `.codex/`, `.agents/`, and `AGENTS.md` according to the selected integration mode.
4. Inspect installed hook scripts and compare them with the init manifest. Confirm prompt-submission and subagent registrations still point to `prompt-guard.py`.
5. Confirm bundled `.crosslink/rules/*.md` files are zero bytes. Treat nonempty managed files as drift while preserving supported `rules.local/` loader inputs.
6. Review session, issue, lock, trust, kickoff, and branch conventions against actual team practice.
7. Present drift, security implications, compatibility impact, and exact repair commands.

Offer targeted updates first. Use `crosslink init --update` for managed upgrades and `crosslink init --force` only when the user accepts replacement semantics. Preserve local overrides and never reset configuration merely to match defaults.
