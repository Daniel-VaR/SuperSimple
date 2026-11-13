//! The data: subjects -> tasks -> subtasks, and every operation on them.
//! Both the TUI and the agent CLI go through these functions.

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Data {
    #[serde(default = "one")]
    pub next_id: u64,
    #[serde(default)]
    pub subjects: Vec<Subject>,
}

fn one() -> u64 {
    1
}

impl Default for Data {
    fn default() -> Self {
        Data { next_id: 1, subjects: vec![] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Subject {
    pub id: u64,
    pub title: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

/// A task or a subtask (a subtask is a `Task` inside `Task::subtasks`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: u64,
    pub title: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtasks: Vec<Task>,
    /// A note: free text that sits among the tasks, cannot be finished and has nothing under it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub note: bool,
    /// The text inside a note (many lines). Empty for tasks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Subject,
    Task,
    Subtask,
}

/// A deleted item together with where it was, so it can be restored.
#[derive(Debug, Clone)]
pub enum Removed {
    Subject { index: usize, subject: Subject },
    Task { subject_id: u64, index: usize, task: Task },
    Subtask { task_id: u64, index: usize, task: Task },
}

impl Removed {
    pub fn title(&self) -> &str {
        match self {
            Removed::Subject { subject, .. } => &subject.title,
            Removed::Task { task, .. } | Removed::Subtask { task, .. } => &task.title,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loc {
    Subject(usize),
    Task(usize, usize),
    Subtask(usize, usize, usize),
}

impl Data {
    fn alloc(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    /// Make sure `next_id` is above every id in use (the file may have been hand-edited).
    pub fn normalize(&mut self) {
        let mut max = 0;
        for s in &self.subjects {
            max = max.max(s.id);
            for t in &s.tasks {
                max = max.max(t.id);
                for u in &t.subtasks {
                    max = max.max(u.id);
                }
            }
        }
        self.next_id = self.next_id.max(max + 1);
    }

    fn new_task(&mut self, title: &str) -> Task {
        Task {
            id: self.alloc(),
            title: title.to_string(),
            created_at: Utc::now(),
            done_at: None,
            subtasks: vec![],
            note: false,
            body: String::new(),
        }
    }

    /// Replace the text inside a note.
    pub fn set_body(&mut self, id: u64, body: String) -> Result<()> {
        let note = self
            .task_mut(id)
            .filter(|t| t.note)
            .ok_or_else(|| anyhow!("{id} is not a note"))?;
        note.body = body;
        Ok(())
    }

    /// No title, nothing under it and (for a note) no text inside.
    pub fn is_blank(&self, id: u64) -> bool {
        let Some(title) = self.title(id) else { return false };
        title.trim().is_empty()
            && !self.has_children(id)
            && self.task(id).is_none_or(|t| t.body.trim().is_empty())
    }

    pub fn is_note(&self, id: u64) -> bool {
        self.task(id).is_some_and(|t| t.note)
    }

    pub fn find(&self, id: u64) -> Option<Loc> {
        for (si, s) in self.subjects.iter().enumerate() {
            if s.id == id {
                return Some(Loc::Subject(si));
            }
            for (ti, t) in s.tasks.iter().enumerate() {
                if t.id == id {
                    return Some(Loc::Task(si, ti));
                }
                for (ui, u) in t.subtasks.iter().enumerate() {
                    if u.id == id {
                        return Some(Loc::Subtask(si, ti, ui));
                    }
                }
            }
        }
        None
    }

    fn loc(&self, id: u64) -> Result<Loc> {
        self.find(id).ok_or_else(|| anyhow!("no item with id {id}"))
    }

    pub fn kind(&self, id: u64) -> Option<Kind> {
        self.find(id).map(|l| match l {
            Loc::Subject(_) => Kind::Subject,
            Loc::Task(..) => Kind::Task,
            Loc::Subtask(..) => Kind::Subtask,
        })
    }

    pub fn title(&self, id: u64) -> Option<&str> {
        Some(match self.find(id)? {
            Loc::Subject(s) => &self.subjects[s].title,
            Loc::Task(s, t) => &self.subjects[s].tasks[t].title,
            Loc::Subtask(s, t, u) => &self.subjects[s].tasks[t].subtasks[u].title,
        })
    }

    pub fn task(&self, id: u64) -> Option<&Task> {
        match self.find(id)? {
            Loc::Subject(_) => None,
            Loc::Task(s, t) => Some(&self.subjects[s].tasks[t]),
            Loc::Subtask(s, t, u) => Some(&self.subjects[s].tasks[t].subtasks[u]),
        }
    }

    fn task_mut(&mut self, id: u64) -> Option<&mut Task> {
        match self.find(id)? {
            Loc::Subject(_) => None,
            Loc::Task(s, t) => Some(&mut self.subjects[s].tasks[t]),
            Loc::Subtask(s, t, u) => Some(&mut self.subjects[s].tasks[t].subtasks[u]),
        }
    }

    pub fn has_children(&self, id: u64) -> bool {
        match self.find(id) {
            Some(Loc::Subject(s)) => !self.subjects[s].tasks.is_empty(),
            Some(Loc::Task(s, t)) => !self.subjects[s].tasks[t].subtasks.is_empty(),
            _ => false,
        }
    }

    /// "Subject › Task › Subtask" for an item.
    pub fn path(&self, id: u64) -> String {
        match self.find(id) {
            Some(Loc::Subject(s)) => self.subjects[s].title.clone(),
            Some(Loc::Task(s, t)) => {
                format!("{} › {}", self.subjects[s].title, self.subjects[s].tasks[t].title)
            }
            Some(Loc::Subtask(s, t, u)) => format!(
                "{} › {} › {}",
                self.subjects[s].title,
                self.subjects[s].tasks[t].title,
                self.subjects[s].tasks[t].subtasks[u].title
            ),
            None => String::new(),
        }
    }

    /// New subject, placed after the subject that contains `after` (or at the end).
    pub fn add_subject(&mut self, title: &str, after: Option<u64>) -> u64 {
        let id = self.alloc();
        let subject = Subject { id, title: title.to_string(), created_at: Utc::now(), tasks: vec![] };
        let at = match after.and_then(|a| self.find(a)) {
            Some(Loc::Subject(s)) | Some(Loc::Task(s, _)) | Some(Loc::Subtask(s, _, _)) => s + 1,
            None => self.subjects.len(),
        };
        self.subjects.insert(at, subject);
        id
    }

    /// New task at the end of a subject.
    pub fn add_task(&mut self, subject_id: u64, title: &str) -> Result<u64> {
        let Loc::Subject(s) = self.loc(subject_id)? else {
            bail!("{subject_id} is not a subject");
        };
        let task = self.new_task(title);
        let id = task.id;
        self.subjects[s].tasks.push(task);
        Ok(id)
    }

    /// New subtask at the end of a task.
    pub fn add_subtask(&mut self, task_id: u64, title: &str) -> Result<u64> {
        let Loc::Task(s, t) = self.loc(task_id)? else {
            bail!("{task_id} is not a task (subtasks cannot have subtasks)");
        };
        if self.subjects[s].tasks[t].note {
            bail!("a note cannot have subtasks");
        }
        let task = self.new_task(title);
        let id = task.id;
        self.subjects[s].tasks[t].subtasks.push(task);
        Ok(id)
    }

    /// Enter key: a new line right below. On a subject -> first task of it;
    /// on a task -> next task; on a subtask -> next subtask.
    pub fn new_below(&mut self, id: u64) -> Result<u64> {
        let loc = self.loc(id)?;
        let on_note = self.is_note(id);
        let mut task = self.new_task("");
        task.note = on_note; // Enter on a note continues with another note line
        let new_id = task.id;
        match loc {
            Loc::Subject(s) => self.subjects[s].tasks.insert(0, task),
            Loc::Task(s, t) => self.subjects[s].tasks.insert(t + 1, task),
            Loc::Subtask(s, t, u) => self.subjects[s].tasks[t].subtasks.insert(u + 1, task),
        }
        Ok(new_id)
    }

    /// "Create task" shortcut: on a subject -> new task in it; on a task -> new
    /// subtask of it; on a subtask -> another subtask next to it.
    pub fn new_child(&mut self, id: u64) -> Result<u64> {
        match self.loc(id)? {
            Loc::Subject(_) => self.add_task(id, ""),
            Loc::Task(s, t) if self.subjects[s].tasks[t].note => {
                let task = self.new_task("");
                let new_id = task.id;
                self.subjects[s].tasks.insert(t + 1, task);
                Ok(new_id)
            }
            Loc::Task(..) => self.add_subtask(id, ""),
            Loc::Subtask(s, t, u) => {
                let task = self.new_task("");
                let new_id = task.id;
                self.subjects[s].tasks[t].subtasks.insert(u + 1, task);
                Ok(new_id)
            }
        }
    }

    /// "New note" shortcut: on a subject or task -> a note right under it;
    /// on a subtask or a note -> a note right after it.
    pub fn new_note(&mut self, id: u64) -> Result<u64> {
        let loc = self.loc(id)?;
        let on_note = self.is_note(id);
        let mut note = self.new_task("");
        note.note = true;
        let new_id = note.id;
        match loc {
            Loc::Subject(s) => self.subjects[s].tasks.insert(0, note),
            Loc::Task(s, t) if on_note => self.subjects[s].tasks.insert(t + 1, note),
            Loc::Task(s, t) => self.subjects[s].tasks[t].subtasks.insert(0, note),
            Loc::Subtask(s, t, u) => self.subjects[s].tasks[t].subtasks.insert(u + 1, note),
        }
        Ok(new_id)
    }

    /// New note at the end of a subject or a task (used by the command line).
    pub fn add_note(&mut self, parent_id: u64, text: &str) -> Result<u64> {
        let loc = self.loc(parent_id)?;
        if self.is_note(parent_id) || matches!(loc, Loc::Subtask(..)) {
            bail!("a note goes under a subject or a task");
        }
        let mut note = self.new_task(text);
        note.note = true;
        let new_id = note.id;
        match loc {
            Loc::Subject(s) => self.subjects[s].tasks.push(note),
            Loc::Task(s, t) => self.subjects[s].tasks[t].subtasks.push(note),
            Loc::Subtask(..) => unreachable!(),
        }
        Ok(new_id)
    }

    pub fn set_title(&mut self, id: u64, title: String) -> Result<()> {
        match self.loc(id)? {
            Loc::Subject(s) => self.subjects[s].title = title,
            _ => self.task_mut(id).unwrap().title = title,
        }
        Ok(())
    }

    /// Mark done (stamping the time) or not done. Returns the new state.
    pub fn set_done(&mut self, id: u64, done: bool) -> Result<bool> {
        let task = self
            .task_mut(id)
            .ok_or_else(|| anyhow!("{id} is not a task or subtask"))?;
        if task.note {
            bail!("notes cannot be finished");
        }
        if done {
            if task.done_at.is_none() {
                task.done_at = Some(Utc::now());
            }
        } else {
            task.done_at = None;
        }
        Ok(done)
    }

    pub fn toggle_done(&mut self, id: u64) -> Result<bool> {
        let done = self
            .task(id)
            .ok_or_else(|| anyhow!("subjects cannot be marked done"))?
            .done_at
            .is_some();
        self.set_done(id, !done)
    }

    pub fn delete(&mut self, id: u64) -> Result<()> {
        self.remove(id).map(|_| ())
    }

    /// Delete an item and hand back everything needed to put it back (undo).
    pub fn remove(&mut self, id: u64) -> Result<Removed> {
        Ok(match self.loc(id)? {
            Loc::Subject(s) => Removed::Subject { index: s, subject: self.subjects.remove(s) },
            Loc::Task(s, t) => Removed::Task {
                subject_id: self.subjects[s].id,
                index: t,
                task: self.subjects[s].tasks.remove(t),
            },
            Loc::Subtask(s, t, u) => Removed::Subtask {
                task_id: self.subjects[s].tasks[t].id,
                index: u,
                task: self.subjects[s].tasks[t].subtasks.remove(u),
            },
        })
    }

    /// Put a removed item back where it was, with everything that was under it.
    /// Returns its id. Fails if the subject or task it lived in is gone.
    pub fn restore(&mut self, removed: Removed) -> Result<u64> {
        match removed {
            Removed::Subject { index, subject } => {
                let id = subject.id;
                let at = index.min(self.subjects.len());
                self.subjects.insert(at, subject);
                Ok(id)
            }
            Removed::Task { subject_id, index, task } => {
                let Some(Loc::Subject(s)) = self.find(subject_id) else {
                    bail!("cannot bring “{}” back: its subject no longer exists", task.title);
                };
                let id = task.id;
                let at = index.min(self.subjects[s].tasks.len());
                self.subjects[s].tasks.insert(at, task);
                Ok(id)
            }
            Removed::Subtask { task_id, index, task } => {
                let Some(Loc::Task(s, t)) = self.find(task_id) else {
                    bail!("cannot bring “{}” back: its task no longer exists", task.title);
                };
                let id = task.id;
                let at = index.min(self.subjects[s].tasks[t].subtasks.len());
                self.subjects[s].tasks[t].subtasks.insert(at, task);
                Ok(id)
            }
        }
    }

    /// Move a task to another subject, or a subtask to another task.
    /// Also allowed: subtask -> subject (becomes a task), and a task without
    /// subtasks -> task (becomes a subtask).
    pub fn move_item(&mut self, id: u64, to: u64) -> Result<()> {
        if id == to {
            bail!("cannot move an item into itself");
        }
        let src = self.loc(id)?;
        let dst = self.loc(to)?;
        match (src, dst) {
            (Loc::Subject(_), _) => bail!("subjects cannot be moved into other items"),
            (_, Loc::Subtask(..)) => bail!("cannot move into a subtask"),
            (_, Loc::Task(s, t)) if self.subjects[s].tasks[t].note => {
                bail!("cannot move into a note")
            }
            (Loc::Task(s, t), Loc::Task(..)) if !self.subjects[s].tasks[t].subtasks.is_empty() => {
                bail!("a task that has subtasks cannot become a subtask")
            }
            _ => {}
        }
        let item = match src {
            Loc::Task(s, t) => self.subjects[s].tasks.remove(t),
            Loc::Subtask(s, t, u) => self.subjects[s].tasks[t].subtasks.remove(u),
            Loc::Subject(_) => unreachable!(),
        };
        match self.find(to) {
            Some(Loc::Subject(s)) => self.subjects[s].tasks.push(item),
            Some(Loc::Task(s, t)) => self.subjects[s].tasks[t].subtasks.push(item),
            _ => bail!("move target disappeared"),
        }
        Ok(())
    }

    /// Reorder among siblings: `delta` is -1 (up) or +1 (down). Returns whether it moved.
    pub fn shift(&mut self, id: u64, delta: i32) -> Result<bool> {
        fn swap<T>(v: &mut [T], i: usize, delta: i32) -> bool {
            let j = i as i64 + delta as i64;
            if j < 0 || j as usize >= v.len() {
                return false;
            }
            v.swap(i, j as usize);
            true
        }
        Ok(match self.loc(id)? {
            Loc::Subject(s) => swap(&mut self.subjects, s, delta),
            Loc::Task(s, t) => swap(&mut self.subjects[s].tasks, t, delta),
            Loc::Subtask(s, t, u) => swap(&mut self.subjects[s].tasks[t].subtasks, u, delta),
        })
    }

    /// Find a subject by id ("12") or by title (case-insensitive).
    pub fn subject_by_name(&self, name: &str) -> Option<u64> {
        if let Ok(id) = name.trim_start_matches('#').parse::<u64>() {
            if matches!(self.find(id), Some(Loc::Subject(_))) {
                return Some(id);
            }
        }
        let want = name.trim().to_lowercase();
        self.subjects.iter().find(|s| s.title.trim().to_lowercase() == want).map(|s| s.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Data, u64, u64, u64, u64) {
        let mut d = Data::default();
        let work = d.add_subject("Work", None);
        let home = d.add_subject("Home", None);
        let report = d.add_task(work, "Write report").unwrap();
        let outline = d.add_subtask(report, "Outline").unwrap();
        (d, work, home, report, outline)
    }

    #[test]
    fn ids_are_unique_and_findable() {
        let (d, work, home, report, outline) = sample();
        assert_eq!(d.kind(work), Some(Kind::Subject));
        assert_eq!(d.kind(home), Some(Kind::Subject));
        assert_eq!(d.kind(report), Some(Kind::Task));
        assert_eq!(d.kind(outline), Some(Kind::Subtask));
        assert_eq!(d.path(outline), "Work › Write report › Outline");
    }

    #[test]
    fn create_shortcut_makes_child_of_right_kind() {
        let (mut d, work, _, report, outline) = sample();
        let t = d.new_child(work).unwrap();
        assert_eq!(d.kind(t), Some(Kind::Task));
        let st = d.new_child(report).unwrap();
        assert_eq!(d.kind(st), Some(Kind::Subtask));
        let sib = d.new_child(outline).unwrap();
        assert_eq!(d.kind(sib), Some(Kind::Subtask));
        assert!(d.add_subtask(outline, "nope").is_err());
    }

    #[test]
    fn done_stamps_time_and_toggles_back() {
        let (mut d, work, _, report, _) = sample();
        assert!(d.toggle_done(report).unwrap());
        assert!(d.task(report).unwrap().done_at.is_some());
        assert!(!d.toggle_done(report).unwrap());
        assert!(d.task(report).unwrap().done_at.is_none());
        assert!(d.toggle_done(work).is_err());
    }

    #[test]
    fn move_task_between_subjects_and_subtask_between_tasks() {
        let (mut d, work, home, report, outline) = sample();
        let other = d.add_task(home, "Other").unwrap();
        d.move_item(outline, other).unwrap();
        assert_eq!(d.path(outline), "Home › Other › Outline");
        d.move_item(report, home).unwrap();
        assert_eq!(d.path(report), "Home › Write report");
        assert!(d.subjects.iter().find(|s| s.id == work).unwrap().tasks.is_empty());
        // a task with subtasks cannot become a subtask
        assert!(d.move_item(other, report).is_err());
        assert!(d.move_item(work, home).is_err());
    }

    #[test]
    fn shift_delete_and_normalize() {
        let (mut d, work, home, report, _) = sample();
        assert!(d.shift(home, -1).unwrap());
        assert_eq!(d.subjects[0].id, home);
        assert!(!d.shift(home, -1).unwrap());
        d.delete(report).unwrap();
        assert!(d.find(report).is_none());
        d.next_id = 1;
        d.normalize();
        assert!(d.next_id > work.max(home));
    }

    #[test]
    fn notes_sit_under_subjects_and_tasks_and_cannot_be_finished() {
        let (mut d, work, home, report, outline) = sample();
        let n1 = d.add_note(work, "remember the deadline").unwrap();
        let n2 = d.add_note(report, "use last year's template").unwrap();
        assert!(d.is_note(n1) && d.is_note(n2) && !d.is_note(report));
        assert_eq!(d.path(n2), "Work › Write report › use last year's template");
        assert!(d.set_done(n1, true).is_err());
        assert!(d.add_subtask(n1, "x").is_err());
        assert!(d.add_note(outline, "x").is_err());
        assert!(d.add_note(n1, "x").is_err());
        assert!(d.move_item(outline, n1).is_err());
        // Enter on a note continues the note; the task shortcut does not
        let next = d.new_below(n2).unwrap();
        assert!(d.is_note(next));
        let task = d.new_child(n1).unwrap();
        assert!(!d.is_note(task) && d.kind(task) == Some(Kind::Task));
        // the note shortcut puts the note directly under the item
        let top = d.new_note(home).unwrap();
        assert!(d.is_note(top) && d.kind(top) == Some(Kind::Task));
        // a note has text inside; tasks do not
        d.set_body(n2, "line one\nline two".into()).unwrap();
        assert!(d.set_body(report, "x".into()).is_err());
        assert!(!d.is_blank(n2));
        d.set_title(n2, String::new()).unwrap();
        assert!(!d.is_blank(n2)); // untitled but has text: must not be auto-removed
        // notes can be moved and survive a save/load round trip
        d.move_item(n1, home).unwrap();
        let back: Data = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert!(back.is_note(n1) && back.path(n1).starts_with("Home"));
        assert_eq!(back.task(n2).unwrap().body, "line one\nline two");
    }

    #[test]
    fn removed_items_can_be_restored_in_place() {
        let (mut d, work, _, report, outline) = sample();
        let extra = d.add_task(work, "Extra").unwrap();
        let before = d.clone();
        // a task comes back with its subtasks, in the same position
        let gone = d.remove(report).unwrap();
        assert!(d.find(report).is_none() && d.find(outline).is_none());
        assert_eq!(d.restore(gone).unwrap(), report);
        assert_eq!(d, before);
        // a whole subject comes back too
        let gone = d.remove(work).unwrap();
        d.restore(gone).unwrap();
        assert_eq!(d, before);
        // restoring into a subject that no longer exists fails cleanly
        let gone = d.remove(extra).unwrap();
        d.delete(work).unwrap();
        assert!(d.restore(gone).is_err());
    }

    #[test]
    fn subject_lookup_by_name_or_id() {
        let (d, work, _, report, _) = sample();
        assert_eq!(d.subject_by_name("work"), Some(work));
        assert_eq!(d.subject_by_name(&work.to_string()), Some(work));
        assert_eq!(d.subject_by_name(&report.to_string()), None);
    }
}
