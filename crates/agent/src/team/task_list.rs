//! Shared task list for an agents team.
//!
//! `Entity<TaskList>` + `EventEmitter<TaskListEvent>`. The leader and every
//! member operate on the same entity via the `task_*` tools; the member panel
//! subscribes to `TaskListEvent` to re-render the board. Tasks are
//! insertion-ordered (a `Vec`, not a `BTreeMap`) so the board reads in creation
//! order — this is a coordination board, not a priority queue, and ids are
//! never recycled so a stale id held in flight never aliases a recreated task.

use gpui::{App, AppContext as _, Context, Entity, EventEmitter};

/// Lifecycle of a task. Three states only: each is wired into the `task_update`
/// tool and rendered on the board; adding a state without that wiring would be
/// a half-built field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single coordination task on the shared list.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: Option<String>,
    pub status: TaskStatus,
    /// Owning member name; `None` is the unassigned pool.
    pub owner: Option<String>,
}

/// Emitted by `TaskList` mutations. The board re-reads the whole list on any
/// event — the list is tiny (a single squad's tasks) and a full re-read is
/// cheaper and simpler than per-field deltas.
#[derive(Debug, Clone)]
pub enum TaskListEvent {
    Created(String),
    Updated(String),
    Deleted(String),
}

pub struct TaskList {
    tasks: Vec<Task>,
    /// Monotonic counter backing `T1`/`T2` ids; never decremented.
    next_seq: u64,
}

impl EventEmitter<TaskListEvent> for TaskList {}

impl TaskList {
    /// Construct an empty list. Use `cx.new(|_| TaskList::new())` to wrap in an
    /// entity; the entity is what the tools and the board subscribe to.
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            next_seq: 0,
        }
    }

    /// Allocate an entity-wrapped empty list.
    pub fn new_entity(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self::new())
    }

    /// All tasks in insertion order.
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Create a task in the `Pending` state and return its id. Emits `Created`.
    pub fn create(
        &mut self,
        subject: String,
        description: Option<String>,
        cx: &mut Context<Self>,
    ) -> String {
        self.next_seq += 1;
        let id = format!("T{}", self.next_seq);
        self.tasks.push(Task {
            id: id.clone(),
            subject,
            description,
            status: TaskStatus::Pending,
            owner: None,
        });
        cx.emit(TaskListEvent::Created(id.clone()));
        cx.notify();
        id
    }

    /// Update fields of a task. Each parameter is an "apply if present" tri-state:
    /// `None` leaves the field untouched; for `owner`/`subject` an outer `Some`
    /// means "apply now", with an inner `None` clearing the field (unassigning a
    /// task, clearing a subject). Returns `Err` if the id is unknown.
    pub fn update(
        &mut self,
        id: &str,
        status: Option<TaskStatus>,
        owner: Option<Option<String>>,
        subject: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let task = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("task {id} not found"))?;
        let mut changed = false;
        if let Some(s) = status {
            task.status = s;
            changed = true;
        }
        if let Some(o) = owner {
            task.owner = o;
            changed = true;
        }
        if let Some(s) = subject {
            task.subject = s;
            changed = true;
        }
        if changed {
            cx.emit(TaskListEvent::Updated(id.to_string()));
            cx.notify();
        }
        Ok(())
    }

    /// Delete a task by id. Emits `Deleted`. Returns `Err` if the id is unknown.
    pub fn delete(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        if self.tasks.len() == before {
            return Err(format!("task {id} not found"));
        }
        cx.emit(TaskListEvent::Deleted(id.to_string()));
        cx.notify();
        Ok(())
    }
}

impl Default for TaskList {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::{Arc, Mutex};

    fn make(cx: &mut App) -> Entity<TaskList> {
        TaskList::new_entity(cx)
    }

    #[test]
    fn create_assigns_sequential_ids_and_starts_pending() {
        let cx = TestAppContext::single();
        let list = cx.update(make);
        let a = cx.update(|cx| list.update(cx, |l, cx| l.create("a".into(), None, cx)));
        let b = cx.update(|cx| list.update(cx, |l, cx| l.create("b".into(), Some("d".into()), cx)));
        assert_eq!(a, "T1");
        assert_eq!(b, "T2");
        cx.update(|cx| {
            list.read_with(cx, |l, _| {
                assert_eq!(l.tasks().len(), 2);
                assert_eq!(l.get("T1").unwrap().subject, "a");
                assert_eq!(l.get("T1").unwrap().description, None);
                assert_eq!(l.get("T1").unwrap().status, TaskStatus::Pending);
                assert_eq!(l.get("T1").unwrap().owner, None);
                assert_eq!(l.get("T2").unwrap().description.as_deref(), Some("d"));
            })
        });
    }

