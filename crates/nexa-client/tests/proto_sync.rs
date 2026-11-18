//! `proto/nexa_client.proto` (what the Swift and C# apps generate their types from) and
//! `src/proto.rs` (the core's prost types) are kept in step by hand. They drifted
//! twice (the iOS app lacked SetQuality, SetResolution and `request`), so this checks
//! that every message has the same field numbers on both sides, oneof members included.

use std::collections::{BTreeMap, BTreeSet};

type Fields = BTreeMap<String, BTreeSet<u32>>;

/// The body of each `<keyword> Name { … }` at brace depth 0, by name.
fn blocks<'a>(text: &'a str, keyword: &str) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    let mut rest = text;
    let key = format!("{keyword} ");
    while let Some(at) = rest.find(&key) {
        let after = &rest[at + key.len()..];
        let Some(open) = after.find('{') else { break };
        let name = after[..open].trim();
        let mut depth = 0usize;
        let mut end = open;
        for (i, c) in after[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        if name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.is_empty() {
            out.push((name, &after[open + 1..end]));
        }
        rest = &after[end..];
    }
    out
}

fn proto_messages(text: &str) -> Fields {
    // Without comments, which hold "= 1" in prose.
    let text: String = text
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    blocks(&text, "message")
        .into_iter()
        .map(|(name, body)| {
            let numbers = body
                .split(';')
                .filter_map(|field| {
                    let field = field.trim();
                    let (_, n) = field.rsplit_once('=')?;
                    if field.starts_with("reserved") || field.starts_with("option") {
                        return None;
                    }
                    n.trim().parse().ok()
                })
                .collect();
            (name.to_owned(), numbers)
        })
        .collect()
}

/// `enum Name { A = 0; B = 1; }`: the values, by name.
fn proto_enums(text: &str) -> Fields {
    let text: String = text
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    blocks(&text, "enum")
        .into_iter()
        .map(|(name, body)| {
            let values = body
                .split(';')
                .filter_map(|v| v.rsplit_once('=')?.1.trim().parse().ok())
                .collect();
            (name.to_owned(), values)
        })
        .collect()
}

/// `#[derive(…Enumeration)] pub enum Name { A = 0, … }`: the values, by name.
fn rust_enums(text: &str) -> Fields {
    blocks(text, "pub enum")
        .into_iter()
        .filter(|(name, _)| {
            text.find(&format!("pub enum {name} "))
                .is_some_and(|at| text[at.saturating_sub(120)..at].contains("Enumeration)]"))
        })
        .map(|(name, body)| {
            let values = body
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.starts_with("//"))
                .filter_map(|l| {
                    l.trim_end_matches(',')
                        .rsplit_once('=')?
                        .1
                        .trim()
                        .parse()
                        .ok()
                })
                .collect();
            (name.to_owned(), values)
        })
        .collect()
}

fn rust_messages(text: &str) -> Fields {
    // Oneof enums: their variants' tags, by enum name.
    let oneofs: BTreeMap<&str, BTreeSet<u32>> = blocks(text, "pub enum")
        .into_iter()
        .map(|(name, body)| (name, tags(body, "tag = \"")))
        .collect();
    blocks(text, "pub struct")
        .into_iter()
        .filter(|(name, _)| {
            text.contains(&format!(
                "#[derive(Clone, PartialEq, Message)]\npub struct {name} "
            ))
        })
        .map(|(name, body)| {
            let mut numbers = tags(body, "tag = \"");
            for line in body.lines() {
                if let Some(at) = line.find("oneof = \"") {
                    let enum_name = line[at + 9..].split('"').next().unwrap_or("");
                    numbers.extend(oneofs.get(enum_name).cloned().unwrap_or_default());
                }
            }
            (name.to_owned(), numbers)
        })
        .collect()
}

/// Numbers after each `marker` up to the closing quote.
fn tags(body: &str, marker: &str) -> BTreeSet<u32> {
    body.match_indices(marker)
        .filter_map(|(at, _)| body[at + marker.len()..].split('"').next()?.parse().ok())
        .collect()
}

#[test]
fn the_proto_file_matches_the_core_s_types() {
    let dir = env!("CARGO_MANIFEST_DIR");
    let proto =
        proto_messages(&std::fs::read_to_string(format!("{dir}/proto/nexa_client.proto")).unwrap());
    let rust = rust_messages(&std::fs::read_to_string(format!("{dir}/src/proto.rs")).unwrap());
    assert!(
        proto.len() > 20 && rust.len() > 20,
        "parsed {} and {}",
        proto.len(),
        rust.len()
    );
    let mut problems = Vec::new();
    let proto_text = std::fs::read_to_string(format!("{dir}/proto/nexa_client.proto")).unwrap();
    let rust_text = std::fs::read_to_string(format!("{dir}/src/proto.rs")).unwrap();
    let (proto_e, rust_e) = (proto_enums(&proto_text), rust_enums(&rust_text));
    assert!(
        proto_e.len() > 5 && rust_e.len() > 5,
        "parsed {} and {} enums",
        proto_e.len(),
        rust_e.len()
    );
    for (name, values) in &rust_e {
        match proto_e.get(name) {
            None => problems.push(format!(
                "enum {name}: in proto.rs, not in nexa_client.proto"
            )),
            Some(p) if p != values => problems.push(format!(
                "enum {name}: proto.rs has values {values:?}, nexa_client.proto {p:?}"
            )),
            Some(_) => {}
        }
    }
    // Mode is nexa-wire's, used by proto.rs; its values are checked against that.
    let wire_mode = [0, 1, 2].into_iter().collect::<BTreeSet<u32>>();
    assert_eq!(
        [
            nexa_wire::control::Mode::Office as u32,
            nexa_wire::control::Mode::Balanced as u32,
            nexa_wire::control::Mode::Game as u32
        ]
        .into_iter()
        .collect::<BTreeSet<_>>(),
        wire_mode
    );
    if proto_e.get("Mode") != Some(&wire_mode) {
        problems.push(format!(
            "enum Mode: nexa-wire has {wire_mode:?}, nexa_client.proto {:?}",
            proto_e.get("Mode")
        ));
    }
    for name in proto_e
        .keys()
        .filter(|n| *n != "Mode" && !rust_e.contains_key(*n))
    {
        problems.push(format!(
            "enum {name}: in nexa_client.proto, not in proto.rs"
        ));
    }
    for (name, numbers) in &rust {
        match proto.get(name) {
            None => problems.push(format!("{name}: in proto.rs, not in nexa_client.proto")),
            Some(p) if p != numbers => problems.push(format!(
                "{name}: proto.rs has fields {numbers:?}, nexa_client.proto {p:?}"
            )),
            Some(_) => {}
        }
    }
    for name in proto.keys().filter(|n| !rust.contains_key(*n)) {
        problems.push(format!("{name}: in nexa_client.proto, not in proto.rs"));
    }
    assert!(
        problems.is_empty(),
        "the two have drifted:\n{}",
        problems.join("\n")
    );
}
