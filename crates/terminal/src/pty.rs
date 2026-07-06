//! PTY bridge — `portable-pty` wrapper.
//!
//! Stage 1 implements `spawn` (openpty + `CommandBuilder` + slave spawn), the
//! reader thread (a dedicated `std::thread` doing blocking `master.read` into
//! an `async_channel`), `resize`, and child-exit detection. The reader thread
//! never touches `Term`; it only forwards bytes to the gpui side, which feeds
//! them to `Processor::advance(&mut term, ..)` under the `FairMutex` lock.
