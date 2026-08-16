#!/usr/bin/env python3









import json
import os
import subprocess
import sys
import time


sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from crosslink_config import find_crosslink_binary, find_crosslink_dir
from hook_protocol import claim_event, normalize_input

HEARTBEAT_INTERVAL_SECONDS = 120


def main():
    try:
        event = normalize_input(json.load(sys.stdin))
    except (json.JSONDecodeError, ValueError, TypeError, OSError):
        sys.exit(0)
    if not claim_event("crosslink-heartbeat", event):
        sys.exit(0)





    crosslink_dir = find_crosslink_dir()

    if not crosslink_dir:
        sys.exit(0)


    if not os.path.exists(os.path.join(crosslink_dir, "agent.json")):
        sys.exit(0)


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


    try:
        os.makedirs(cache_dir, exist_ok=True)
        with open(stamp_file, "w") as f:
            f.write(str(now))
    except OSError:
        pass


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
