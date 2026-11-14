//! What was finished in a time window (the weekly retrospective).

use crate::model::Data;
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DoneItem {
    pub id: u64,
    pub done_at: DateTime<Utc>,
    pub subject: String,
    /// Empty when the line is a closed subject
    #[serde(skip_serializing_if = "String::is_empty")]
    pub task: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtask: Option<String>,
    /// The whole subject was finished (closed)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub subject_closed: bool,
}

impl DoneItem {
    pub fn label(&self) -> String {
        match &self.subtask {
            _ if self.subject_closed => format!("✓ {} (subject closed)", self.subject),
            Some(st) => format!("{} › {} › {}", self.subject, self.task, st),
            None => format!("{} › {}", self.subject, self.task),
        }
    }
}

/// Tasks, subtasks and subjects finished at or after `since`, newest first.
pub fn completed_since(data: &Data, since: DateTime<Utc>) -> Vec<DoneItem> {
    let mut out = vec![];
    for s in &data.subjects {
        if let Some(at) = s.done_at.filter(|at| *at >= since) {
            out.push(DoneItem {
                id: s.id,
                done_at: at,
                subject: s.title.clone(),
                task: String::new(),
                subtask: None,
                subject_closed: true,
            });
        }
        for t in &s.tasks {
            if let Some(at) = t.done_at.filter(|at| *at >= since) {
                out.push(DoneItem {
                    id: t.id,
                    done_at: at,
                    subject: s.title.clone(),
                    task: t.title.clone(),
                    subtask: None,
                    subject_closed: false,
                });
            }
            for u in &t.subtasks {
                if let Some(at) = u.done_at.filter(|at| *at >= since) {
                    out.push(DoneItem {
                        id: u.id,
                        done_at: at,
                        subject: s.title.clone(),
                        task: t.title.clone(),
                        subtask: Some(u.title.clone()),
                        subject_closed: false,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| b.done_at.cmp(&a.done_at));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn only_items_inside_the_window_newest_first() {
        let mut d = Data::default();
        let s = d.add_subject("Work", None);
        let old = d.add_task(s, "Old").unwrap();
        let recent = d.add_task(s, "Recent").unwrap();
        let sub = d.add_subtask(recent, "Part").unwrap();
        let _open = d.add_task(s, "Open").unwrap();
        d.set_done(old, true).unwrap();
        d.set_done(recent, true).unwrap();
        d.set_done(sub, true).unwrap();
        let now = Utc::now();
        // push "Old" 10 days back and "Part" 1 hour back
        d.subjects[0].tasks[0].done_at = Some(now - Duration::days(10));
        d.subjects[0].tasks[1].subtasks[0].done_at = Some(now - Duration::hours(1));

        let items = completed_since(&d, now - Duration::days(7));
        let ids: Vec<u64> = items.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![recent, sub]);
        assert_eq!(items[1].label(), "Work › Recent › Part");

        // a closed subject is one line of its own
        d.set_done(s, true).unwrap();
        let items = completed_since(&d, now - Duration::days(7));
        assert_eq!(items[0].id, s);
        assert_eq!(items[0].label(), "✓ Work (subject closed)");
    }
}
