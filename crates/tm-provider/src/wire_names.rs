//! Tool names on the wire: a reversible, per-request mapping from tm's own tool names to names
//! every provider accepts.
//!
//! tm's agent tools are dotted (`fs.read`, `ticket.create_child`, `shell.run`). Real providers
//! reject that: the Anthropic Messages API requires a tool `name` to match
//! `^[a-zA-Z0-9_-]{1,128}$`, and OpenAI's Chat Completions and Responses APIs require
//! `^[a-zA-Z0-9_-]{1,64}$`. Every OpenAI-compatible backend inherits OpenAI's rule, or at least
//! cannot be relied on to be laxer. So each real provider rewrites names at its own wire boundary,
//! through [`WireNames`], and maps them back when it parses the response.
//!
//! The mapping lives here, in the providers, and not in [`crate::fabric::Fabric`]: it is a wire
//! concern, and [`crate::mock::MockProvider`] keys its scripts on a hash of the whole request, so a
//! fabric-level rewrite would change every scripted test's request.
//!
//! # The mapping
//!
//! [`WireNames::for_request`] collects every name in a request: each [`ToolDef::name`] and each
//! [`ContentBlock::ToolUse::name`] anywhere in the history (including ones nested in a
//! [`ContentBlock::ToolResult`], and ones for tools no longer offered). Then:
//!
//! - A name that is already valid (`[A-Za-z0-9_-]`, 1 to [`MAX_WIRE_NAME_LEN`] characters) goes
//!   on the wire unchanged. These are reserved first, so a valid name never gets renamed because
//!   of an invalid one.
//! - Every other name, in sorted order, is sanitized ([`sanitize`]: each character outside the
//!   allowed set becomes `_`, then truncate to [`MAX_WIRE_NAME_LEN`]; empty becomes `_`). If the
//!   result is already taken, a numeric suffix `_2`, `_3`, ... is appended, truncating the base so
//!   the whole name still fits.
//!
//! The result is a pure function of the set of names, not of their order, so the same request
//! always maps the same way. [`WireNames::decode_completion`] maps a response's tool calls back.
//! A wire name the map doesn't know (a model inventing a tool, or echoing the original dotted
//! name it saw in the prompt text) maps to itself, so the agent's own dispatcher reports it as an
//! unknown tool exactly as it would have without this layer.
//!
//! 64 is the tighter of the two limits and is used for every provider. Gemini accepts dots and
//! would take the names as they are, but runs through the same map anyway: one rule for every
//! provider is easier to reason about than a per-provider exception, and the map is harmless where
//! it isn't needed.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::types::{Completion, CompletionRequest, ContentBlock};

/// The longest tool name sent on the wire: OpenAI's limit, the tighter of OpenAI's (64) and
/// Anthropic's (128).
pub const MAX_WIRE_NAME_LEN: usize = 64;

fn is_wire_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Whether `name` already satisfies `^[A-Za-z0-9_-]{1,64}$` and can go on the wire unchanged.
pub fn is_valid_wire_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_WIRE_NAME_LEN && name.chars().all(is_wire_char)
}

/// `name` with every character outside `[A-Za-z0-9_-]` replaced by `_`, truncated to
/// [`MAX_WIRE_NAME_LEN`] characters; `_` for an empty name. Always a valid wire name, but not
/// necessarily a unique one: [`WireNames`] handles collisions.
pub fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if is_wire_char(c) { c } else { '_' })
        .take(MAX_WIRE_NAME_LEN)
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// `base` with `_<n>` appended, `base` first truncated so the whole thing fits in
/// [`MAX_WIRE_NAME_LEN`]. `base` is always ASCII here (it comes from [`sanitize`]), so byte
/// truncation cannot split a character.
fn with_suffix(base: &str, n: usize) -> String {
    let suffix = format!("_{n}");
    let keep = MAX_WIRE_NAME_LEN
        .saturating_sub(suffix.len())
        .min(base.len());
    format!("{}{suffix}", &base[..keep])
}

