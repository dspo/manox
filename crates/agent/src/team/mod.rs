//! Agents team coordination layer (ported from the retired manox harness to
//! the pi facade).
//!
//! A `Team` is a session-scoped runtime entity (not a process-global config
//! registry like agent definitions / mcp): long-lived members + a shared
//! [`TaskList`] + peer messaging. The leader is the main thread itself;
//! worker members are independent pi `Entity<Thread>`s that coordinate via
//! `SendMessage` and the shared task list.
//!
//! Message routing: `deliver` pushes a [`PeerMessage`] onto the target's
//! inbox and, if the target is idle, immediately flushes it (append a
//! user-role message + emit [`ThreadEvent::PeerMessage`] + `run_turn`). A
//! busy target keeps the message queued; the team's `Stop` subscription
//! (wired at member spawn) calls `flush_inbox` on the target's turn end —
//! this keeps peer delivery append-only per thread.

pub mod task_list;
pub mod tools;

use std::collections::{BTreeMap, VecDeque};

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, WeakEntity};

pub use task_list::{Task, TaskList, TaskListEvent, TaskStatus};

use crate::language_model::StopReason;
use crate::thread::Thread;

/// The leader's member name (matches the main thread's `agent_label`).
pub const LEADER_NAME: &str = "lead";

/// Sender label of system-synthesized team notices (member stop lifecycle
/// notifications) delivered to the leader like peer mail.
pub const TEAM_NOTICE_FROM: &str = "team";

/// Attribution for a routing name: the leader resolves to the main agent,
/// every other name is a named agent (team member / manifest definition).
pub fn author_for(name: &str) -> crate::message::MessageAuthor {
    crate::message::MessageAuthor::from_routing(name)
}

/// Maximum worker members (excluding the leader). The whole team — leader +
/// workers — is bounded at 6.
pub const MAX_WORKERS: usize = 5;

/// A peer-to-peer message between team members. Routed by [`Team::deliver`].
#[derive(Debug, Clone)]
pub struct PeerMessage {
    pub from: String,
    pub content: String,
}

/// A worker member of a team. The leader is the main thread itself and is
/// held as a `WeakEntity<Thread>` directly on [`Team`] (the leader `Thread`
/// owns the `Entity<Team>`, so the team must not strongly hold the leader
/// back); workers live in the `members` map, strongly owned by the team —
/// their `Entity<Thread>` lifetime is the team's lifetime, and `disband`
/// drops them. The `member → team` edge (via `Thread.team`) is also strong,
/// so team↔member is a strong cycle; `disband` breaks it by clearing
/// `member.team` before dropping the roster, and `TeamCreate` refuses a
/// second team while one is active so the cycle can't accumulate across
/// recreations.
pub struct Member {
    pub name: String,
    pub role: String,
    thread: Entity<Thread>,
    inbox: VecDeque<PeerMessage>,
    /// The member sent the leader at least one message during its current
    /// turn — the completion-report signal the stop notification carries.
    reported: bool,
    /// Last terminal stop reason observed for this member.
    last_stop: Option<StopReason>,
    /// Unix seconds of `last_stop` (last activity for the roster view).
    last_stop_at: Option<i64>,
}

impl Member {
    pub fn new(name: String, role: String, thread: Entity<Thread>) -> Self {
        Self {
            name,
            role,
            thread,
            inbox: VecDeque::new(),
            reported: false,
            last_stop: None,
            last_stop_at: None,
        }
    }

