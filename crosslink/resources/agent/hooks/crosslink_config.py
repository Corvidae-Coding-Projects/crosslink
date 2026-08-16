#!/usr/bin/env python3







import json
import os
import subprocess
from contextlib import suppress


def project_root_from_script():

    try:
        current = os.path.dirname(os.path.abspath(__file__))
        for _ in range(10):
            marker = os.path.join(current, ".crosslink", "hook-config.json")
            if os.path.isfile(marker):
                return current
            parent = os.path.dirname(current)
            if parent == current:
                break
            current = parent
        return None
    except (NameError, OSError):
        return None


def get_project_root():





    root = project_root_from_script()
    if root and os.path.isdir(root):
        return root
    return os.getcwd()


def _resolve_main_repo_root(start_dir):






    try:
        common = subprocess.run(
            ["git", "-C", start_dir, "rev-parse", "--git-common-dir"],
            capture_output=True, text=True, timeout=3
        )
        git_dir = subprocess.run(
            ["git", "-C", start_dir, "rev-parse", "--git-dir"],
            capture_output=True, text=True, timeout=3
        )
        if common.returncode != 0 or git_dir.returncode != 0:
            return None

        common_path = os.path.realpath(
            common.stdout.strip() if os.path.isabs(common.stdout.strip())
            else os.path.join(start_dir, common.stdout.strip())
        )
        git_dir_path = os.path.realpath(
            git_dir.stdout.strip() if os.path.isabs(git_dir.stdout.strip())
            else os.path.join(start_dir, git_dir.stdout.strip())
        )

        if common_path != git_dir_path:

            return os.path.dirname(common_path)
        return start_dir
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
        return None


def _is_initialized_crosslink_dir(candidate):








    return os.path.isfile(os.path.join(candidate, 'hook-config.json'))


def find_crosslink_dir():









    root = project_root_from_script()
    if root:
        candidate = os.path.join(root, '.crosslink')
        if os.path.isdir(candidate) and _is_initialized_crosslink_dir(candidate):
            return candidate


    current = os.getcwd()
    start = current
    for _ in range(10):
        candidate = os.path.join(current, '.crosslink')
        if os.path.isdir(candidate) and _is_initialized_crosslink_dir(candidate):
            return candidate
        parent = os.path.dirname(current)
        if parent == current:
            break
        current = parent


    main_root = _resolve_main_repo_root(start)
    if main_root:
        candidate = os.path.join(main_root, '.crosslink')
        if os.path.isdir(candidate) and _is_initialized_crosslink_dir(candidate):
            return candidate

    return None


def _merge_with_extend(base, override):














    for key, value in override.items():
        if key.startswith("+"):
            real_key = key[1:]
            if isinstance(value, list) and isinstance(base.get(real_key), list):
                base[real_key] = base[real_key] + value
            else:
                base[real_key] = value
        else:
            base[key] = value
    return base


def load_config_merged(crosslink_dir):







    if not crosslink_dir:
        return {}

    config = {}
    config_path = os.path.join(crosslink_dir, "hook-config.json")
    if os.path.isfile(config_path):


        with suppress(json.JSONDecodeError, OSError):
            with open(config_path, "r", encoding="utf-8") as f:
                config = json.load(f)

    local_path = os.path.join(crosslink_dir, "hook-config.local.json")
    if os.path.isfile(local_path):
        with suppress(json.JSONDecodeError, OSError):
            with open(local_path, "r", encoding="utf-8") as f:
                local = json.load(f)
            _merge_with_extend(config, local)

    return config


def load_tracking_mode(crosslink_dir):

    config = load_config_merged(crosslink_dir)
    mode = config.get("tracking_mode", "strict")
    if mode in ("strict", "normal", "relaxed"):
        return mode
    return "strict"


