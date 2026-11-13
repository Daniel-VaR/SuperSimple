//! The editor: an outline you type into, plus the weekly retrospective screen.

use crate::ai::{self, Outcome};
use crate::model::{Data, Kind, Removed, Task};
use crate::{retro, store};
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Local, Utc};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use std::io::{self, stdout, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

type Term = Terminal<CrosstermBackend<Stdout>>;

const RETRO_DAYS: u32 = 7;
const ACCENT: Color = Color::Cyan;

fn enter() -> Result<Term> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout()))?)
}

fn leave() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen, DisableBracketedPaste);
}

pub fn run() -> Result<()> {
    let mut app = App::new()?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        hook(info);
    }));
    let mut term = enter()?;
    let result = app.main_loop(&mut term);
    app.shutdown();
    leave();
    result
}

#[derive(Clone, Copy)]
struct Row {
    id: u64,
}

struct Picker {
    /// true: search box that jumps to an item; false: "move to" chooser
    find: bool,
    /// the notes list: Enter opens the chosen note instead of only jumping to it
    open_note: bool,
    title: String,
    item: u64,
    filter: String,
    idx: usize,
    options: Vec<(u64, String)>,
}

impl Picker {
    fn visible(&self) -> Vec<&(u64, String)> {
        // every typed word must appear somewhere in the label
        let f = self.filter.to_lowercase();
        let words: Vec<&str> = f.split_whitespace().collect();
        self.options
            .iter()
            .filter(|(_, label)| {
                let l = label.to_lowercase();
                words.iter().all(|w| l.contains(w))
            })
            .collect()
    }
}

enum Mode {
    Edit,
    Move(Picker),
    Find(Picker),
    Note(NoteEditor),
    ConfirmDelete(u64),
    Retro,
}

enum AiEvent {
    Progress(String),
    Done(Box<Outcome>),
}

struct AiRun {
    started: Instant,
    limit_secs: u64,
    progress: String,
    rx: mpsc::Receiver<AiEvent>,
    cancel: Arc<AtomicBool>,
}

struct App {
    data: Data,
    stamp: Option<(SystemTime, u64)>,
    sel: Option<u64>,
    col: usize,
    scroll: usize,
    show_done: bool,
    mode: Mode,
    status: Option<(String, Instant)>,
    /// Things deleted in this session, newest last. Ctrl+Z brings them back.
    trash: Vec<Removed>,
    // retrospective screen
    retro_scroll: u16,
    ai_scroll: u16,
    ai_run: Option<AiRun>,
    ai_report: Option<(String, String)>,
    ai_error: Option<String>,
}

fn is_ctrl(k: &KeyEvent, c: char) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char(c)
}

/// Shown after a note's title in the list while nothing has been written in it yet.
fn note_info(t: &Task) -> String {
    if !t.note {
        return String::new();
    }
    if t.body.trim().is_empty() {
        "  (empty - Enter to write)".into()
    } else {
        String::new()
    }
}

fn stamp_text(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%b %d %H:%M").to_string()
}

impl App {
    fn new() -> Result<App> {
        let data = store::load()?;
        let sel = data.subjects.first().map(|s| s.id);
        let col = sel.and_then(|id| data.title(id)).map(|t| t.chars().count()).unwrap_or(0);
        Ok(App {
            data,
            stamp: store::stamp(),
            sel,
            col,
            scroll: 0,
            show_done: true,
            mode: Mode::Edit,
            status: None,
            trash: vec![],
            retro_scroll: 0,
            ai_scroll: 0,
            ai_run: None,
            ai_report: ai::load_last_report(),
            ai_error: None,
        })
    }

