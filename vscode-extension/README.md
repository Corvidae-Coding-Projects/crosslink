# Crosslink Issue Tracker - VS Code Extension

A simple, lean issue tracker for AI-assisted development, integrated directly into VS Code.

## Features

- **Session Management**: Start/end work sessions with handoff notes for context preservation
- **Context Compression Resilience**: Breadcrumb tracking via `session action` survives AI context resets
- **Issue Tracking**: Create, update, and manage issues without leaving your editor
- **Quick Workflow**: `crosslink quick` creates, labels, and starts work in one command
- **Issue Templates**: Built-in templates for bugs, features, audits, investigations, and more
- **JSON & Quiet Modes**: `--json` for structured output, `--quiet` for pipe-friendly results
- **Stale Session Detection**: Auto-ends sessions idle >4 hours on next startup
- **Daemon Auto-Start**: Background daemon keeps session state fresh
- **Cross-Platform**: Works on Windows, Linux, and macOS
- **Agent-Agnostic**: Context provider script works with any AI coding assistant

## Requirements

- **Python 3.6+**: Required for Claude Code hooks to function. The extension will warn you if Python is not detected in your PATH.

## Installation

1. Install from the VS Code Extensions Marketplace (search "Crosslink Issue Tracker")
2. Open a project folder
3. Run `Crosslink: Initialize Project` from the command palette

## Commands

All commands are available from the VS Code Command Palette (Ctrl+Shift+P / Cmd+Shift+P).

### Session Management

| VS Code Command | CLI Equivalent | Description |
|-----------------|----------------|-------------|
| `Crosslink: Start Session` | `crosslink session start` | Start a new work session |
| `Crosslink: End Session` | `crosslink session end --notes "..."` | End session with optional handoff notes |
| `Crosslink: Session Status` | `crosslink session status` | Show current session info and last action |
| `Crosslink: Set Working Issue` | `crosslink session work <id>` | Set the issue you're currently working on |
| `Crosslink: Record Action Breadcrumb` | `crosslink session action "..."` | Record a breadcrumb (survives context compression) |
| `Crosslink: Show Last Handoff Notes` | `crosslink session last-handoff` | Retrieve handoff notes from the previous session |

### Issue Creation

| VS Code Command | CLI Equivalent | Description |
|-----------------|----------------|-------------|
| `Crosslink: Create Issue` | `crosslink create <title> -p <priority>` | Create a new issue with priority picker |
| `Crosslink: Quick Create` | `crosslink quick <title> -p <pri> -l <label>` | Create + label + set as active work item |
| `Crosslink: Create from Template` | `crosslink create <title> --template <tmpl>` | Create from template (bug/feature/audit/etc.) |
| `Crosslink: Create Subissue` | `crosslink subissue <parent> <title>` | Create a subissue under a parent |

### Issue Management

| VS Code Command | CLI Equivalent | Description |
|-----------------|----------------|-------------|
| `Crosslink: Show Issue Details` | `crosslink show <id>` | View details of a specific issue |
| `Crosslink: Update Issue` | `crosslink update <id> ...` | Update title, description, or priority |
| `Crosslink: Close Issue` | `crosslink close <id>` | Close an issue |
| `Crosslink: Close All Issues` | `crosslink close-all` | Close all open issues (with confirmation) |
| `Crosslink: Reopen Issue` | `crosslink reopen <id>` | Reopen a closed issue |
| `Crosslink: Delete Issue` | `crosslink delete <id>` | Delete an issue (with confirmation) |

### Comments, Labels & Dependencies

