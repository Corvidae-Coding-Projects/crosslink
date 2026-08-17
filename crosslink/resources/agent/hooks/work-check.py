#!/usr/bin/env python3









import json
import sys
import os
import io
import sqlite3
import re


sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8')


sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from crosslink_config import (
    find_crosslink_binary,
    find_crosslink_dir,
    is_agent_context,
    load_config_merged,
    normalize_git_command,
    run_crosslink,
)
from hook_protocol import claim_event, emit_warning, normalize_input


DEFAULT_BLOCKED_GIT = [
    "git push",  "git rebase",
    "git reset", "git clean",
]






DEFAULT_AGENT_BLOCKED_GIT = [
    "git push --force", "git push -f",
    "git merge", "git rebase", "git cherry-pick",
    "git reset",
    "git clean -f", "git clean -fd", "git clean -fdx",
    "git checkout .", "git restore .",
    "git stash", "git tag", "git am", "git apply",
    "git branch -d", "git branch -D", "git branch -m",
]



DEFAULT_GATED_GIT = [
    "git commit",
]

DEFAULT_ALLOWED_BASH = [
    "crosslink ",
    "git status", "git diff", "git log", "git branch", "git show",
    "jj log", "jj diff", "jj status", "jj show", "jj bookmark list",
    "cargo test", "cargo build", "cargo check", "cargo clippy", "cargo fmt",
    "npm test", "npm run", "npx ",
    "tsc", "node ", "python ",
    "ls", "dir", "pwd", "echo",

    "gh ",
    "cat ", "head ", "tail ", "wc ",
    "grep ", "rg ", "find ", "sort ", "uniq ",
    "which ", "command ",
    "mktemp", "sleep ",
    "date", "env", "uname", "id ",
    "basename ", "dirname ", "realpath ", "stat ", "file ",
]


def load_config(crosslink_dir):












    blocked = list(DEFAULT_BLOCKED_GIT)
    gated = list(DEFAULT_GATED_GIT)
    allowed = list(DEFAULT_ALLOWED_BASH)
    mode = "strict"
    discipline = "encouraged"
    is_agent = is_agent_context(crosslink_dir)

    config = load_config_merged(crosslink_dir)
    if not config:
        if is_agent:

            return "relaxed", list(DEFAULT_AGENT_BLOCKED_GIT), list(DEFAULT_GATED_GIT), allowed, True, "off"
        return mode, blocked, gated, allowed, False, discipline

    if config.get("tracking_mode") in ("strict", "normal", "relaxed"):
        mode = config["tracking_mode"]
    if "blocked_git_commands" in config:
        blocked = config["blocked_git_commands"]
    if "gated_git_commands" in config:
        gated = config["gated_git_commands"]
    if "allowed_bash_prefixes" in config:
        allowed = config["allowed_bash_prefixes"]
    if config.get("comment_discipline") in ("required", "encouraged", "off"):
        discipline = config["comment_discipline"]


    if is_agent:
        overrides = config.get("agent_overrides", {})
        mode = overrides.get("tracking_mode", "relaxed")
        blocked = overrides.get("blocked_git_commands", list(DEFAULT_AGENT_BLOCKED_GIT))


        gated = overrides.get("gated_git_commands", list(DEFAULT_GATED_GIT))
        discipline = overrides.get("comment_discipline", "off")

        for cmd in overrides.get("agent_lint_commands", []):
            if cmd not in allowed:
                allowed.append(cmd)
        for cmd in overrides.get("agent_test_commands", []):
            if cmd not in allowed:
                allowed.append(cmd)

    return mode, blocked, gated, allowed, is_agent, discipline


def _matches_command_list(command, cmd_list):





    normalized = normalize_git_command(command)
    for entry in cmd_list:
        if normalized.startswith(entry):
            return True

    for sep in (" && ", " ; ", " | "):
        for part in command.split(sep):
            part = part.strip()
            if part:
                norm_part = normalize_git_command(part)
                for entry in cmd_list:
                    if norm_part.startswith(entry):
                        return True
    return False


