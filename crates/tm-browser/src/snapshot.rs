//! The accessibility-tree snapshot: the default agent-facing observation of a page.
//!
//! An agent perceives a page as a tree of roled, named nodes with stable [`AxRef`]s — not
//! pixels — and acts by ref (`SPEC.md` §19.2). [`Snapshot::from_ax_tree`] builds one from the
//! raw JSON returned by CDP's `Accessibility.getFullAXTree`; that construction is a pure
//! function so it can be tested against recorded CDP JSON fixtures without a live browser.
//! [`Snapshot::render_text`] produces the compact indented form agents read, and
//! [`Snapshot::diff`] lets an agent see what its last action changed.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A stable reference to one accessibility node, e.g. `"e17"`.
///
/// Stable means: the same backend DOM node yields the same `AxRef` across snapshots taken
/// moments apart, so an agent can act on a ref it read from an earlier snapshot as long as the
/// underlying element still exists. Refs are derived from CDP's `backendDOMNodeId`, not from
/// tree position, so insertions/removals elsewhere in the tree never renumber unrelated nodes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AxRef(pub String);

impl AxRef {
    /// Derive the stable ref for a given CDP backend DOM node id.
    ///
    /// # Invariants
    /// Pure and deterministic: the same `backend_node_id` always yields the same `AxRef`,
    /// across processes and across snapshots.
    pub fn from_backend_node_id(backend_node_id: i64) -> AxRef {
        let hash = blake3::hash(&backend_node_id.to_le_bytes());
        AxRef(format!("e{}", &hash.to_hex()[..6]))
    }
}

impl std::fmt::Display for AxRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One node in an accessibility tree: role, name, value, state and a stable ref, per
/// `SPEC.md` §19.2's example rendering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxNode {
    /// The ARIA/accessibility role, e.g. `"button"`, `"textbox"`, `"list"`.
    pub role: String,
    /// The accessible name (roughly: the visible label).
    pub name: String,
    /// The current value, for form controls that have one.
    pub value: Option<String>,
    /// Boolean and enum accessibility states (`disabled`, `checked`, `expanded`, ...), keyed by
    /// CDP property name.
    pub state: BTreeMap<String, Value>,
    /// This node's stable reference, used by [`crate::session::BrowserSession`] action methods.
    pub reference: AxRef,
    /// Child nodes, in document order.
    pub children: Vec<AxNode>,
}

/// A full-page accessibility snapshot: the default observation an agent reads before deciding
/// its next action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Top-level nodes (normally a single document root, occasionally more for detached
    /// subtrees CDP still reports).
    pub roots: Vec<AxNode>,
}

/// One raw CDP `AXNode` entry, as returned in the `nodes` array of
/// `Accessibility.getFullAXTree`.
struct RawNode {
    node_id: String,
    backend_node_id: i64,
    ignored: bool,
    role: String,
    name: String,
    value: Option<String>,
    state: BTreeMap<String, Value>,
    child_ids: Vec<String>,
}

/// Pull a CDP `{type, value}` wrapped property's string value, if present.
fn wrapped_string(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.get("value")).map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