    #[test]
    fn update_applies_present_fields_and_ignores_absent() {
        let cx = TestAppContext::single();
        let list = cx.update(make);
        cx.update(|cx| {
            list.update(cx, |l, cx| {
                l.create("a".into(), None, cx);
            })
        });
        // All three present: assign owner, set in_progress, rename subject.
        cx.update(|cx| {
            list.update(cx, |l, cx| {
                l.update(
                    "T1",
                    Some(TaskStatus::InProgress),
                    Some(Some("plan".into())),
                    Some("A2".into()),
                    cx,
                )
            })
        })
        .unwrap();
        cx.update(|cx| {
            list.read_with(cx, |l, _| {
                let t = l.get("T1").unwrap();
                assert_eq!(t.status, TaskStatus::InProgress);
                assert_eq!(t.owner.as_deref(), Some("plan"));
                assert_eq!(t.subject, "A2");
            })
        });
        // All absent: nothing changes, no error.
        cx.update(|cx| list.update(cx, |l, cx| l.update("T1", None, None, None, cx)))
            .unwrap();
        cx.update(|cx| {
            list.read_with(cx, |l, _| {
                let t = l.get("T1").unwrap();
                assert_eq!(t.status, TaskStatus::InProgress);
                assert_eq!(t.owner.as_deref(), Some("plan"));
                assert_eq!(t.subject, "A2");
            })
        });
        // Unassign via inner None.
        cx.update(|cx| list.update(cx, |l, cx| l.update("T1", None, Some(None), None, cx)))
            .unwrap();
        cx.update(|cx| list.read_with(cx, |l, _| assert_eq!(l.get("T1").unwrap().owner, None)));
    }

    #[test]
    fn update_unknown_id_errors() {
        let cx = TestAppContext::single();
        let list = cx.update(make);
        let err = cx
            .update(|cx| {
                list.update(cx, |l, cx| {
                    l.update("T9", Some(TaskStatus::Completed), None, None, cx)
                })
            })
            .unwrap_err();
        assert!(err.contains("T9"), "error should name the id: {err}");
    }

    #[test]
    fn delete_removes_and_errors_on_unknown() {
        let cx = TestAppContext::single();
        let list = cx.update(make);
        cx.update(|cx| {
            list.update(cx, |l, cx| {
                l.create("a".into(), None, cx);
            })
        });
        cx.update(|cx| {
            list.update(cx, |l, cx| {
                l.create("b".into(), None, cx);
            })
        });
        cx.update(|cx| list.update(cx, |l, cx| l.delete("T1", cx)))
            .unwrap();
        cx.update(|cx| {
            list.read_with(cx, |l, _| {
                assert_eq!(l.tasks().len(), 1);
                assert!(l.get("T1").is_none());
                assert!(l.get("T2").is_some());
            })
        });
        let err = cx
            .update(|cx| list.update(cx, |l, cx| l.delete("T1", cx)))
            .unwrap_err();
        assert!(err.contains("T1"));
        // Ids are not recycled: creating after a delete continues the counter.
        let c = cx.update(|cx| list.update(cx, |l, cx| l.create("c".into(), None, cx)));
        assert_eq!(c, "T3");
    }

    /// Every mutation emits the matching event. Held-to-end subscription so the
    /// `Subscription` lives across the assertions.
    #[test]
    fn mutations_emit_events() {
        let cx = TestAppContext::single();
        let list = cx.update(make);
        let events: Arc<Mutex<Vec<TaskListEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&list, move |_, e: &TaskListEvent, _cx| {
                ev.lock().unwrap().push(e.clone());
            })
        });
        let id = cx.update(|cx| list.update(cx, |l, cx| l.create("a".into(), None, cx)));
        cx.update(|cx| {
            list.update(cx, |l, cx| {
                l.update(
                    &id,
                    Some(TaskStatus::InProgress),
                    Some(Some("plan".into())),
                    None,
                    cx,
                )
                .unwrap();
            })
        });
        cx.update(|cx| list.update(cx, |l, cx| l.delete(&id, cx)))
            .unwrap();
        cx.run_until_parked();
        let got = events.lock().unwrap();
        assert_eq!(got.len(), 3, "expected created/updated/deleted: {got:?}");
        assert!(matches!(got[0], TaskListEvent::Created(ref s) if s == &id));
        assert!(matches!(got[1], TaskListEvent::Updated(ref s) if s == &id));
        assert!(matches!(got[2], TaskListEvent::Deleted(ref s) if s == &id));
    }
}