def is_blocked_git(input_data, blocked_list):

    command = input_data.get("tool_input", {}).get("command", "").strip()
    return _matches_command_list(command, blocked_list)


def is_gated_git(input_data, gated_list):

    command = input_data.get("tool_input", {}).get("command", "").strip()
    return _matches_command_list(command, gated_list)


def _is_single_command_allowed(command, allowed_list):

    for prefix in allowed_list:
        if command.startswith(prefix):
            return True
    return False


def is_allowed_bash(input_data, allowed_list):






    command = input_data.get("tool_input", {}).get("command", "").strip()
    if not command:
        return False


    parts = [command]
    for sep in (" && ", " ; ", " | "):
        expanded = []
        for part in parts:
            expanded.extend(part.split(sep))
        parts = expanded


    for part in parts:
        part = part.strip()
        if part and not _is_single_command_allowed(part, allowed_list):
            return False
    return True


def is_kickoff_status_edit(event):
    return (
        event.tool_kind == "edit"
        and bool(event.affected_paths)
        and not event.deleted_paths
        and all(os.path.basename(path) == ".kickoff-status" for path in event.affected_paths)
    )


def is_provider_memory_path(paths):

    if not paths:
        return False
    home = os.path.expanduser("~")
    provider_dirs = tuple(
        os.path.normcase(os.path.abspath(os.path.join(home, name))) + os.sep
        for name in (".claude", ".codex")
    )
    normalized = [os.path.normcase(os.path.abspath(path)) for path in paths]
    return all(any(path.startswith(root) for root in provider_dirs) for path in normalized)


def get_active_issue_id(crosslink_dir):




    status = run_crosslink(["session", "status", "--json"], crosslink_dir)
    if not status:
        return None
    try:
        data = json.loads(status)
        working_on = data.get("working_on")
        if working_on and working_on.get("id"):
            return int(working_on["id"])
    except (json.JSONDecodeError, ValueError, TypeError):
        pass
    return None


def issue_has_comment_kind(crosslink_dir, issue_id, kind):





    db_path = os.path.join(crosslink_dir, "issues.db")
    if not os.path.exists(db_path):
        return True
    try:
        conn = sqlite3.connect(db_path, timeout=1)
        cursor = conn.execute(
            "SELECT COUNT(*) FROM comments WHERE issue_id = ? AND kind = ?",
            (issue_id, kind),
        )
        count = cursor.fetchone()[0]
        conn.close()
        return count > 0
    except (sqlite3.Error, TypeError):
        return True


def is_issue_close_command(input_data):




    command = input_data.get("tool_input", {}).get("command", "").strip()


    m = re.search(r'crosslink\s+(?:-[qQ]\s+)?(?:issue\s+)?close\s+(\S+)', command)
    if m:
        issue_arg = m.group(1)

        if issue_arg.startswith('-'):
            return None
        return issue_arg
    return None


def check_control_flags(crosslink_dir):







    if not crosslink_dir:
        return
    import subprocess
    try:
        proc = subprocess.run(
            [find_crosslink_binary(crosslink_dir), "agent", "flags", "--strict"],
            capture_output=True,
            text=True,
            cwd=crosslink_dir,
            timeout=3,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):


        return
    if proc.returncode != 2:
        return




    try:
        state = json.loads(proc.stdout.strip())
    except (json.JSONDecodeError, ValueError):
        return
    if not isinstance(state, dict):
        return
    if state.get("kill"):


        print(
            "AGENT KILL REQUESTED — an operator (dashboard or CLI) has "
            "asked this agent to stop after the current tool use.\n"
            "Acknowledge the request, summarise progress, then exit "
            "your session cleanly. Do not attempt further tool calls.",
            file=sys.stderr,
        )
        sys.exit(2)
    if state.get("paused") or state.get("reprioritise"):
        hint = state.get("reprioritise")
        extra = ""
        if hint:
            extra = (
                f"\nReprioritise hint pending: switch focus to issue "
                f"#{hint.get('issue_id')} when resuming."
            )
        print(
            "AGENT PAUSED — an operator has paused this agent via the "
            "dashboard. Tool use is blocked until they resume.\n"
            "Wait for the resume signal or explain to the user that "
            "you've been paused." + extra,
            file=sys.stderr,
        )
        sys.exit(2)


