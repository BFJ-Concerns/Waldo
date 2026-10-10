//! The shipped desktop-control skill's literal JSON examples validate against
//! the shared wire types, and every action variant the schema accepts has one.
//!
//! The skill is what a stock agent reads before its first Act, so an example
//! that the schema rejects, or a variant the skill never shows, is a defect
//! this test catches at the contract rather than in an agent's turn.

use std::{collections::BTreeSet, fs, path::Path};

use serde_json::Value;
use waldo_protocol::{
    ActRequest, Action, ObserveRequest, Predicate, QueryRequest, SourceSelector, published_schema,
};

fn skill_text() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/waldo/skills/desktop-control/SKILL.md");
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the plugin ships the skill at {}: {error}", path.display()))
}

/// Every JSON object the skill shows: an inline backtick span, or a fenced
/// `json` block, each with the line it starts on.
fn json_examples(text: &str) -> Vec<(usize, String)> {
    let mut examples = Vec::new();
    let mut fenced: Option<(usize, String)> = None;
    for (index, line) in text.lines().enumerate() {
        if let Some((start, body)) = fenced.as_mut() {
            if line.trim_start().starts_with("```") {
                examples.push((*start, body.clone()));
                fenced = None;
            } else {
                body.push_str(line);
                body.push('\n');
            }
            continue;
        }
        if line.trim_start().starts_with("```json") {
            fenced = Some((index + 1, String::new()));
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(length) = after.find('`') else {
                break;
            };
            let span = &after[..length];
            if span.starts_with('{') && span.ends_with('}') {
                examples.push((index + 1, span.to_owned()));
            }
            rest = &after[length + 1..];
        }
    }
    examples
}

fn client_schema() -> Value {
    serde_json::to_value(published_schema().client_messages).expect("the schema serialises")
}

/// The `type` tags of one tagged-enum definition in the client schema.
fn variant_tags(definition: &str) -> BTreeSet<String> {
    client_schema()["$defs"][definition]["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("the {definition} definition is a oneOf over its variants"))
        .iter()
        .map(|variant| {
            variant["properties"]["type"]["const"]
                .as_str()
                .unwrap_or_else(|| panic!("each {definition} variant carries a constant type tag"))
                .to_owned()
        })
        .collect()
}

#[test]
fn every_json_example_in_the_skill_decodes_as_a_wire_type() {
    let actions = variant_tags("Action");
    let predicates = variant_tags("Predicate");
    let mut failures = Vec::new();
    for (line, example) in json_examples(&skill_text()) {
        let value: Value = match serde_json::from_str(&example) {
            Ok(value) => value,
            Err(error) => {
                failures.push(format!("line {line}: not JSON ({error}): {example}"));
                continue;
            }
        };
        let result = match value.get("type").and_then(Value::as_str) {
            Some(tag) if actions.contains(tag) => {
                // `window` names both an action and a source selector; an
                // object that is not the action may be the bare selector.
                match serde_json::from_value::<Action>(value.clone()) {
                    Ok(_) => Ok(()),
                    Err(as_action) => serde_json::from_value::<SourceSelector>(value)
                        .map(drop)
                        .map_err(|_| format!("as an action: {as_action}")),
                }
            }
            Some(tag) if predicates.contains(tag) => serde_json::from_value::<Predicate>(value)
                .map(drop)
                .map_err(|e| e.to_string()),
            Some(tag) => Err(format!("{tag:?} is neither an action nor a predicate tag")),
            None if value.get("observation_id").is_some() => {
                serde_json::from_value::<ActRequest>(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }
            None if value.get("source").is_some() => {
                serde_json::from_value::<ObserveRequest>(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }
            None if value.get("include_windows").is_some() => {
                serde_json::from_value::<QueryRequest>(value)
                    .map(drop)
                    .map_err(|e| e.to_string())
            }
            None => Err("no type tag and not a recognised request shape".to_owned()),
        };
        if let Err(reason) = result {
            failures.push(format!("line {line}: {reason}: {example}"));
        }
    }
    assert!(
        failures.is_empty(),
        "skill examples the schema rejects:\n{}",
        failures.join("\n")
    );
}

#[test]
fn every_action_variant_has_a_literal_example_in_the_skill() {
    let text = skill_text();
    // A tag counts wherever it appears: an action inside a request's
    // `actions`, a predicate inside a wait or a request's `verification`.
    // A `source` subtree is a selector, whose `window`/`output` tags must
    // not be credited as actions.
    fn collect_tags(value: &Value, shown: &mut BTreeSet<String>) {
        match value {
            Value::Object(fields) => {
                if let Some(tag) = fields.get("type").and_then(Value::as_str) {
                    shown.insert(tag.to_owned());
                }
                fields
                    .iter()
                    .filter(|(name, _)| name.as_str() != "source")
                    .for_each(|(_, field)| collect_tags(field, shown));
            }
            Value::Array(items) => items.iter().for_each(|item| collect_tags(item, shown)),
            _ => {}
        }
    }
    let mut shown = BTreeSet::new();
    for (_, example) in json_examples(&text) {
        if let Ok(value) = serde_json::from_str::<Value>(&example) {
            collect_tags(&value, &mut shown);
        }
    }
    let missing: Vec<_> = variant_tags("Action").difference(&shown).cloned().collect();
    assert!(
        missing.is_empty(),
        "action variants the schema accepts but the skill never shows: {missing:?}"
    );
    let missing: Vec<_> = variant_tags("Predicate")
        .difference(&shown)
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "predicate variants the schema accepts but the skill never shows: {missing:?}"
    );
}