    pub fn thread(&self) -> &Entity<Thread> {
        &self.thread
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    /// Whether the member reported to the leader during its current turn.
    pub fn reported(&self) -> bool {
        self.reported
    }

    /// The member's last terminal stop reason, if any.
    pub fn last_stop(&self) -> Option<StopReason> {
        self.last_stop
    }

    /// Unix seconds of the last terminal stop (roster last-activity).
    pub fn last_stop_at(&self) -> Option<i64> {
        self.last_stop_at
    }
}

/// Events emitted by `Team`. The roster UI re-reads `members()` on
/// `MembersChanged`; per-member liveness/status comes from subscribing to
/// each member's `ThreadEvent`, not from the team.
#[derive(Debug, Clone)]
pub enum TeamEvent {
    /// A member was added or removed.
    MembersChanged,
}

/// A member's pending authorization routed to the leader under a composite
/// id (`<member>::<child_id>`); the leader's verdict routes back through
/// [`Team::resolve_child_auth`].
#[derive(Debug, Clone)]
struct ChildAuth {
    member: WeakEntity<Thread>,
    child_id: String,
}

pub struct Team {
    name: String,
    leader: WeakEntity<Thread>,
    leader_inbox: VecDeque<PeerMessage>,
    members: BTreeMap<String, Member>,
    /// Per-member event subscriptions kept alive for the member's lifetime:
    /// `ToolCallAuthorization` bubbles to the leader as `<name>::<auth>`,
    /// terminal `Stop` flushes the member's inbox.
    member_subs: BTreeMap<String, gpui::Subscription>,
    /// The leader's own `Stop` subscription, flushing `leader_inbox` when
    /// the leader's turn ends. `None` until `TeamCreate` wires it.
    leader_sub: Option<gpui::Subscription>,
    /// Pending member authorizations keyed by composite id.
    child_auth: BTreeMap<String, ChildAuth>,
    tasks: Entity<TaskList>,
}

impl EventEmitter<TeamEvent> for Team {}

impl Team {
    /// Construct a team with a fresh shared `TaskList`. The leader is the
    /// main thread (held weakly to avoid a retain cycle: the leader `Thread`
    /// owns the `Entity<Team>`, so the team must not strongly hold the
    /// leader).
    pub fn new(name: String, leader: WeakEntity<Thread>, cx: &mut App) -> Entity<Self> {
        let tasks = TaskList::new_entity(cx);
        cx.new(|_| Self {
            name,
            leader,
            leader_inbox: VecDeque::new(),
            members: BTreeMap::new(),
            member_subs: BTreeMap::new(),
            leader_sub: None,
            child_auth: BTreeMap::new(),
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

    /// Whether `thread` is this team's leader.
    pub fn is_leader(&self, thread: &Entity<Thread>) -> bool {
        self.leader
            .upgrade()
            .is_some_and(|leader| leader.entity_id() == thread.entity_id())
    }

    /// The shared task list. Leader and every member operate on this entity
    /// via the `task_*` tools.
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
            return Err(format!("team is full ({} workers max)", MAX_WORKERS));
        }
        if self.members.contains_key(&member.name) {
            return Err(format!("member '{}' already exists", member.name));
        }
        self.members.insert(member.name.clone(), member);
        cx.emit(TeamEvent::MembersChanged);
        cx.notify();
        Ok(())
    }

    /// Remove a worker member by name. Emits `MembersChanged`.
    pub fn remove_member(&mut self, name: &str, cx: &mut Context<Self>) -> Result<(), String> {
        if self.members.remove(name).is_none() {
            return Err(format!("member '{name}' not found"));
        }
        self.member_subs.remove(name);
        cx.emit(TeamEvent::MembersChanged);
        cx.notify();
        Ok(())
    }

    /// Resolve a worker member name to its thread.
    pub fn thread_for(&self, name: &str) -> Option<&Entity<Thread>> {
        self.members.get(name).map(|m| &m.thread)
    }

    /// Store a member's auth/`Stop` subscription. Called after
    /// `insert_member`; the subscription lives for the member's tenure.
    pub fn set_member_sub(&mut self, name: String, sub: gpui::Subscription) {
        self.member_subs.insert(name, sub);
    }

    /// Store the leader's `Stop` subscription. Called once by `TeamCreate`.
    pub fn set_leader_sub(&mut self, sub: gpui::Subscription) {
        self.leader_sub = Some(sub);
    }

    /// Register a member's pending authorization under a composite id so the
    /// leader's verdict can route back to the member.
    pub fn register_child_auth(
        &mut self,
        composite: String,
        member: WeakEntity<Thread>,
        child_id: String,
    ) {
        self.child_auth
            .insert(composite, ChildAuth { member, child_id });
    }

    /// Resolve a composite authorization id to the member thread + the
    /// member-local authorization id.
    pub fn resolve_child_auth(&self, composite: &str) -> Option<(Entity<Thread>, String)> {
        let auth = self.child_auth.get(composite)?;
        Some((auth.member.upgrade()?, auth.child_id.clone()))
    }

    /// Drop a resolved authorization entry (after the verdict landed).
    pub fn clear_child_auth(&mut self, composite: &str) {
        self.child_auth.remove(composite);
    }

    /// Deliver a peer message. `to` is a member name, [`LEADER_NAME`], or
    /// `"all"` for broadcast (leader + every member except the sender). An
    /// idle target receives the message immediately (flush); a busy target's
    /// message queues for `flush_inbox` on turn end.
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
        // A member→leader delivery is the completion-report signal the
        // stop notification carries; broadcast counts too (it routes here
        // per target).
        if to == LEADER_NAME
            && from != LEADER_NAME
            && let Some(m) = self.members.get_mut(from)
        {
            m.reported = true;
        }
        let msg = PeerMessage {
            from: from.to_string(),
            content,
        };
        if to == LEADER_NAME {
            return self.deliver_to_leader(msg, cx);
        }
        let Some(m) = self.members.get_mut(to) else {
            return Err(format!("unknown team member '{to}'"));
        };
        if m.thread.read(cx).is_running() {
            m.inbox.push_back(msg);
            return Ok(());
        }
        let thread = m.thread.clone();
        thread.update(cx, |th, cx| th.deliver_peer_messages(vec![msg], cx));
        Ok(())
    }

