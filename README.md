# Super Simple

A terminal task manager for macOS that you type into like a text editor.
Subjects contain tasks, tasks contain subtasks. AI agents can change the list
from the command line and you see it update live.

## Install

```bash
./install.sh
```

Builds the program and puts the `supersimple` command on your `PATH`
(`~/.local/bin`), so you can run `supersimple` from any terminal. Run it again
after changing the code.

## The editor

Run `supersimple`. Just type: the line under the cursor is edited in place and
saved on every keystroke.

| Key | What it does |
|---|---|
| `Enter` | New line below (on a subject: its first task) |
| `Ctrl+N` | Create task. On a task it creates a subtask |
| `Ctrl+S` | New subject |
| `Ctrl+O` | New note under the subject or task (cannot be finished). Type its title, then `Enter` opens it |
| `Enter` on a note | Opens the note full-screen, like a text file: write as many lines as you want, `Esc` goes back |
| `Ctrl+L` | List all notes; type to filter, `Enter` opens one |
| `Ctrl+D` | Finish / reopen the task or subtask (records the time) |
| `Ctrl+T` | Move: a task to another subject, a subtask to another task |
| `Ctrl+X` | Delete (asks first) |
| `Ctrl+Z` | Undo delete: brings back the last deleted item with everything under it |
| `Shift+↑/↓` | Reorder |
| `Ctrl+F` | Find: type to search all items, `Enter` jumps to the chosen one |
| `Ctrl+V` | Show / hide finished items |
| `Ctrl+R` | Weekly retrospective (last 7 days) |
| `Ctrl+Q` | Quit |

Also: arrows, `Home`/`End`, `Ctrl+A`/`Ctrl+E`, `Ctrl+W` (delete word),
`Backspace` on an empty line removes it.

## Weekly retrospective and AI analytics

`Ctrl+R` shows what you finished in the last 7 days, grouped by day. In that
screen:

- `a` runs AI analytics: Claude (through the Claude Agent SDK) loads the
  `weekly-retro` skill, whose two scripts collect your finished tasks and a
  digest of the week's Claude Code session transcripts (`~/.claude/projects`),
  and writes a short factual summary that links the two.
- `s` runs the setup wizard (also `supersimple setup`). It checks Node.js,
  Python, the SDK, credentials and the transcript folder, helps add what is
  missing, and finishes with a test run.
- `c` cancels a running analysis.

Guards on every AI run (edit `~/.supersimple/config.json`):

| Setting | Default | Meaning |
|---|---|---|
| `timeout_secs` | 300 | Hard wall-clock limit; the run and everything it started is killed |
| `max_turns` | 15 | Maximum agent turns |
| `max_budget_usd` | 1.0 | Maximum estimated spend |
| `model` | `claude-opus-5-5` | Model used |
| `transcripts_dir` | `~/.claude/projects` | Where session transcripts are read from |

The agent may only run the skill's two scripts; every other shell command is
denied. Every run writes a log to `~/.supersimple/logs/` (start, limits, every
message, stderr, why it ended, cost).

```bash
supersimple ai status   # what is configured
supersimple ai test     # the small connection test
supersimple ai run      # run the analysis and print it
```

## For AI agents

```bash
supersimple agent-help
```

prints a short guide (list / add / done / rename / move / delete / retro, all
by numeric id). Changes made this way appear in an open editor within a
fraction of a second.

## Files

Everything lives in `~/.supersimple/` (`data.json`, `config.json`,
`analytics/`, `logs/`, `last-analysis.md`). Set `SUPERSIMPLE_HOME` to use
another folder.
