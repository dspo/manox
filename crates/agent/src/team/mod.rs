//! Agents team coordination layer.
//!
//! A `Team` is a session-scoped runtime entity (not a process-global config
//! registry like `agent_def` / `mcp`): long-lived members + a shared
//! [`TaskList`] + peer messaging. The leader is the main thread itself; worker
//! members are independent `Entity<Thread>`s that coordinate via
//! `send_message` and the shared task list.
//!
//! Message routing: `deliver` pushes a [`PeerMessage`] onto the target's inbox
//! and, if the target is idle, immediately flushes it (append a user-role
//! message + emit [`ThreadEvent::PeerMessage`] + `run_turn`). A busy target
//! keeps the message queued; the team's `Stop` subscription (wired at member
//! spawn in `team_spawn`) calls `flush_inbox` on the target's turn end, after
//! `running_turn` has cleared — the `run_turn` guard prevents a double
//! trigger, and `cx.defer` pushes the flush past the in-flight turn's
//! teardown so the guard sees `running_turn == None`. This keeps peer
//! delivery append-only per thread, so each member's `build_completion_request`
//! prefix is stable across turns.

use std::collections::{BTreeMap, VecDeque};

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, WeakEntity};

use crate::thread::Thread;
use crate::tool::ToolContext;

pub mod task_list;

pub use task_list::{Task, TaskList, TaskListEvent, TaskStatus};

/// The leader's member name (matches the main thread's `agent_label`).
pub const LEADER_NAME: &str = "lead";

/// Maximum worker members (excluding the leader). The whole team — leader +
/// workers — is bounded at 6.
pub const MAX_WORKERS: usize = 5;

/// A peer-to-peer message between team members. Routed by [`Team::deliver`].
#[derive(Debug, Clone)]
pub struct PeerMessage {
    pub from: String,
    pub content: String,
}

/// A worker member of a team. The leader is the main thread itself and is held
/// as a `WeakEntity<Thread>` directly on [`Team`]; workers live in the
/// `members` map with their own inbox.
pub struct Member {
    pub name: String,
    pub role: String,
    thread: WeakEntity<Thread>,
    inbox: VecDeque<PeerMessage>,
}

impl Member {
    /// Construct a member descriptor. `team_spawn` (see `team/tools.rs`) builds
    /// the underlying `Entity<Thread>` via `Thread::new_subagent` then wraps it
    /// here.
    pub fn new(name: String, role: String, thread: WeakEntity<Thread>) -> Self {
        Self {
            name,
            role,
            thread,
            inbox: VecDeque::new(),
        }
    }

    pub fn thread(&self) -> WeakEntity<Thread> {
        self.thread.clone()
    }

    pub fn role(&self) -> &str {
        &self.role
    }
}

/// Events emitted by `Team`. The roster UI re-reads `members()` on
/// `MembersChanged`; per-member liveness/status comes from subscribing to each
/// member's `ThreadEvent` (TurnStarted/Stop), not from the team.
#[derive(Debug, Clone)]
pub enum TeamEvent {
    /// A member was added or removed.
    MembersChanged,
}

pub struct Team {
    name: String,
    leader: WeakEntity<Thread>,
    leader_inbox: VecDeque<PeerMessage>,
    members: BTreeMap<String, Member>,
    tasks: Entity<TaskList>,
}

impl EventEmitter<TeamEvent> for Team {}

