//! The x-manox extension surface — AHP's `x-` namespace, used for the domain
//! state the protocol does not model.
//!
//! AHP deliberately stays agent-agnostic: plan mode and plan artefacts, goal,
//! compaction, background work, browser suites, the sub-agent tree, thread
//! pinning/ordering/grouping, the workspace rows, the command catalogue, the
//! aggregated conversation metrics and client-contributed tools have **no**
//! protocol counterpart. They travel as:
//!
//! - extension channels (`x-manox-plan:/<chat-id>`, …) carrying state, and
//! - extension actions/commands/requests named `x-manox/*`, and
//! - `_meta` keys on standard channels where the state already has a slot.
//!
//! [`declaration`] is the single declaration table for all of it, advertised
//! once per connection as `initialize` result `_meta["x-manox"]`, so a client
//! (or a proxy) can tell what this host serves. Unknown extension names are
//! ignored by peers — a third-party AHP host simply serves none of this, and
//! the manox client degrades to the standard surface.

use serde_json::{Value, json};

pub mod reducer;

pub use reducer::{Outcome as ExtOutcome, XManoxState};

/// `_meta` key carrying the declaration on both `initialize` params and result.
pub const META_KEY: &str = "x-manox";

/// Extension surface version. Bump when an extension is renamed or removed.
///
/// 2: the v2 gateway's deletion took `x-manox-modelchat:/` and the
/// `fetchEntries`/`modelChat`/`modelChatCancel`/`cancelDelivery`/`shutdown`
/// commands off the surface. A client that caches version 1's declaration would
/// otherwise keep offering names this build answers `unsupported` to.
pub const VERSION: u32 = 2;

/// State-bearing extension channel prefixes, in declaration order.
pub mod channels {
    /// Plan mode, plan document, plan review lifecycle (per chat).
    pub const PLAN: &str = "x-manox-plan:/";
    /// Goal, background tasks, browser suites, sub-agent tree (per session).
    pub const WORK: &str = "x-manox-work:/";
    /// Aggregated conversation metrics — the v2 Q face (per chat).
    pub const METRICS: &str = "x-manox-metrics:/";
    /// Durable workspace rows: directory identity plus ordered session account.
    pub const WORKSPACES: &str = "x-manox-workspaces://";
    /// Command / skill catalogue (stateless snapshot channel).
    pub const COMMANDS: &str = "x-manox-commands://";

    /// Every declared channel prefix — and, like [`super::commands::ALL`], only
    /// the ones this build actually serves.
    ///
    /// The v2 surface also carried a bare-model completion side stream
    /// (`x-manox-modelchat:/`). It is **not** declared: the module that produced
    /// it was deleted with the v2 gateway, so a subscriber would wait forever on
    /// a channel that can only ever answer `null`. Re-declaring it is the right
    /// move the moment a model-completion implementation exists to back it.
    pub const ALL: &[&str] = &[PLAN, WORK, METRICS, WORKSPACES, COMMANDS];
}

