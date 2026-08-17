# Claude repository reference

This file describes Crosslink’s local development conventions for Claude Code. It is project documentation, not a substitute for the current user request or the provider’s instruction hierarchy.

## Development flow

Start repository work from `develop` unless the user selects another base. Use a focused branch, inspect the existing working tree before editing, and keep unrelated changes intact. Record substantial work in Crosslink when a session and issue are available.

Run the checks appropriate to the changed components. Do not bypass hooks, discard working-tree state, rewrite published history, or perform a destructive Git operation unless the user explicitly requests it. Publishing, pull requests, and merges follow the user’s current direction.

Use conventional commit subjects when the repository’s history does. Commit messages must describe the delivered behavior without provider attribution trailers.

## Output conventions

Write complete sentences. Refer to GitHub work as `owner/repository#number` and local Crosslink work as `#number`. Use repository-relative paths when documenting source locations.

## Crosslink commands

Issue lifecycle:

```bash
crosslink issue create "title" -p medium -d "details"
crosslink issue quick "title" -p medium -l feature
crosslink issue list -s all
crosslink issue show <id>
crosslink issue update <id> -t "new title"
crosslink issue comment <id> "note" --kind observation
crosslink issue close <id>
crosslink issue reopen <id>
crosslink issue delete <id>
crosslink issue ready
crosslink issue blocked
```

Sessions:

```bash
crosslink session start
crosslink session work <id>
crosslink session action "progress note"
crosslink session status
crosslink session end --notes "handoff"
```

Coordination and automation:

```bash
crosslink sync
crosslink agent status
crosslink locks list
crosslink kickoff run "task"
crosslink kickoff status <agent>
crosslink kickoff logs <agent>
crosslink swarm status
crosslink trust list
```

Project services:

```bash
crosslink knowledge list
crosslink config show
crosslink workflow diff
crosslink integrity schema
crosslink tui
crosslink serve
```

Most commands support `--json` for structured output and `--quiet` for reduced terminal output. Use `crosslink --help` and subcommand help as the authoritative command reference.

## Typical session

Begin with `crosslink session start`, select or create the active issue, record decisions while implementing, verify the change, and finish with a concise handoff. The files under `.crosslink/rules/` are intentionally zero bytes, while the prompt hook remains connected to their paths.
