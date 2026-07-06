//! `ManoxListener` — bridges alacritty's `EventListener` to `TerminalEvent`.
//!
//! Stage 1 implements `send_event` to map each `alacritty_terminal::event::Event`
//! variant onto a `TerminalEvent`, forwarded to the gpui Entity over an
//! `async_channel`. `ManoxListener: Send` (the channel sender is `Send+Sync`),
//! so `Term<ManoxListener>: Send` and may live behind `FairMutex`.
