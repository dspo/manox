//! The served method surface, declared once.
//!
//! AHP's Rust side ships types, not a machine-readable method registry (the
//! schema is a type dictionary with no method index), so the method names this
//! host answers live here — and only here. Two properties are enforced by
//! construction:
//!
//! - [`Command::ALL`] is the single list; [`Command::of_method`] is the single
//!   lookup; the router matches on it. A method that is not in the list is
//!   answered `MethodNotFound`, which is also how an unimplemented surface stays
//!   *invisible* to a client instead of looking like an empty feature.
//! - [`Command::intent`] is an exhaustive match with no `_` arm, so a new command
//!   cannot be added without stating whether this build serves it or declines it.
//!
//! Method names are spelled exactly as the upstream protocol spells them
//! (`types/common/messages.ts` in microsoft/agent-host-protocol, the `CommandMap`
//! keys); a mismatch would be invisible to our own tests and fatal to a client,
//! so `tests/command_surface.rs` pins the ones we serve.

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
    /// Our own extension namespace (`x-manox/*`).
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
    /// Every command name this host looks up, paired with the command it names.
    ///
    /// The order is the wire order of `types/common/messages.ts`'s `CommandMap`,
    /// so a diff against upstream is a straight read.
    pub const ALL: &'static [(&'static str, Command)] = &[
        ("initialize", Command::Initialize),
        ("ping", Command::Ping),
        ("reconnect", Command::Reconnect),
        ("subscribe", Command::Subscribe),
        ("unsubscribe", Command::Unsubscribe),
        ("dispatchAction", Command::DispatchAction),
        ("listSessions", Command::ListSessions),
        ("createSession", Command::CreateSession),
        ("disposeSession", Command::DisposeSession),
        ("createChat", Command::CreateChat),
        ("disposeChat", Command::DisposeChat),
        ("fetchTurns", Command::FetchTurns),
        ("resolveSessionConfig", Command::ResolveSessionConfig),
        ("completions", Command::Completions),
        ("resourceRead", Command::ResourceRead),
        ("resourceWrite", Command::ResourceWrite),
        ("resourceList", Command::ResourceList),
        ("resourceDelete", Command::ResourceDelete),
    ];

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
            | Self::FetchTurns
            | Self::ResolveSessionConfig
            | Self::Completions
            | Self::ResourceRead
            | Self::ResourceWrite
            | Self::ResourceList
            | Self::ResourceDelete
            | Self::Extension => CommandIntent::Implemented,
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

    /// The intent table is the served surface; it is the honest answer for a
    /// client. Every listed command is served today, and the router matches on
    /// this same table, so the two cannot disagree.
    #[test]
    fn every_declared_command_is_served() {
        let declined: Vec<&str> = Command::ALL
            .iter()
            .filter(|(_, command)| command.intent() == CommandIntent::Declined)
            .map(|(name, _)| *name)
            .collect();
        assert!(
            declined.is_empty(),
            "the router serves the names in Command::ALL, so a Declined entry \
             would be routed anyway: {declined:?}"
        );
    }
}
