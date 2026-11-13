#!/usr/bin/env python3
"""Print what was finished in Super Simple in the last N days.

Reads the Super Simple data file ($SUPERSIMPLE_HOME/data.json, default
~/.supersimple/data.json). Output is plain text for the weekly-retro skill.
"""
import argparse
import json
import os
import sys
from datetime import datetime, timedelta, timezone


def parse_time(value):
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None


def local(dt):
    return dt.astimezone().strftime("%Y-%m-%d %H:%M")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=7)
    args = ap.parse_args()

    home = os.environ.get("SUPERSIMPLE_HOME") or os.path.expanduser("~/.supersimple")
    path = os.path.join(home, "data.json")
    now = datetime.now(timezone.utc)
    since = now - timedelta(days=args.days)
    print(f"# Super Simple: last {args.days} days ({local(since)[:10]} to {local(now)[:10]}, local time)")

    try:
        with open(path, encoding="utf-8") as f:
            data = json.load(f)
    except FileNotFoundError:
        print("No Super Simple data file found. Nothing was tracked.")
        return
    except (OSError, ValueError) as e:
        print(f"Could not read {path}: {e}")
        sys.exit(1)

    done_tasks = done_subtasks = 0
    still_open = []
    print("\n## Finished")
    any_done = False
    for subject in data.get("subjects", []):
        lines = []
        for task in subject.get("tasks", []):
            if task.get("note"):
                continue  # notes are free text, not work items
            t_done = parse_time(task.get("done_at"))
            t_in = t_done is not None and t_done >= since
            sub_lines = []
            for sub in task.get("subtasks", []):
                if sub.get("note"):
                    continue
                s_done = parse_time(sub.get("done_at"))
                if s_done is not None and s_done >= since:
                    done_subtasks += 1
                    sub_lines.append(f"    - subtask done {local(s_done)}: {sub.get('title', '')}")
                elif s_done is None:
                    created = parse_time(sub.get("created_at"))
                    if created and created >= since:
                        still_open.append(f"{subject.get('title', '')} › {task.get('title', '')} › {sub.get('title', '')}")
            if t_in:
                done_tasks += 1
                lines.append(f"- task done {local(t_done)}: {task.get('title', '')}")
            elif sub_lines:
                state = "task finished earlier" if t_done else "task still open"
                lines.append(f"- ({state}) {task.get('title', '')}")
            lines.extend(sub_lines)
            if t_done is None:
                created = parse_time(task.get("created_at"))
                if created and created >= since:
                    still_open.append(f"{subject.get('title', '')} › {task.get('title', '')}")
        if lines:
            any_done = True
            print(f"\n### Subject: {subject.get('title', '')}")
            print("\n".join(lines))
    if not any_done:
        print("Nothing was marked finished in this period.")

    print("\n## Created in this period and still open")
    if still_open:
        for item in still_open:
            print(f"- {item}")
    else:
        print("None.")

    print(f"\n## Totals\n{done_tasks} tasks and {done_subtasks} subtasks finished.")


if __name__ == "__main__":
    main()
