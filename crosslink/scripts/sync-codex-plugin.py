#!/usr/bin/env python3


from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import sys
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / "resources" / "plugins" / "crosslink-codex"
CANONICAL = ROOT / "resources" / "agent"
MARKETPLACE = ROOT / "resources" / ".agents" / "plugins" / "marketplace.json"
GENERATED_COMPONENTS = ("hooks", "mcp", "skills")
FORBIDDEN_OPERATIONAL_PATHS = (".claude/hooks/", ".claude/mcp/")


def digest(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()


def hook_config() -> bytes:
    source = json.loads((ROOT / "resources/providers/codex/hooks.json").read_text())
    pattern = re.compile(r"/\.crosslink/integrations/hooks/([a-z0-9_-]+\.py)")
    for groups in source["hooks"].values():
        for group in groups:
            for hook in group["hooks"]:
                match = pattern.search(hook["command"])
                if not match:
                    raise ValueError(f"cannot identify canonical hook in {hook['command']}")
                script = match.group(1)
                hook["command"] = (
                    f'CROSSLINK_HOOK_PROVIDER=codex python3 "$PLUGIN_ROOT/hooks/{script}"'
                )
                hook["commandWindows"] = (
                    "powershell -NoProfile -Command \""
                    "$env:CROSSLINK_HOOK_PROVIDER='codex'; "
                    f"py -3 (Join-Path $env:PLUGIN_ROOT 'hooks\\{script}'); "
                    "exit $LASTEXITCODE\""
                )
    return (json.dumps(source, indent=2) + "\n").encode()


def mcp_config() -> bytes:
    config = {
        "mcpServers": {
            "crosslink-knowledge": {
                "command": "uv",
                "args": ["run", "${PLUGIN_ROOT}/mcp/knowledge-server.py"],
            },
            "crosslink-agent-prompt": {
                "command": "uv",
                "args": ["run", "${PLUGIN_ROOT}/mcp/agent-prompt-server.py"],
            },
        }
    }
    return (json.dumps(config, indent=2) + "\n").encode()


def expected_files() -> dict[Path, bytes]:
    files: dict[Path, bytes] = {}
    provenance: dict[str, str] = {}
    for component in GENERATED_COMPONENTS:
        for source in sorted((CANONICAL / component).rglob("*")):
            if (
                not source.is_file()
                or "__pycache__" in source.parts
                or "fixtures" in source.parts
            ):
                continue
            relative = source.relative_to(CANONICAL)
            content = source.read_bytes()
            files[PLUGIN / relative] = content
            provenance[str(relative)] = digest(content)

    files[PLUGIN / "hooks/hooks.json"] = hook_config()
    files[PLUGIN / ".mcp.json"] = mcp_config()
    provenance["providers/codex/hooks.json"] = digest(
        (ROOT / "resources/providers/codex/hooks.json").read_bytes()
    )
    provenance["mcp.json"] = digest((ROOT / "resources/mcp.json").read_bytes())
    files[PLUGIN / "generated-assets.json"] = (
        json.dumps({"schema": 1, "canonical_sha256": provenance}, indent=2) + "\n"
    ).encode()
    return files


def validate_canonical_skills() -> list[str]:
    failures: list[str] = []
    for source in sorted((CANONICAL / "skills").rglob("*.md")):
        content = source.read_text()
        for stale_path in FORBIDDEN_OPERATIONAL_PATHS:
            if stale_path in content:
                failures.append(
                    f"{source.relative_to(ROOT)} (stale operational path {stale_path})"
                )
    return failures


def sync(check: bool) -> int:
    expected = expected_files()
    failures = validate_canonical_skills()
    expected_paths = set(expected)
    protected = {
        PLUGIN / ".codex-plugin" / "plugin.json",
    }
    actual_paths = {
        path
        for path in PLUGIN.rglob("*")
        if path.is_file() and "__pycache__" not in path.parts
    }
    for extra in sorted(actual_paths - expected_paths - protected):
        if check:
            failures.append(f"{extra.relative_to(ROOT)} (unexpected generated file)")
        else:
            extra.unlink()
    for destination, content in expected.items():
        if check:
            if not destination.is_file() or destination.read_bytes() != content:
                failures.append(str(destination.relative_to(ROOT)))
        else:
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(content)

    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text())
    manifest_path = PLUGIN / ".codex-plugin/plugin.json"
    manifest = json.loads(manifest_path.read_text())
    expected_version = cargo["package"]["version"]
    marketplace = json.loads(MARKETPLACE.read_text())
    entries = [
        entry
        for entry in marketplace.get("plugins", [])
        if entry.get("name") == manifest.get("name")
    ]
    if marketplace.get("name") != "personal" or len(entries) != 1:
        failures.append(f"{MARKETPLACE.relative_to(ROOT)} (plugin entry)")
    elif entries[0].get("source", {}).get("path") != "./plugins/crosslink-codex":
        failures.append(f"{MARKETPLACE.relative_to(ROOT)} (source path)")
    if check:
        if manifest.get("version") != expected_version:
            failures.append(f"{manifest_path.relative_to(ROOT)} (version)")
    else:
        manifest["version"] = expected_version
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")

    if failures:
        print("Codex plugin assets are stale:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        print("Run: python3 scripts/sync-codex-plugin.py", file=sys.stderr)
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    return sync(args.check)


if __name__ == "__main__":
    raise SystemExit(main())
