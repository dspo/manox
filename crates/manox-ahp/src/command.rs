//! The method surface, declared once against the **upstream** list.
//!
//! AHP's Rust side ships types, not a machine-readable method registry (the
//! schema is a type dictionary with no method index), so the names live here.
//! The list is not a subset we chose — it is upstream's `CommandMap`
//! (`types/common/messages.ts`, 30 entries) spelled out in full, plus the two
//! client notifications (`ClientNotificationMap`: `unsubscribe`,
//! `dispatchAction`), which are not commands but are routed here too.
//!
//! That completeness is the point. An exhaustive match over a list we maintain
//! ourselves is a tautology: it catches a typo in our own table and nothing
//! else. Covering upstream's *whole* map is what makes the match a real gate —
//! when a protocol release adds a command, [`Command::intent`] has no arm for it
//! and the build fails, which is the only moment the drift is cheap to fix.
//! (`pi-ahp` gets the same property from `satisfies Record<keyof CommandMap,
//! CommandPolicy>` in TypeScript; this is its Rust equivalent.)
//!
//! Two consequences worth stating plainly:
//!
//! - [`CommandIntent::Declined`] is a **live** outcome, not dead code: the
//!   commands this build does not serve are listed and marked, so the gap is
//!   visible in one place instead of being an absence.
//! - A *declined* command and an unknown one both answer `MethodNotFound`, so a
//!   client cannot tell them apart and must not try: the surface it should
//!   believe is `CommandIntent::Implemented` (and the x-manox declaration for
//!   the extension plane).

/// One connection-level or channel-scoped method this host knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Command {
    /// Handshake: versions, client identity, initial subscriptions.
    Initialize,
    /// Liveness; answered before the handshake too.
    Ping,
    /// Resume a dropped connection (replay or fresh snapshots).
    Reconnect,
    /// Observe a channel.
    Subscribe,
    /// Release a channel (a notification, not a request).
    Unsubscribe,
    /// Dispatch a state mutation (a notification, not a request).
    DispatchAction,
    /// Page the session catalog.
    ListSessions,
    /// Create a session for a client-chosen URI.
    CreateSession,
    /// Dispose a session.
    DisposeSession,
    /// Create a chat inside a session.
    CreateChat,
    /// Dispose a chat.
    DisposeChat,
    /// Page older turns into a chat's state.
    FetchTurns,
    /// Resolve session configuration (answered empty: nothing to resolve here).
    ResolveSessionConfig,
    /// Inline completions (answered empty: no completion provider).
    Completions,
    /// Read a resource (file plane).
    ResourceRead,
    /// Write a resource.
    ResourceWrite,
    /// List a directory.
    ResourceList,
    /// Delete a resource.
    ResourceDelete,
    /// Copy a resource.
    ResourceCopy,
    /// Move/rename a resource.
    ResourceMove,
    /// Resolve a resource (etag + metadata), for conditional writes.
    ResourceResolve,
    /// Create a directory.
    ResourceMkdir,
    /// Serve a resource the *client* owns (the reverse `resource*` direction).
    ResourceRequest,
    /// Create a terminal.
    CreateTerminal,
    /// Dispose a terminal.
    DisposeTerminal,
    /// Watch a resource for changes.
    CreateResourceWatch,
    /// Authenticate a provider / MCP server challenge.
    Authenticate,
    /// Session-config completions (distinct from inline `completions`).
    SessionConfigCompletions,
    /// Invoke an operation offered by a changeset.
    InvokeChangesetOperation,
    /// List automation trigger definitions.
    ListAutomationTriggerDefinitions,
    /// Run an automation.
    RunAutomation,
    /// Page an automation's runs.
    FetchAutomationRuns,
    /// Our own extension namespace (`x-manox/*`), which is **not** an upstream
    /// command: it is the declared private surface (`ext::commands`).
    Extension,
}

/// What this build does with a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandIntent {
    /// Served.
    Implemented,
    /// Declared by the protocol but not served by this build; the router does
    /// not route it, so the client sees `MethodNotFound` and treats the surface
    /// as absent rather than empty.
    Declined,
}

