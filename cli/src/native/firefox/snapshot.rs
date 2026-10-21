use serde_json::{json, Value};

use crate::native::cdp::types::{AXNode, AXProperty, AXValue};

/// Converts a Firefox DevTools `accessible.snapshot` tree into Chrome-shaped accessibility nodes.
/// `durable_id` supplies the stable integer stored as `backend_d_o_m_node_id` for each node.
pub fn ax_nodes_from_snapshot(
    root: &Value,
    durable_id: &mut dyn FnMut(&Value) -> Option<i64>,
) -> Vec<AXNode> {
    let mut nodes = Vec::new();
    push_node(root, durable_id, &mut nodes);
    nodes
}

fn ax_value(kind: &str, value: Value) -> AXValue {
    AXValue {
        value_type: kind.to_string(),
        value: Some(value),
    }
}

fn property(name: &str, value: bool) -> AXProperty {
    AXProperty {
        name: name.to_string(),
        value: ax_value("boolean", Value::Bool(value)),
    }
}

fn push_node(
    node: &Value,
    durable_id: &mut dyn FnMut(&Value) -> Option<i64>,
    nodes: &mut Vec<AXNode>,
) -> String {
    let id = nodes.len().to_string();
    let role = match node["role"].as_str().unwrap_or("") {
        "document" => "RootWebArea",
        "text leaf" => "StaticText",
        "internal frame" => "Iframe",
        other => other,
    };
    let states: Vec<&str> = node["states"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut properties: Vec<AXProperty> = [
        ("checked", "checked"),
        ("expanded", "expanded"),
        ("selected", "selected"),
        ("unavailable", "disabled"),
        ("required", "required"),
    ]
    .iter()
    .filter(|(state, _)| states.contains(state))
    .map(|(_, name)| property(name, true))
    .collect();
    let level = node["level"]
        .as_str()
        .or_else(|| node["attributes"]["level"].as_str())
        .and_then(|l| l.parse::<i64>().ok());
    if let Some(level) = level {
        properties.push(AXProperty {
            name: "level".to_string(),
            value: ax_value("integer", json!(level)),
        });
    }
    let text = |key: &str| {
        node[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| ax_value("string", json!(s)))
    };
    nodes.push(AXNode {
        node_id: id.clone(),
        role: Some(ax_value("role", json!(role))),
        name: text("name"),
        value: text("value"),
        description: None,
        properties: Some(properties),
        child_ids: None,
        backend_d_o_m_node_id: durable_id(node),
        ignored: None,
    });
    let child_ids: Vec<String> = node["children"]
        .as_array()
        .map(|c| c.iter().map(|c| push_node(c, durable_id, nodes)).collect())
        .unwrap_or_default();
    let index = id.parse::<usize>().unwrap_or(0);
    nodes[index].child_ids = Some(child_ids);
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::element::RefMap;
    use crate::native::snapshot::{snapshot_from_ax_nodes, SnapshotOptions};
    use std::collections::HashMap;

    fn leaf(role: &str, name: &str, sel: &str, states: &[&str]) -> Value {
        json!({"role": role, "name": name, "nodeCssSelector": sel, "states": states, "children": []})
    }

    fn page(children: Vec<Value>) -> Value {
        json!({"role": "document", "name": "", "nodeCssSelector": "", "states": [], "children": children})
    }

    fn render_with(root: &Value, ref_map: &mut RefMap, ids: &mut HashMap<String, i64>) -> String {
        let mut id = |n: &Value| {
            let key = n["nodeCssSelector"].as_str()?.to_string();
            let next = ids.len() as i64 + 1;
            Some(*ids.entry(key).or_insert(next))
        };
        let nodes = ax_nodes_from_snapshot(root, &mut id);
        let options = SnapshotOptions {
            selector: None,
            interactive: false,
            compact: false,
            depth: None,
            urls: false,
        };
        ref_map.observe_document("s", None, "s", Some("loader"));
        snapshot_from_ax_nodes(&nodes, &options, ref_map, "s", None, true)
    }

    #[test]
    fn renders_chrome_format() {
        let mut email = leaf(
            "textbox",
            "Email",
            "body > input:nth-child(5)",
            &["editable"],
        );
        email["value"] = json!("a@b.c");
        let root = page(vec![
            leaf("button", "OK", "body > button:nth-child(2)", &["focusable"]),
            email,
            leaf(
                "checkbox",
                "Agree",
                "body > input:nth-child(6)",
                &["checked", "focusable"],
            ),
            leaf(
                "button",
                "Send",
                "body > button:nth-child(7)",
                &["unavailable"],
            ),
            leaf("text leaf", "hello", "body > p", &[]),
        ]);
        let out = render_with(&root, &mut RefMap::new(), &mut HashMap::new());
        assert!(out.contains("button \"OK\" [ref=e1]"), "{out}");
        assert!(out.contains("textbox \"Email\" [ref=e2]: a@b.c"), "{out}");
        assert!(
            out.contains("checkbox \"Agree\" [checked=true, ref=e3]"),
            "{out}"
        );
        assert!(out.contains("button \"Send\" [disabled, ref=e4]"), "{out}");
        assert!(out.contains("StaticText \"hello\""), "{out}");
    }

    #[test]
    fn duplicate_buttons_get_distinct_refs_with_nth() {
        let root = page(vec![
            leaf("button", "OK", "body > button:nth-child(2)", &[]),
            leaf("button", "OK", "body > button:nth-child(3)", &[]),
        ]);
        let mut map = RefMap::new();
        render_with(&root, &mut map, &mut HashMap::new());
        assert_eq!(map.get("e1").unwrap().nth, Some(0));
        assert_eq!(map.get("e2").unwrap().nth, Some(1));
    }

    #[test]
    fn refs_survive_insertion() {
        let ok = || leaf("button", "OK", "body > button:nth-child(2)", &[]);
        let mut map = RefMap::new();
        let mut ids = HashMap::new();
        render_with(&page(vec![ok()]), &mut map, &mut ids);
        let out = render_with(
            &page(vec![
                leaf("button", "New", "body > button:nth-child(1)", &[]),
                ok(),
            ]),
            &mut map,
            &mut ids,
        );
        assert!(out.contains("button \"OK\" [ref=e1]"), "{out}");
        assert!(out.contains("button \"New\" [ref=e2]"), "{out}");
    }
}