| VS Code Command | CLI Equivalent | Description |
|-----------------|----------------|-------------|
| `Crosslink: Add Comment` | `crosslink comment <id> "text"` | Add a comment to an issue |
| `Crosslink: Add Label` | `crosslink label <id> <label>` | Add a label to an issue |
| `Crosslink: Remove Label` | `crosslink unlabel <id> <label>` | Remove a label from an issue |
| `Crosslink: Block Issue` | `crosslink block <id> <blocker>` | Mark issue as blocked by another |
| `Crosslink: Unblock Issue` | `crosslink unblock <id> <blocker>` | Remove blocking relationship |
| `Crosslink: Relate Issues` | `crosslink relate <id1> <id2>` | Link two related issues together |
| `Crosslink: Unrelate Issues` | `crosslink unrelate <id1> <id2>` | Remove relationship between issues |

### Navigation & Search

| VS Code Command | CLI Equivalent | Description |
|-----------------|----------------|-------------|
| `Crosslink: List Issues` | `crosslink list` | Show all open issues |
| `Crosslink: Show Ready Issues` | `crosslink ready` | List issues ready to work on (no blockers) |
| `Crosslink: Show Blocked Issues` | `crosslink blocked` | List all blocked issues |
| `Crosslink: Suggest Next Issue` | `crosslink next` | Recommend the next issue to work on |
| `Crosslink: Show Issue Tree` | `crosslink tree` | Show all issues in a tree hierarchy |
| `Crosslink: Search Issues` | `crosslink search <query>` | Search issues by keyword |

### Setup & Daemon

| VS Code Command | CLI Equivalent | Description |
|-----------------|----------------|-------------|
| `Crosslink: Initialize Project` | `crosslink init` | Initialize crosslink in current workspace |
| `Crosslink: Start Daemon` | `crosslink daemon start` | Manually start the background daemon |
| `Crosslink: Stop Daemon` | `crosslink daemon stop` | Stop the background daemon |
| `Crosslink: Daemon Status` | `crosslink daemon status` | Check if daemon is running |

> **Tip:** All commands also work via CLI. Add `--quiet` / `-q` for minimal output, or `--json` for structured output.

## Configuration

### VS Code Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `crosslink.binaryPath` | `""` | Override path to crosslink binary (for development) |
| `crosslink.autoStartDaemon` | `true` | Auto-start daemon when .crosslink project detected |
| `crosslink.showOutputChannel` | `false` | Show output channel for daemon logs |

### Hook Configuration

AI behavior is controlled by `.crosslink/hook-config.json` in your project:

```json
{
  "tracking_mode": "strict",
  "blocked_git_commands": ["git push", "git commit", "..."],
  "allowed_bash_prefixes": ["crosslink ", "git status", "..."]
}
```

#### Tracking Mode

| Mode | Behavior | Best For |
|------|----------|----------|
| `strict` | **Blocks** code changes without an active issue. Forceful prompt language. | Every change must be tracked |
| `normal` | **Reminds** but doesn't block. Gentle prompt language. | Balanced — tracks most work |
| `relaxed` | **No enforcement**. Only git mutation blocks apply. | Opt-in tracking only |

Each mode loads its wording from `.crosslink/rules/tracking-{mode}.md` — edit these files to customize the prompt language.

#### Git Command Blocking

Git mutation commands (push, commit, merge, rebase, etc.) are **permanently blocked in all modes**. Read-only commands (status, diff, log) are always allowed. Both lists are customizable in `hook-config.json`.

### Customizable Rules

Rules in `.crosslink/rules/` control what gets injected into AI prompts:

| File | Purpose |
|------|---------|
| `global.md` | Security, correctness, and style rules |
| `tracking-strict.md` | Strict mode issue tracking instructions |
| `tracking-normal.md` | Normal mode issue tracking instructions |
| `tracking-relaxed.md` | Relaxed mode tracking reference |
| `project.md` | Your project-specific rules |
| `rust.md`, `python.md`, etc. | Language-specific best practices |

Edit any file and changes take effect on the next prompt. Reset with `crosslink init --force`.

## Development

### Building the Extension

```bash
# Install dependencies
cd vscode-extension
npm install

# Compile TypeScript
npm run compile

# Build binaries for all platforms
npm run build:binaries

# Package the extension
npm run package
```