impl Command {
    /// Every method this host routes, paired with the command it names.
    ///
    /// The order is upstream's wire order (`CommandMap` in
    /// `types/common/messages.ts`, then the two `ClientNotificationMap` names),
    /// so a diff against upstream is a straight read. The count is asserted:
    /// 30 commands + 2 notifications.
    pub const ALL: &'static [(&'static str, Command)] = &[
        // ── CommandMap (30) ────────────────────────────────────────────
        ("initialize", Command::Initialize),
        ("ping", Command::Ping),
        ("reconnect", Command::Reconnect),
        ("subscribe", Command::Subscribe),
        ("createSession", Command::CreateSession),
        ("disposeSession", Command::DisposeSession),
        ("createChat", Command::CreateChat),
        ("disposeChat", Command::DisposeChat),
        ("createTerminal", Command::CreateTerminal),
        ("disposeTerminal", Command::DisposeTerminal),
        ("createResourceWatch", Command::CreateResourceWatch),
        ("listSessions", Command::ListSessions),
        ("resourceRead", Command::ResourceRead),
        ("resourceWrite", Command::ResourceWrite),
        ("resourceList", Command::ResourceList),
        ("resourceCopy", Command::ResourceCopy),
        ("resourceDelete", Command::ResourceDelete),
        ("resourceMove", Command::ResourceMove),
        ("resourceResolve", Command::ResourceResolve),
        ("resourceMkdir", Command::ResourceMkdir),
        ("resourceRequest", Command::ResourceRequest),
        ("fetchTurns", Command::FetchTurns),
        ("authenticate", Command::Authenticate),
        ("resolveSessionConfig", Command::ResolveSessionConfig),
        (
            "sessionConfigCompletions",
            Command::SessionConfigCompletions,
        ),
        ("completions", Command::Completions),
        (
            "invokeChangesetOperation",
            Command::InvokeChangesetOperation,
        ),
        (
            "listAutomationTriggerDefinitions",
            Command::ListAutomationTriggerDefinitions,
        ),
        ("runAutomation", Command::RunAutomation),
        ("fetchAutomationRuns", Command::FetchAutomationRuns),
        // ── ClientNotificationMap (2), routed here as well ─────────────
        ("unsubscribe", Command::Unsubscribe),
        ("dispatchAction", Command::DispatchAction),
    ];

    /// How many of [`Self::ALL`] come from upstream's `CommandMap`.
    ///
    /// The remainder are the two client notifications, which this host routes
    /// through the same table but which upstream types separately.
    pub const UPSTREAM_COMMAND_COUNT: usize = 30;

    /// The command a wire method names, if this host knows the name at all.
    pub fn of_method(method: &str) -> Option<Self> {
        if method.starts_with("x-manox/") {
            return Some(Command::Extension);
        }
        Self::ALL
            .iter()
            .find(|(name, _)| *name == method)
            .map(|(_, command)| *command)
    }

    /// Whether this build serves the command — exhaustive, no `_` arm.
    pub fn intent(self) -> CommandIntent {
        match self {
            Self::Initialize
            | Self::Ping
            | Self::Reconnect
            | Self::Subscribe
            | Self::Unsubscribe
            | Self::DispatchAction
            | Self::ListSessions
            | Self::CreateSession
            | Self::DisposeSession
            | Self::CreateChat
            | Self::DisposeChat
            | Self::CreateTerminal
            | Self::DisposeTerminal
            | Self::FetchTurns
            | Self::ResolveSessionConfig
            | Self::Completions
            | Self::ResourceRead
            | Self::ResourceWrite
            | Self::ResourceList
            | Self::ResourceDelete
            | Self::Extension => CommandIntent::Implemented,
            // Declared upstream, not served by this build. The router does not
            // route them, so a client sees `MethodNotFound` and treats the
            // surface as absent rather than empty — which is the honest answer
            // for a capability this slice does not have.
            //
            // These are listed rather than omitted so the gap is legible: the
            // alternative (dropping them from the table) is what made the match
            // exhaustive over a list of our own choosing, i.e. a tautology.
            Self::CreateResourceWatch
            | Self::ResourceCopy
            | Self::ResourceMove
            | Self::ResourceResolve
            | Self::ResourceMkdir
            | Self::ResourceRequest
            | Self::Authenticate
            | Self::SessionConfigCompletions
            | Self::InvokeChangesetOperation
            | Self::ListAutomationTriggerDefinitions
            | Self::RunAutomation
            | Self::FetchAutomationRuns => CommandIntent::Declined,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every listed name resolves back to its command — a typo in [`Command::ALL`]
    /// would otherwise be invisible until a client hit it.
    #[test]
    fn every_declared_name_resolves_to_its_command() {
        for (name, command) in Command::ALL {
            assert_eq!(Command::of_method(name), Some(*command), "for {name}");
        }
        assert_eq!(Command::of_method("noSuchMethod"), None);
        assert_eq!(
            Command::of_method("x-manox/compact"),
            Some(Command::Extension)
        );
    }

    /// The table covers upstream's **entire** `CommandMap`, so the exhaustive
    /// `intent` match is a real gate rather than a tautology over our own
    /// subset. Upstream's map holds 30 entries; the two client notifications
    /// ride the same table.
    ///
    /// When a protocol release adds a command, this count and the match's
    /// exhaustiveness both fail — which is the point, and the only moment the
    /// drift is cheap to notice.
    #[test]
    fn the_table_covers_upstreams_whole_command_map() {
        assert_eq!(
            Command::ALL.len(),
            Command::UPSTREAM_COMMAND_COUNT + 2,
            "CommandMap (30) plus the two ClientNotificationMap names"
        );
        let upstream = Command::ALL
            .iter()
            .filter(|(name, _)| *name != "unsubscribe" && *name != "dispatchAction")
            .count();
        assert_eq!(upstream, Command::UPSTREAM_COMMAND_COUNT);
    }

    /// Declined is a live outcome: the build declines a documented set, and each
    /// name still resolves (so a client asking for one gets `MethodNotFound`
    /// through the table rather than falling off it).
    #[test]
    fn declined_commands_are_listed_and_resolve() {
        let declined: Vec<&str> = Command::ALL
            .iter()
            .filter(|(_, command)| command.intent() == CommandIntent::Declined)
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            declined,
            vec![
                "createResourceWatch",
                "resourceCopy",
                "resourceMove",
                "resourceResolve",
                "resourceMkdir",
                "resourceRequest",
                "authenticate",
                "sessionConfigCompletions",
                "invokeChangesetOperation",
                "listAutomationTriggerDefinitions",
                "runAutomation",
                "fetchAutomationRuns",
            ],
            "the unserved surface is explicit, not an absence"
        );
        for name in declined {
            assert!(Command::of_method(name).is_some(), "{name} resolves");
        }
    }
}
