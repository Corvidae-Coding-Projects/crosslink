---
allowed-tools: Bash(crosslink *), Bash(which *), Bash(tmux *)
description: Create a worktree and launch a background agent in tmux to implement a feature
argument-hint: <feature description> [--issue <id>] [--verify local|ci|thorough] [--container docker|podman]
---

## Context

- Current repo root: !`git rev-parse --show-toplevel`
- Current branch: !`git branch --show-current`
- tmux available: !`which tmux`
- agent binary available: !`which $(crosslink config get agent.binary 2>/dev/null || echo claude)`

## Your task

The user provides a feature description (e.g. "add batch retry logic") and optionally additional context. You will delegate to the `crosslink kickoff run` CLI command which handles worktree creation, agent prompt generation, and tmux session launch.

### Arguments

The user may pass these flags after the feature description:

- `--verify <level>`: Controls post-implementation verification depth.
  - `local` (default): Local tests + self-review checklist only.
  - `ci`: Push branch, open draft PR, wait for CI to pass, fix failures.
  - `thorough`: Everything in `ci` plus a structured adversarial self-review.
- `--issue <id>`: Use an existing crosslink issue instead of creating a new one.
- `--container <runtime>`: Use `docker` or `podman` instead of local tmux. Default: `none`.
- `--model <model>`: LLM model to use (provider/model format, e.g. `opencode-go/deepseek-v4-flash`, `google-vertex/gemini-3.1-pro-preview`). Default: from `hook-config.json` or `opus`.
- `--timeout <duration>`: Max runtime (e.g. `1h`, `30m`). Default: `1h`.
- `--template <path>`: Interpolate the built prompt into a template file rather than using the built-in prompt directly (gh#62). See **Prompt templates** below.
- All other text is the feature description.

**Parsing**: Split ARGUMENTS on whitespace. Extract recognized `--flag value` pairs. Everything remaining is the feature description.

### Steps

1. **Validate prerequisites**: Check that `tmux` and the configured agent binary are available (for local mode). If `--verify ci` or `--verify thorough`, check that `gh` is available. If missing, tell the user what to install and stop.

2. **Build the crosslink kickoff command**: Map parsed arguments to CLI flags:

```bash
crosslink kickoff run "<feature description>" \
  --verify <level> \
  --container <runtime> \
  --model <model> \
  --timeout <duration>
```

Add `--issue <id>` if the user specified one. Add `--dry-run` if the user asked for a dry run.

3. **Run the command**: Execute `crosslink kickoff run` with all flags. The CLI handles:
   - Creating the feature branch and worktree
   - Creating or assigning the crosslink issue
   - Initializing the agent identity
   - Detecting project conventions
   - Building the self-contained KICKOFF.md prompt
   - Launching the tmux session (or container)

4. **Report**: The CLI prints the summary. Relay it to the user. Remind them to:
   - Approve trust: `tmux attach -t <session-name>`
   - Check status: `crosslink kickoff status <agent-id>` or `/check <session-name>`

## Configuration

The agent binary and default model are configured via `hook-config.json`:

```jsonc
{
    "agent": {
        "binary": "opencode"
    },
    "sentinel": {
        "default_agent": {
            "model": "opencode-go/deepseek-v4-flash"
        }
    }
}
```

When a non-Claude binary is configured, the wrapper automatically omits Anthropic-specific environment variables (`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`) and credential mounts (`~/.claude`).

### Prompt templates (gh#62)

By default `kickoff run`/`plan` build a self-contained prompt. To wrap or extend it, supply a template whose body is *interpolated* (not replaced): the built prompt and its dispatch context are substituted into the placeholders the template references.

Sources, highest precedence first:

1. `--template <path>` — a per-invocation template file (dispatch-scoped, so parallel dispatches never race on one global path).
2. `agent.kickoff_template` in `hook-config.json` — a repo-global default (path resolved relative to the crosslink directory).
3. None — the built-in prompt is used unchanged.

`agent.no_template: true` skips the prompt entirely and overrides both.

Placeholders (Decision D2):

| Placeholder | Value |
| --- | --- |
| `{{built_prompt}}` | The full built-in prompt |
| `{{issue_id}}` | Issue number (`0` for a plan with no `--issue`) |
| `{{branch}}` | Feature branch (empty in plan mode) |
| `{{description}}` | Feature description (empty in plan mode) |
| `{{model}}` | `--model` value |
| `{{effort}}` | `--effort` value (empty if unset) |
| `{{doc_path}}` | Design-doc path (empty if none) |
| `{{allowed_tools}}` | Comma-joined allowed-tools list (empty in plan mode) |

A template with no placeholders is used verbatim (wholesale replacement), so existing `agent.kickoff_template` files keep working unchanged. The assembled prompt is still written to `KICKOFF.md` as the run's audit record.

**Swarm:** drop a per-phase template at `<crosslink-dir>/swarm-templates/<phase-slug>.md`; every agent launched for that phase uses it (overriding the repo-global config) while still rendering its own `{{description}}`.

## Constraints

- Never force-push or delete branches.
- Do not push the branch to a remote from this skill. (The child agent handles pushing when `--verify ci` or `--verify thorough`.)
- All prompt building and agent lifecycle is handled by `crosslink kickoff run`.
- If a tmux session with the same name already exists, the CLI appends a random suffix automatically.