/// Actions served by the extension channels.
///
/// A declaration row exists the moment the host *emits* the action; rows are
/// grouped by channel in the declaration payload.
pub mod actions {
    /// Plan mode toggled (client-dispatchable).
    pub const PLAN_MODE_CHANGED: &str = "x-manox-plan/planModeChanged";
    /// The plan document changed (host-emitted).
    pub const PLAN_CHANGED: &str = "x-manox-plan/planChanged";
    /// A plan review is due, carrying the verdict request (host-emitted).
    pub const PLAN_VERDICT_REQUESTED: &str = "x-manox-plan/verdictRequested";
    /// A plan verdict settled (client-dispatchable).
    pub const PLAN_VERDICT: &str = "x-manox-plan/verdict";
    /// Goal set/cleared (host-emitted; clients set it through `x-manox/goal`).
    pub const WORK_GOAL_CHANGED: &str = "x-manox-work/goalChanged";
    /// Background-task registry snapshot (host-emitted).
    pub const WORK_BACKGROUND_TASKS: &str = "x-manox-work/backgroundTasksChanged";
    /// Background task stopped (client-dispatchable).
    pub const WORK_BACKGROUND_TASK_STOPPED: &str = "x-manox-work/backgroundTaskStopped";
    /// Active browser suites (host-emitted; clients set through `session/configChanged`).
    pub const WORK_BROWSER_SUITES: &str = "x-manox-work/browserSuitesChanged";
    /// Sub-agent tree / progress (host-emitted).
    pub const WORK_SUBAGENTS: &str = "x-manox-work/subagentsChanged";
    /// The engine's active tool set (host-emitted; AHP's `session/serverToolsChanged`
    /// describes *advertised* tools, not the model's currently-visible subset).
    pub const WORK_ACTIVE_TOOLS: &str = "x-manox-work/activeToolsChanged";
    /// Aggregated metrics snapshot (host-emitted).
    pub const METRICS_CHANGED: &str = "x-manox-metrics/changed";
    /// Workspace catalogue baseline (host-emitted).
    pub const WORKSPACES_BASELINE: &str = "x-manox-workspaces/baseline";
    /// One workspace row upserted (host-emitted).
    pub const WORKSPACES_CHANGED: &str = "x-manox-workspaces/changed";
    /// A workspace row removed (host-emitted).
    pub const WORKSPACES_REMOVED: &str = "x-manox-workspaces/removed";
    /// Thread/workspace display order changed (client-dispatchable).
    pub const ORDER_CHANGED: &str = "x-manox/orderChanged";
    /// Thread pinned flag changed (client-dispatchable; AHP has no pin bit).
    pub const PINNED_CHANGED: &str = "x-manox/pinnedChanged";
    /// A label row attached to an entry (host-emitted; the durable row carries
    /// no target id, so it is announced as session state, not per-entry state).
    pub const LABEL_CHANGED: &str = "x-manox/labelChanged";
    /// Session-info annotation row (host-emitted; AHP has no session-info field).
    pub const SESSION_INFO_CHANGED: &str = "x-manox/sessionInfoChanged";
    /// Leaf cursor redirected (host-emitted; the journal's branch cursor has no
    /// AHP field — forking a chat is the protocol-visible half of the same fact).
    pub const LEAF_CHANGED: &str = "x-manox/leafChanged";

    /// Every declared action.
    pub const ALL: &[&str] = &[
        PLAN_MODE_CHANGED,
        PLAN_CHANGED,
        PLAN_VERDICT_REQUESTED,
        PLAN_VERDICT,
        WORK_GOAL_CHANGED,
        WORK_BACKGROUND_TASKS,
        WORK_BACKGROUND_TASK_STOPPED,
        WORK_BROWSER_SUITES,
        WORK_SUBAGENTS,
        WORK_ACTIVE_TOOLS,
        METRICS_CHANGED,
        WORKSPACES_BASELINE,
        WORKSPACES_CHANGED,
        WORKSPACES_REMOVED,
        ORDER_CHANGED,
        PINNED_CHANGED,
        LABEL_CHANGED,
        SESSION_INFO_CHANGED,
        LEAF_CHANGED,
    ];
}

/// Extension commands (connection-level unless the declaration says otherwise;
/// every params object still carries `channel`).
pub mod commands {
    /// Compact older history into a summary (journal rewrite).
    pub const COMPACT: &str = "x-manox/compact";
    /// Seed plan execution after a verdict.
    pub const PLAN_EXECUTE: &str = "x-manox/planExecute";
    /// Set or clear the session goal.
    pub const GOAL: &str = "x-manox/goal";

    /// Every command this build **serves**, which is what [`declaration`]
    /// advertises.
    ///
    /// The rule is the same one the whole extension surface follows: a name in
    /// here is a promise, and a promise the runtime does not keep is worse than
    /// a name that was never offered — a client that reads the declaration and
    /// sends `x-manox/modelChat` has no way to tell "not built" from "broken".
    /// So a capability is listed here only once something answers for it.
    ///
    /// Client-contributed tools are **not** here: AHP models them natively (see
    /// [`CLIENT_TOOLS_VIA_ACTION`]), so the v2 `RegisterSessionTools` capability
    /// has no `x-manox` command.
    ///
    /// Capabilities the v2 surface had and this face does **not** serve, none of
    /// which is declared above:
    ///
    /// - history paging — AHP's own `fetchTurns` covers it, cursor and all;
    /// - bare-model completions (`modelChat` / `modelChatCancel`) — the channel
    ///   they fed was deleted with the v2 gateway, so there is nothing to call;
    /// - adjudication retraction (`cancelDelivery`) — deliveries are the deleted
    ///   v2 waterfall's concept; AHP settles through channel actions;
    /// - process shutdown — a host-lifecycle concern, not a session one.
    pub const ALL: &[&str] = &[COMPACT, PLAN_EXECUTE, GOAL];
}

