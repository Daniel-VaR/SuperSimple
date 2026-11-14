mod ai;
mod model;
mod retro;
mod store;
mod tui;

use anyhow::{anyhow, bail, Result};
use chrono::{Duration, Local, Utc};
use clap::{Parser, Subcommand};
use model::Data;
use std::sync::atomic::AtomicBool;

#[derive(Parser)]
#[command(
    name = "supersimple",
    version,
    about = "Super Simple - a terminal task manager you type into like a text editor",
    long_about = "Run with no arguments to open the editor.\n\
                  The subcommands are for scripts and AI agents: every change they make \
                  shows up live in an open editor. Run `supersimple agent-help` for a guide."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show all subjects, tasks and subtasks with their ids
    List {
        #[arg(long)]
        json: bool,
    },
    /// Create a subject
    AddSubject {
        title: String,
        #[arg(long)]
        json: bool,
    },
    /// Create a task (--subject <id or name>) or a subtask (--parent <task id>).
    /// With --note it creates a note under that subject or task instead
    Add {
        title: String,
        /// Subject id or name; created if no subject has that name
        #[arg(long)]
        subject: Option<String>,
        /// Task id: the new item becomes a subtask of it
        #[arg(long)]
        parent: Option<u64>,
        /// Create a note (free text that cannot be finished) instead of a task
        #[arg(long)]
        note: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show all notes with their ids
    Notes {
        #[arg(long)]
        json: bool,
    },
    /// Show the text inside a note, or change it with --set / --append
    Note {
        id: u64,
        /// Replace the note's text
        #[arg(long)]
        set: Option<String>,
        /// Add a line (or several) at the end of the note's text
        #[arg(long)]
        append: Option<String>,
    },
    /// Mark a task, subtask or subject finished (records the time)
    Done {
        id: u64,
        /// On a subject: also finish every task and subtask still open in it
        #[arg(long)]
        all: bool,
    },
    /// Mark a task, subtask or subject not finished
    Undone { id: u64 },
    /// Change the text of a subject, task or subtask
    Rename { id: u64, title: String },
    /// Move a task to another subject, or a subtask to another task
    Move {
        id: u64,
        /// Id of the subject or task to move into
        #[arg(long)]
        to: u64,
    },
    /// Delete a subject, task or subtask (and everything under it)
    Delete { id: u64 },
    /// What was finished in the last days
    Retro {
        #[arg(long, default_value_t = 7)]
        days: u32,
        #[arg(long)]
        json: bool,
    },
    /// Set up AI analytics (same as `ai setup`)
    Setup,
    /// AI analytics: setup, test, run, status
    Ai {
        #[command(subcommand)]
        cmd: AiCmd,
    },
    /// Print a short guide for AI agents
    AgentHelp,
}

