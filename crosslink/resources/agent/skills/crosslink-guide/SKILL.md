---
name: crosslink-guide
description: "Reference Crosslink’s issue, session, coordination, kickoff, swarm, knowledge, configuration, and diagnostic commands."
---

# Crosslink CLI guide

Crosslink keeps local state under `.crosslink/` and shares coordination data through dedicated Git refs. Confirm exact flags with `crosslink <group> <command> --help`.

## Issues

```bash
crosslink issue create "title" -p medium -d "details"
crosslink issue quick "title" -p high -l bug
crosslink issue list -s all
crosslink issue search "terms"
crosslink issue show <id>
crosslink issue update <id> -t "title" -p low
crosslink issue close <id>
crosslink issue close <id> --no-changelog
crosslink issue reopen <id>
crosslink issue delete <id>
crosslink issue label <id> <label>
crosslink issue block <id> <blocker-id>
crosslink issue relate <id> <other-id>
crosslink issue comment <id> "text" --kind decision
crosslink issue intervene <id> "description" --trigger <type> --context "activity"
crosslink issue ready
crosslink issue blocked
crosslink issue tree
```

Closing can update `CHANGELOG.md`; use `--no-changelog` when the closed item is not release-note material. `quick --work` needs an active session before it can select the new issue.

Issue identifiers may be positive shared display IDs, negative local IDs, or the CLI’s `L<number>` local form. Priorities are `critical`, `high`, `medium`, and `low`.

## Sessions and time

```bash
crosslink session start
crosslink session work <id>
crosslink session action "breadcrumb"
crosslink session status
crosslink session last-handoff
crosslink session end --notes "handoff"
crosslink timer start <id>
crosslink timer stop <id>
crosslink timer show <id>
```

## Shared coordination

```bash
crosslink agent init <name>
crosslink agent status
crosslink sync
crosslink locks list
crosslink locks claim <id>
crosslink locks release <id>
crosslink trust list
crosslink trust pending
crosslink trust approve <fingerprint>
crosslink compact
crosslink prune
```

## Knowledge and organization

```bash
crosslink knowledge add <title>
crosslink knowledge list
crosslink knowledge show <slug>
crosslink knowledge search <query>
crosslink milestone create <name>
crosslink milestone list
crosslink archive list
crosslink export <path>
crosslink import <path>
```

## Kickoff and swarm

```bash
crosslink kickoff run "task" --verify local
crosslink kickoff status <agent>
crosslink kickoff logs <agent>
crosslink kickoff report <agent>
crosslink kickoff stop <agent>
crosslink kickoff cleanup
crosslink swarm init
crosslink swarm status
crosslink swarm launch
crosslink swarm gate
crosslink swarm harvest
```

Kickoff may use tmux or a container and creates an isolated worktree. Monitor the sentinel and process output before declaring completion.

## Configuration and diagnostics

```bash
crosslink config show
crosslink config get <key>
crosslink config set <key> <value>
crosslink workflow diff
crosslink context check
crosslink integrity schema
crosslink integrity hydration
crosslink migrate to-shared
crosslink migrate from-shared
crosslink daemon status
crosslink tui
crosslink mc
crosslink serve
```

`--json` requests structured output where supported; `--quiet` suppresses nonessential terminal text. The `.crosslink/rules/*.md` files are zero bytes, and the existing prompt hook still resolves those paths.
