#!/usr/bin/env python3
"""Zenith example plugin.

Protocol: one JSON object per line on stdin/stdout.
  editor -> plugin: {"id": 1, "method": "execute", "params": {"command": "...", "context": {...}}}
  plugin -> editor: {"id": 1, "result": {"actions": [ ... ]}}
Actions: notify, replace_selection, insert, set_text, open_file, run_in_terminal, send_to_agent,
set_panel, set_status (status bar item; empty text removes it).
Print anything you like to stderr for logs.
"""
import json
import sys


def reply(msg_id, actions):
    sys.stdout.write(json.dumps({"id": msg_id, "result": {"actions": actions}}) + "\n")
    sys.stdout.flush()


def notify(text, level="info"):
    return {"type": "notify", "message": text, "level": level}


def handle(command, ctx):
    sel = ctx.get("selection") or ""
    text = ctx.get("text") or ""
    if command == "text-tools.word-count":
        target = sel or text
        words = len(target.split())
        lines = target.count("\n") + (1 if target else 0)
        scope = "selection" if sel else "file"
        return [notify(f"{words} words, {lines} lines, {len(target)} chars in {scope}")]
    if not sel and command != "text-tools.ai-docs":
        return [notify("Select some text first", "warning")]
    if command == "text-tools.sort-lines":
        ends_nl = sel.endswith("\n")
        lines = sorted(sel.rstrip("\n").split("\n"), key=str.lower)
        return [{"type": "replace_selection", "text": "\n".join(lines) + ("\n" if ends_nl else "")}]
    if command == "text-tools.upper":
        return [{"type": "replace_selection", "text": sel.upper()}]
    if command == "text-tools.lower":
        return [{"type": "replace_selection", "text": sel.lower()}]
    if command == "text-tools.ai-docs":
        code = sel or text
        lang = ctx.get("language") or ""
        prompt = f"Write clear documentation comments for this {lang} code:\n```\n{code}\n```"
        return [{"type": "send_to_agent", "text": prompt}]
    return [notify(f"Unknown command {command}", "error")]


def status_word_count(path):
    """Show the file's word count in the status bar (click = details)."""
    text = ""
    if path:
        try:
            with open(path, encoding="utf-8") as f:
                text = f.read()
        except (OSError, UnicodeDecodeError):
            text = ""
    words = len(text.split())
    sys.stdout.write(json.dumps({"method": "action", "params": {
        "type": "set_status", "id": "words", "align": "right",
        "text": f"{words} words" if path else "",
        "tooltip": "Word count of the saved file", "command": "text-tools.word-count",
    }}) + "\n")
    sys.stdout.flush()


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = msg.get("method")
        params = msg.get("params") or {}
        if method == "execute":
            try:
                reply(msg.get("id"), handle(params.get("command"), params.get("context") or {}))
            except Exception as e:  # never crash the plugin
                reply(msg.get("id"), [notify(f"text-tools error: {e}", "error")])
        elif method == "event":
            data = params.get("data") or {}
            if params.get("event") in ("active_editor_changed", "file_saved"):
                status_word_count(data.get("path"))
        elif method == "shutdown":
            break


if __name__ == "__main__":
    main()