### Building Binaries

The extension bundles platform-specific binaries. To build them:

```bash
# Build all platforms (Windows native, Linux via WSL)
node scripts/build-binaries.js

# Build specific platform
node scripts/build-binaries.js --platform windows
node scripts/build-binaries.js --platform linux
```

**Requirements:**
- Windows: Visual Studio Build Tools with Rust
- Linux: WSL with Fedora 42 (or another distro with Rust installed)
- macOS: Xcode Command Line Tools with Rust

### Testing Locally

1. Open the `vscode-extension` folder in VS Code
2. Press F5 to launch Extension Development Host
3. Set `crosslink.binaryPath` to your local debug binary path

## Architecture

```
vscode-extension/
├── src/
│   ├── extension.ts    # Extension entry point, command registration
│   ├── daemon.ts       # Daemon lifecycle management
│   └── platform.ts     # Platform detection, binary resolution
├── bin/                # Platform binaries (populated by build script)
│   ├── crosslink-win.exe
│   ├── crosslink-linux
│   └── crosslink-darwin
├── scripts/
│   └── build-binaries.js  # Cross-compilation orchestration
└── package.json
```

## Daemon Behavior

The daemon runs as a background process that:
- Auto-flushes session state every 30 seconds
- Self-terminates when VS Code closes (zombie prevention via stdin monitoring)
- Writes logs to `.crosslink/daemon.log`

## Using with Any AI Agent

Crosslink includes a context provider script that works with **any** AI coding assistant, not just Claude Code.

### Context Provider

After running `Crosslink: Initialize Project`, you'll have a context provider at:
```
.crosslink/integrations/context-provider.py
```

This script generates intelligent context including:
- Current session state and handoff notes
- Open/ready issues
- Project structure
- Language-specific coding rules

### Shell Aliases

Add to your `~/.bashrc`, `~/.zshrc`, or PowerShell profile:

**Bash/Zsh:**
```bash
# Copy crosslink context to clipboard
crosslink-ctx() {
    python .crosslink/integrations/context-provider.py --clipboard
}

# Aider with crosslink context
aider-cl() {
    python .crosslink/integrations/context-provider.py --format md > /tmp/cl-ctx.md
    aider --read /tmp/cl-ctx.md "$@"
}
```

**PowerShell:**
```powershell
function crosslink-ctx {
    python .crosslink\integrations\context-provider.py | Set-Clipboard
}
```

### Usage Examples

```bash
# Full context (XML format, best for LLMs)
python .crosslink/integrations/context-provider.py

# Markdown format (human readable)
python .crosslink/integrations/context-provider.py --format md

# Just coding rules
python .crosslink/integrations/context-provider.py --rules

# Copy to clipboard for web UIs
python .crosslink/integrations/context-provider.py --clipboard

# Generate .cursorrules for Cursor
python .crosslink/integrations/context-provider.py --format md --rules > .cursorrules
```

### Agent-Specific Integration

| Agent | Method |
|-------|--------|
| **Cursor** | `python context-provider.py --format md --rules > .cursorrules` |
| **Aider** | `aider --read context.md` (generate context.md first) |
| **Continue.dev** | Add exec context provider in `.continue/config.json` |
| **Web UIs** | `--clipboard` then paste as first message |
| **Claude Code** | Built-in hooks, no setup needed |

### What Gets Injected

```xml
<crosslink-session>
Session #5 active
Working on: #12 Fix authentication bug
</crosslink-session>

<crosslink-issues>
Ready issues (unblocked):
  #12   high     Fix authentication bug
</crosslink-issues>

<coding-rules>
### Rust Best Practices
- Use `?` operator over `.unwrap()`
...
</coding-rules>
```

For full documentation, see the [main README](https://github.com/dollspace-gay/crosslink#using-crosslink-with-any-ai-agent).

## License

MIT
