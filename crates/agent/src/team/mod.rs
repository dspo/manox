//! Agents team coordination layer.
//!
//! A `Team` is a session-scoped runtime entity (not a process-global config
//! registry like `agent_def` / `mcp`): long-lived members + a shared
//! [`TaskList`] + peer messaging. The leader is the main thread itself; worker
//! members are independent `Entity<Thread>`s that coordinate via
//! `send_message` and the shared task list.

pub mod task_list;

pub use task_list::{Task, TaskList, TaskListEvent, TaskStatus};