/// Visit every tool-use name in `blocks`, recursing into tool results.
fn collect_block_names<'a>(blocks: &'a [ContentBlock], out: &mut BTreeSet<&'a str>) {
    for block in blocks {
        match block {
            ContentBlock::ToolUse { name, .. } => {
                out.insert(name.as_str());
            }
            ContentBlock::ToolResult { content, .. } => collect_block_names(content, out),
            ContentBlock::Text { .. } => {}
        }
    }
}

/// Rename every tool-use name in `blocks` through `f`, recursing into tool results.
fn rename_blocks(blocks: &mut [ContentBlock], f: &impl Fn(&str) -> String) {
    for block in blocks {
        match block {
            ContentBlock::ToolUse { name, .. } => *name = f(name),
            ContentBlock::ToolResult { content, .. } => rename_blocks(content, f),
            ContentBlock::Text { .. } => {}
        }
    }
}

/// One request's reversible map between tm's tool names and the names sent on the wire. See the
/// module docs for how names are assigned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireNames {
    to_wire: BTreeMap<String, String>,
    from_wire: BTreeMap<String, String>,
}

impl WireNames {
    /// The map for every tool name `req` mentions: its tool definitions and every tool use in its
    /// history.
    pub fn for_request(req: &CompletionRequest) -> Self {
        let mut names: BTreeSet<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        for msg in &req.messages {
            collect_block_names(&msg.content, &mut names);
        }
        Self::from_names(names)
    }

    /// The map for an arbitrary set of names. Order and duplicates don't matter.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let names: BTreeSet<&str> = names.into_iter().collect();
        let mut map = WireNames::default();