    fn flash(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    // ------------------------------------------------------------ data helpers

    fn rows(&self) -> Vec<Row> {
        let mut rows = vec![];
        let show = |t: &Task| self.show_done || t.done_at.is_none();
        for s in &self.data.subjects {
            rows.push(Row { id: s.id });
            for t in s.tasks.iter().filter(|t| show(t)) {
                rows.push(Row { id: t.id });
                for u in t.subtasks.iter().filter(|u| show(u)) {
                    rows.push(Row { id: u.id });
                }
            }
        }
        rows
    }

    fn sel_index(&self, rows: &[Row]) -> Option<usize> {
        let sel = self.sel?;
        rows.iter().position(|r| r.id == sel)
    }

    fn title_len(&self, id: u64) -> usize {
        self.data.title(id).map(|t| t.chars().count()).unwrap_or(0)
    }

    fn select(&mut self, id: u64, at_end: bool) {
        self.sel = Some(id);
        let len = self.title_len(id);
        self.col = if at_end { len } else { self.col.min(len) };
    }

    /// Keep the selection valid after the data changed; `prev` is where it was.
    fn fix_selection(&mut self, prev: Option<usize>) {
        let rows = self.rows();
        if self.sel_index(&rows).is_none() {
            self.sel = match (rows.is_empty(), prev) {
                (true, _) => None,
                (false, Some(i)) => Some(rows[i.min(rows.len() - 1)].id),
                (false, None) => Some(rows[0].id),
            };
        }
        if let Some(id) = self.sel {
            self.col = self.col.min(self.title_len(id));
        }
    }

    /// Apply a change through the shared store (lock, re-read, change, save).
    fn apply<T>(&mut self, f: impl FnOnce(&mut Data) -> Result<T>) -> Option<T> {
        let prev = self.sel_index(&self.rows());
        match store::mutate(f) {
            Ok((data, out)) => {
                self.data = data;
                self.stamp = store::stamp();
                self.fix_selection(prev);
                Some(out)
            }
            Err(e) => {
                self.flash(format!("{e:#}"));
                self.reload();
                None
            }
        }
    }

    fn reload(&mut self) {
        let prev = self.sel_index(&self.rows());
        match store::load() {
            Ok(data) => {
                self.data = data;
                self.stamp = store::stamp();
                self.fix_selection(prev);
            }
            Err(e) => {
                self.stamp = store::stamp();
                self.flash(format!("{e:#}"));
            }
        }
    }

    // ------------------------------------------------------------ main loop

    fn main_loop(&mut self, term: &mut Term) -> Result<()> {
        loop {
            self.tick();
            term.draw(|f| self.draw(f))?;
            if !event::poll(Duration::from_millis(150))? {
                continue;
            }
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if self.on_key(k, term)? {
                        return Ok(());
                    }
                }
                Event::Paste(text) => {
                    if matches!(self.mode, Mode::Edit) {
                        let clean: String =
                            text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
                        self.insert_text(clean.trim_end());
                    } else if matches!(self.mode, Mode::Note(_)) {
                        self.note_paste(&text);
                    }
                }
                _ => {}
            }
        }
    }

    fn tick(&mut self) {
        // changes made by agents or other windows
        if store::stamp() != self.stamp {
            self.reload();
            self.flash("↻ updated from outside");
            // an open note follows what is on disk (an agent may have changed or removed it)
            if let Mode::Note(ed) = &mut self.mode {
                match self.data.task(ed.id).filter(|t| t.note) {
                    Some(note) => {
                        if note.body != ed.text() {
                            ed.set_text(&note.body);
                        }
                    }
                    None => {
                        self.mode = Mode::Edit;
                        self.status = Some(("the note was removed from outside".into(), Instant::now()));
                    }
                }
            }
        }
        if self.status.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(4)) {
            self.status = None;
        }
        // AI run progress
        let mut finished = None;
        if let Some(run) = &mut self.ai_run {
            loop {
                match run.rx.try_recv() {
                    Ok(AiEvent::Progress(p)) => run.progress = p,
                    Ok(AiEvent::Done(out)) => {
                        finished = Some(*out);
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.ai_error = Some("the AI run ended unexpectedly".into());
                        self.ai_run = None;
                        return;
                    }
                }
            }
        }
        if let Some(out) = finished {
            self.ai_run = None;
            if out.ok {
                ai::save_last_report(&out);
                self.ai_report = ai::load_last_report();
                self.ai_error = None;
                self.ai_scroll = 0;
            } else {
                self.ai_error = Some(format!(
                    "{}\nlog: {}",
                    out.error.unwrap_or_else(|| "failed".into()),
                    out.log_path.display()
                ));
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(run) = self.ai_run.take() {
            run.cancel.store(true, Ordering::Relaxed);
            // wait briefly so the runner process is really stopped before we exit
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                match run.rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(AiEvent::Done(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    _ => {}
                }
            }
        }
    }

    // ------------------------------------------------------------ keys

    fn on_key(&mut self, k: KeyEvent, term: &mut Term) -> Result<bool> {
        if is_ctrl(&k, 'q') || is_ctrl(&k, 'c') {
            self.prune_if_empty(self.sel);
            return Ok(true);
        }
        let before = self.sel;
        match &self.mode {
            Mode::Edit => self.key_edit(k),
            Mode::Move(_) | Mode::Find(_) => self.key_move(k),
            Mode::ConfirmDelete(id) => {
                let id = *id;
                self.mode = Mode::Edit;
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                    if let Some(removed) = self.apply(|d| d.remove(id)) {
                        self.trash.push(removed);
                        self.flash("deleted - press Ctrl+Z to bring it back");
                    }
                }
            }
            Mode::Retro => self.key_retro(k, term)?,
            Mode::Note(_) => self.key_note(k),
        }
        // Leaving a line that was never filled in removes it, so no empty items pile up.
        if self.sel != before || matches!(self.mode, Mode::Retro) {
            self.prune_if_empty(before);
        }
        Ok(false)
    }

    /// Delete `id` if it has no text and nothing under it.
    fn prune_if_empty(&mut self, id: Option<u64>) {
        let Some(id) = id else { return };
        if self.data.is_blank(id) {
            self.apply(|d| {
                // re-check on the fresh data: someone else may have filled it in meanwhile
                if d.is_blank(id) {
                    d.delete(id)?;
                }
                Ok(())
            });
        }
    }

    fn key_edit(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Up if shift => self.reorder(-1),
            KeyCode::Down if shift => self.reorder(1),
            KeyCode::Up => self.move_sel(-1),
            KeyCode::Down => self.move_sel(1),
            KeyCode::Left => self.col = self.col.saturating_sub(1),
            KeyCode::Right => {
                if let Some(id) = self.sel {
                    self.col = (self.col + 1).min(self.title_len(id));
                }
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.sel.map(|id| self.title_len(id)).unwrap_or(0),
            KeyCode::Char('a') if ctrl => self.col = 0,
            KeyCode::Char('e') if ctrl => {
                self.col = self.sel.map(|id| self.title_len(id)).unwrap_or(0)
            }
            KeyCode::Char('n') if ctrl => self.create(|d, id| d.new_child(id)),
            KeyCode::Char('o') if ctrl => {
                if self.sel.is_some() {
                    self.create(|d, id| d.new_note(id));
                } else {
                    self.flash("create a subject first - a note goes under a subject or a task");
                }
            }
            KeyCode::Char('l') if ctrl => self.open_notes(),
            KeyCode::Enter => match self.sel {
                // Enter on a note opens it, like opening a text file
                Some(id) if self.data.is_note(id) => self.open_note(id),
                _ => self.create(|d, id| d.new_below(id)),
            },
            KeyCode::Char('s') if ctrl => {
                let after = self.sel;
                if let Some(id) = self.apply(|d| Ok(d.add_subject("", after))) {
                    self.select(id, true);
                }
            }
            KeyCode::Char('d') if ctrl => self.toggle_done(),
            KeyCode::Char('z') if ctrl => self.undo_delete(),
            KeyCode::Char('t') if ctrl => self.open_move(),
            KeyCode::Char('x') if ctrl => {
                if let Some(id) = self.sel {
                    self.mode = Mode::ConfirmDelete(id);
                }
            }
            KeyCode::Char('f') if ctrl => self.open_find(),
            KeyCode::Char('v') if ctrl => {
                let prev = self.sel_index(&self.rows());
                self.show_done = !self.show_done;
                self.fix_selection(prev);
                self.flash(if self.show_done { "showing finished items" } else { "hiding finished items" });
            }
            KeyCode::Char('r') if ctrl => {
                self.mode = Mode::Retro;
                self.retro_scroll = 0;
            }
            KeyCode::Char('w') if ctrl => self.edit_title(|chars, col| {
                let mut start = *col;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && chars[start - 1] != ' ' {
                    start -= 1;
                }
                chars.drain(start..*col);
                *col = start;
            }),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.edit_title(|chars, col| {
                if *col < chars.len() {
                    chars.remove(*col);
                }
            }),
            KeyCode::Char(c) if !ctrl && !alt => self.insert_text(&c.to_string()),
            _ => {}
        }
    }

    fn move_sel(&mut self, delta: i64) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let cur = self.sel_index(&rows).unwrap_or(0) as i64;
        let next = (cur + delta).clamp(0, rows.len() as i64 - 1) as usize;
        self.select(rows[next].id, false);
    }

    fn reorder(&mut self, delta: i32) {
        if let Some(id) = self.sel {
            self.apply(|d| d.shift(id, delta));
        }
    }

    /// Create a new item relative to the selection and put the cursor in it.
    fn create(&mut self, f: impl FnOnce(&mut Data, u64) -> Result<u64>) {
        let new_id = match self.sel {
            Some(id) => self.apply(|d| f(d, id)),
            None => self.apply(|d| Ok(d.add_subject("", None))),
        };
        if let Some(id) = new_id {
            self.select(id, true);
        }
    }

    /// Bring back the most recently deleted item (and everything that was under it).
    fn undo_delete(&mut self) {
        let Some(removed) = self.trash.pop() else {
            self.flash("nothing to undo - Ctrl+Z brings back items deleted in this session");
            return;
        };
        let title = removed.title().to_string();
        if let Some(id) = self.apply(|d| d.restore(removed)) {
            // a finished item may be hidden: show finished items so it is visible again
            if !self.rows().iter().any(|r| r.id == id) {
                self.show_done = true;
            }
            self.select(id, true);
            self.flash(format!("brought back “{title}”"));
        }
    }

    fn toggle_done(&mut self) {
        let Some(id) = self.sel else { return };
        if self.data.kind(id) == Some(Kind::Subject) {
            self.flash("a subject cannot be finished - only tasks and subtasks");
            return;
        }
        if self.data.is_note(id) {
            self.flash("a note cannot be finished");
            return;
        }
        if let Some(done) = self.apply(|d| d.toggle_done(id)) {
            self.flash(if done { "✓ finished - time recorded" } else { "reopened" });
        }
    }

    fn edit_title(&mut self, f: impl FnOnce(&mut Vec<char>, &mut usize)) {
        let Some(id) = self.sel else { return };
        let Some(title) = self.data.title(id) else { return };
        let mut chars: Vec<char> = title.chars().collect();
        let mut col = self.col.min(chars.len());
        f(&mut chars, &mut col);
        let new: String = chars.into_iter().collect();
        if self.apply(|d| d.set_title(id, new)).is_some() {
            self.col = col;
        }
    }

    fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.sel.is_none() {
            // empty list: typing starts the first subject
            match self.apply(|d| Ok(d.add_subject("", None))) {
                Some(id) => self.select(id, true),
                None => return,
            }
        }
        self.edit_title(|chars, col| {
            for (i, c) in text.chars().enumerate() {
                chars.insert(*col + i, c);
            }
            *col += text.chars().count();
        });
    }

    fn backspace(&mut self) {
        let Some(id) = self.sel else { return };
        if self.col > 0 {
            self.edit_title(|chars, col| {
                chars.remove(*col - 1);
                *col -= 1;
            });
        } else if self.title_len(id) == 0 && self.data.is_blank(id) {
            // like deleting an empty line in a text editor: go to the end of the line above
            let rows = self.rows();
            let above = self.sel_index(&rows).and_then(|i| i.checked_sub(1)).map(|i| rows[i].id);
            if self.apply(|d| d.delete(id)).is_some() {
                if let Some(a) = above {
                    self.select(a, true);
                }
            }
        }
    }

    fn open_move(&mut self) {
        let Some(id) = self.sel else { return };
        let mut options = vec![];
        match self.data.find(id) {
            Some(loc) if self.data.is_note(id) => {
                // a note can go under any other subject or task
                let (si, parent_task) = match loc {
                    crate::model::Loc::Task(si, _) => (si, None),
                    crate::model::Loc::Subtask(si, ti, _) => (si, Some(self.data.subjects[si].tasks[ti].id)),
                    crate::model::Loc::Subject(si) => (si, None),
                };
                for (i, s) in self.data.subjects.iter().enumerate() {
                    if i != si || parent_task.is_some() {
                        options.push((s.id, s.title.clone()));
                    }
                    for t in s.tasks.iter().filter(|t| !t.note && Some(t.id) != parent_task) {
                        options.push((t.id, format!("{} › {}", s.title, t.title)));
                    }
                }
            }
            Some(crate::model::Loc::Task(si, _)) => {
                for (i, s) in self.data.subjects.iter().enumerate() {
                    if i != si {
                        options.push((s.id, s.title.clone()));
                    }
                }
            }
            Some(crate::model::Loc::Subtask(si, ti, _)) => {
                let parent = self.data.subjects[si].tasks[ti].id;
                for s in &self.data.subjects {
                    for t in s.tasks.iter().filter(|t| t.id != parent && !t.note) {
                        options.push((t.id, format!("{} › {}", s.title, t.title)));
                    }
                }
            }
            _ => {
                self.flash("only tasks and subtasks can be moved (⇧↑/⇧↓ reorders a subject)");
                return;
            }
        }
        if options.is_empty() {
            self.flash("nowhere to move it yet - create another subject or task first");
            return;
        }
        let title = format!(" Move “{}” to… ", self.data.title(id).unwrap_or(""));
        self.mode =
            Mode::Move(Picker { find: false, open_note: false, title, item: id, filter: String::new(), idx: 0, options });
    }

    /// Search box: every subject, task and subtask, filtered as you type.
    fn open_find(&mut self) {
        let mut options = vec![];
        let label = |path: String, t: &Task| match t.done_at {
            _ if t.note => format!("{path}  ✎"),
            Some(_) => format!("{path}  ✓"),
            None => path,
        };
        for s in &self.data.subjects {
            options.push((s.id, s.title.clone()));
            for t in &s.tasks {
                options.push((t.id, label(format!("{} › {}", s.title, t.title), t)));
                for u in &t.subtasks {
                    options.push((u.id, label(format!("{} › {} › {}", s.title, t.title, u.title), u)));
                }
            }
        }
        if options.is_empty() {
            self.flash("nothing to search yet");
            return;
        }
        self.mode = Mode::Find(Picker {
            find: true,
            open_note: false,
            title: " Find ".into(),
            item: 0,
            filter: String::new(),
            idx: 0,
            options,
        });
    }

    /// All notes in one list; Enter jumps to the chosen one.
    fn open_notes(&mut self) {
        let mut options = vec![];
        for s in &self.data.subjects {
            for t in &s.tasks {
                if t.note {
                    options.push((t.id, format!("{}  ·  {}", t.title, s.title)));
                }
                for u in t.subtasks.iter().filter(|u| u.note) {
                    options.push((u.id, format!("{}  ·  {} › {}", u.title, s.title, t.title)));
                }
            }
        }
        if options.is_empty() {
            self.flash("no notes yet - press Ctrl+O on a subject or task to add one");
            return;
        }
        self.mode = Mode::Find(Picker {
            find: true,
            open_note: true,
            title: format!(" Notes · {} ", options.len()),
            item: 0,
            filter: String::new(),
            idx: 0,
            options,
        });
    }

    fn key_move(&mut self, k: KeyEvent) {
        let (Mode::Move(p) | Mode::Find(p)) = &mut self.mode else { return };
        let count = p.visible().len();
        match k.code {
            KeyCode::Esc => self.mode = Mode::Edit,
            KeyCode::Up => p.idx = p.idx.saturating_sub(1),
            KeyCode::Down => p.idx = (p.idx + 1).min(count.saturating_sub(1)),
            KeyCode::Backspace => {
                p.filter.pop();
                p.idx = 0;
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                p.filter.push(c);
                p.idx = 0;
            }
            KeyCode::Enter => {
                let target = p.visible().get(p.idx).map(|(id, _)| *id);
                let item = p.item;
                let find = p.find;
                let open = p.open_note;
                self.mode = Mode::Edit;
                if find {
                    if let Some(id) = target {
                        // a finished item may be hidden: show finished items so we can land on it
                        if !self.rows().iter().any(|r| r.id == id) {
                            self.show_done = true;
                        }
                        self.select(id, true);
                        if open {
                            self.open_note(id);
                        }
                    }
                } else if let Some(to) = target {
                    if self.apply(|d| d.move_item(item, to)).is_some() {
                        self.sel = Some(item);
                        let prev = self.sel_index(&self.rows());
                        self.fix_selection(prev);
                        self.flash(format!("moved to {}", self.data.path(to)));
                    }
                }
            }
            _ => {}
        }
    }

    fn key_retro(&mut self, k: KeyEvent, term: &mut Term) -> Result<()> {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Edit,
            KeyCode::Up | KeyCode::Char('k') => self.retro_scroll = self.retro_scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.retro_scroll = self.retro_scroll.saturating_add(1),
            KeyCode::PageUp => self.ai_scroll = self.ai_scroll.saturating_sub(5),
            KeyCode::PageDown => self.ai_scroll = self.ai_scroll.saturating_add(5),
            KeyCode::Char('a') => self.start_ai(),
            KeyCode::Char('c') => {
                if let Some(run) = &self.ai_run {
                    run.cancel.store(true, Ordering::Relaxed);
                    self.flash("cancelling…");
                }
            }
            KeyCode::Char('s') if self.ai_run.is_none() => {
                // the wizard is a plain question-and-answer flow: step out of the full-screen UI
                leave();
                println!();
                if let Err(e) = ai::setup_wizard() {
                    println!("setup failed: {e:#}");
                }
                println!("\nPress Enter to go back to Super Simple…");
                let mut line = String::new();
                let _ = io::stdin().read_line(&mut line);
                *term = enter()?;
                term.clear()?;
                self.ai_error = None;
            }
            _ => {}
        }
        Ok(())
    }

    fn start_ai(&mut self) {
        if self.ai_run.is_some() {
            return;
        }
        let cfg = ai::load_config();
        if cfg.tested_ok_at.is_none() {
            self.ai_error = Some("AI analytics is not set up yet. Press s to run the setup.".into());
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel2 = cancel.clone();
        let limit_secs = cfg.timeout_secs.max(20);
        std::thread::spawn(move || {
            let tx2 = tx.clone();
            let out = ai::run(ai::Mode::Retro { days: RETRO_DAYS }, &cfg, &cancel2, move |p| {
                let _ = tx2.send(AiEvent::Progress(p));
            });
            let _ = tx.send(AiEvent::Done(Box::new(out)));
        });
        self.ai_error = None;
        self.ai_run = Some(AiRun {
            started: Instant::now(),
            limit_secs,
            progress: "starting…".into(),
            rx,
            cancel,
        });
    }

    // ------------------------------------------------------------ drawing

    fn draw(&mut self, f: &mut Frame) {
        match self.mode {
            Mode::Retro => self.draw_retro(f),
            Mode::Note(_) => self.draw_note(f),
            _ => self.draw_main(f),
        }
    }

    fn header(&self, title: &str, right: String) -> Line<'static> {
        let mut spans = vec![
            Span::styled(" Super Simple ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw(format!(" {title}")),
        ];
        if let Some((msg, _)) = &self.status {
            spans.push(Span::styled(format!("   {msg}"), Style::new().fg(Color::Yellow)));
        } else {
            spans.push(Span::styled(format!("   {right}"), Style::new().dim()));
        }
        Line::from(spans)
    }

    fn draw_main(&mut self, f: &mut Frame) {
        let [head, body, foot] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(2)])
                .areas(f.area());

        let (mut open, mut done) = (0, 0);
        for s in &self.data.subjects {
            for t in &s.tasks {
                for x in std::iter::once(t).chain(&t.subtasks).filter(|x| !x.note) {
                    if x.done_at.is_some() {
                        done += 1;
                    } else {
                        open += 1;
                    }
                }
            }
        }
        f.render_widget(
            Paragraph::new(self.header("", format!("{open} open · {done} finished"))),
            head,
        );

        // Build the lines; remember where the cursor goes.
        let mut lines: Vec<Line> = vec![];
        let mut cursor: Option<(usize, u16)> = None; // (line, x)
        let sel = self.sel;
        let col = self.col;
        let show = |t: &Task| self.show_done || t.done_at.is_none();
        let mut push = |lines: &mut Vec<Line>, id: u64, kind: Kind, note: bool, title: &str, done: Option<DateTime<Utc>>, extra: String| {
            let selected = sel == Some(id);
            let gutter = if selected {
                Span::styled("▌", Style::new().fg(ACCENT))
            } else {
                Span::raw(" ")
            };
            let (indent, placeholder) = match kind {
                Kind::Subject => (" ", "subject name…"),
                Kind::Task => ("   ", "task…"),
                Kind::Subtask => ("       ", "subtask…"),
            };
            let placeholder = if note { "note…" } else { placeholder };
            let mut spans = vec![gutter, Span::raw(indent)];
            let mut prefix = 1 + indent.len();
            if kind != Kind::Subject {
                let (boxed, style) = match done {
                    _ if note => (" ✎  ", Style::new().fg(Color::Yellow)),
                    Some(_) => ("[x] ", Style::new().fg(Color::Green)),
                    None => ("[ ] ", Style::new().dim()),
                };
                spans.push(Span::styled(boxed, style));
                prefix += 4;
            }
            let style = match (kind, done) {
                (Kind::Subject, _) => Style::new().fg(ACCENT).bold(),
                (_, Some(_)) => Style::new().dim().crossed_out(),
                (Kind::Task, None) => Style::new(),
                (Kind::Subtask, None) => Style::new(),
            };
            if title.is_empty() {
                spans.push(Span::styled(placeholder, Style::new().dim().italic()));
            } else {
                spans.push(Span::styled(title.to_string(), style));
            }
            if let Some(at) = done {
                spans.push(Span::styled(format!("  ✓ {}", stamp_text(at)), Style::new().fg(Color::Green).dim()));
            }
            if !extra.is_empty() {
                spans.push(Span::styled(extra, Style::new().dim()));
            }
            if selected {
                let byte = title.char_indices().nth(col).map(|(i, _)| i).unwrap_or(title.len());
                cursor = Some((lines.len(), (prefix + title[..byte].width()) as u16));
            }
            lines.push(Line::from(spans));
        };

        for (i, s) in self.data.subjects.iter().enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            let total = s.tasks.iter().filter(|t| !t.note).count();
            let finished = s.tasks.iter().filter(|t| t.done_at.is_some()).count();
            let extra = if total > 0 { format!("  {finished}/{total}") } else { String::new() };
            push(&mut lines, s.id, Kind::Subject, false, &s.title, None, extra);
            for t in s.tasks.iter().filter(|t| show(t)) {
                push(&mut lines, t.id, Kind::Task, t.note, &t.title, t.done_at, note_info(t));
                for u in t.subtasks.iter().filter(|u| show(u)) {
                    push(&mut lines, u.id, Kind::Subtask, u.note, &u.title, u.done_at, note_info(u));
                }
            }
        }
        if lines.is_empty() {
            lines.push(Line::default());
            lines.push(Line::styled(
                "  Nothing here yet. Just start typing to create your first subject,",
                Style::new().dim(),
            ));
            lines.push(Line::styled("  then press Enter to add tasks under it.", Style::new().dim()));
        }

        // keep the cursor line on screen
        let height = body.height as usize;
        if let Some((line, _)) = cursor {
            if line < self.scroll {
                self.scroll = line;
            } else if line >= self.scroll + height {
                self.scroll = line + 1 - height;
            }
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(1));
        f.render_widget(Paragraph::new(lines).scroll((self.scroll as u16, 0)), body);

        // footer
        let key = |k: &'static str, what: &'static str| {
            vec![Span::styled(k, Style::new().fg(ACCENT)), Span::styled(format!(" {what}   "), Style::new().dim())]
        };
        let footer: Vec<Line> = match &self.mode {
            Mode::ConfirmDelete(id) => {
                let what = self.data.title(*id).unwrap_or("");
                let more = if self.data.has_children(*id) { " and everything under it" } else { "" };
                vec![
                    Line::styled(format!(" Delete “{what}”{more}?"), Style::new().fg(Color::Red).bold()),
                    Line::from([key("y", "delete"), key("any other key", "keep")].concat()),
                ]
            }
            _ => vec![
                Line::from(
                    [key(" Enter", "new line / open note"), key("^N", "task / subtask"), key("^S", "subject"), key("^O", "note"), key("^D", "finish"), key("^T", "move"), key("^X", "delete"), key("^Z", "undo delete")].concat(),
                ),
                Line::from(
                    [key(" ⇧↑↓", "reorder"), key("^F", "find"), key("^L", "notes"), key("^V", "show/hide finished"), key("^R", "week retro"), key("^Q", "quit")].concat(),
                ),
            ],
        };
        f.render_widget(Paragraph::new(footer), foot);

        match &self.mode {
            Mode::Move(p) | Mode::Find(p) => self.draw_picker(f, p),
            Mode::Edit => {
                if let Some((line, x)) = cursor {
                    let y = line.saturating_sub(self.scroll) as u16;
                    if y < body.height && x < body.width {
                        f.set_cursor_position(Position::new(body.x + x, body.y + y));
                    }
                }
            }
            _ => {}
        }
    }

    fn draw_picker(&self, f: &mut Frame, p: &Picker) {
        let area = f.area();
        let visible = p.visible();
        let w = (area.width.saturating_sub(4)).min(70).max(20);
        let h = (visible.len() as u16 + 4).min(area.height.saturating_sub(2)).max(5);
        let rect = Rect::new(
            area.x + (area.width.saturating_sub(w)) / 2,
            area.y + (area.height.saturating_sub(h)) / 2,
            w.min(area.width),
            h.min(area.height),
        );
        let hint = if p.open_note {
            " type to search · ↑↓ choose · Enter open it · Esc cancel "
        } else if p.find {
            " type to search · ↑↓ choose · Enter go to it · Esc cancel "
        } else {
            " ↑↓ choose · Enter move · Esc cancel · type to filter "
        };
        let block = Block::bordered()
            .title(p.title.clone())
            .title_bottom(hint)
            .border_style(Style::new().fg(ACCENT));
        let inner = block.inner(rect);
        f.render_widget(Clear, rect);
        f.render_widget(block, rect);
        let mut lines = vec![Line::from(vec![
            Span::styled(if p.find { "search: " } else { "filter: " }, Style::new().dim()),
            Span::raw(p.filter.clone()),
        ])];
        let list_h = inner.height.saturating_sub(1) as usize;
        let first = p.idx.saturating_sub(list_h.saturating_sub(1));
        for (i, (_, label)) in visible.iter().enumerate().skip(first).take(list_h) {
            let label = if label.is_empty() { "(untitled)" } else { label.as_str() };
            if i == p.idx {
                lines.push(Line::styled(format!("▌ {label}"), Style::new().fg(ACCENT).bold()));
            } else {
                lines.push(Line::raw(format!("  {label}")));
            }
        }
        if visible.is_empty() {
            lines.push(Line::styled("  no match", Style::new().dim()));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_retro(&mut self, f: &mut Frame) {
        let [head, body, foot] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
                .areas(f.area());
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);

        let since = Utc::now() - ChronoDuration::days(RETRO_DAYS as i64);
        let items = retro::completed_since(&self.data, since);
        f.render_widget(
            Paragraph::new(self.header(
                "· weekly retrospective",
                format!(
                    "{} – {}",
                    since.with_timezone(&Local).format("%b %d"),
                    Local::now().format("%b %d")
                ),
            )),
            head,
        );

        // left: what was finished, grouped by day
        let mut lines: Vec<Line> = vec![];
        let mut day = String::new();
        for item in &items {
            let at = item.done_at.with_timezone(&Local);
            let d = at.format("%A, %b %d").to_string();
            if d != day {
                if !day.is_empty() {
                    lines.push(Line::default());
                }
                let n = items
                    .iter()
                    .filter(|i| i.done_at.with_timezone(&Local).format("%A, %b %d").to_string() == d)
                    .count();
                lines.push(Line::from(vec![
                    Span::styled(d.clone(), Style::new().fg(ACCENT).bold()),
                    Span::styled(format!("  {n}"), Style::new().dim()),
                ]));
                day = d;
            }
            lines.push(Line::from(vec![
                Span::styled(format!(" {}  ", at.format("%H:%M")), Style::new().dim()),
                Span::raw(item.label()),
            ]));
        }
        if items.is_empty() {
            lines.push(Line::styled(
                "Nothing finished in the last 7 days. Finish a task with Ctrl+D and it will show up here.",
                Style::new().dim(),
            ));
        }
        let max_scroll = (lines.len() as u16).saturating_sub(1);
        self.retro_scroll = self.retro_scroll.min(max_scroll);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.retro_scroll, 0))
                .block(Block::bordered().title(format!(" Finished · last {RETRO_DAYS} days · {} ", items.len()))),
            left,
        );

        // right: AI analytics
        let mut ai: Vec<Line> = vec![];
        if let Some(run) = &self.ai_run {
            let secs = run.started.elapsed().as_secs();
            let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
                [(run.started.elapsed().as_millis() / 120) as usize % 10];
            ai.push(Line::styled(
                format!("{spinner} Analysing… {secs}s of max {}s", run.limit_secs),
                Style::new().fg(Color::Yellow).bold(),
            ));
            ai.push(Line::styled(format!("  {}", run.progress), Style::new().dim()));
            ai.push(Line::default());
        }
        if let Some(err) = &self.ai_error {
            for l in err.lines() {
                ai.push(Line::styled(l.to_string(), Style::new().fg(Color::Red)));
            }
            ai.push(Line::default());
        }
        match &self.ai_report {
            Some((meta, text)) => {
                ai.push(Line::styled(format!("Last analysis: {meta}"), Style::new().dim()));
                ai.push(Line::default());
                for l in text.lines() {
                    if let Some(h) = l.strip_prefix('#') {
                        ai.push(Line::styled(
                            h.trim_start_matches('#').trim().to_string(),
                            Style::new().fg(ACCENT).bold(),
                        ));
                    } else {
                        ai.push(Line::raw(l.replace("**", "")));
                    }
                }
            }
            None if self.ai_run.is_none() && self.ai_error.is_none() => {
                ai.push(Line::raw("Press a to run AI analytics."));
                ai.push(Line::default());
                ai.push(Line::styled(
                    "It matches what you finished here against your Claude Code sessions of the same week and writes a short factual summary. First time: press s to set it up.",
                    Style::new().dim(),
                ));
            }
            None => {}
        }
        f.render_widget(
            Paragraph::new(ai)
                .wrap(Wrap { trim: false })
                .scroll((self.ai_scroll, 0))
                .block(Block::bordered().title(" AI analytics ")),
            right,
        );

        let key = |k: &'static str, what: &'static str| {
            vec![Span::styled(k, Style::new().fg(ACCENT)), Span::styled(format!(" {what}   "), Style::new().dim())]
        };
        let keys = if self.ai_run.is_some() {
            [key(" c", "cancel run"), key("↑↓", "scroll list"), key("PgUp/PgDn", "scroll analysis"), key("Esc", "back")].concat()
        } else {
            [key(" a", "run AI analytics"), key("s", "AI setup"), key("↑↓", "scroll list"), key("PgUp/PgDn", "scroll analysis"), key("Esc", "back")].concat()
        };
        f.render_widget(Paragraph::new(Line::from(keys)), foot);
    }
}