/// Client-contributed session tools: an AHP-native action, not an extension
/// command.
///
/// It lands on the **standard** surface rather than under `x-manox` because
/// AHP already models the whole lifecycle:
///
/// - registration is `session/activeClientSet` carrying the client's own
///   [`SessionActiveClient`](ahp_types::state::SessionActiveClient), whose
///   `tools` field *is* the contribution — ordinary session state that every
///   subscriber folds through the upstream reducer and reads at
///   `SessionState.activeClients[].tools`;
/// - invocation is `SessionInputRequestKind::ToolClientExecution`, a running
///   client-contributed tool call completed by `chat/toolCallComplete`.
///
/// A separate `x-manox/registerSessionTools` command therefore does not exist.
/// Serving both would be two sources of truth for one fact — the extension
/// command's payload would have to be re-derived into `activeClients` anyway —
/// and the extension command carried no wire contract this action lacks. The
/// name is absent from [`commands::ALL`] rather than kept as a dead
/// declaration: this host's rule is that `_meta["x-manox"]` advertises only
/// what it serves.
pub const CLIENT_TOOLS_VIA_ACTION: &str = "session/activeClientSet";

/// Host → client requests (AHP permits host-initiated requests; the `resource*`
/// family is the standard precedent). Routed by client capability, fail-closed.
pub mod requests {
    /// Drive the embedded browser.
    pub const BROWSER_OP: &str = "x-manox/browserOp";
    /// Read the client clipboard.
    pub const CLIPBOARD_READ: &str = "x-manox/clipboardRead";
    /// Open a path/URL in the client's default handler.
    pub const OPEN_EXTERNAL: &str = "x-manox/openExternal";
    /// Invoke a tool the client contributed to the session.
    pub const INVOKE_TOOL: &str = "x-manox/invokeTool";

    /// Every declared host → client request.
    pub const ALL: &[&str] = &[BROWSER_OP, CLIPBOARD_READ, OPEN_EXTERNAL, INVOKE_TOOL];
}

/// The subset of the acceptance table that is *not* extension-namespaced: the
/// AHP actions this host accepts from clients and turns into runtime work.
///
/// Read-only against the spec: an action absent here is refused with
/// [`crate::codes::X_MANOX_ACTION_REJECTED`] rather than silently dropped, so a
/// client always learns whether its write landed.
pub const ACCEPTED_ACTIONS: &[&str] = &[
    // chat
    "chat/turnStarted",
    "chat/pendingMessageSet",
    "chat/pendingMessageRemoved",
    "chat/queuedMessagesReordered",
    "chat/turnCancelled",
    "chat/turnResume",
    "chat/truncated",
    "chat/draftChanged",
    "chat/toolCallConfirmed",
    "chat/toolCallComplete",
    "chat/toolCallResultConfirmed",
    "chat/inputAnswerChanged",
    "chat/inputCompleted",
    // session
    "session/isReadChanged",
    "session/isArchivedChanged",
    "session/titleChanged",
    "session/configChanged",
    "session/workingDirectorySet",
    "session/workingDirectoryRemoved",
    "session/workingDirectoryReplaced",
    "session/activeClientSet",
    "session/activeClientRemoved",
    // chat working directories
    "chat/workingDirectorySet",
    "chat/workingDirectoryRemoved",
    // terminal
    "terminal/input",
    "terminal/resized",
    "terminal/claimed",
    "terminal/cleared",
];

/// The notification method carrying an extension channel's baseline state.
pub const BASELINE_NOTIFICATION: &str = "x-manox/baseline";

/// Whether `uri` is a **per-session** extension channel (`…:/<session-id>`)
/// rather than a connection-level catalogue.
///
/// The distinction is load-bearing: a per-session channel's baseline is that
/// session's journal fold, while the catalogue channels
/// ([`channels::WORKSPACES`], [`channels::COMMANDS`]) describe the whole host
/// and take no session id at all. A reader that treated them alike would look
/// for a session named after a workspace prefix and find nothing.
pub fn is_session_scoped_channel(uri: &str) -> bool {
    channels::ALL
        .iter()
        .filter(|prefix| prefix.ends_with(":/"))
        .any(|prefix| uri.starts_with(prefix))
}

