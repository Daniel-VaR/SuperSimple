#!/usr/bin/env python3
"""Print a compact digest of the last N days of Claude Code session transcripts.

Scans $SUPERSIMPLE_TRANSCRIPTS_DIR (default ~/.claude/projects) for session
files (*.jsonl). For every session with activity in the period it prints the
project, time span, the user's requests (shortened), files changed and git
commits. The output is size-capped so a busy week cannot blow up the token
bill; anything left out is stated explicitly at the end.
"""
import argparse
import glob
import json
import os
import re
from datetime import datetime, timedelta, timezone

MAX_TOTAL_CHARS = 45000
NOISE_PREFIXES = ("<", "Caveat:", "[Request interrupted", "This session is being continued")


def parse_time(value):
    if not value:
        return None
    try:
        return datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    except ValueError:
        return None


def local(dt, fmt="%m-%d %H:%M"):
    return dt.astimezone().strftime(fmt)


def one_line(text, limit):
    text = " ".join(str(text).split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def user_text(message):
    """The text a human typed, or None for tool results / injected content."""
    content = message.get("content")
    if isinstance(content, str):
        text = content
    elif isinstance(content, list):
        if any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
            return None
        text = " ".join(b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text")
    else:
        return None
    text = text.strip()
    if not text or text.startswith(NOISE_PREFIXES):
        return None
    return text


def read_session(path, since):
    s = {"cwd": None, "title": None, "first": None, "last": None,
         "prompts": [], "files": [], "commits": [], "id": os.path.basename(path)[:8]}
    try:
        f = open(path, encoding="utf-8", errors="replace")
    except OSError:
        return None
    with f:
        for line in f:
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if not isinstance(rec, dict):
                continue
            kind = rec.get("type")
            if kind == "custom-title" and rec.get("customTitle"):
                s["title"] = rec["customTitle"]
                continue
            if kind not in ("user", "assistant") or rec.get("isSidechain") or rec.get("isMeta"):
                continue
            ts = parse_time(rec.get("timestamp"))
            if ts is None or ts < since:
                continue
            if rec.get("cwd") and not s["cwd"]:
                s["cwd"] = rec["cwd"]
            s["first"] = ts if s["first"] is None else min(s["first"], ts)
            s["last"] = ts if s["last"] is None else max(s["last"], ts)
            message = rec.get("message") or {}
            if kind == "user":
                text = user_text(message)
                if text:
                    s["prompts"].append((ts, text))
            else:
                for block in message.get("content") or []:
                    if not isinstance(block, dict) or block.get("type") != "tool_use":
                        continue
                    name, inp = block.get("name"), block.get("input") or {}
                    if name in ("Edit", "Write", "NotebookEdit"):
                        fp = inp.get("file_path") or inp.get("notebook_path")
                        if fp and fp not in s["files"]:
                            s["files"].append(fp)
                    elif name == "Bash" and "git commit" in str(inp.get("command", "")):
                        m = re.search(r"-m\s+(?:\"\$\(cat <<'?EOF'?\s*)?[\"']?([^\n\"']{3,120})", str(inp["command"]))
                        s["commits"].append(one_line(m.group(1), 100) if m else "(commit)")
    return s if s["first"] and (s["prompts"] or s["files"] or s["commits"]) else None


def render(sessions, max_prompts, prompt_len, max_files):
    by_project = {}
    for s in sessions:
        by_project.setdefault(s["cwd"] or "(unknown project)", []).append(s)
    out = []
    for project in sorted(by_project):
        out.append(f"\n## Project: {project}")
        for s in sorted(by_project[project], key=lambda x: x["first"]):
            title = f' "{one_line(s["title"], 60)}"' if s["title"] else ""
            out.append(
                f"### Session {s['id']}{title}: {local(s['first'])} to {local(s['last'])}"
                f" | {len(s['prompts'])} user requests | {len(s['files'])} files changed"
                f" | {len(s['commits'])} commits"
            )
            prompts = s["prompts"]
            if len(prompts) > max_prompts:
                head = max_prompts - max_prompts // 3
                shown = prompts[:head] + prompts[len(prompts) - (max_prompts - head):]
                skipped = len(prompts) - len(shown)
            else:
                shown, skipped = prompts, 0
            for ts, text in shown:
                out.append(f"- [{local(ts)}] {one_line(text, prompt_len)}")
            if skipped:
                out.append(f"- ({skipped} more requests not shown)")
            if s["files"]:
                names = [os.path.basename(p) for p in s["files"][:max_files]]
                more = f" (+{len(s['files']) - max_files} more)" if len(s["files"]) > max_files else ""
                out.append(f"files: {', '.join(names)}{more}")
            if s["commits"]:
                out.append("commits: " + " | ".join(s["commits"][:6]))
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=7)
    args = ap.parse_args()

    root = os.environ.get("SUPERSIMPLE_TRANSCRIPTS_DIR") or os.path.expanduser("~/.claude/projects")
    own_home = os.path.realpath(os.environ.get("SUPERSIMPLE_HOME") or os.path.expanduser("~/.supersimple"))
    now = datetime.now(timezone.utc)
    since = now - timedelta(days=args.days)
    print(f"# Claude Code sessions: last {args.days} days ({local(since, '%Y-%m-%d')} to {local(now, '%Y-%m-%d')}, local time)")

    if not os.path.isdir(root):
        print(f"No transcript folder at {root}. No session data.")
        return

    sessions = []
    for path in glob.glob(os.path.join(root, "*", "*.jsonl")):
        try:
            if os.path.getmtime(path) < since.timestamp():
                continue
        except OSError:
            continue
        s = read_session(path, since)
        if s is None:
            continue
        # skip the sessions created by Super Simple's own AI runs
        if s["cwd"] and os.path.realpath(s["cwd"]).startswith(own_home):
            continue
        sessions.append(s)

    if not sessions:
        print("No sessions with activity in this period.")
        return

    # Shrink detail until the digest fits the size cap.
    body = ""
    for max_prompts, prompt_len, max_files in ((9, 220, 15), (6, 160, 10), (4, 110, 6), (2, 80, 3)):
        body = render(sessions, max_prompts, prompt_len, max_files)
        if len(body) <= MAX_TOTAL_CHARS:
            break
    truncated = len(body) > MAX_TOTAL_CHARS
    if truncated:
        body = body[:MAX_TOTAL_CHARS].rsplit("\n", 1)[0]
    print(body)
    projects = len({s["cwd"] for s in sessions})
    print(f"\n## Totals\n{len(sessions)} sessions in {projects} projects.")
    if truncated:
        print("NOTE: the digest was cut at the size limit; later projects above are incomplete or missing.")


if __name__ == "__main__":
    main()