// ---------------------------------------------------------------- note editor

fn cwidth(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Split one line into screen rows of at most `width` columns, breaking at
/// spaces where possible. Returns (start, end) character ranges.
fn wrap(chars: &[char], width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut out = vec![];
    let mut start = 0;
    loop {
        let (mut w, mut end, mut last_space) = (0, start, None);
        while end < chars.len() {
            let cw = cwidth(chars[end]);
            if w + cw > width {
                break;
            }
            w += cw;
            if chars[end] == ' ' {
                last_space = Some(end);
            }
            end += 1;
        }
        if end >= chars.len() {
            out.push((start, chars.len()));
            return out;
        }
        let brk = match last_space {
            Some(sp) => sp + 1,
            None => end.max(start + 1),
        };
        out.push((start, brk));
        start = brk;
    }
}

/// The text inside one note, edited like a small text file.
struct NoteEditor {
    id: u64,
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    scroll: usize,
    /// column the cursor tries to keep while moving up and down
    want_x: Option<usize>,
    width: usize,
    height: usize,
}

impl NoteEditor {
    fn new(id: u64, body: &str) -> NoteEditor {
        let mut ed = NoteEditor {
            id,
            lines: vec![],
            row: 0,
            col: 0,
            scroll: 0,
            want_x: None,
            width: 80,
            height: 20,
        };
        ed.set_text(body);
        ed.row = ed.lines.len() - 1;
        ed.col = ed.lines[ed.row].len();
        ed
    }

    fn set_text(&mut self, body: &str) {
        self.lines = body.split('\n').map(|l| l.chars().collect()).collect();
        if self.lines.is_empty() {
            self.lines.push(vec![]);
        }
        self.row = self.row.min(self.lines.len() - 1);
        self.col = self.col.min(self.lines[self.row].len());
    }

    fn text(&self) -> String {
        let lines: Vec<String> = self.lines.iter().map(|l| l.iter().collect()).collect();
        lines.join("\n")
    }

    /// Every screen row as (line, start, end).
    fn vrows(&self) -> Vec<(usize, usize, usize)> {
        let mut rows = vec![];
        for (i, line) in self.lines.iter().enumerate() {
            for (s, e) in wrap(line, self.width) {
                rows.push((i, s, e));
            }
        }
        rows
    }

    fn cursor_vrow(&self, rows: &[(usize, usize, usize)]) -> usize {
        let mut last = 0;
        for (i, (line, _, end)) in rows.iter().enumerate() {
            if *line == self.row {
                last = i;
                if self.col < *end {
                    return i;
                }
            }
        }
        last
    }

    fn vertical(&mut self, delta: i64, keep_x: Option<usize>) {
        let rows = self.vrows();
        let cur = self.cursor_vrow(&rows);
        let (line, start, _) = rows[cur];
        let x = keep_x
            .unwrap_or_else(|| self.lines[line][start..self.col].iter().map(|c| cwidth(*c)).sum());
        let target = (cur as i64 + delta).clamp(0, rows.len() as i64 - 1) as usize;
        let (tl, ts, te) = rows[target];
        let is_last = rows.get(target + 1).is_none_or(|r| r.0 != tl);
        let chars = &self.lines[tl];
        let (mut c, mut w) = (ts, 0);
        while c < te && w + cwidth(chars[c]) <= x {
            w += cwidth(chars[c]);
            c += 1;
        }
        if !is_last && c >= te && te > ts {
            c = te - 1; // stay on this screen row
        }
        self.row = tl;
        self.col = c;
        self.want_x = Some(x);
    }

    fn insert_str(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '\r' => {}
                '\n' => {
                    let rest = self.lines[self.row].split_off(self.col);
                    self.lines.insert(self.row + 1, rest);
                    self.row += 1;
                    self.col = 0;
                }
                '\t' => {
                    for _ in 0..4 {
                        self.lines[self.row].insert(self.col, ' ');
                        self.col += 1;
                    }
                }
                c if c.is_control() => {}
                c => {
                    self.lines[self.row].insert(self.col, c);
                    self.col += 1;
                }
            }
        }
    }

    /// Returns whether the text changed.
    fn handle(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let keep_x = self.want_x.take();
        let len = self.lines[self.row].len();
        match k.code {
            KeyCode::Up => self.vertical(-1, keep_x),
            KeyCode::Down => self.vertical(1, keep_x),
            KeyCode::PageUp => self.vertical(-(self.height.max(2) as i64 - 1), keep_x),
            KeyCode::PageDown => self.vertical(self.height.max(2) as i64 - 1, keep_x),
            KeyCode::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                }
            }
            KeyCode::Right => {
                if self.col < len {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = len,
            KeyCode::Char('a') if ctrl => self.col = 0,
            KeyCode::Char('e') if ctrl => self.col = len,
            KeyCode::Enter => {
                self.insert_str("\n");
                return true;
            }
            KeyCode::Tab => {
                self.insert_str("\t");
                return true;
            }
            KeyCode::Backspace => {
                if self.col > 0 {
                    self.col -= 1;
                    self.lines[self.row].remove(self.col);
                } else if self.row > 0 {
                    let cur = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                    self.lines[self.row].extend(cur);
                } else {
                    return false;
                }
                return true;
            }
            KeyCode::Delete => {
                if self.col < len {
                    self.lines[self.row].remove(self.col);
                } else if self.row + 1 < self.lines.len() {
                    let next = self.lines.remove(self.row + 1);
                    self.lines[self.row].extend(next);
                } else {
                    return false;
                }
                return true;
            }
            KeyCode::Char('w') if ctrl => {
                let line = &mut self.lines[self.row];
                let mut start = self.col;
                while start > 0 && line[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && line[start - 1] != ' ' {
                    start -= 1;
                }
                if start == self.col {
                    return false;
                }
                line.drain(start..self.col);
                self.col = start;
                return true;
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_str(&c.to_string());
                return true;
            }
            _ => {}
        }
        false
    }
}