fn wrapped_bool(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    obj.get(key)
        .and_then(|v| v.get("value"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Parse one raw CDP AXNode JSON object into a [`RawNode`].
fn parse_raw_node(obj: &Value) -> Result<RawNode, SnapshotError> {
    let obj = obj
        .as_object()
        .ok_or_else(|| SnapshotError::Decode("AXNode entry is not an object".to_string()))?;

    let node_id = obj
        .get("nodeId")
        .and_then(Value::as_str)
        .ok_or_else(|| SnapshotError::Decode("AXNode missing string nodeId".to_string()))?
        .to_string();

    let backend_node_id = obj
        .get("backendDOMNodeId")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            SnapshotError::Decode(format!("AXNode {node_id} missing backendDOMNodeId"))
        })?;

    let role = wrapped_string(obj, "role")
        .ok_or_else(|| SnapshotError::Decode(format!("AXNode {node_id} missing role")))?;

    let name = wrapped_string(obj, "name").unwrap_or_default();
    let value = wrapped_string(obj, "value");
    let ignored = wrapped_bool(obj, "ignored");

    let mut state = BTreeMap::new();
    if let Some(Value::Array(props)) = obj.get("properties") {
        for prop in props {
            let Some(prop_obj) = prop.as_object() else {
                continue;
            };
            let Some(name) = prop_obj.get("name").and_then(Value::as_str) else {
                continue;
            };
            if let Some(v) = prop_obj.get("value").and_then(|v| v.get("value")) {
                state.insert(name.to_string(), v.clone());
            }
        }
    }

    let child_ids = match obj.get("childIds") {
        Some(Value::Array(ids)) => ids
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };

    Ok(RawNode {
        node_id,
        backend_node_id,
        ignored,
        role,
        name,
        value,
        state,
        child_ids,
    })
}

/// Recursively build an [`AxNode`] tree from `node_id`, dropping ignored nodes (reparenting
/// their children) unless `include_ignored`. `path` tracks the current root-to-node chain of
/// CDP `nodeId`s to detect cycles.
fn build_node(
    node_id: &str,
    by_id: &HashMap<String, RawNode>,
    include_ignored: bool,
    path: &mut Vec<String>,
) -> Result<Vec<AxNode>, SnapshotError> {
    if path.iter().any(|p| p == node_id) {
        return Err(SnapshotError::Decode(format!(
            "cycle detected at nodeId {node_id}"
        )));
    }
    let Some(raw) = by_id.get(node_id) else {
        // A childId that doesn't resolve to a node in the map: skip it rather than error, CDP
        // can report dangling ids for nodes outside the requested subtree.
        return Ok(Vec::new());
    };

    path.push(node_id.to_string());
    let mut built_children = Vec::new();
    for child_id in &raw.child_ids {
        built_children.extend(build_node(child_id, by_id, include_ignored, path)?);
    }
    path.pop();

    if raw.ignored && !include_ignored {
        // Drop this node but promote its already-built children in its place.
        return Ok(built_children);
    }

    Ok(vec![AxNode {
        role: raw.role.clone(),
        name: raw.name.clone(),
        value: raw.value.clone(),
        state: raw.state.clone(),
        reference: AxRef::from_backend_node_id(raw.backend_node_id),
        children: built_children,
    }])
}

impl Snapshot {
    /// Build a snapshot from the raw JSON array CDP's `Accessibility.getFullAXTree` returns
    /// (the `nodes` field of its result).
    ///
    /// # Errors
    /// [`SnapshotError::Decode`] when `raw` is not an array of well-formed CDP `AXNode`
    /// objects (each needs at minimum `nodeId`, `backendDOMNodeId` and `role`).
    ///
    /// # Invariants
    /// Nodes CDP marks `ignored: true` are dropped (and their children reparented to the
    /// nearest kept ancestor) unless `include_ignored` is set, matching what a screen reader
    /// would actually announce. The tree is rebuilt from CDP's flat `nodeId`-keyed list plus
    /// each node's `childIds`, so node order in `raw` does not need to be tree order.
    pub fn from_ax_tree(raw: &Value, include_ignored: bool) -> Result<Snapshot, SnapshotError> {
        let entries = raw
            .as_array()
            .ok_or_else(|| SnapshotError::Decode("expected a JSON array of AXNodes".to_string()))?;

        if entries.is_empty() {
            return Ok(Snapshot { roots: Vec::new() });
        }

        let mut by_id: HashMap<String, RawNode> = HashMap::new();
        let mut order = Vec::new();
        for entry in entries {
            let node = parse_raw_node(entry)?;
            order.push(node.node_id.clone());
            by_id.insert(node.node_id.clone(), node);
        }

        // A node is a root if no other node lists it as a child.
        let mut is_child: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for node in by_id.values() {
            for c in &node.child_ids {
                is_child.insert(c.as_str());
            }
        }

        let root_ids: Vec<String> = order
            .iter()
            .filter(|id| !is_child.contains(id.as_str()))
            .cloned()
            .collect();

        // Fall back to every node if all nodes are (incorrectly) referenced as children, e.g.
        // a malformed cyclic input; build_node's cycle check will then surface the problem.
        let root_ids = if root_ids.is_empty() { order } else { root_ids };

        let mut roots = Vec::new();
        for root_id in &root_ids {
            let mut path = Vec::new();
            roots.extend(build_node(root_id, &by_id, include_ignored, &mut path)?);
        }

        Ok(Snapshot { roots })
    }

    /// Find a node by its ref, searching the whole tree.
    pub fn find_ref(&self, r: &AxRef) -> Option<&AxNode> {
        fn search<'a>(nodes: &'a [AxNode], r: &AxRef) -> Option<&'a AxNode> {
            for node in nodes {
                if &node.reference == r {
                    return Some(node);
                }
                if let Some(found) = search(&node.children, r) {
                    return Some(found);
                }
            }
            None
        }
        search(&self.roots, r)
    }

    /// Render the compact indented text form agents read, matching the shape in `SPEC.md`
    /// §19.2:
    /// ```text
    /// - button "Approve T-184" [ref=e17]
    /// - textbox "Objective" [ref=e21] value="fix auth refresh race"
    /// ```
    pub fn render_text(&self) -> String {
        fn is_presentational(node: &AxNode) -> bool {
            (node.role == "none" || node.role == "presentation")
                && node.name.is_empty()
                && node.children.is_empty()
        }

        fn render(nodes: &[AxNode], depth: usize, out: &mut String) {
            for node in nodes {
                if is_presentational(node) {
                    continue;
                }
                out.push_str(&"  ".repeat(depth));
                out.push_str("- ");
                out.push_str(&node.role);
                out.push_str(" \"");
                out.push_str(&node.name);
                out.push_str("\" [ref=");
                out.push_str(&node.reference.0);
                out.push(']');
                if let Some(value) = &node.value {
                    out.push_str(" value=\"");
                    out.push_str(value);
                    out.push('"');
                }
                for (key, value) in &node.state {
                    match value {
                        Value::Bool(true) => {
                            out.push(' ');
                            out.push_str(key);
                        }
                        Value::Bool(false) => {}
                        other => {
                            out.push(' ');
                            out.push_str(key);
                            out.push('=');
                            match other {
                                Value::String(s) => out.push_str(s),
                                other => out.push_str(&other.to_string()),
                            }
                        }
                    }
                }
                out.push('\n');
                render(&node.children, depth + 1, out);
            }
        }

        let mut out = String::new();
        render(&self.roots, 0, &mut out);
        out
    }

    /// Compute what changed between `self` (the earlier snapshot) and `after` (the later one),
    /// keyed by [`AxRef`] so an agent can see the effect of its last action.
    pub fn diff(&self, after: &Snapshot) -> SnapshotDiff {
        fn flatten(nodes: &[AxNode], out: &mut BTreeMap<AxRef, AxNode>) {
            for node in nodes {
                out.insert(node.reference.clone(), node.clone());
                flatten(&node.children, out);
            }
        }

        let mut before_map = BTreeMap::new();
        flatten(&self.roots, &mut before_map);
        let mut after_map = BTreeMap::new();
        flatten(&after.roots, &mut after_map);

        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut changed = Vec::new();

        for (reference, before_node) in &before_map {
            match after_map.get(reference) {
                None => removed.push(before_node.clone()),
                Some(after_node) => {
                    if after_node != before_node {
                        changed.push(AxNodeChange {
                            reference: reference.clone(),
                            before: before_node.clone(),
                            after: after_node.clone(),
                        });
                    }
                }
            }
        }
        for (reference, after_node) in &after_map {
            if !before_map.contains_key(reference) {
                added.push(after_node.clone());
            }
        }

        SnapshotDiff {
            added,
            removed,
            changed,
        }
    }
}

