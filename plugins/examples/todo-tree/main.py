#!/usr/bin/env python3
"""Zenith example plugin with a sidebar panel.

editor -> plugin: {"method": "event", "params": {"event": "panel_opened", "data": {"panel": "todos", "context": {...}}}}
plugin -> editor: {"method": "action", "params": {"type": "set_panel", "panel": "todos", "items": [...]}}

Panel item fields: label, detail, icon, header, indent, path + line (click opens
the file), command + id (click runs a plugin command), tooltip.
"""
import json
import os
import re
import subprocess
import sys

TAG = re.compile(r"\b(TODO|FIXME|HACK|XXX)\b[:\s]*(.*)")
SKIP_DIRS = {".git", "target", "node_modules", "dist", "build", ".venv", "__pycache__"}
ICONS = {"TODO": "○", "FIXME": "⚠", "HACK": "◇", "XXX": "◆"}
MAX_FILES = 5000

workspace = None
panel_open = False
last = []  # (rel, line, tag, text)


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def files(root):
    """Tracked + untracked (not ignored) files via git, else a directory walk."""
    try:
        out = subprocess.run(
            ["git", "ls-files", "-co", "--exclude-standard"], cwd=root, capture_output=True, text=True, timeout=10
        )
        if out.returncode == 0:
            return out.stdout.splitlines()[:MAX_FILES]
    except (OSError, subprocess.SubprocessError):
        pass
    found = []
    for d, dirs, names in os.walk(root):
        dirs[:] = [x for x in dirs if x not in SKIP_DIRS and not x.startswith(".")]
        for n in names:
            found.append(os.path.relpath(os.path.join(d, n), root))
            if len(found) >= MAX_FILES:
                return found
    return found


def scan(root):
    hits = []
    for rel in files(root):
        path = os.path.join(root, rel)
        try:
            if os.path.getsize(path) > 1_000_000:
                continue
            with open(path, encoding="utf-8") as f:
                for i, line in enumerate(f, 1):
                    m = TAG.search(line)
                    if m:
                        hits.append((rel.replace(os.sep, "/"), i, m.group(1), m.group(2).strip()))
        except (OSError, UnicodeDecodeError):
            continue
    return hits


def refresh():
    global last
    if not workspace:
        send_panel([], "Open a folder to see its TODOs.")
        return
    last = scan(workspace)
    items, current = [], None
    for rel, line, tag, text in last:
        if rel != current:
            current = rel
            count = sum(1 for h in last if h[0] == rel)
            items.append({"label": os.path.basename(rel), "detail": f"{os.path.dirname(rel)}  {count}", "header": True})
        items.append({
            "icon": ICONS.get(tag, "○"),
            "label": text or tag,
            "detail": f"{tag} · L{line}",
            "indent": 1,
            "path": rel,
            "line": line,
            "tooltip": f"{rel}:{line}",
        })
    n = len(last)
    send_panel(items, "No TODOs found." if n == 0 else f"{n} item(s) in {len({h[0] for h in last})} file(s)")


def send_panel(items, message):
    buttons = [{"title": "✳ Ask AI", "command": "todo-tree.ask-ai"}] if last else []
    send({"method": "action", "params": {
        "type": "set_panel", "panel": "todos", "items": items, "message": message, "buttons": buttons,
    }})


def ask_ai():
    if not last:
        return [{"type": "notify", "message": "No TODOs to resolve"}]
    lines = "\n".join(f"- @{rel}:{line} {tag}: {text}" for rel, line, tag, text in last[:50])
    prompt = f"Here are the open TODO/FIXME comments in this project:\n{lines}\nPick the most important ones and resolve them."
    return [{"type": "send_to_agent", "text": prompt}]


def main():
    global workspace, panel_open
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            continue
        method, params = msg.get("method"), msg.get("params") or {}
        try:
            if method == "event":
                data = params.get("data") or {}
                if params.get("event") == "panel_opened":
                    panel_open = True
                    workspace = (data.get("context") or {}).get("workspace") or workspace
                    refresh()
                elif params.get("event") == "file_saved" and panel_open:
                    refresh()
            elif method == "execute":
                ctx = params.get("context") or {}
                workspace = ctx.get("workspace") or workspace
                actions = ask_ai() if params.get("command") == "todo-tree.ask-ai" else []
                send({"id": msg.get("id"), "result": {"actions": actions}})
            elif method == "shutdown":
                break
        except Exception as e:  # never crash the plugin
            print(f"todo-tree error: {e}", file=sys.stderr)


if __name__ == "__main__":
    main()