#[derive(Subcommand)]
enum AiCmd {
    /// Guided setup: checks what is missing, helps add it, runs a test
    Setup,
    /// Run the small connection test
    Test,
    /// Run the weekly analysis and print it
    Run {
        #[arg(long, default_value_t = 7)]
        days: u32,
    },
    /// List the saved analyses, newest first
    History,
    /// Print a saved analysis (1 = the latest, 2 = the one before, ...)
    Show {
        #[arg(default_value_t = 1)]
        number: usize,
    },
    /// Show what is configured, without changing anything
    Status,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.cmd {
        None => tui::run(),
        Some(cmd) => run_cmd(cmd),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn print_id(json: bool, id: u64, what: &str) {
    if json {
        println!("{}", serde_json::json!({ "id": id }));
    } else {
        println!("created {what} #{id}");
    }
}

fn run_cmd(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::List { json } => {
            let data = store::load()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&data)?);
            } else {
                print_tree(&data);
            }
        }
        Cmd::AddSubject { title, json } => {
            let (_, id) = store::mutate(|d| Ok(d.add_subject(&title, None)))?;
            print_id(json, id, "subject");
        }
        Cmd::Add { title, subject, parent, note, json } => {
            let (_, (id, what)) = store::mutate(|d| match (parent, &subject) {
                (Some(_), Some(_)) => bail!("use either --subject or --parent, not both"),
                (Some(p), None) if note => Ok((d.add_note(p, &title)?, "note")),
                (Some(p), None) => Ok((d.add_subtask(p, &title)?, "subtask")),
                (None, Some(name)) => {
                    let sid = match d.subject_by_name(name) {
                        Some(sid) => sid,
                        None if name.trim_start_matches('#').parse::<u64>().is_ok() => {
                            bail!("no subject with id {name}")
                        }
                        None => d.add_subject(name, None),
                    };
                    if note {
                        Ok((d.add_note(sid, &title)?, "note"))
                    } else {
                        Ok((d.add_task(sid, &title)?, "task"))
                    }
                }
                (None, None) => bail!("say where: --subject <id or name> or --parent <task id>"),
            })?;
            print_id(json, id, what);
        }
        Cmd::Notes { json } => {
            let data = store::load()?;
            let mut notes = vec![];
            for s in &data.subjects {
                for t in &s.tasks {
                    for x in std::iter::once(t).chain(&t.subtasks).filter(|x| x.note) {
                        notes.push((x.id, data.path(x.id)));
                    }
                }
            }
            if json {
                let list: Vec<_> = notes
                    .iter()
                    .map(|(id, path)| serde_json::json!({ "id": id, "path": path }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else if notes.is_empty() {
                println!("No notes.");
            } else {
                for (id, path) in notes {
                    println!("#{id}  {path}");
                }
            }
        }
        Cmd::Note { id, set, append } => {
            if set.is_some() || append.is_some() {
                let (d, _) = store::mutate(|d| {
                    let mut body = match set {
                        Some(text) => text,
                        None => d.task(id).map(|t| t.body.clone()).unwrap_or_default(),
                    };
                    if let Some(more) = append {
                        if !body.is_empty() && !body.ends_with('\n') {
                            body.push('\n');
                        }
                        body.push_str(&more);
                    }
                    d.set_body(id, body)
                })?;
                println!("updated note: {}", d.path(id));
            } else {
                let data = store::load()?;
                match data.task(id).filter(|t| t.note) {
                    Some(note) => {
                        println!("# {}", data.path(id));
                        println!("{}", note.body);
                    }
                    None => bail!("{id} is not a note"),
                }
            }
        }
        Cmd::Done { id, all } => {
            let is_subject = |d: &Data| d.kind(id) == Some(model::Kind::Subject);
            let (d, finished) = store::mutate(|d| {
                if all && !is_subject(d) {
                    bail!("--all is for a subject: it also finishes the tasks in it");
                }
                if all {
                    d.finish_all(id)
                } else {
                    d.set_done(id, true).map(|_| 0)
                }
            })?;
            println!("done: {}", d.path(id));
            if all {
                println!("also finished {finished} open tasks and subtasks in it");
            } else if is_subject(&d) && d.open_under(id) > 0 {
                println!(
                    "{} tasks and subtasks in it are still open (add --all to finish them too)",
                    d.open_under(id)
                );
            }
        }
        Cmd::Undone { id } => {
            let (d, _) = store::mutate(|d| d.set_done(id, false))?;
            println!("reopened: {}", d.path(id));
        }
        Cmd::Rename { id, title } => {
            let (d, _) = store::mutate(|d| d.set_title(id, title))?;
            println!("renamed: {}", d.path(id));
        }
        Cmd::Move { id, to } => {
            let (d, _) = store::mutate(|d| d.move_item(id, to))?;
            println!("moved: {}", d.path(id));
        }
        Cmd::Delete { id } => {
            let (_, path) = store::mutate(|d| {
                let path = d.path(id);
                d.delete(id)?;
                Ok(path)
            })?;
            println!("deleted: {path}");
        }
        Cmd::Retro { days, json } => {
            let data = store::load()?;
            let items = retro::completed_since(&data, Utc::now() - Duration::days(days as i64));
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if items.is_empty() {
                println!("Nothing finished in the last {days} days.");
            } else {
                println!("Finished in the last {days} days ({}):", items.len());
                for i in items {
                    let at = i.done_at.with_timezone(&Local).format("%a %b %d %H:%M");
                    println!("  {at}  {}", i.label());
                }
            }
        }
        Cmd::Setup | Cmd::Ai { cmd: AiCmd::Setup } => {
            if !ai::setup_wizard()? {
                std::process::exit(1);
            }
        }
        Cmd::Ai { cmd: AiCmd::Status } => ai::print_status(),
        Cmd::Ai { cmd: AiCmd::History } => {
            let reports = ai::load_reports();
            if reports.is_empty() {
                println!("No saved analyses yet. Run one with `supersimple ai run`.");
            }
            for (i, (meta, _)) in reports.iter().enumerate() {
                println!("{:>3}  {meta}", i + 1);
            }
        }
        Cmd::Ai { cmd: AiCmd::Show { number } } => {
            let reports = ai::load_reports();
            match number.checked_sub(1).and_then(|i| reports.get(i)) {
                Some((meta, text)) => println!("{meta}\n\n{text}"),
                None => bail!("there is no analysis number {number} ({} saved)", reports.len()),
            }
        }
        Cmd::Ai { cmd: AiCmd::Test } => {
            let out = ai::run_test(&AtomicBool::new(false), |p| eprintln!("  … {p}"));
            println!("{}", out.summary_line());
            println!("log: {}", out.log_path.display());
            if !out.ok {
                return Err(anyhow!(out.error.unwrap_or_else(|| "test failed".into())));
            }
        }
        Cmd::Ai { cmd: AiCmd::Run { days } } => {
            let cfg = ai::load_config();
            let out = ai::run(ai::Mode::Retro { days }, &cfg, &AtomicBool::new(false), |p| {
                eprintln!("  … {p}")
            });
            if out.ok {
                ai::save_last_report(&out);
                println!("{}", out.text);
            }
            eprintln!("{}", out.summary_line());
            eprintln!("log: {}", out.log_path.display());
            if !out.ok {
                return Err(anyhow!(out.error.unwrap_or_else(|| "run failed".into())));
            }
        }
        Cmd::AgentHelp => print!("{AGENT_HELP}"),
    }
    Ok(())
}

fn print_tree(data: &Data) {
    if data.subjects.is_empty() {
        println!("(empty)");
    }
    let mark = |t: &model::Task| match t.done_at {
        _ if t.note && !t.body.is_empty() => {
            ("note", format!("  ({} lines of text)", t.body.lines().count()))
        }
        _ if t.note => ("note", String::new()),
        Some(at) => (
            "[x]",
            format!("  (done {})", at.with_timezone(&Local).format("%Y-%m-%d %H:%M")),
        ),
        None => ("[ ]", String::new()),
    };
    for s in &data.subjects {
        let when = match s.done_at {
            Some(at) => format!("  (done {})", at.with_timezone(&Local).format("%Y-%m-%d %H:%M")),
            None => String::new(),
        };
        println!("#{} {}{when}", s.id, s.title);
        for t in &s.tasks {
            let (m, when) = mark(t);
            println!("  {m} #{} {}{when}", t.id, t.title);
            for u in &t.subtasks {
                let (m, when) = mark(u);
                println!("      {m} #{} {}{when}", u.id, u.title);
            }
        }
    }
}

const AGENT_HELP: &str = r#"Super Simple - guide for AI agents
==================================
The user's task list is managed with the `supersimple` command. Every change
you make appears live in the user's open editor. Do not edit the data file
directly; use these commands.

Structure: subjects contain tasks, tasks contain subtasks. Every item has a
numeric id, shown as #N by `supersimple list`. A note is free text under a
subject or task; it cannot be finished, but can be renamed, moved and deleted.
A note has a title (one line, shown in the list) and text inside (many lines).
A finished subject is closed: it is hidden in the editor with everything in it
until the user shows finished items. Adding a task to it reopens it.

  supersimple list [--json]                      show everything with ids
  supersimple add-subject "Title"                new subject
  supersimple add "Title" --subject <id|name>    new task (subject is created if the name is new)
  supersimple add "Title" --parent <task id>     new subtask
  supersimple add "Text" --subject <id|name> --note   new note under a subject
  supersimple add "Text" --parent <task id> --note    new note under a task
  supersimple notes [--json]                     all notes with ids
  supersimple note <id>                          print the text inside a note
  supersimple note <id> --set "text"             replace the text inside a note
  supersimple note <id> --append "text"          add text at the end of a note
  supersimple done <id> / undone <id>            finish / reopen a task, subtask or subject
  supersimple done <subject id> --all            finish a subject and every task still open in it
  supersimple rename <id> "New title"
  supersimple move <id> --to <id>                task -> subject, subtask -> task
  supersimple delete <id>                        also deletes everything under it
  supersimple retro [--days 7] [--json]          what was finished recently

Add --json to add/add-subject to get {"id": N} back.
"#;