    fn deliver_to_leader(&mut self, msg: PeerMessage, cx: &mut App) -> Result<(), String> {
        let Some(leader) = self.leader.upgrade() else {
            // Leader gone — there is no one to deliver to; drop the message.
            return Ok(());
        };
        if leader.read(cx).is_running() {
            self.leader_inbox.push_back(msg);
            return Ok(());
        }
        leader.update(cx, |th, cx| th.deliver_peer_messages(vec![msg], cx));
        Ok(())
    }

    /// Drain a target's inbox and feed all queued messages to it in one
    /// turn. Called by the team's `Stop` subscriptions after the target's
    /// turn has ended. A no-op when the inbox is empty.
    pub fn flush_inbox(&mut self, who: &str, cx: &mut App) {
        if who == LEADER_NAME {
            let msgs: Vec<PeerMessage> = self.leader_inbox.drain(..).collect();
            if msgs.is_empty() {
                return;
            }
            if let Some(t) = self.leader.upgrade() {
                t.update(cx, |th, cx| th.deliver_peer_messages(msgs, cx));
            }
            return;
        }
        let Some(m) = self.members.get_mut(who) else {
            return;
        };
        let msgs: Vec<PeerMessage> = m.inbox.drain(..).collect();
        if msgs.is_empty() {
            return;
        }
        let thread = m.thread.clone();
        thread.update(cx, |th, cx| th.deliver_peer_messages(msgs, cx));
    }

    /// A member began a turn: `reported` now describes the new turn, so the
    /// previous turn's flag no longer applies.
    pub fn member_turn_started(&mut self, name: &str) {
        if let Some(m) = self.members.get_mut(name) {
            m.reported = false;
        }
    }

    /// Record a member's terminal stop and deliver the lifecycle
    /// notification to the leader like peer mail: an idle leader wakes
    /// immediately, a busy one drains it at turn end. The leader's playbook
    /// decides the reaction (dismiss / nudge / replace). Emits
    /// [`TeamEvent::MembersChanged`] so roster observers (member panel chip)
    /// re-read `last_stop` without waiting for an unrelated redraw.
    pub fn member_stopped(&mut self, name: &str, reason: StopReason, cx: &mut Context<Self>) {
        let Some(m) = self.members.get_mut(name) else {
            return;
        };
        m.last_stop = Some(reason);
        m.last_stop_at = Some(chrono::Utc::now().timestamp());
        let reported = m.reported;
        let _ = self.deliver_to_leader(
            PeerMessage {
                from: TEAM_NOTICE_FROM.to_string(),
                content: format!("{name} stopped: reason={reason:?}, reported={reported}"),
            },
            cx,
        );
        cx.emit(TeamEvent::MembersChanged);
        cx.notify();
    }