impl App {
    fn open_note(&mut self, id: u64) {
        if let Some(note) = self.data.task(id).filter(|t| t.note) {
            self.sel = Some(id);
            self.mode = Mode::Note(NoteEditor::new(id, &note.body));
        }
    }

    /// Save the open note's text (called after every change, so nothing is ever lost).
    fn save_note(&mut self, ed: NoteEditor) {
        let (id, text) = (ed.id, ed.text());
        if self.apply(|d| d.set_body(id, text)).is_some() {
            self.mode = Mode::Note(ed);
        }
    }

    fn key_note(&mut self, k: KeyEvent) {
        let Mode::Note(mut ed) = std::mem::replace(&mut self.mode, Mode::Edit) else { return };
        if k.code == KeyCode::Esc {
            return; // back to the list; the text is already saved
        }
        if ed.handle(k) {
            self.save_note(ed);
        } else {
            self.mode = Mode::Note(ed);
        }
    }

    fn note_paste(&mut self, text: &str) {
        let Mode::Note(mut ed) = std::mem::replace(&mut self.mode, Mode::Edit) else { return };
        ed.insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        self.save_note(ed);
    }

    fn draw_note(&mut self, f: &mut Frame) {
        let Mode::Note(mut ed) = std::mem::replace(&mut self.mode, Mode::Edit) else { return };
        let [head, body, foot] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
                .areas(f.area());

        let title = match self.data.title(ed.id) {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => "(untitled note)".to_string(),
        };
        let place = self.data.path(ed.id);
        let place = place.rsplit_once(" › ").map(|(p, _)| p.to_string()).unwrap_or_default();
        f.render_widget(Paragraph::new(self.header(&format!("✎ {title}"), format!("in {place}"))), head);

        let area = Rect {
            x: body.x + 2,
            width: body.width.saturating_sub(4).max(1),
            ..body
        };
        ed.width = area.width as usize;
        ed.height = area.height as usize;
        let rows = ed.vrows();
        let cur = ed.cursor_vrow(&rows);
        if cur < ed.scroll {
            ed.scroll = cur;
        } else if ed.height > 0 && cur >= ed.scroll + ed.height {
            ed.scroll = cur + 1 - ed.height;
        }
        let mut lines: Vec<Line> = rows
            .iter()
            .skip(ed.scroll)
            .take(ed.height)
            .map(|(l, s, e)| Line::raw(ed.lines[*l][*s..*e].iter().collect::<String>()))
            .collect();
        if ed.lines.len() == 1 && ed.lines[0].is_empty() {
            lines = vec![Line::styled("Start writing…", Style::new().dim().italic())];
        }
        f.render_widget(Paragraph::new(lines), area);

        let (line, start, _) = rows[cur];
        let x: usize = ed.lines[line][start..ed.col].iter().map(|c| cwidth(*c)).sum();
        let x = (x as u16).min(area.width.saturating_sub(1));
        let y = (cur - ed.scroll) as u16;
        if y < area.height {
            f.set_cursor_position(Position::new(area.x + x, area.y + y));
        }

        let footer = Line::from(vec![
            Span::styled(" Esc", Style::new().fg(ACCENT)),
            Span::styled(" back to the list   ", Style::new().dim()),
            Span::styled("saved as you type", Style::new().dim()),
            Span::styled(
                format!("   line {} of {}", ed.row + 1, ed.lines.len()),
                Style::new().dim(),
            ),
        ]);
        f.render_widget(Paragraph::new(footer), foot);
        self.mode = Mode::Note(ed);
    }
}