def main():
    try:
        input_data = json.load(sys.stdin)
        event = normalize_input(input_data)
    except (json.JSONDecodeError, ValueError, TypeError):
        print(
            "work-check: failed to parse stdin — blocking tool call (fail-closed)",
            file=sys.stderr,
        )
        sys.exit(2)

    if event.tool_kind not in ("edit", "shell"):
        sys.exit(0)
    if not claim_event("crosslink-work-check", event):
        sys.exit(0)


    tool_name = "Bash" if event.tool_kind == "shell" else "Edit"
    tool_input = {"command": event.command}
    if event.affected_paths:
        tool_input["file_path"] = event.affected_paths[0]
    input_data = {"tool_name": tool_name, "tool_input": tool_input}



    check_control_flags(find_crosslink_dir())




    if tool_name == "Edit" and is_provider_memory_path(event.affected_paths):
        sys.exit(0)

    if is_kickoff_status_edit(event):
        sys.exit(0)

    crosslink_dir = find_crosslink_dir()
    tracking_mode, blocked_git, gated_git, allowed_bash, is_agent, comment_discipline = load_config(crosslink_dir)


    if tool_name == 'Bash' and is_blocked_git(input_data, blocked_git):
        print(
            "MANDATORY COMPLIANCE — DO NOT ATTEMPT TO WORK AROUND THIS BLOCK.\n\n"
            "Git mutation commands (push, merge, rebase, reset, etc.) are "
            "PERMANENTLY FORBIDDEN. The human performs all git write operations.\n\n"
            "You MUST NOT:\n"
            "  - Retry this command\n"
            "  - Rewrite the command to achieve the same effect\n"
            "  - Use a different tool to perform git mutations\n"
            "  - Ask the user if you should bypass this restriction\n\n"
            "You MUST instead:\n"
            "  - Inform the user that this is a manual step for them\n"
            "  - Continue with your other work\n\n"
            "Read-only git commands (status, diff, log, show, branch) are allowed.\n\n"
            "--- INTERVENTION LOGGING ---\n"
            "Log this blocked action for the audit trail:\n"
            "  crosslink issue intervene <issue-id> \"Attempted: <command>\" "
            "--trigger tool_blocked --context \"<what you were trying to accomplish>\"",
            file=sys.stderr,
        )
        sys.exit(2)


    if tool_name == 'Bash' and is_gated_git(input_data, gated_git):
        if not crosslink_dir:

            sys.exit(0)
        status = run_crosslink(["session", "status"], crosslink_dir)
        if not (status and ("Working on: #" in status or "Working on: L" in status)):
            print(
                "Git commit requires an active crosslink issue.\n\n"
                "Create one first:\n"
                "  crosslink quick \"<describe the work>\" -p <priority> -l <label>\n\n"
                "Or pick an existing issue:\n"
                "  crosslink issue list -s open\n"
                "  crosslink session work <id>\n\n"
                "--- INTERVENTION LOGGING ---\n"
                "If a human redirected you here, log the intervention:\n"
                "  crosslink issue intervene <issue-id> \"Redirected to create issue before commit\" "
                "--trigger redirect --context \"Attempted git commit without active issue\"",
                file=sys.stderr,
            )
            sys.exit(2)


        if comment_discipline in ("required", "encouraged"):
            issue_id = get_active_issue_id(crosslink_dir)
            if issue_id and not issue_has_comment_kind(crosslink_dir, issue_id, "plan"):
                msg = (
                    "Comment discipline: git commit requires a --kind plan comment "
                    "on the active issue before committing.\n\n"
                    "Add one now:\n"
                    "  crosslink issue comment {id} \"<your approach>\" --kind plan\n\n"
                    "This documents WHY the change was made, not just WHAT changed."
                ).format(id=issue_id)
                if comment_discipline == "required":
                    print(msg, file=sys.stderr)
                    sys.exit(2)
                else:
                    emit_warning(event, "Reminder: " + msg)

        sys.exit(0)


    if tool_name == 'Bash' and crosslink_dir and comment_discipline in ("required", "encouraged"):
        close_target = is_issue_close_command(input_data)
        if close_target:

            try:
                issue_id = int(close_target.lstrip('#'))
            except ValueError:

                show_output = run_crosslink(["issue", "show", close_target, "--json"], crosslink_dir)
                issue_id = None
                if show_output:
                    try:
                        issue_id = json.loads(show_output).get("id")
                    except (json.JSONDecodeError, TypeError):
                        pass

            if issue_id and not issue_has_comment_kind(crosslink_dir, issue_id, "result"):
                msg = (
                    "Comment discipline: closing an issue requires a --kind result "
                    "comment documenting what was delivered.\n\n"
                    "Add one now:\n"
                    "  crosslink issue comment {id} \"<what was delivered>\" --kind result\n\n"
                    "This creates the audit trail for the work that was done."
                ).format(id=issue_id)
                if comment_discipline == "required":
                    print(msg, file=sys.stderr)
                    sys.exit(2)
                else:
                    emit_warning(event, "Reminder: " + msg)


    if tool_name == 'Bash' and is_allowed_bash(input_data, allowed_bash):
        sys.exit(0)


    if tracking_mode == "relaxed":
        sys.exit(0)

    if not crosslink_dir:
        sys.exit(0)



    sentinel = os.path.join(crosslink_dir, ".active-issue")
    if os.path.isfile(sentinel):
        try:
            with open(sentinel) as f:
                content = f.read().strip()
            if content:
                sys.exit(0)
        except OSError:
            pass


    status = run_crosslink(["session", "status"], crosslink_dir)
    if not status:

        sys.exit(0)


    if "Working on: #" in status or "Working on: L" in status:
        sys.exit(0)


    strict_msg = (
        "MANDATORY COMPLIANCE — DO NOT ATTEMPT TO WORK AROUND THIS BLOCK.\n\n"
        "You cannot Write, Edit, or run Bash commands without an active crosslink issue. "
        "This is a hard requirement enforced by the project, not a suggestion.\n\n"
        "You MUST NOT:\n"
        "  - Retry the same tool call hoping it will work\n"
        "  - Try alternative tools or approaches to bypass this check\n"
        "  - Proceed without creating an issue first\n"
        "  - Tell the user the hook is preventing you from working\n\n"
        "You MUST do exactly this, right now, before anything else:\n\n"
        "  crosslink quick \"<describe what you are about to do>\" -p <priority> -l <label>\n\n"
        "This single command creates an issue AND sets it as your active work item. "
        "After running it, your tool calls will succeed.\n\n"
        "If an issue already exists for this work, use:\n"
        "  crosslink issue list -s open\n"
        "  crosslink session work <id>\n\n"
        "This is how the project tracks work. Comply, then continue.\n\n"
        "--- INTERVENTION LOGGING ---\n"
        "After creating the issue, log this intervention:\n"
        "  crosslink issue intervene <issue-id> \"Blocked: no active issue\" "
        "--trigger tool_blocked --context \"<what you were about to do>\""
    )

    normal_msg = (
        "Reminder: No active crosslink issue. You should create one before making changes.\n\n"
        "  crosslink quick \"<describe what you are about to do>\" -p <priority> -l <label>\n\n"
        "Or pick an existing issue:\n"
        "  crosslink issue list -s open\n"
        "  crosslink session work <id>"
    )

    if tracking_mode == "strict":
        print(strict_msg, file=sys.stderr)
        sys.exit(2)
    else:

        emit_warning(event, normal_msg)
        sys.exit(0)


if __name__ == "__main__":
    main()
