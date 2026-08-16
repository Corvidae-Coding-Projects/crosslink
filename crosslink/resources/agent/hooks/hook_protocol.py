#!/usr/bin/env python3


from __future__ import annotations

import hashlib
import json
import os
import re
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class HookEvent:
    provider: str
    event_name: str
    session_id: str
    turn_id: str
    tool_use_id: str
    tool_name: str
    tool_kind: str
    command: str
    affected_paths: list[str] = field(default_factory=list)
    deleted_paths: list[str] = field(default_factory=list)
    cwd: str = ""
    source: str = ""
    raw: dict = field(default_factory=dict)


def detect_provider(raw: dict) -> str:
    explicit = os.environ.get("CROSSLINK_HOOK_PROVIDER", "").strip().lower()
    if explicit in ("claude", "codex"):
        return explicit
    if raw.get("model") is not None or raw.get("turn_id") is not None:
        return "codex"
    return "claude"


def _patch_paths(command: str) -> tuple[list[str], list[str]]:
    paths: list[str] = []
    deleted: list[str] = []
    pending_update: str | None = None
    patterns = (
        (r"^\*\*\* (Add|Update|Delete) File: (.+)$", True),
        (r"^\*\*\* Move to: (.+)$", False),
        (r"^(?:\+\+\+|---) [ab]/(.+)$", False),
    )
    for line in command.splitlines():
        for pattern, has_operation in patterns:
            match = re.match(pattern, line)
            if match:
                operation = match.group(1) if has_operation else ""
                path = match.group(2 if has_operation else 1).strip()
                if operation == "Update":
                    pending_update = path
                elif not has_operation and line.startswith("*** Move to:"):



                    if pending_update and pending_update not in deleted:
                        deleted.append(pending_update)
                    pending_update = None
                elif has_operation:
                    pending_update = None
                if path != "/dev/null" and path not in paths:
                    paths.append(path)
                if operation == "Delete" and path not in deleted:
                    deleted.append(path)
                break
    return paths, deleted


def _canonicalize_paths(paths: list[str], cwd: str) -> list[str]:
    root = os.path.realpath(cwd)
    canonical: list[str] = []
    for path in paths:
        candidate = os.path.realpath(path if os.path.isabs(path) else os.path.join(root, path))
        try:
            if os.path.commonpath((root, candidate)) != root:
                raise ValueError(f"patch path escapes working root: {path}")
        except ValueError as error:
            raise ValueError(f"invalid patch path: {path}") from error
        if candidate not in canonical:
            canonical.append(candidate)
    return canonical


def normalize_input(raw: dict) -> HookEvent:
    provider = detect_provider(raw)
    tool_name = str(raw.get("tool_name", ""))
    tool_input = raw.get("tool_input") if isinstance(raw.get("tool_input"), dict) else {}
    command = str(tool_input.get("command", ""))
    if tool_name in ("Write", "Edit", "apply_patch"):
        tool_kind = "edit"
    elif tool_name == "Bash":
        tool_kind = "shell"
    elif tool_name.startswith("mcp__"):
        tool_kind = "mcp"
    else:
        tool_kind = "other"

    paths: list[str] = []
    deleted: list[str] = []
    file_path = tool_input.get("file_path")
    if isinstance(file_path, str) and file_path:
        paths.append(file_path)
    if tool_name == "apply_patch":
        patch_paths, deleted = _patch_paths(command)
        for path in patch_paths:
            if path not in paths:
                paths.append(path)
    cwd = str(raw.get("cwd") or os.getcwd())
    paths = _canonicalize_paths(paths, cwd)
    deleted = _canonicalize_paths(deleted, cwd)

    return HookEvent(
        provider=provider,
        event_name=str(raw.get("hook_event_name", "")),
        session_id=str(raw.get("session_id", "")),
        turn_id=str(raw.get("turn_id", "")),
        tool_use_id=str(raw.get("tool_use_id", "")),
        tool_name=tool_name,
        tool_kind=tool_kind,
        command=command,
        affected_paths=paths,
        deleted_paths=deleted,
        cwd=cwd,
        source=str(raw.get("source", "")),
        raw=raw,
    )


def emit_context(event: HookEvent, text: str) -> None:
    if event.provider == "codex" or event.event_name == "PostToolUse":
        payload = {
            "hookSpecificOutput": {
                "hookEventName": event.event_name,
                "additionalContext": text,
            }
        }
        print(json.dumps(payload))
    else:
        print(text)


def emit_warning(event: HookEvent, text: str) -> None:
    if event.provider == "codex":
        print(json.dumps({
            "hookSpecificOutput": {
                "hookEventName": event.event_name,
                "additionalContext": text,
            }
        }))
    else:
        print(text)


def block(reason: str) -> None:
    print(reason, file=sys.stderr)
    raise SystemExit(2)


def _project_root(event: HookEvent) -> Path:
    try:
        result = __import__("subprocess").run(
            ["git", "-C", event.cwd, "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            timeout=2,
        )
        if result.returncode == 0:
            return Path(result.stdout.strip())
    except (OSError, TimeoutError):
        pass
    return Path(event.cwd)


def claim_event(hook_id: str, event: HookEvent, ttl_seconds: int = 600) -> bool:

    identity = "\0".join(
        (
            hook_id,
            event.provider,
            event.event_name,
            event.session_id,
            event.turn_id,
            event.tool_use_id,
            event.tool_name,
            event.source,
        )
    )
    if not any((event.session_id, event.turn_id, event.tool_use_id)):
        return True
    digest = hashlib.sha256(identity.encode("utf-8")).hexdigest()
    cache = _project_root(event) / ".crosslink" / ".cache" / "hook-dedupe"
    try:
        cache.mkdir(parents=True, exist_ok=True)
        now = time.time()
        for entry in cache.iterdir():
            try:
                if now - entry.stat().st_mtime > ttl_seconds:
                    entry.unlink()
            except OSError:
                continue
        descriptor = os.open(cache / digest, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        os.close(descriptor)
        return True
    except FileExistsError:
        return False
    except OSError:
        return True