/// The result of [`Snapshot::diff`]: what an action changed, keyed by [`AxRef`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SnapshotDiff {
    /// Nodes present after but not before.
    pub added: Vec<AxNode>,
    /// Nodes present before but not after.
    pub removed: Vec<AxNode>,
    /// Nodes present in both snapshots whose role, name, value or state changed.
    pub changed: Vec<AxNodeChange>,
}

impl SnapshotDiff {
    /// `true` when nothing changed at all.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// One node that exists in both snapshots but differs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AxNodeChange {
    /// The changed node's stable ref.
    pub reference: AxRef,
    /// The node as it was in the earlier snapshot.
    pub before: AxNode,
    /// The node as it is in the later snapshot.
    pub after: AxNode,
}

/// Failures building a [`Snapshot`] from CDP JSON.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// `raw` was not a well-formed `Accessibility.getFullAXTree` result.
    #[error("failed to decode accessibility tree: {0}")]
    Decode(String),
}

impl From<SnapshotError> for tm_types::TmError {
    fn from(e: SnapshotError) -> Self {
        tm_types::TmError::Parse(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn same_backend_node_id_yields_same_ref() {
        assert_eq!(
            AxRef::from_backend_node_id(42),
            AxRef::from_backend_node_id(42)
        );
    }

    #[test]
    fn different_backend_node_ids_yield_different_refs() {
        assert_ne!(
            AxRef::from_backend_node_id(1),
            AxRef::from_backend_node_id(2)
        );
    }

    #[test]
    fn ref_has_e_prefix_and_six_hex_chars() {
        let r = AxRef::from_backend_node_id(7);
        assert!(r.0.starts_with('e'));
        assert_eq!(r.0.len(), 7);
    }

    fn simple_tree_json() -> Value {
        json!([
            {
                "nodeId": "1",
                "backendDOMNodeId": 100,
                "role": { "type": "role", "value": "RootWebArea" },
                "name": { "type": "computedString", "value": "Doc" },
                "childIds": ["2"]
            },
            {
                "nodeId": "2",
                "backendDOMNodeId": 101,
                "role": { "type": "role", "value": "button" },
                "name": { "type": "computedString", "value": "Approve T-184" },
                "properties": [
                    { "name": "disabled", "value": { "type": "boolean", "value": false } }
                ],
                "childIds": []
            }
        ])
    }

    #[test]
    fn builds_tree_from_well_formed_cdp_json() {
        let snap = Snapshot::from_ax_tree(&simple_tree_json(), false).unwrap();
        assert_eq!(snap.roots.len(), 1);
        assert_eq!(snap.roots[0].role, "RootWebArea");
        assert_eq!(snap.roots[0].children.len(), 1);
        assert_eq!(snap.roots[0].children[0].name, "Approve T-184");
    }

    #[test]
    fn errors_when_raw_is_not_an_array() {
        let err = Snapshot::from_ax_tree(&json!({"not": "an array"}), false).unwrap_err();
        assert!(matches!(err, SnapshotError::Decode(_)));
    }

    #[test]
    fn errors_when_node_missing_role() {
        let raw = json!([
            { "nodeId": "1", "backendDOMNodeId": 1, "name": { "type": "computedString", "value": "x" } }
        ]);
        let err = Snapshot::from_ax_tree(&raw, false).unwrap_err();
        assert!(matches!(err, SnapshotError::Decode(_)));
    }

    #[test]
    fn errors_when_node_missing_backend_node_id() {
        let raw = json!([
            { "nodeId": "1", "role": { "type": "role", "value": "button" } }
        ]);
        let err = Snapshot::from_ax_tree(&raw, false).unwrap_err();
        assert!(matches!(err, SnapshotError::Decode(_)));
    }

    #[test]
    fn errors_on_cyclic_child_ids() {
        let raw = json!([
            {
                "nodeId": "1",
                "backendDOMNodeId": 1,
                "role": { "type": "role", "value": "group" },
                "childIds": ["2"]
            },
            {
                "nodeId": "2",
                "backendDOMNodeId": 2,
                "role": { "type": "role", "value": "group" },
                "childIds": ["1"]
            }
        ]);
        let err = Snapshot::from_ax_tree(&raw, false).unwrap_err();
        assert!(matches!(err, SnapshotError::Decode(_)));
    }

    #[test]
    fn empty_array_yields_empty_snapshot() {
        let snap = Snapshot::from_ax_tree(&json!([]), false).unwrap();
        assert!(snap.roots.is_empty());
    }

    #[test]
    fn ignored_node_is_dropped_and_children_reparented() {
        let raw = json!([
            {
                "nodeId": "1",
                "backendDOMNodeId": 1,
                "role": { "type": "role", "value": "RootWebArea" },
                "childIds": ["2"]
            },
            {
                "nodeId": "2",
                "backendDOMNodeId": 2,
                "role": { "type": "role", "value": "generic" },
                "ignored": { "type": "boolean", "value": true },
                "childIds": ["3"]
            },
            {
                "nodeId": "3",
                "backendDOMNodeId": 3,
                "role": { "type": "role", "value": "button" },
                "name": { "type": "computedString", "value": "Click" },
                "childIds": []
            }
        ]);
        let snap = Snapshot::from_ax_tree(&raw, false).unwrap();
        assert_eq!(snap.roots[0].children.len(), 1);
        assert_eq!(snap.roots[0].children[0].role, "button");
    }

    #[test]
    fn include_ignored_keeps_ignored_nodes() {
        let raw = json!([
            {
                "nodeId": "1",
                "backendDOMNodeId": 1,
                "role": { "type": "role", "value": "RootWebArea" },
                "childIds": ["2"]
            },
            {
                "nodeId": "2",
                "backendDOMNodeId": 2,
                "role": { "type": "role", "value": "generic" },
                "ignored": { "type": "boolean", "value": true },
                "childIds": []
            }
        ]);
        let snap = Snapshot::from_ax_tree(&raw, true).unwrap();
        assert_eq!(snap.roots[0].children.len(), 1);
        assert_eq!(snap.roots[0].children[0].role, "generic");
    }

    #[test]
    fn find_ref_locates_nested_node() {
        let snap = Snapshot::from_ax_tree(&simple_tree_json(), false).unwrap();
        let target = snap.roots[0].children[0].reference.clone();
        let found = snap.find_ref(&target).unwrap();
        assert_eq!(found.name, "Approve T-184");
    }

    #[test]
    fn find_ref_returns_none_when_absent() {
        let snap = Snapshot::from_ax_tree(&simple_tree_json(), false).unwrap();
        assert!(snap.find_ref(&AxRef("e000000".to_string())).is_none());
    }

    #[test]
    fn render_text_matches_spec_example_shape() {
        let snap = Snapshot::from_ax_tree(&simple_tree_json(), false).unwrap();
        let text = snap.render_text();
        let button_ref = &snap.roots[0].children[0].reference.0;
        let expected_line = format!("- button \"Approve T-184\" [ref={button_ref}]\n");
        assert!(text.contains(&expected_line));
    }

    #[test]
    fn render_text_includes_value_and_true_boolean_state() {
        let node = AxNode {
            role: "textbox".to_string(),
            name: "Objective".to_string(),
            value: Some("fix auth refresh race".to_string()),
            state: BTreeMap::from([("focused".to_string(), Value::Bool(true))]),
            reference: AxRef("e21".to_string()),
            children: Vec::new(),
        };
        let snap = Snapshot { roots: vec![node] };
        let text = snap.render_text();
        assert_eq!(
            text,
            "- textbox \"Objective\" [ref=e21] value=\"fix auth refresh race\" focused\n"
        );
    }

    #[test]
    fn render_text_omits_presentational_leaf_nodes() {
        let deco = AxNode {
            role: "presentation".to_string(),
            name: String::new(),
            value: None,
            state: BTreeMap::new(),
            reference: AxRef("e01".to_string()),
            children: Vec::new(),
        };
        let snap = Snapshot { roots: vec![deco] };
        assert_eq!(snap.render_text(), "");
    }

    #[test]
    fn render_text_keeps_presentational_node_with_children() {
        let child = AxNode {
            role: "button".to_string(),
            name: "Go".to_string(),
            value: None,
            state: BTreeMap::new(),
            reference: AxRef("e02".to_string()),
            children: Vec::new(),
        };
        let wrapper = AxNode {
            role: "none".to_string(),
            name: String::new(),
            value: None,
            state: BTreeMap::new(),
            reference: AxRef("e01".to_string()),
            children: vec![child],
        };
        let snap = Snapshot {
            roots: vec![wrapper],
        };
        let text = snap.render_text();
        assert!(text.contains("none"));
        assert!(text.contains("button \"Go\""));
    }

    fn node(reference: &str, role: &str, name: &str) -> AxNode {
        AxNode {
            role: role.to_string(),
            name: name.to_string(),
            value: None,
            state: BTreeMap::new(),
            reference: AxRef(reference.to_string()),
            children: Vec::new(),
        }
    }

    #[test]
    fn diff_of_identical_snapshots_is_empty() {
        let snap = Snapshot {
            roots: vec![node("e1", "button", "Go")],
        };
        assert!(snap.diff(&snap.clone()).is_empty());
    }

    #[test]
    fn diff_detects_added_node() {
        let before = Snapshot {
            roots: vec![node("e1", "button", "Go")],
        };
        let after = Snapshot {
            roots: vec![node("e1", "button", "Go"), node("e2", "button", "Stop")],
        };
        let diff = before.diff(&after);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].reference.0, "e2");
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn diff_detects_removed_node() {
        let before = Snapshot {
            roots: vec![node("e1", "button", "Go"), node("e2", "button", "Stop")],
        };
        let after = Snapshot {
            roots: vec![node("e1", "button", "Go")],
        };
        let diff = before.diff(&after);
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].reference.0, "e2");
    }

    #[test]
    fn diff_detects_changed_node_name() {
        let before = Snapshot {
            roots: vec![node("e1", "button", "Go")],
        };
        let after = Snapshot {
            roots: vec![node("e1", "button", "Stop")],
        };
        let diff = before.diff(&after);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].before.name, "Go");
        assert_eq!(diff.changed[0].after.name, "Stop");
    }
}