/// Whether `uri` names one of the declared extension channels.
///
/// The set is the declaration's own list, so a channel advertised in
/// `_meta["x-manox"]` and a channel the host will answer for cannot drift apart.
pub fn is_extension_channel(uri: &str) -> bool {
    channels::ALL.iter().any(|prefix| uri.starts_with(prefix))
}

/// Whether an action tag is on the extension surface rather than AHP's own.
///
/// AHP's `x-` prefix is the reserved private namespace and no AHP reducer knows
/// these actions, so a host that "failed to fold" one would only be logging its
/// own bug. Peers treat them as unknown-tolerant (§D.3).
pub fn is_extension_action(tag: &str) -> bool {
    tag.starts_with("x-")
}

/// Whether the host accepts `action_type` dispatched on `channel_uri`.
///
/// Channel scoping is part of the check: a chat action on a session channel is
/// a client bug, and refusing it keeps the acceptance table honest instead of
/// letting reducers silently no-op.
pub fn accepts_action(channel_uri: &str, action_type: &str) -> bool {
    if action_type.starts_with("x-manox") {
        return actions::ALL.contains(&action_type);
    }
    if !ACCEPTED_ACTIONS.contains(&action_type) {
        return false;
    }
    let scope = action_type.split('/').next().unwrap_or_default();
    match scope {
        "chat" => channel_uri.starts_with("ahp-chat:/"),
        "session" => channel_uri.starts_with("ahp-session:/"),
        "terminal" => channel_uri.starts_with("ahp-terminal:/"),
        _ => false,
    }
}

/// The declaration payload advertised as `_meta["x-manox"]`.
pub fn declaration_meta() -> ahp_types::common::JsonObject {
    let mut meta = ahp_types::common::JsonObject::new();
    meta.insert(META_KEY.to_string(), declaration());
    meta
}

pub fn declaration() -> Value {
    json!({
        "version": VERSION,
        "channels": channels::ALL,
        "actions": actions::ALL,
        "acceptedActions": ACCEPTED_ACTIONS,
        "commands": commands::ALL,
        "serverRequests": requests::ALL,
        // How a subscriber receives an extension channel's state: these channels
        // are not state-bearing, so `subscribe` answers with this notification
        // rather than a snapshot. Declared because a client cannot guess the
        // method name, and a channel it cannot read is a channel not served.
        "baselineNotification": BASELINE_NOTIFICATION,
        "capabilities": {
            // Sessions are journals of branchable chats; the active-session
            // pointer is `SessionState.defaultChat`.
            "multipleChats": { "fork": true, "sideChat": true },
            // Granted working directories are equal peers (the kernel fences
            // tool access per granted root); no protected primary.
            "multipleWorkingDirectories": { "immutablePrimary": false },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_names_are_namespaced() {
        for name in actions::ALL
            .iter()
            .chain(commands::ALL)
            .chain(requests::ALL)
        {
            assert!(
                name.starts_with("x-manox"),
                "extension name outside the reserved prefix: {name}"
            );
        }
        for channel in channels::ALL {
            assert!(channel.starts_with("x-manox"));
        }
    }

    #[test]
    fn acceptance_table_is_channel_scoped() {
        assert!(accepts_action(
            "ahp-chat:/c-1",
            "chat/pendingMessageRemoved"
        ));
        assert!(!accepts_action(
            "ahp-session:/s-1",
            "chat/pendingMessageRemoved"
        ));
        assert!(!accepts_action("ahp-chat:/c-1", "chat/unknownFutureAction"));
        assert!(accepts_action(
            "x-manox-plan:/c-1",
            actions::PLAN_MODE_CHANGED
        ));
    }

    #[test]
    fn declaration_lists_every_surface() {
        let value = declaration();
        assert_eq!(value["version"], VERSION);
        assert_eq!(
            value["channels"].as_array().unwrap().len(),
            channels::ALL.len()
        );
        assert_eq!(
            value["commands"].as_array().unwrap().len(),
            commands::ALL.len()
        );
    }
}
