//! The mapping doc's `x-manox*` names stay honest against the declared surface.
//!
//! `docs/ahp-mapping.md` narrates where v2 vocabulary landed on AHP. The
//! extension names it cites must exist on the wire — [`manox_ahp::ext`]'s
//! `actions::ALL` / `commands::ALL` / `requests::ALL` — or sit on a line
//! carrying the `not-on-wire` marker (v2-era proposals and removed rows are
//! narrated on purpose, but must never read as servable). A rename or a
//! removal that leaves the doc quietly promising a dead name fails here.

use manox_ahp::ext;

/// Names a reader could mistake for servable surface, gathered from the doc.
fn declared() -> Vec<&'static str> {
    [ext::actions::ALL, ext::commands::ALL, ext::requests::ALL].concat()
}

/// Every `x-manox…/name` citation in one doc line: `x-manox` plus optional
/// `-segment` groups, then `/` plus the action name. Channel URIs
/// (`x-manox-plan:/…`, `x-manox-workspaces://`) and prose (`x-manox/*`,
/// `x-manox-*`) do not match — the character after the prefix family is never
/// `/` + a word character.
fn extract_names(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut names = Vec::new();
    let mut i = 0;
    while let Some(at) = line[i..].find("x-manox") {
        let name_start = i + at;
        let mut cursor = name_start + "x-manox".len();
        loop {
            let rest = &bytes[cursor..];
            if rest.first() == Some(&b'-') && rest.get(1).is_some_and(|b| b.is_ascii_lowercase()) {
                cursor += 1;
                while bytes.get(cursor).is_some_and(|b| b.is_ascii_lowercase()) {
                    cursor += 1;
                }
            } else {
                break;
            }
        }
        if bytes.get(cursor) == Some(&b'/') {
            let start = cursor;
            cursor += 1;
            while bytes.get(cursor).is_some_and(|b| b.is_ascii_alphanumeric()) {
                cursor += 1;
            }
            if cursor > start + 1 {
                names.push(line[name_start..cursor].to_string());
            }
        }
        i = cursor.max(i + at + 1);
    }
    names
}

#[test]
fn the_mapping_doc_cites_only_declared_extension_names() {
    let declared = declared();
    let doc = include_str!("../../../docs/ahp-mapping.md");
    let mut extracted = 0;
    let mut violations = Vec::new();
    for (index, line) in doc.lines().enumerate() {
        for name in extract_names(line) {
            extracted += 1;
            if !declared.contains(&name.as_str()) && !line.contains("not-on-wire") {
                violations.push(format!("  line {}: {name}\n    {line}", index + 1));
            }
        }
    }
    assert!(
        extracted >= 15,
        "the extractor went blind: only {extracted} citations found in the doc"
    );
    assert!(
        violations.is_empty(),
        "docs/ahp-mapping.md cites names that are not on the declared surface \
         (declare them, fix the name, or mark the line `not-on-wire`):\n{}",
        violations.join("\n")
    );
}
