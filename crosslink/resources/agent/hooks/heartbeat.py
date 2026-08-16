#!/usr/bin/env python3
"""
Provider-neutral PostToolUse hook that pushes agent heartbeats on a throttled interval.

Fires on every tool call but only invokes `crosslink heartbeat` if at least
2 minutes have elapsed since the last push. This gives accurate liveness
detection: heartbeats flow while an agent is actively working, and stop when
it hangs — which is exactly the staleness signal lock detection needs.
"""

import json
import os
import subprocess
import sys
import time

# Add hooks directory to path for shared module import
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from crosslink_config import find_crosslink_binary, find_crosslink_dir
from hook_protocol import claim_event, normalize_input

HEARTBEAT_INTERVAL_SECONDS = 120  # 2 minutes


def main():
    try:
        event = normalize_input(json.load(sys.stdin))
    except (json.JSONDecodeError, ValueError, TypeError, OSError):
        sys.exit(0)
    if not claim_event("crosslink-heartbeat", event):
        sys.exit(0)

    # Find the INITIALIZED .crosslink directory (GH#625-safe): candidates
    # without hook-config.json are strays seeded by cwd drift and are skipped,
    # never bound — binding one would throttle/cache heartbeats in the wrong
    # place.
    crosslink_dir = find_crosslink_dir()

    if not crosslink_dir:
        sys.exit(0)

    # Only push heartbeats if we're in an agent context (agent.json exists)
    if not os.path.exists(os.path.join(crosslink_dir, "agent.json")):
        sys.exit(0)

    # Throttle: check timestamp file
    cache_dir = os.path.join(crosslink_dir, ".cache")
    stamp_file = os.path.join(cache_dir, "last-heartbeat")

    now = time.time()
    try:
        if os.path.exists(stamp_file):
            last = os.path.getmtime(stamp_file)
            if now - last < HEARTBEAT_INTERVAL_SECONDS:
                sys.exit(0)
    except OSError:
        pass

    # Update timestamp before pushing (avoid thundering herd on slow push)
    try:
        os.makedirs(cache_dir, exist_ok=True)
        with open(stamp_file, "w") as f:
            f.write(str(now))
    except OSError:
        pass

    # Push heartbeat in background (don't block the tool call)
    try:
        subprocess.Popen(
            [find_crosslink_binary(crosslink_dir), "heartbeat"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except OSError:
        pass

    sys.exit(0)


if __name__ == "__main__":
    main()
