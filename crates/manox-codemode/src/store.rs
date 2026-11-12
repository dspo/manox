//! The `store`/`load` state machine's host half.
//!
//! The prelude works on a snapshot of JSON text (limits enforced in the VM,
//! where violations become catchable `RangeError`s) and reports the keys it
//! touched with a successful `done`. This module holds the snapshot type and
//! decodes the reported entries back into [`StoreWrites`]. Persistence is the
//! host's business (MR2 journals them); the crate never touches the disk.
//!
//! A failed execution never produces writes: the host only decodes entries
//! from a successful `done`.

use std::collections::BTreeMap;

use crate::types::StoreWrites;

/// Initial store snapshot passed into an execution: key -> serialized JSON.
pub type StoreSnapshot = BTreeMap<String, String>;

/// Decode the prelude's store-writes payload: an array of `["key"]` (delete)
/// and `["key", "json"]` (set) entries. Values stay serialized: the snapshot
/// is JSON text end to end, so nothing round-trips through a Rust value.
///
/// Malformed payloads mean the VM's `JSON.stringify` was tampered with; the
/// caller turns this into a sandbox error rather than trusting the traffic.
pub fn parse_writes(payload: &str) -> Result<StoreWrites, String> {
    let parsed: serde_json::Value = serde_json::from_str(payload)
        .map_err(|error| format!("store writes are not valid JSON: {error}"))?;
    let serde_json::Value::Array(entries) = parsed else {
        return Err("store writes are not an array".into());
    };
    let mut writes = StoreWrites::default();
    for entry in entries {
        let serde_json::Value::Array(parts) = entry else {
            return Err("store writes contain a malformed entry".into());
        };
        match parts.as_slice() {
            [serde_json::Value::String(key)] => writes.delete.push(key.clone()),
            // The second element is the value's JSON text, not a JSON value:
            // writes travel as serialized text end to end.
            [
                serde_json::Value::String(key),
                serde_json::Value::String(json),
            ] => {
                writes.set.push((key.clone(), json.clone()));
            }
            _ => return Err("store writes contain a malformed entry".into()),
        }
    }
    Ok(writes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_sets_and_deletes() {
        let writes = parse_writes(r#"[["a","1"],["b"],["c","{\"x\":null}"]]"#).expect("parses");
        assert_eq!(
            writes.set,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("c".to_owned(), "{\"x\":null}".to_owned())
            ]
        );
        assert_eq!(writes.delete, vec!["b".to_owned()]);
        // A non-string second element is malformed: values travel as JSON text.
        assert!(parse_writes(r#"[["a",1]]"#).is_err());
    }

    #[test]
    fn rejects_malformed_payloads() {
        assert!(parse_writes("not json").is_err());
        assert!(parse_writes("{}").is_err());
        assert!(parse_writes(r#"[[1,"x"]]"#).is_err());
        assert!(parse_writes(r#"[["a","1","extra"]]"#).is_err());
        assert!(parse_writes(r#"[[]]"#).is_err());
    }

    #[test]
    fn empty_array_is_no_writes() {
        let writes = parse_writes("[]").expect("parses");
        assert_eq!(writes, StoreWrites::default());
    }
}