    /// Stop running members, break the team↔member cycle, and release
    /// roster state — the store-free half of [`Team::disband`]. Exposed so
    /// [`crate::thread_store::ThreadStore::archive_thread`] can tear down a
    /// live team without re-entering the store (disband itself archives and
    /// would double-borrow if called from inside a store update).
    pub fn teardown(&mut self, cx: &mut Context<Self>) {
        for m in self.members.values() {
            m.thread.update(cx, |t, cx| {
                if t.is_running() {
                    t.cancel(cx);
                }
                t.clear_team(cx);
            });
        }
        self.members.clear();
        self.member_subs.clear();
        self.leader_sub = None;
        self.leader_inbox.clear();
        self.child_auth.clear();
        cx.emit(TeamEvent::MembersChanged);
        cx.notify();
    }

    /// Tear down the team and archive every member session (cleanup
    /// invariant — the sidebar row leaves the active list; the jsonl stays
    /// on disk for audit). The leader's own `team` field is cleared
    /// separately by the `TeamDisband` op — the team entity holds the leader
    /// only weakly and cannot reach it.
    pub fn disband(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .members
            .values()
            .map(|m| m.thread.read(cx).id.0.clone())
            .collect();
        self.teardown(cx);
        if let Some(store) = crate::thread_store::try_global() {
            for id in ids {
                store.update(cx, |s, cx| s.archive_thread(&id, true, cx));
            }
        }
    }
}

/// Rendered leader lifecycle playbook (stop-notice reactions, dismiss /
/// nudge / replace, cleanup etiquette) for the thread's agent language;
/// delivered to the leader once at `TeamCreate`.
pub fn render_leader_playbook(lang: crate::language::Language) -> anyhow::Result<String> {
    crate::prompt::render(
        crate::prompt::PromptTemplate::SystemTeam,
        lang,
        &crate::prompt::TeamPromptData { is_leader: true },
    )
}

/// Rendered member obligations (final report before stopping); appended to
/// the member's opening prompt at spawn.
pub fn render_member_obligations(lang: crate::language::Language) -> anyhow::Result<String> {
    crate::prompt::render(
        crate::prompt::PromptTemplate::SystemTeam,
        lang,
        &crate::prompt::TeamPromptData { is_leader: false },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::ThreadEvent;
    use gpui::TestAppContext;
    use std::sync::{Arc, Mutex};

    /// A bare thread with a scripted engine: `run_turn` records the prompt
    /// on the engine instead of touching a provider, so delivery tests stay
    /// hermetic.
    fn bare_thread(label: &str, cx: &mut TestAppContext) -> Entity<Thread> {
        let thread = crate::thread::tests::thread_with_engine(
            crate::thread::HistoryPhase::Ready,
            Arc::new(crate::thread::tests::FakeEngine::new()),
            cx,
        );
        cx.update(|cx| thread.update(cx, |t, _cx| t.set_label_for_test(label.to_string())));
        thread
    }

    fn capture_peer_events(
        thread: &Entity<Thread>,
        cx: &mut gpui::App,
    ) -> Arc<Mutex<Vec<(String, String)>>> {
        let events: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        cx.subscribe(thread, move |_t, e: &ThreadEvent, _cx: &mut gpui::App| {
            if let ThreadEvent::PeerMessage { from, content } = e {
                ev.lock().unwrap().push((from.clone(), content.clone()));
            }
        })
        .detach();
        events
    }

    #[test]
    fn deliver_to_idle_member_inserts_user_message_and_emits() {
        let mut cx = TestAppContext::single();
        let leader = bare_thread("lead", &mut cx);
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        let member_thread = bare_thread("plan", &mut cx);
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member_thread.clone()),
                    cx,
                )
            })
            .unwrap()
        });
        let events = cx.update(|cx| capture_peer_events(&member_thread, cx));

        cx.update(|cx| team.update(cx, |t, cx| t.deliver("lead", "plan", "hello".into(), cx)))
            .unwrap();
        cx.run_until_parked();

        let msgs = cx.update(|cx| member_thread.read(cx).messages().to_vec());
        assert_eq!(msgs.len(), 1, "one user message injected");
        let text = msgs[0]
            .content
            .iter()
            .map(|c| match c {
                crate::language_model::MessageContent::Text(t) => t.as_str(),
                _ => "",
            })
            .collect::<String>();
        assert!(text.contains("lead"), "got: {text}");
        assert!(text.contains("hello"), "got: {text}");
        let ui = msgs[0]
            .ui
            .as_ref()
            .expect("peer delivery carries attribution");
        assert_eq!(ui.author, Some(crate::message::MessageAuthor::Lead));
        assert_eq!(ui.peer, Some(true));
        assert_eq!(ui.display_text.as_deref(), Some("hello"));
        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 1, "PeerMessage emitted once: {evs:?}");
        assert_eq!(evs[0], ("lead".to_string(), "hello".to_string()));
    }

    #[test]
    fn deliver_to_busy_member_enqueues_then_flush_drains() {
        let mut cx = TestAppContext::single();
        let leader = bare_thread("lead", &mut cx);
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        let member_thread = bare_thread("plan", &mut cx);
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member_thread.clone()),
                    cx,
                )
            })
            .unwrap()
        });

        // Mark the member busy: delivery must enqueue, not inject.
        cx.update(|cx| member_thread.update(cx, |t, _cx| t.set_running_for_test(true)));
        cx.update(|cx| team.update(cx, |t, cx| t.deliver("lead", "plan", "queued".into(), cx)))
            .unwrap();
        cx.update(|cx| {
            assert!(
                member_thread.read(cx).messages().is_empty(),
                "busy member does not receive immediately"
            );
        });

        // Turn ends → flush delivers the queued message.
        cx.update(|cx| member_thread.update(cx, |t, _cx| t.set_running_for_test(false)));
        cx.update(|cx| team.update(cx, |t, cx| t.flush_inbox("plan", cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            let msgs = member_thread.read(cx).messages();
            assert_eq!(msgs.len(), 1, "flushed message landed");
        });
    }

    #[test]
    fn insert_member_rejects_duplicates_and_full_roster() {
        let mut cx = TestAppContext::single();
        let leader = bare_thread("lead", &mut cx);
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        for i in 0..MAX_WORKERS {
            let member = bare_thread(&format!("m{i}"), &mut cx);
            cx.update(|cx| {
                team.update(cx, |t, cx| {
                    t.insert_member(Member::new(format!("m{i}"), "role".into(), member), cx)
                })
                .unwrap()
            });
        }
        let extra = bare_thread("extra", &mut cx);
        let full = cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(Member::new("extra".into(), "role".into(), extra), cx)
            })
        });
        assert!(full.is_err(), "roster full");

        let leader2 = bare_thread("lead2", &mut cx);
        let team2 = cx.update(|cx| Team::new("squad2".into(), leader2.downgrade(), cx));
        let a = bare_thread("dup", &mut cx);
        let b = bare_thread("dup", &mut cx);
        cx.update(|cx| {
            team2
                .update(cx, |t, cx| {
                    t.insert_member(Member::new("dup".into(), "r".into(), a), cx)
                })
                .unwrap()
        });
        let dup = cx.update(|cx| {
            team2.update(cx, |t, cx| {
                t.insert_member(Member::new("dup".into(), "r".into(), b), cx)
            })
        });
        assert!(dup.is_err(), "duplicate name rejected");
    }

    #[test]
    fn broadcast_reaches_everyone_except_sender() {
        let mut cx = TestAppContext::single();
        let leader = bare_thread("lead", &mut cx);
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        let m1 = bare_thread("alice", &mut cx);
        let m2 = bare_thread("bob", &mut cx);
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(Member::new("alice".into(), "r".into(), m1.clone()), cx)
                    .unwrap();
                t.insert_member(Member::new("bob".into(), "r".into(), m2.clone()), cx)
                    .unwrap();
            });
        });

        cx.update(|cx| team.update(cx, |t, cx| t.deliver("alice", "all", "hi all".into(), cx)))
            .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(m1.read(cx).messages().len(), 0, "sender excluded");
            assert_eq!(m2.read(cx).messages().len(), 1, "other member got it");
        });
    }

    #[test]
    fn disband_clears_members_and_back_references() {
        let mut cx = TestAppContext::single();
        let leader = bare_thread("lead", &mut cx);
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        let member_thread = bare_thread("plan", &mut cx);
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "r".into(), member_thread.clone()),
                    cx,
                )
                .unwrap();
            });
            member_thread.update(cx, |m, cx| m.set_team(team.clone(), cx));
        });

        cx.update(|cx| team.update(cx, |t, cx| t.disband(cx)));
        cx.update(|cx| {
            assert!(team.read(cx).members().is_empty());
            assert!(member_thread.read(cx).team().is_none(), "back-ref cleared");
        });
    }

    #[test]
    fn child_auth_round_trips_through_composite_id() {
        let mut cx = TestAppContext::single();
        let leader = bare_thread("lead", &mut cx);
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        let member_thread = bare_thread("plan", &mut cx);
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "r".into(), member_thread.clone()),
                    cx,
                )
                .unwrap();
            });
        });

        cx.update(|cx| {
            team.update(cx, |t, _| {
                t.register_child_auth(
                    "plan::auth-1".into(),
                    member_thread.downgrade(),
                    "auth-1".into(),
                )
            });
            let (resolved_member, child_id) =
                team.read(cx).resolve_child_auth("plan::auth-1").unwrap();
            assert_eq!(child_id, "auth-1");
            assert_eq!(
                resolved_member.entity_id(),
                member_thread.entity_id(),
                "resolved to the member thread"
            );
            team.update(cx, |t, _| t.clear_child_auth("plan::auth-1"));
            assert!(team.read(cx).resolve_child_auth("plan::auth-1").is_none());
        });
    }

    /// Cleanup invariant: disband archives every member session so the rows
    /// leave the sidebar active list (store present ⇒ archival runs).
    #[test]
    fn disband_archives_member_sessions() {
        let _store_lock = crate::thread_store::store_test_lock().lock().unwrap();
        let mut cx = TestAppContext::single();
        let db_path =
            std::env::temp_dir().join(format!("team-disband-{}.db", uuid::Uuid::new_v4()));
        let db = std::sync::Arc::new(
            crate::db::ThreadsDatabase::open(&db_path).expect("open temp threads db"),
        );
        cx.update(|cx| crate::thread_store::init_for_test(db, cx));

        let leader = bare_thread("lead", &mut cx);
        let member_thread = bare_thread("plan", &mut cx);
        let (leader_id, member_id) = cx.update(|cx| {
            (
                leader.read(cx).id.0.clone(),
                member_thread.read(cx).id.0.clone(),
            )
        });
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        cx.update(|cx| {
            let store = crate::thread_store::global();
            store.update(cx, |s, _| {
                s.insert_summary_for_test(&leader_id, None);
                s.insert_summary_for_test(&member_id, Some(&leader_id));
            });
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member_thread.clone()),
                    cx,
                )
            })
            .unwrap();
        });

        cx.update(|cx| team.update(cx, |t, cx| t.disband(cx)));

        cx.update(|cx| {
            let store = crate::thread_store::global();
            let archived = store.read(cx).archived_summaries().to_vec();
            assert!(
                archived.iter().any(|s| s.id == member_id && s.archived),
                "member session archived on disband: {archived:?}"
            );
        });
        crate::thread_store::drop_for_test();
        std::fs::remove_file(db_path).ok();
    }

    /// A member that reported then stopped produces a lifecycle notice on
    /// the leader carrying reason + reported=true.
    #[test]
    fn member_stop_notifies_leader_with_reason_and_reported() {
        let mut cx = TestAppContext::single();
        let leader = bare_thread("lead", &mut cx);
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        let member_thread = bare_thread("plan", &mut cx);
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member_thread.clone()),
                    cx,
                )
            })
            .unwrap()
        });
        let events = cx.update(|cx| capture_peer_events(&leader, cx));

        // Member reports to the leader, then its turn ends.
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.deliver("plan", LEADER_NAME, "done".into(), cx)
            })
        })
        .unwrap();
        // The scripted engine never emits a terminal Stop, so the facade
        // stays "running" after the report; settle it like a real turn end.
        cx.update(|cx| leader.update(cx, |t, _cx| t.set_running_for_test(false)));
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.member_stopped("plan", crate::language_model::StopReason::EndTurn, cx)
            })
        });
        cx.run_until_parked();

        let evs = events.lock().unwrap();
        assert_eq!(evs.len(), 2, "report + lifecycle notice: {evs:?}");
        assert_eq!(evs[0].0, "plan");
        assert_eq!(evs[1].0, TEAM_NOTICE_FROM);
        assert!(
            evs[1].1.contains("plan stopped") && evs[1].1.contains("reported=true"),
            "notice content: {:?}",
            evs[1].1
        );
    }

    /// Centralized archive invariant: archiving a live leader through the
    /// store tears the team down (roster released, member back-refs
    /// cleared) even when no caller disbanded first.
    #[test]
    fn archive_leader_tears_down_team_via_store() {
        let _store_lock = crate::thread_store::store_test_lock().lock().unwrap();
        let mut cx = TestAppContext::single();
        let db_path =
            std::env::temp_dir().join(format!("team-archive-{}.db", uuid::Uuid::new_v4()));
        let db = std::sync::Arc::new(
            crate::db::ThreadsDatabase::open(&db_path).expect("open temp threads db"),
        );
        cx.update(|cx| crate::thread_store::init_for_test(db, cx));

        let leader = bare_thread("lead", &mut cx);
        let member_thread = bare_thread("plan", &mut cx);
        cx.update(|cx| {
            leader.update(cx, |t, _| t.set_id_for_test("lead-t".into()));
            member_thread.update(cx, |t, _| t.set_id_for_test("plan-t".into()));
        });
        let (leader_id, member_id) = ("lead-t".to_string(), "plan-t".to_string());
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        cx.update(|cx| {
            leader.update(cx, |t, cx| t.set_team(team.clone(), cx));
            member_thread.update(cx, |t, cx| t.set_team(team.clone(), cx));
            let store = crate::thread_store::global();
            store.update(cx, |s, _| {
                s.insert_summary_for_test(&leader_id, None);
                s.insert_summary_for_test(&member_id, Some(&leader_id));
            });
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member_thread.clone()),
                    cx,
                )
            })
            .unwrap();
        });

        // No disband: the store cascade alone must tear the team down.
        cx.update(|cx| {
            let store = crate::thread_store::global();
            store.update(cx, |s, cx| s.archive_thread(&leader_id, true, cx));
        });

        cx.update(|cx| {
            assert!(team.read(cx).members().is_empty(), "roster released");
            assert!(
                member_thread.read(cx).team().is_none(),
                "member back-ref cleared"
            );
            assert!(leader.read(cx).team().is_none(), "leader back-ref cleared");
        });
        crate::thread_store::drop_for_test();
        std::fs::remove_file(db_path).ok();
    }

    /// N1 regression: archiving a MEMBER row only clears its back-ref and
    /// archives the row — it must not disband the team or touch the leader.
    #[test]
    fn archive_member_row_does_not_destroy_team() {
        let _store_lock = crate::thread_store::store_test_lock().lock().unwrap();
        let mut cx = TestAppContext::single();
        let db_path =
            std::env::temp_dir().join(format!("team-archmember-{}.db", uuid::Uuid::new_v4()));
        let db = std::sync::Arc::new(
            crate::db::ThreadsDatabase::open(&db_path).expect("open temp threads db"),
        );
        cx.update(|cx| crate::thread_store::init_for_test(db, cx));

        let leader = bare_thread("lead", &mut cx);
        let member_thread = bare_thread("plan", &mut cx);
        let sibling = bare_thread("rev", &mut cx);
        cx.update(|cx| {
            leader.update(cx, |t, _| t.set_id_for_test("lead-m".into()));
            member_thread.update(cx, |t, _| t.set_id_for_test("plan-m".into()));
            sibling.update(cx, |t, _| t.set_id_for_test("rev-m".into()));
        });
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        cx.update(|cx| {
            leader.update(cx, |t, cx| t.set_team(team.clone(), cx));
            member_thread.update(cx, |t, cx| t.set_team(team.clone(), cx));
            sibling.update(cx, |t, cx| t.set_team(team.clone(), cx));
            let store = crate::thread_store::global();
            store.update(cx, |s, _| {
                s.insert_summary_for_test("lead-m", None);
                s.insert_summary_for_test("plan-m", Some("lead-m"));
                s.insert_summary_for_test("rev-m", Some("lead-m"));
            });
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member_thread.clone()),
                    cx,
                )
            })
            .unwrap();
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("rev".into(), "reviewer".into(), sibling.clone()),
                    cx,
                )
            })
            .unwrap();
        });

        cx.update(|cx| {
            let store = crate::thread_store::global();
            store.update(cx, |s, cx| s.archive_thread("plan-m", true, cx));
        });

        cx.update(|cx| {
            // Team intact: leader still leads, sibling untouched.
            assert!(leader.read(cx).team().is_some(), "leader keeps its team");
            assert!(sibling.read(cx).team().is_some(), "sibling back-ref kept");
            assert!(
                team.read(cx).members().contains_key("rev"),
                "sibling remains in roster"
            );
            // Only the archived member lost its back-ref.
            assert!(member_thread.read(cx).team().is_none());
        });
        crate::thread_store::drop_for_test();
        std::fs::remove_file(db_path).ok();
    }

    /// N2: disband (archives members) followed by archiving the leader fires
    /// `SessionEnd` exactly once per thread — the cascade skip guard holds.
    #[test]
    fn session_end_fires_once_across_disband_then_leader_archive() {
        let _store_lock = crate::thread_store::store_test_lock().lock().unwrap();
        let _ = crate::plugin_hooks::drain_fired_for_test();
        let mut cx = TestAppContext::single();
        let db_path = std::env::temp_dir().join(format!("team-once-{}.db", uuid::Uuid::new_v4()));
        let db = std::sync::Arc::new(
            crate::db::ThreadsDatabase::open(&db_path).expect("open temp threads db"),
        );
        cx.update(|cx| crate::thread_store::init_for_test(db, cx));

        let leader = bare_thread("lead", &mut cx);
        let member_thread = bare_thread("plan", &mut cx);
        cx.update(|cx| {
            leader.update(cx, |t, _| t.set_id_for_test("lead-o".into()));
            member_thread.update(cx, |t, _| t.set_id_for_test("plan-o".into()));
        });
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        cx.update(|cx| {
            leader.update(cx, |t, cx| t.set_team(team.clone(), cx));
            member_thread.update(cx, |t, cx| t.set_team(team.clone(), cx));
            let store = crate::thread_store::global();
            store.update(cx, |s, _| {
                s.insert_summary_for_test("lead-o", None);
                s.insert_summary_for_test("plan-o", Some("lead-o"));
            });
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member_thread.clone()),
                    cx,
                )
            })
            .unwrap();
        });

        cx.update(|cx| team.update(cx, |t, cx| t.disband(cx)));
        cx.update(|cx| {
            let store = crate::thread_store::global();
            store.update(cx, |s, cx| s.archive_thread("lead-o", true, cx));
        });

        let fired = crate::plugin_hooks::drain_fired_for_test();
        let ends_for = |id: &str| {
            fired
                .iter()
                .filter(|(ev, payload)| {
                    *ev == crate::plugin_hooks::HookEvent::SessionEnd
                        && payload.contains(&format!("\"{id}\""))
                })
                .count()
        };
        assert_eq!(ends_for("plan-o"), 1, "member SessionEnd exactly once");
        assert_eq!(ends_for("lead-o"), 1, "leader SessionEnd exactly once");
        crate::thread_store::drop_for_test();
        std::fs::remove_file(db_path).ok();
    }

    /// `reported` is set by a member→leader delivery and cleared when the
    /// member starts its next turn.
    #[test]
    fn reported_set_by_report_and_cleared_on_turn_start() {
        let mut cx = TestAppContext::single();
        let leader = bare_thread("lead", &mut cx);
        let team = cx.update(|cx| Team::new("squad".into(), leader.downgrade(), cx));
        let member_thread = bare_thread("plan", &mut cx);
        cx.update(|cx| {
            team.update(cx, |t, cx| {
                t.insert_member(
                    Member::new("plan".into(), "explorer".into(), member_thread),
                    cx,
                )
            })
            .unwrap()
        });

        cx.update(|cx| {
            assert!(!team.read(cx).members()["plan"].reported());
        });
        cx.update(|cx| team.update(cx, |t, cx| t.deliver("plan", LEADER_NAME, "r".into(), cx)))
            .unwrap();
        cx.update(|cx| {
            assert!(team.read(cx).members()["plan"].reported());
        });
        cx.update(|cx| team.update(cx, |t, _| t.member_turn_started("plan")));
        cx.update(|cx| {
            assert!(!team.read(cx).members()["plan"].reported());
        });
    }
}
