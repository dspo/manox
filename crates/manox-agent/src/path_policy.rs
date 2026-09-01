//! FS path policy — reduced to a re-export of the shared sandbox helpers.
//!
//! The read deny-list (`ReadPolicy`), the write-confinement `WritePolicy`,
//! and their `ToolCall` hooks were manox-original hardening outside the
//! deepseek mode vocabulary: reads are ungated in every mode, and bash + the
//! fs write fence carry the file-effect policy. Removed for parity. The two
//! helpers still reached as `crate::path_policy::*` by host callers are
//! re-exported from the extension-layer sandbox module.

pub use manox_harness::sandbox::{canonicalize_best_effort, is_temp_scratch};