impl Team {
    /// Construct a team with a fresh shared `TaskList`. The leader is the main
    /// thread (held weakly to avoid a retain cycle: the leader `Thread` owns
    /// the `Entity<Team>`, so the team must not strongly hold the leader).
    pub fn new(name: String, leader: WeakEntity<Thread>, cx: &mut App) -> Entity<Self> {
        let tasks = TaskList::new_entity(cx);
        cx.new(|_| Self {
            name,
            leader,
            leader_inbox: VecDeque::new(),
            members: BTreeMap::new(),
            tasks,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The leader thread (weak). Delivery to [`LEADER_NAME`] routes here.
    pub fn leader(&self) -> WeakEntity<Thread> {
        self.leader.clone()
    }

    /// The shared task list. Leader and every member operate on this entity via
    /// the `task_*` tools.
    pub fn tasks(&self) -> &Entity<TaskList> {
        &self.tasks
    }

    pub fn members(&self) -> &BTreeMap<String, Member> {
        &self.members
    }

    /// Whether the roster still has room for another worker.
    pub fn has_room(&self) -> bool {
        self.members.len() < MAX_WORKERS
    }

    /// Add a worker member. Returns `Err` if a member with that name already
    /// exists or the roster is full. Emits `MembersChanged` so the roster UI
    /// refreshes.
    pub fn insert_member(&mut self, member: Member, cx: &mut Context<Self>) -> Result<(), String> {
        if !self.has_room() {
            return Err(format!(
                "team is full ({} workers max)",
                MAX_WORKERS
            ));
        }
        if self.members.contains_key(&member.name) {
            return Err(format!("member '{}' already exists", member.name));
        }
        self.members.insert(member.name.clone(), member);
        cx.emit(TeamEvent::MembersChanged);
        cx.notify();
        Ok(())
    }

    /// Remove a worker member by name. Emits `MembersChanged`. The member's
    /// `Entity<Thread>` is dropped by the caller (the team only held a weak
    /// reference); queued inbox messages are discarded — a disbanded member
    /// does not receive further peer messages.
    pub fn remove_member(&mut self, name: &str, cx: &mut Context<Self>) -> Result<(), String> {
        if self.members.remove(name).is_none() {
            return Err(format!("member '{name}' not found"));
        }
        cx.emit(TeamEvent::MembersChanged);
        cx.notify();
        Ok(())
    }

    /// Resolve a member name (or [`LEADER_NAME`]) to its thread.
    pub fn thread_for(&self, name: &str) -> Option<WeakEntity<Thread>> {
        if name == LEADER_NAME {
            Some(self.leader.clone())
        } else {
            self.members.get(name).map(|m| m.thread.clone())
        }
    }

    /// Deliver a peer message. `to` is a member name, [`LEADER_NAME`], or
    /// `"all"` for broadcast (leader + every member except the sender). An idle
    /// target receives the message immediately (flush); a busy target's message
    /// queues for `flush_inbox` on turn end.
    pub fn deliver(
        &mut self,
        from: &str,
        to: &str,
        content: String,
        cx: &mut App,
    ) -> Result<(), String> {
        if to == "all" {
            let targets: Vec<String> = std::iter::once(LEADER_NAME.to_string())
                .chain(self.members.keys().cloned())
                .filter(|n| n != from)
                .collect();
            for t in targets {
                self.deliver_one(from, &t, content.clone(), cx)?;
            }
            return Ok(());
        }
        self.deliver_one(from, to, content, cx)
    }

    fn deliver_one(
        &mut self,
        from: &str,
        to: &str,
        content: String,
        cx: &mut App,
    ) -> Result<(), String> {
        let msg = PeerMessage {
            from: from.to_string(),
            content,
        };
        let thread = if to == LEADER_NAME {
            self.leader.clone()
        } else {
            match self.members.get(to) {
                Some(m) => m.thread.clone(),
                None => return Err(format!("unknown team member '{to}'")),
            }
        };
        let busy = thread
            .upgrade()
            .map(|t| t.read(cx).is_running())
            .unwrap_or(false);
        if busy {
            if to == LEADER_NAME {
                self.leader_inbox.push_back(msg);
            } else {
                self.members
                    .get_mut(to)
                    .expect("member checked above")
                    .inbox
                    .push_back(msg);
            }
            return Ok(());
        }
        if let Some(t) = thread.upgrade() {
            t.update(cx, |th, cx| th.deliver_peer_messages(vec![msg], cx));
        }
        Ok(())
    }

    /// Drain a target's inbox and feed all queued messages to it in one turn.
    /// Called by the team's `Stop` subscription (wired at member spawn) after
    /// the target's turn has ended — `running_turn` is `None` by then, so the
    /// `run_turn` inside `deliver_peer_messages` proceeds. A no-op when the
    /// inbox is empty.
    pub fn flush_inbox(&mut self, who: &str, cx: &mut App) {
        let (thread, msgs) = if who == LEADER_NAME {
            (self.leader.clone(), self.leader_inbox.drain(..).collect::<Vec<_>>())
        } else {
            let Some(m) = self.members.get_mut(who) else {
                return;
            };
            (m.thread.clone(), m.inbox.drain(..).collect::<Vec<_>>())
        };
        if msgs.is_empty() {
            return;
        }
        if let Some(t) = thread.upgrade() {
            t.update(cx, |th, cx| th.deliver_peer_messages(msgs, cx));
        }
    }
}

/// Convenience: read the owning thread's team off a [`ToolContext`], if any.
/// Team tools use this to reach the shared [`TaskList`] and the message router.
pub fn team_from_ctx(ctx: &dyn ToolContext) -> Option<Entity<Team>> {
    ctx.team().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ThreadRecord;
    use crate::thread::ThreadEvent;
    use gpui::TestAppContext;
    use std::sync::{Arc, Mutex};

    /// A bare thread for routing tests — no model, so `run_turn` emits an
    /// `Error` and returns without occupying `running_turn`. The team layer
    /// only touches `insert_user_message` / `run_turn` / `is_running`, all of
    /// which behave safely without a provider.
    fn bare_thread(id: &str, cx: &mut App) -> Entity<Thread> {
        Thread::restore(
            ThreadRecord::for_test(id, "/tmp", Vec::new()),
            None,
            cx,
        )
    }

    /// Capture `ThreadEvent::PeerMessage` emissions on a thread. The returned
    /// `Arc` is read after `cx.run_until_parked()` flushes deferred delivery.
    fn capture_peer_events(
        thread: &Entity<Thread>,
        cx: &mut App,
    ) -> Arc<Mutex<Vec<(String, String)>>> {
        let events: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        cx.subscribe(thread, move |_, e: &ThreadEvent, _| {
            if let ThreadEvent::PeerMessage { from, content } = e {
                ev.lock().unwrap().push((from.clone(), content.clone()));
            }
        })
        .detach();
        events
    }

    #[test]
    fn deliver_to_idle_member_inserts_user_message_and_emits() {
        crate::agent_def::init();
        let cx = TestAppContext::single();
        let (team, member_thread) = cx.update(|cx| {
            let leader = bare_thread("lead", cx);
            let team = Team::new("squad".into(), leader.downgrade(), cx);
            let member = bare_thread("plan", cx);
            let m = Member::new("plan".into(), "explorer".into(), member.downgrade());
            team.update(cx, |t, cx| t.insert_member(m, cx)).unwrap();
            (team, member)
        });
        let events = cx.update(|cx| capture_peer_events(&member_thread, cx));

        cx.update(|cx| {
            team.update(cx, |t, cx| t.deliver("lead", "plan", "hello".into(), cx))
        })
        .unwrap();
        cx.run_until_parked();

        let msgs = cx.update(|cx| member_thread.read(cx).messages().to_vec());
        assert_eq!(msgs.len(), 1, "one user message injected");
        assert_eq!(msgs[0].role, crate::language_model::Role::User);
        let text = msgs[0]
            .content
            .iter()
            .map(|c| match c {
                crate::language_model::MessageContent::Text(t) => t.as_str(),
                _ => "",
            })
            .collect::<String>();
        assert!(text.contains("[from lead]"), "got: {text}");
        assert!(text.contains("hello"), "got: {text}");
        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 1, "PeerMessage emitted once: {evs:?}");
        assert_eq!(evs[0], ("lead".to_string(), "hello".to_string()));
    }

    #[test]
    fn deliver_to_busy_member_enqueues_for_flush() {
        crate::agent_def::init();
        let cx = TestAppContext::single();
        let (team, member_thread) = cx.update(|cx| {
            let leader = bare_thread("lead", cx);
            let team = Team::new("squad".into(), leader.downgrade(), cx);
            let member = bare_thread("plan", cx);
            let m = Member::new("plan".into(), "explorer".into(), member.downgrade());
            team.update(cx, |t, cx| t.insert_member(m, cx)).unwrap();
            (team, member)
        });
        let events = cx.update(|cx| capture_peer_events(&member_thread, cx));

        // Mark the member busy by occupying its running_turn slot.
        cx.update(|cx| {
            let task = cx.background_spawn(async {});
            member_thread.update(cx, |t, _| t.set_running_turn_for_test(Some(task)));
        });

        // Deliver while busy → enqueues, does not emit, does not inject.
        cx.update(|cx| {
            team.update(cx, |t, cx| t.deliver("lead", "plan", "queued".into(), cx))
        })
        .unwrap();
        cx.run_until_parked();
        assert!(events.lock().unwrap().is_empty(), "no PeerMessage while busy");
        assert!(
            cx.update(|cx| member_thread.read(cx).messages().is_empty()),
            "no user message injected while busy"
        );

        // Turn ends (running_turn cleared). Flush drains the inbox.
        cx.update(|cx| {
            member_thread.update(cx, |t, _| t.set_running_turn_for_test(None));
        });
        cx.update(|cx| team.update(cx, |t, cx| t.flush_inbox("plan", cx)));
        cx.run_until_parked();
        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 1, "flush delivered the queued message: {evs:?}");
        assert_eq!(evs[0].1, "queued");
    }

    #[test]
    fn flush_drains_multiple_messages_into_one_turn() {
        crate::agent_def::init();
        let cx = TestAppContext::single();
        let (team, member_thread) = cx.update(|cx| {
            let leader = bare_thread("lead", cx);
            let team = Team::new("squad".into(), leader.downgrade(), cx);
            let member = bare_thread("plan", cx);
            let m = Member::new("plan".into(), "explorer".into(), member.downgrade());
            team.update(cx, |t, cx| t.insert_member(m, cx)).unwrap();
            (team, member)
        });
        cx.update(|cx| {
            let task = cx.background_spawn(async {});
            member_thread.update(cx, |t, _| t.set_running_turn_for_test(Some(task)));
        });
        cx.update(|cx| {
            team.update(cx, |t, cx| t.deliver("lead", "plan", "one".into(), cx))
        })
        .unwrap();
        cx.update(|cx| {
            team.update(cx, |t, cx| t.deliver("lead", "plan", "two".into(), cx))
        })
        .unwrap();
        cx.update(|cx| {
            member_thread.update(cx, |t, _| t.set_running_turn_for_test(None));
        });
        cx.update(|cx| team.update(cx, |t, cx| t.flush_inbox("plan", cx)));
        cx.run_until_parked();
        let msgs = cx.update(|cx| member_thread.read(cx).messages().to_vec());
        assert_eq!(msgs.len(), 2, "both queued messages injected in one flush");
    }

    #[test]
    fn deliver_unknown_target_errors() {
        crate::agent_def::init();
        let cx = TestAppContext::single();
        let (team, _leader) = cx.update(|cx| {
            let leader = bare_thread("lead", cx);
            (Team::new("squad".into(), leader.downgrade(), cx), leader)
        });
        let err = cx
            .update(|cx| {
                team.update(cx, |t, cx| t.deliver("lead", "ghost", "x".into(), cx))
            })
            .unwrap_err();
        assert!(err.contains("ghost"), "error names the target: {err}");
    }

    #[test]
    fn broadcast_reaches_leader_and_members_except_sender() {
        crate::agent_def::init();
        let cx = TestAppContext::single();
        let (team, leader, plan_thread, expl_thread) = cx.update(|cx| {
            let leader = bare_thread("lead", cx);
            let team = Team::new("squad".into(), leader.downgrade(), cx);
            let plan = bare_thread("plan", cx);
            let expl = bare_thread("expl", cx);
            team.update(cx, |t, cx| {
                t.insert_member(Member::new("plan".into(), "p".into(), plan.downgrade()), cx)
                    .unwrap();
                t.insert_member(Member::new("expl".into(), "e".into(), expl.downgrade()), cx)
                    .unwrap();
            });
            (team, leader, plan, expl)
        });
        let leader_ev = cx.update(|cx| capture_peer_events(&leader, cx));
        let plan_ev = cx.update(|cx| capture_peer_events(&plan_thread, cx));
        let expl_ev = cx.update(|cx| capture_peer_events(&expl_thread, cx));

        // plan broadcasts: leader + expl get it; plan (sender) does not.
        cx.update(|cx| {
            team.update(cx, |t, cx| t.deliver("plan", "all", "standup".into(), cx))
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(leader_ev.lock().unwrap().len(), 1, "leader got broadcast");
        assert!(plan_ev.lock().unwrap().is_empty(), "sender excluded");
        assert_eq!(expl_ev.lock().unwrap().len(), 1, "expl got broadcast");
    }

    #[test]
    fn insert_member_rejects_full_roster_and_duplicate_name() {
        crate::agent_def::init();
        let cx = TestAppContext::single();
        let (team, _leader) = cx.update(|cx| {
            let leader = bare_thread("lead", cx);
            (Team::new("squad".into(), leader.downgrade(), cx), leader)
        });
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                for i in 0..MAX_WORKERS {
                    let th = bare_thread(&format!("m{i}"), cx);
                    t.insert_member(
                        Member::new(format!("m{i}"), "r".into(), th.downgrade()),
                        cx,
                    )
                    .unwrap();
                }
            })
        });
        let err = cx
            .update(|cx| {
                let th = bare_thread("extra", cx);
                team.update(cx, |t, cx| {
                    t.insert_member(Member::new("extra".into(), "r".into(), th.downgrade()), cx)
                })
            })
            .unwrap_err();
        assert!(err.contains("full"), "roster cap enforced: {err}");

        // Duplicate name on a non-full team (the full check short-circuits the
        // dup check, so test dup separately on a fresh team).
        let (team2, _leader2) = cx.update(|cx| {
            let leader = bare_thread("lead2", cx);
            (Team::new("squad2".into(), leader.downgrade(), cx), leader)
        });
        cx.update(|cx| {
            let th = bare_thread("m0", cx);
            team2.update(cx, |t, cx| {
                t.insert_member(Member::new("m0".into(), "r".into(), th.downgrade()), cx)
            })
        })
        .unwrap();
        let err = cx
            .update(|cx| {
                let th = bare_thread("m0b", cx);
                team2.update(cx, |t, cx| {
                    t.insert_member(Member::new("m0".into(), "r".into(), th.downgrade()), cx)
                })
            })
            .unwrap_err();
        assert!(err.contains("already exists"), "dup rejected: {err}");
    }
}
