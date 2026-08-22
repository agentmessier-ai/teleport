#!/usr/bin/env python3
"""Idempotently merge the SessionStart/SessionEnd hooks (same-machine poke,
docs/same-machine-poke-design.md §1) into ~/.claude/settings.json — without
clobbering unrelated hooks (one from an unrelated project) or any
other key in the file.

Run with --uninstall to remove exactly the entries this script would have
added (matched by command string), leaving everything else untouched.
"""

import json
import sys
from pathlib import Path

SETTINGS_PATH = Path.home() / ".claude" / "settings.json"
TP_BIN = str(Path.home() / ".local" / "bin" / "tp")

HOOK_COMMANDS = {
    "SessionStart": f"{TP_BIN} register --from-hook",
    "SessionEnd": f"{TP_BIN} unregister --from-hook",
}


def load_settings() -> dict:
    if not SETTINGS_PATH.exists():
        return {}
    with open(SETTINGS_PATH) as f:
        return json.load(f)


def save_settings(settings: dict) -> None:
    SETTINGS_PATH.parent.mkdir(parents=True, exist_ok=True)
    with open(SETTINGS_PATH, "w") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")


def install() -> None:
    settings = load_settings()
    hooks = settings.setdefault("hooks", {})
    for event, command in HOOK_COMMANDS.items():
        matchers = hooks.setdefault(event, [])
        already = any(
            h.get("command") == command for m in matchers for h in m.get("hooks", [])
        )
        if already:
            continue
        matchers.append({"hooks": [{"type": "command", "command": command}]})
    save_settings(settings)
    print(f"teleport: SessionStart/SessionEnd hooks present in {SETTINGS_PATH}")


def uninstall() -> None:
    settings = load_settings()
    hooks = settings.get("hooks", {})
    for event, command in HOOK_COMMANDS.items():
        if event not in hooks:
            continue
        kept = []
        for matcher in hooks[event]:
            matcher["hooks"] = [
                h for h in matcher.get("hooks", []) if h.get("command") != command
            ]
            if matcher["hooks"]:
                kept.append(matcher)
        hooks[event] = kept
        if not hooks[event]:
            del hooks[event]
    if not hooks:
        settings.pop("hooks", None)
    save_settings(settings)
    print(f"teleport: removed SessionStart/SessionEnd hooks from {SETTINGS_PATH}")


if __name__ == "__main__":
    (uninstall if "--uninstall" in sys.argv else install)()