        // Valid names first, unchanged, so none of them is ever displaced by a sanitized one.
        for name in names.iter().copied().filter(|n| is_valid_wire_name(n)) {
            map.insert(name.to_string(), name.to_string());
        }
        for name in names.iter().copied().filter(|n| !is_valid_wire_name(n)) {
            let base = sanitize(name);
            let mut wire = base.clone();
            let mut n = 2;
            while map.from_wire.contains_key(&wire) {
                wire = with_suffix(&base, n);
                n += 1;
            }
            map.insert(name.to_string(), wire);
        }
        map
    }

    fn insert(&mut self, name: String, wire: String) {
        self.from_wire.insert(wire.clone(), name.clone());
        self.to_wire.insert(name, wire);
    }

    /// Whether every name maps to itself, i.e. the request needs no rewriting.
    pub fn is_identity(&self) -> bool {
        self.to_wire.iter().all(|(name, wire)| name == wire)
    }

    /// The wire name for `name`. A name this map was not built from (which a provider should never
    /// ask for) is sanitized on the spot, so the result is at least always valid.
    pub fn to_wire(&self, name: &str) -> String {
        match self.to_wire.get(name) {
            Some(wire) => wire.clone(),
            None => sanitize(name),
        }
    }

    /// tm's name for the wire name `wire`; `wire` itself when the map doesn't know it.
    pub fn from_wire(&self, wire: &str) -> String {
        match self.from_wire.get(wire) {
            Some(name) => name.clone(),
            None => wire.to_string(),
        }
    }

    /// `req` with every tool definition and history tool-use name replaced by its wire name.
    /// Borrowed unchanged when nothing needs renaming.
    pub fn encode_request<'a>(&self, req: &'a CompletionRequest) -> Cow<'a, CompletionRequest> {
        if self.is_identity() {
            return Cow::Borrowed(req);
        }
        let mut out = req.clone();
        for tool in &mut out.tools {
            tool.name = self.to_wire(&tool.name);
        }
        for msg in &mut out.messages {
            rename_blocks(&mut msg.content, &|n| self.to_wire(n));
        }
        Cow::Owned(out)
    }

    /// `completion` with every tool call's wire name mapped back to tm's name.
    pub fn decode_completion(&self, mut completion: Completion) -> Completion {
        for candidate in &mut completion.candidates {
            rename_blocks(&mut candidate.content, &|n| self.from_wire(n));
        }
        completion
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Candidate, Message, MessageRole, ModelId, StopReason, ToolDef, Usage};
    use std::time::Duration;

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn request(tools: &[&str], history: Vec<Message>) -> CompletionRequest {
        CompletionRequest {
            system: None,
            messages: history,
            tools: tools.iter().map(|n| tool(n)).collect(),
            max_tokens: 16,
            temperature: None,
            stop_sequences: Vec::new(),
            stream: false,
            n: 1,
            model: None,
        }
    }

    fn tool_use(id: &str, name: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({}),
        }
    }

    fn completion_calling(names: &[&str]) -> Completion {
        Completion {
            model: ModelId::new("p", "m"),
            candidates: vec![Candidate {
                content: names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| tool_use(&format!("c{i}"), n))
                    .collect(),
                stop_reason: StopReason::ToolUse,
            }],
            usage: Usage::default(),
            latency: Duration::ZERO,
            received_at: tm_types::Timestamp::EPOCH,
        }
    }

    fn called_names(c: &Completion) -> Vec<String> {
        c.candidates[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn valid_names_pass_through_unchanged() {
        let map = WireNames::from_names(["get_weather", "run-it", "A1"]);
        assert!(map.is_identity());
        assert_eq!(map.to_wire("get_weather"), "get_weather");
        assert_eq!(map.from_wire("run-it"), "run-it");
    }

    #[test]
    fn dotted_names_are_sanitized_and_round_trip() {
        let map = WireNames::from_names(["fs.read", "ticket.create_child", "shell.run"]);
        assert_eq!(map.to_wire("fs.read"), "fs_read");
        assert_eq!(map.to_wire("ticket.create_child"), "ticket_create_child");
        for name in ["fs.read", "ticket.create_child", "shell.run"] {
            assert!(is_valid_wire_name(&map.to_wire(name)));
            assert_eq!(map.from_wire(&map.to_wire(name)), name);
        }
    }

    #[test]
    fn a_sanitized_name_never_displaces_a_valid_one() {
        let map = WireNames::from_names(["fs.read", "fs_read", "fs:read"]);
        assert_eq!(map.to_wire("fs_read"), "fs_read");
        let dotted = map.to_wire("fs.read");
        let coloned = map.to_wire("fs:read");
        assert_ne!(dotted, "fs_read");
        assert_ne!(coloned, "fs_read");
        assert_ne!(dotted, coloned);
        // Sorted order decides: "fs.read" < "fs:read".
        assert_eq!(dotted, "fs_read_2");
        assert_eq!(coloned, "fs_read_3");
        for name in ["fs.read", "fs_read", "fs:read"] {
            assert_eq!(map.from_wire(&map.to_wire(name)), name);
        }
    }

    #[test]
    fn suffixes_skip_names_that_are_already_taken() {
        let map = WireNames::from_names(["a.b", "a_b", "a_b_2"]);
        assert_eq!(map.to_wire("a.b"), "a_b_3");
    }

    #[test]
    fn the_map_does_not_depend_on_input_order() {
        let one = WireNames::from_names(["x.y", "x:y", "x_y"]);
        let two = WireNames::from_names(["x_y", "x:y", "x.y", "x.y"]);
        assert_eq!(one, two);
    }

    #[test]
    fn long_names_are_truncated_to_64_and_stay_distinct() {
        let prefix = "a".repeat(70);
        let one = format!("{prefix}.one");
        let two = format!("{prefix}.two");
        let valid_but_long = "b".repeat(100);
        let map = WireNames::from_names([one.as_str(), two.as_str(), valid_but_long.as_str()]);
        let (w1, w2, w3) = (
            map.to_wire(&one),
            map.to_wire(&two),
            map.to_wire(&valid_but_long),
        );
        for w in [&w1, &w2, &w3] {
            assert!(w.len() <= MAX_WIRE_NAME_LEN, "{w} is {} long", w.len());
            assert!(is_valid_wire_name(w));
        }
        assert_ne!(w1, w2);
        assert_eq!(w3, "b".repeat(64));
        assert_eq!(map.from_wire(&w1), one);
        assert_eq!(map.from_wire(&w2), two);
        assert_eq!(map.from_wire(&w3), valid_but_long);
    }

    #[test]
    fn many_colliding_long_names_all_fit() {
        let names: Vec<String> = (0..12).map(|i| format!("{}.{i}", "z".repeat(80))).collect();
        let map = WireNames::from_names(names.iter().map(String::as_str));
        let wires: BTreeSet<String> = names.iter().map(|n| map.to_wire(n)).collect();
        assert_eq!(wires.len(), names.len());
        for (name, wire) in names.iter().map(|n| (n, map.to_wire(n))) {
            assert!(wire.len() <= MAX_WIRE_NAME_LEN);
            assert_eq!(&map.from_wire(&wire), name);
        }
    }

    #[test]
    fn non_ascii_and_empty_names_sanitize_safely() {
        assert_eq!(sanitize(""), "_");
        assert_eq!(sanitize("日本.語"), "____");
        let long_multibyte = "é".repeat(100);
        assert_eq!(sanitize(&long_multibyte), "_".repeat(64));
    }

    #[test]
    fn an_unknown_wire_name_maps_to_itself() {
        let map = WireNames::from_names(["fs.read"]);
        assert_eq!(map.from_wire("made_up"), "made_up");
        // A model echoing the dotted name it read in the prompt gets the dotted name back.
        assert_eq!(map.from_wire("fs.read"), "fs.read");
    }

    #[test]
    fn for_request_covers_history_tool_uses_including_retired_tools_and_nested_results() {
        let req = request(
            &["fs.read"],
            vec![
                Message {
                    role: MessageRole::Assistant,
                    content: vec![tool_use("t1", "fs.read"), tool_use("t2", "old.tool")],
                },
                Message {
                    role: MessageRole::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t2".to_string(),
                        content: vec![tool_use("t3", "nested.tool")],
                        is_error: false,
                    }],
                },
            ],
        );
        let map = WireNames::for_request(&req);
        let encoded = map.encode_request(&req);
        assert_eq!(encoded.tools[0].name, "fs_read");
        match &encoded.messages[0].content[..] {
            [ContentBlock::ToolUse { name: a, .. }, ContentBlock::ToolUse { name: b, .. }] => {
                assert_eq!(a, "fs_read");
                assert_eq!(b, "old_tool");
            }
            other => panic!("unexpected content {other:?}"),
        }
        match &encoded.messages[1].content[0] {
            ContentBlock::ToolResult { content, .. } => match &content[0] {
                ContentBlock::ToolUse { name, .. } => assert_eq!(name, "nested_tool"),
                other => panic!("unexpected nested block {other:?}"),
            },
            other => panic!("unexpected block {other:?}"),
        }
        // The caller's request is untouched.
        assert_eq!(req.tools[0].name, "fs.read");
    }

    #[test]
    fn encode_request_borrows_when_nothing_changes() {
        let req = request(&["get_weather"], Vec::new());
        let map = WireNames::for_request(&req);
        assert!(matches!(map.encode_request(&req), Cow::Borrowed(_)));
    }

    #[test]
    fn decode_completion_maps_calls_back_and_leaves_unknown_ones() {
        let map = WireNames::from_names(["fs.read", "shell.run"]);
        let decoded = map.decode_completion(completion_calling(&["fs_read", "shell_run", "nope"]));
        assert_eq!(called_names(&decoded), vec!["fs.read", "shell.run", "nope"]);
    }
}