def find_crosslink_binary(crosslink_dir):

    import shutil


    config = load_config_merged(crosslink_dir)
    bin_path = config.get("crosslink_binary")
    if bin_path and os.path.isfile(bin_path) and os.access(bin_path, os.X_OK):
        return bin_path


    found = shutil.which("crosslink")
    if found:
        return found


    home = os.path.expanduser("~")
    candidate = os.path.join(home, ".cargo", "bin", "crosslink")
    if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
        return candidate


    root = project_root_from_script()
    if root:
        for profile in ("release", "debug"):
            candidate = os.path.join(root, "crosslink", "target", profile, "crosslink")
            if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                return candidate

    return "crosslink"


def load_guard_state(crosslink_dir):








    if not crosslink_dir:
        return {"prompts_since_crosslink": 0, "total_prompts": 0,
                "last_crosslink_at": None, "last_reminder_at": None}
    state_path = os.path.join(crosslink_dir, ".cache", "guard-state.json")
    try:
        with open(state_path, "r", encoding="utf-8") as f:
            state = json.load(f)

        state.setdefault("prompts_since_crosslink", 0)
        state.setdefault("total_prompts", 0)
        state.setdefault("last_crosslink_at", None)
        state.setdefault("last_reminder_at", None)
        return state
    except (OSError, json.JSONDecodeError):
        return {"prompts_since_crosslink": 0, "total_prompts": 0,
                "last_crosslink_at": None, "last_reminder_at": None}


def save_guard_state(crosslink_dir, state):

    if not crosslink_dir:
        return
    cache_dir = os.path.join(crosslink_dir, ".cache")

    with suppress(OSError):
        os.makedirs(cache_dir, exist_ok=True)
        state_path = os.path.join(cache_dir, "guard-state.json")
        with open(state_path, "w", encoding="utf-8") as f:
            json.dump(state, f)


def reset_drift_counter(crosslink_dir):

    if not crosslink_dir:
        return
    from datetime import datetime
    state = load_guard_state(crosslink_dir)
    state["prompts_since_crosslink"] = 0
    state["last_crosslink_at"] = datetime.now().isoformat()
    save_guard_state(crosslink_dir, state)


def is_agent_context(crosslink_dir):


















    if not crosslink_dir:
        return False
    agent_json_path = os.path.join(crosslink_dir, "agent.json")
    if os.path.isfile(agent_json_path):


        data = None
        with suppress(json.JSONDecodeError, OSError):
            with open(agent_json_path, "r", encoding="utf-8") as f:
                data = json.load(f)
        if isinstance(data, dict) and data.get("role") == "agent":
            return True

    cwd = None
    with suppress(OSError):
        cwd = os.getcwd()
    normalized_cwd = cwd.replace(os.sep, "/") if cwd else ""
    if any(marker in normalized_cwd for marker in (
        "/.claude/worktrees/", "/.codex/worktrees/"
    )):
        return True
    return False


def normalize_git_command(command):






    import shlex

    try:
        parts = shlex.split(command)
    except ValueError:
        return command

    if not parts or parts[0] != "git":
        return command

    i = 1
    while i < len(parts):

        if parts[i] in ("-C", "--git-dir", "--work-tree", "-c") and i + 1 < len(parts):
            i += 2

        elif (
            parts[i].startswith("--git-dir=")
            or parts[i].startswith("--work-tree=")
        ):
            i += 1
        else:
            break

    if i < len(parts):
        return "git " + " ".join(parts[i:])
    return command


_crosslink_bin = None


def run_crosslink(args, crosslink_dir=None):

    global _crosslink_bin
    if _crosslink_bin is None:
        _crosslink_bin = find_crosslink_binary(crosslink_dir)
    try:
        result = subprocess.run(
            [_crosslink_bin] + args,
            capture_output=True,
            text=True,
            timeout=3
        )
        return result.stdout.strip() if result.returncode == 0 else None
    except (subprocess.TimeoutExpired, FileNotFoundError, Exception):
        return None
