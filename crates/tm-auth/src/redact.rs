//! Secret-*shaped*-substring redaction: scrub arbitrary text (a tool's captured stdout, a
//! model's own echoed output, a ticket's free-text objective) for things that merely *look
//! like* a credential, before that text leaves this process (a model call) or before it is
//! durably persisted (an event, a context pack section, an artifact).
//!
//! Distinct from [`crate::Credential`]'s guarantee. `Credential` redacts a *known* secret this
//! crate itself resolved, by never printing/serializing its own value — see that type's docs.
//! This module has no such privileged knowledge: it scans text this codebase did not resolve
//! and has no a priori reason to believe is sensitive, the way `opencode-vibeguard` scans a
//! request/response body for the same reason. The two guarantees compose rather than overlap:
//! `Credential` covers material this codebase holds; this module covers material that merely
//! shows up in free text anyway.
//!
//! Two entry points, deliberately split by where a restore mapping can honestly live:
//!
//! - [`redact`] / [`redact_json`]: pure, deterministic, keyless. Every match of the same
//!   category becomes the identical placeholder text (`<redacted:aws_key>`), with no mapping
//!   and so no way back. This is what every *persistence* boundary uses —
//!   `tm_core::store::StoreTx::append`'s event payload, `tm_core::Store::store_artifact`'s
//!   bytes/meta, `tm_context::pack::compile`'s section bodies — because a persisted value
//!   outlives this process: a restore mapping tied to a value that is about to be written to
//!   disk would be a mapping to nowhere, kept alive for nothing. Determinism matters here for
//!   its own sake too: `pack::compile` is documented byte-identical given identical inputs, and
//!   `xtask hygiene`'s `check_time_and_rand` enforces no-nondeterminism workspace-wide, so the
//!   persistence-path redactor cannot key its placeholders on anything session-random.
//! - [`SessionRedactor`]: wraps the same pattern set but gives each *distinct* secret value its
//!   own placeholder (a keyed fingerprint, not a bare category) and keeps an in-memory
//!   `placeholder -> original` map so [`SessionRedactor::restore`] can reconstruct the original
//!   for a human's own local view. This is the one sanctioned place a restore happens: never
//!   serialized (no `Serialize` derive, following [`crate::Credential`]'s own precedent), never
//!   sent back to a model, dropped the moment the owning `SessionRedactor` is. `tm_provider`'s
//!   `Fabric` owns one and uses it at `Fabric::execute` — the boundary
//!   `docs/audit-2026-09-18-fable.md`'s "M-04" names for "secret redaction ... with local
//!   restore".
//!
//! ## Pattern set
//!
//! Based on the public shape of `opencode-vibeguard`'s own default pattern set
//! (`github.com/inkdust2021/opencode-vibeguard`, `vibeguard.config.json.example`, retrieved
//! 2026-09-20): an OpenAI-style bearer key (`sk-[A-Za-z0-9]{48}`), GitHub tokens
//! (`(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]+`), and an AWS access key id (`AKIA[0-9A-Z]{16}`), plus a
//! generic label-adjacent high-entropy token this repo's own task brief specified
//! (`[A-Za-z0-9_-]{32,}` next to a `key`/`token`/`secret`/`password` label). Two deliberate
//! departures from the cited source:
//!
//! - The `sk-` pattern here is a *minimum* length (20+ trailing chars), not exactly 48: OpenAI's
//!   own keys are 48 chars past the prefix, but Anthropic's `sk-ant-api03-...` keys run
//!   considerably longer, and this codebase talks to Anthropic (`crates/tm-provider/src/anthropic.rs`).
//!   An exact-48 pattern would silently miss the provider this workspace actually calls.
//! - The GitHub pattern's cited `[A-Za-z0-9]+` has no lower bound at all, which would flag a
//!   one-character placeholder like `ghp_x` in a comment. This module requires 20+ trailing
//!   chars.
//!
//! ## What this deliberately does not flag
//!
//! A bare hex string with no adjacent label — a blake3 content hash, a `tm_types` id's hex
//! suffix (`ART-<hex12>`, `L-<hex12>`), a git commit sha — is never touched: the generic rule
//! only fires when a `key`/`token`/`secret`/`password`-shaped label sits immediately next to the
//! value (`key: <value>` / `key=<value>`), and the provider-specific rules require one of their
//! fixed prefixes. See `hashes_and_ids_survive_unredacted` below for the regression test.
//!
//! ## Known gap
//!
//! [`redact_json`]'s label-adjacency check also looks at the *object key* a string value sits
//! under (not only text embedded in the string itself), so `{"api_key": "<32+ char token>"}`
//! is caught even though the label and the value are separate JSON tokens. It only does this
//! one level deep (a string's *own* enclosing object key) and only when the entire string value
//! looks like a bare token (no whitespace/punctuation) — it will not, for example, catch a
//! secret buried inside a longer prose sentence under an unrelated key. No typed
//! `tm_events::payload` struct in this workspace currently has a `key`/`token`/`secret`/
//! `password`-named field, so this is closing a gap for payload shapes that do not exist yet
//! rather than a hole in current coverage; see `docs/decisions/D-011-secret-redaction.md` for
//! the fuller tradeoff writeup.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value;

/// One matchable secret-shaped pattern, the category its matches are labeled as, and which
/// capture group holds the value to redact (`0` = the whole match).
struct SecretPattern {
    category: &'static str,
    regex: Regex,
    group: usize,
}

fn patterns() -> &'static [SecretPattern] {
    static PATTERNS: OnceLock<Vec<SecretPattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let compile = |category: &'static str, group: usize, pattern: &str| SecretPattern {
            category,
            regex: Regex::new(pattern)
                .expect("redact.rs pattern literals are unit-tested to compile, see tests below"),
            group,
        };
        vec![
            compile("api_key", 0, r"\bsk-[A-Za-z0-9_-]{20,}\b"),
            compile("aws_key", 0, r"\bAKIA[0-9A-Z]{16}\b"),
            compile(
                "github_token",
                0,
                r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}\b",
            ),
            compile("private_key", 0, r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
            // Label-adjacent generic token: the one pattern gated on context rather than a fixed
            // prefix, so it is also the one most likely to graze a legitimate long identifier —
            // gating it on an explicit label keeps a bare blake3 hash or ticket id untouched.
            compile(
                "generic_secret",
                1,
                r#"(?i)\b(?:api[_-]?key|access[_-]?token|secret|password|token)\b\s*[:=]\s*['"]?([A-Za-z0-9_-]{32,})['"]?"#,
            ),
        ]
    })
}

/// Whether `key` itself reads as a `key`/`token`/`secret`/`password`-shaped label, for
/// [`redact_json`]'s one-level-deep object-key check.
fn looks_like_secret_label(key: &str) -> bool {
    static LABEL: OnceLock<Regex> = OnceLock::new();
    let re = LABEL.get_or_init(|| {
        Regex::new(r"(?i)^(?:api[_-]?key|access[_-]?token|secret|password|token|apikey)$")
            .expect("redact.rs label pattern is unit-tested to compile, see tests below")
    });
    re.is_match(key)
}

/// Whether `value` looks like a bare token (no whitespace/punctuation split across words) long
/// enough to plausibly be a secret rather than a short flag or a prose value.
///
/// Deliberately a shorter floor (20+ chars) than the in-text generic pattern's 32+: a JSON
/// object key that already reads as a `key`/`token`/`secret`/`password` label is stronger
/// evidence on its own than an in-text label, so this can afford to accept a somewhat shorter
/// bare value under it without the same false-positive exposure the unlabeled 32+ floor guards
/// against.
fn looks_like_bare_token(value: &str) -> bool {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    let re = TOKEN.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9_-]{20,}$")
            .expect("redact.rs token-shape pattern is unit-tested to compile, see tests below")
    });
    re.is_match(value)
}

/// Find every secret-shaped span in `text`, sorted by start offset, with overlapping matches
/// resolved in [`patterns`] priority order (fixed-prefix patterns win over the generic
/// label-adjacent one).
fn scan(text: &str) -> Vec<(usize, usize, &'static str)> {
    let mut spans: Vec<(usize, usize, &'static str)> = Vec::new();
    for pat in patterns() {
        for caps in pat.regex.captures_iter(text) {
            let m = caps
                .get(pat.group)
                .or_else(|| caps.get(0))
                .expect("a successful regex match always has capture group 0");
            let (start, end) = (m.start(), m.end());
            if spans.iter().any(|&(s, e, _)| start < e && s < end) {
                continue;
            }
            spans.push((start, end, pat.category));
        }
    }
    spans.sort_by_key(|&(s, _, _)| s);
    spans
}

/// Apply `spans` to `text`, replacing each with whatever `placeholder_for(category, original)`
/// returns.
fn apply_spans(
    text: &str,
    spans: &[(usize, usize, &'static str)],
    mut placeholder_for: impl FnMut(&'static str, &str) -> String,
) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for &(start, end, category) in spans {
        out.push_str(&text[last..start]);
        out.push_str(&placeholder_for(category, &text[start..end]));
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

/// Pure, deterministic, keyless redaction: every secret-shaped match of the same category
/// becomes the identical `<redacted:{category}>` placeholder. No restore is possible (there is
/// no mapping) — this is the redactor every persistence boundary uses. See this module's docs
/// for why the persistence path cannot key its placeholders.
pub fn redact(text: &str) -> String {
    let spans = scan(text);
    apply_spans(text, &spans, |category, _original| {
        format!("<redacted:{category}>")
    })
}

/// [`redact`], walked recursively over a `serde_json::Value`'s string leaves — for JSON payloads
/// (an event payload, an artifact's `meta`, a tool call's structured input) rather than plain
/// text. Also checks each string's enclosing object key against `key`/`token`/`secret`/
/// `password`-shaped labels: a string value that is itself a bare 20+ char token under such a
/// key is redacted whole, even with no in-string label text to match the generic pattern against
/// (see this module's "Known gap" doc for the one level of nesting this covers).
pub fn redact_json(value: &Value) -> Value {
    redact_json_with(value, None, &mut |s| redact(s))
}

fn redact_json_with(
    value: &Value,
    key_hint: Option<&str>,
    f: &mut dyn FnMut(&str) -> String,
) -> Value {
    match value {
        Value::String(s) => {
            if key_hint.is_some_and(looks_like_secret_label) && looks_like_bare_token(s) {
                Value::String("<redacted:generic_secret>".to_string())
            } else {
                Value::String(f(s))
            }
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| redact_json_with(v, None, f)).collect())
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), redact_json_with(v, Some(k.as_str()), f));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Derive a session-scoped key from process/instance-local material, per this module's
/// determinism constraint: `xtask hygiene`'s `check_time_and_rand` forbids raw OS randomness
/// (`rand::random`/`rand::thread_rng`, `SystemTime::now`) outside `tm-types/src/clock.rs`'s
/// injected-clock substrate everywhere in this workspace, and threading an `IdSource` into
/// `Fabric::new` (the sanctioned substrate for "give me something unique") would ripple its
/// signature across the dozens of existing call sites for a security property that does not
/// actually need cryptographic unpredictability — see this module's top doc and
/// `docs/decisions/D-011-secret-redaction.md` for why. The process id plus a process-local
/// atomic counter is enough to make each `SessionRedactor` instance's fingerprints distinct from
/// any other's without reaching for a forbidden entropy source.
fn fresh_session_key() -> [u8; 32] {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let material = format!("tm-auth::redact::SessionRedactor#{pid}:{counter}");
    *blake3::hash(material.as_bytes()).as_bytes()
}

/// Hard cap on how many distinct secrets one [`SessionRedactor`] will remember for restore, so a
/// long-lived `Fabric` redacting many distinct secrets over a process's lifetime cannot grow an
/// unbounded in-memory table of secret material — an unbounded mapping would itself work against
/// this module's whole "the mapping never leaves process memory" guarantee by making that memory
/// footprint unbounded. Modeled on `opencode-vibeguard`'s own `max 100,000 mappings` cap
/// (`vibeguard.config.json.example`) but smaller, since this workspace's usage (one
/// `SessionRedactor` per process-lifetime `Fabric`, not per HTTP request) has no comparable
/// volume to justify matching it exactly. Once at capacity, a new secret still redacts correctly
/// (the placeholder is still produced) but is not remembered, so [`SessionRedactor::restore`]
/// simply cannot recover that particular value — a bounded degradation, not a correctness
/// failure. This module deliberately does not add vibeguard's other bound (a 1-hour session
/// TTL) on top of this; see `docs/decisions/D-011-secret-redaction.md`.
const MAX_MAPPING_ENTRIES: usize = 10_000;

/// Insert `placeholder -> original` into `mapping` unless it is already at
/// [`MAX_MAPPING_ENTRIES`] and `placeholder` is a genuinely new entry.
fn remember(mapping: &mut HashMap<String, String>, placeholder: &str, original: &str) {
    if !mapping.contains_key(placeholder) && mapping.len() >= MAX_MAPPING_ENTRIES {
        return;
    }
    mapping
        .entry(placeholder.to_string())
        .or_insert_with(|| original.to_string());
}

/// A stateful redactor that gives each distinct secret value its own stable placeholder and
/// keeps an in-memory-only mapping back to the original, so [`SessionRedactor::restore`] can
/// reconstruct it for a human's own local view. See this module's top docs for the full
/// rationale and [`crate::Credential`] for the precedent this follows: the mapping never derives
/// `Serialize`, and this type's own [`std::fmt::Debug`] never prints a held secret.
pub struct SessionRedactor {
    key: [u8; 32],
    mapping: Mutex<HashMap<String, String>>,
}

impl Default for SessionRedactor {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRedactor {
    /// A fresh redactor with an empty mapping and its own session key (see
    /// [`fresh_session_key`]).
    pub fn new() -> Self {
        SessionRedactor {
            key: fresh_session_key(),
            mapping: Mutex::new(HashMap::new()),
        }
    }

    fn fingerprint(&self, secret: &str) -> String {
        let h = blake3::keyed_hash(&self.key, secret.as_bytes());
        h.to_hex()[..12].to_string()
    }

    /// Redact `text`, recording each distinct match's placeholder -> original mapping in this
    /// redactor's local, in-memory state. The result is safe to send onward (to a model, over
    /// the wire); the mapping itself never is.
    pub fn redact(&self, text: &str) -> String {
        let spans = scan(text);
        if spans.is_empty() {
            return text.to_string();
        }
        let mut mapping = self.mapping.lock();
        apply_spans(text, &spans, |category, original| {
            let fp = self.fingerprint(original);
            let placeholder = format!("<redacted:{category}:{fp}>");
            remember(&mut mapping, &placeholder, original);
            placeholder
        })
    }

    /// [`SessionRedactor::redact_json`], applied to any `T` that round-trips through
    /// `serde_json::Value` — the entry point `tm_provider`'s HTTP `systemone` `DecisionProvider`
    /// (`SystemOneProvider::decide`, `crates/tm-provider/src/providers/systemone.rs`, D-020)
    /// calls on a `DecideRequest` before it leaves this machine (decision 7, "Redaction
    /// before anything leaves the machine"): every secret-shaped substring in the request's
    /// free-text `state` and each question's own instructions/statement/labels comes back
    /// scrubbed exactly as [`SessionRedactor::redact`] would scrub the same text in a normal
    /// turn — a `DecideRequest` is a message sent onward to a remote decider, the same category
    /// `SessionRedactor` (not the pure, keyless [`redact`]/[`redact_json`]) already owns for
    /// `Fabric::execute`'s provider calls.
    ///
    /// Lives on `SessionRedactor` rather than as a free function named after the concrete
    /// `DecideRequest` type, because `tm-provider` (where `DecideRequest` lives) already depends
    /// on this crate for [`crate::EnvApiKey`] and friends — naming that concrete type here would
    /// close the dependency into a cycle. Going through `serde_json::Value` sidesteps that: this
    /// method never needs to know `DecideRequest`'s shape, only that it serializes and
    /// deserializes, so the caller in `tm-provider` gets the same
    /// `redactor.redact_decide_request(&req)` call site the task brief asks for, monomorphized to
    /// its own type, with the redaction mapping recorded on the same `SessionRedactor` every
    /// other outbound call already uses.
    ///
    /// Returns `Err` if `T`'s `Serialize`/`Deserialize` round-trip fails, which would mean `T` is
    /// not genuinely JSON-representable (not a real-world case for a request/response DTO like
    /// `DecideRequest`, whose fields are all plain strings, maps and enums) — the caller maps
    /// this into its own `ProviderError` rather than this crate assuming how.
    pub fn redact_decide_request<T>(&self, request: &T) -> Result<T, serde_json::Error>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let value = serde_json::to_value(request)?;
        let redacted = self.redact_json(&value);
        serde_json::from_value(redacted)
    }

    /// [`SessionRedactor::redact`], walked over a JSON value's string leaves — see
    /// [`redact_json`] for the plain-text equivalent's label-adjacency behavior, which this
    /// mirrors.
    pub fn redact_json(&self, value: &Value) -> Value {
        let mapping = &self.mapping;
        let key = &self.key;
        let fp = |secret: &str| {
            let h = blake3::keyed_hash(key, secret.as_bytes());
            h.to_hex()[..12].to_string()
        };
        redact_json_with(value, None, &mut |s| {
            let spans = scan(s);
            if spans.is_empty() {
                return s.to_string();
            }
            let mut mapping = mapping.lock();
            apply_spans(s, &spans, |category, original| {
                let placeholder = format!("<redacted:{category}:{}>", fp(original));
                remember(&mut mapping, &placeholder, original);
                placeholder
            })
        })
    }

    /// Restore every placeholder this redactor has ever produced back to the original value it
    /// replaced, for a human's own local view. Callers must never forward the result to a model
    /// or a persisted store — the whole point of keeping the mapping in-memory-only is that a
    /// restored value never leaves this process. Placeholders this redactor did not itself mint
    /// (or has since forgotten) pass through unchanged.
    pub fn restore(&self, text: &str) -> String {
        let mapping = self.mapping.lock();
        if mapping.is_empty() || !text.contains("<redacted:") {
            return text.to_string();
        }
        let mut out = text.to_string();
        for (placeholder, original) in mapping.iter() {
            if out.contains(placeholder.as_str()) {
                out = out.replace(placeholder.as_str(), original.as_str());
            }
        }
        out
    }

    /// How many distinct secrets this redactor currently holds a restore mapping for.
    pub fn mapping_len(&self) -> usize {
        self.mapping.lock().len()
    }
}

/// Never prints a held secret, following [`crate::Credential`]'s own precedent.
impl std::fmt::Debug for SessionRedactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRedactor")
            .field("mapping_len", &self.mapping.lock().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAI_KEY: &str = "sk-abcdefghijklmnopqrstuvwxyz0123456789ABCD";
    const AWS_KEY: &str = "AKIAABCDEFGHIJKLMNOP";
    const GITHUB_TOKEN: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    const GENERIC_TOKEN: &str = "aB3dEf6HiJ9kLmNoPqR2sTuVwXyZ0123";

    #[test]
    fn redacts_openai_style_key() {
        let text = format!("Authorization: Bearer {OPENAI_KEY}");
        let out = redact(&text);
        assert!(!out.contains(OPENAI_KEY), "{out}");
        assert!(out.contains("<redacted:api_key>"), "{out}");
    }

    #[test]
    fn redacts_aws_access_key() {
        let text = format!("aws_access_key_id = {AWS_KEY}");
        let out = redact(&text);
        assert!(!out.contains(AWS_KEY), "{out}");
        assert!(out.contains("<redacted:aws_key>"), "{out}");
    }

    #[test]
    fn redacts_github_token() {
        let text = format!("token: {GITHUB_TOKEN}");
        let out = redact(&text);
        assert!(!out.contains(GITHUB_TOKEN), "{out}");
        assert!(out.contains("<redacted:github_token>"), "{out}");
    }

    #[test]
    fn redacts_pem_private_key_header() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIB...\n-----END RSA PRIVATE KEY-----";
        let out = redact(text);
        assert!(!out.contains("-----BEGIN RSA PRIVATE KEY-----"), "{out}");
        assert!(out.contains("<redacted:private_key>"), "{out}");
    }

    #[test]
    fn redacts_generic_label_adjacent_token() {
        let text = format!("api_key: \"{GENERIC_TOKEN}\"");
        let out = redact(&text);
        assert!(!out.contains(GENERIC_TOKEN), "{out}");
        assert!(out.contains("<redacted:generic_secret>"), "{out}");
        // The label itself survives -- only the value is redacted.
        assert!(out.contains("api_key:"), "{out}");
    }

    #[test]
    fn generic_pattern_requires_a_label_not_just_length() {
        // A 32+ char run with no key/token/secret/password label nearby is not a secret by this
        // module's rule -- it is exactly the shape of an unlabeled hash or id.
        let text = format!("blake3 content hash: {GENERIC_TOKEN}");
        let out = redact(&text);
        assert_eq!(out, text, "unlabeled long token should survive unredacted");
    }

    #[test]
    fn hashes_and_ids_survive_unredacted() {
        let blake3_hash = blake3::hash(b"hello world").to_hex().to_string();
        let ticket_id = "T-42";
        let artifact_id = "ART-0123456789ab";
        let session_id = "S-7";
        let git_sha = "a94a8fe5ccb19ba61c4c0873d391e987982fbbd3";

        for value in [
            blake3_hash.as_str(),
            ticket_id,
            artifact_id,
            session_id,
            git_sha,
        ] {
            let text = format!("id: {value}");
            assert_eq!(redact(&text), text, "false positive on {value:?}");
        }
    }

    #[test]
    fn does_not_redact_short_or_low_entropy_lookalikes() {
        // Below the length floor for every pattern.
        assert_eq!(redact("sk-short"), "sk-short");
        assert_eq!(redact("ghp_short"), "ghp_short");
    }

    #[test]
    fn redact_is_idempotent() {
        let text = format!("api_key={OPENAI_KEY} and {AWS_KEY}");
        let once = redact(&text);
        let twice = redact(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn redact_json_walks_string_leaves() {
        let value = serde_json::json!({
            "reason": format!("failed calling provider with sk-key {OPENAI_KEY}"),
            "nested": { "detail": format!("token: {GITHUB_TOKEN}") },
            "list": [format!("password={GENERIC_TOKEN}")],
            "ticket": "T-1",
        });
        let out = redact_json(&value);
        let text = out.to_string();
        assert!(!text.contains(OPENAI_KEY), "{text}");
        assert!(!text.contains(GITHUB_TOKEN), "{text}");
        assert!(!text.contains(GENERIC_TOKEN), "{text}");
        assert!(text.contains("T-1"), "unrelated id should survive: {text}");
    }

    #[test]
    fn redact_json_catches_a_bare_token_under_a_labeled_key() {
        // No in-string label -- the label is the sibling JSON key, per this module's "Known gap"
        // doc. Still caught because the whole string value is a bare 20+ char token.
        let value = serde_json::json!({ "api_key": GENERIC_TOKEN });
        let out = redact_json(&value);
        let text = out.to_string();
        assert!(!text.contains(GENERIC_TOKEN), "{text}");
    }

    #[test]
    fn redact_json_does_not_flag_a_hash_under_an_unlabeled_key() {
        let blake3_hash = blake3::hash(b"hello world").to_hex().to_string();
        let value = serde_json::json!({ "hash": blake3_hash, "path": "storage:inline" });
        let out = redact_json(&value);
        assert_eq!(
            out, value,
            "unlabeled hash-shaped field should be untouched"
        );
    }

    #[test]
    fn session_redactor_gives_each_distinct_secret_its_own_placeholder() {
        let redactor = SessionRedactor::new();
        let text = format!("{OPENAI_KEY} and {AWS_KEY}");
        let out = redactor.redact(&text);
        assert!(!out.contains(OPENAI_KEY), "{out}");
        assert!(!out.contains(AWS_KEY), "{out}");
        assert_eq!(redactor.mapping_len(), 2);
    }

    #[test]
    fn session_redactor_restores_locally_and_is_never_sent_onward() {
        let redactor = SessionRedactor::new();
        let text = format!("Authorization: Bearer {OPENAI_KEY}");
        let redacted = redactor.redact(&text);
        assert!(!redacted.contains(OPENAI_KEY));

        let restored = redactor.restore(&redacted);
        assert_eq!(restored, text);
    }

    #[test]
    fn session_redactor_restore_is_a_noop_on_text_with_no_placeholders() {
        let redactor = SessionRedactor::new();
        assert_eq!(
            redactor.restore("nothing to see here"),
            "nothing to see here"
        );
    }

    #[test]
    fn two_session_redactors_fingerprint_the_same_secret_differently() {
        let a = SessionRedactor::new();
        let b = SessionRedactor::new();
        let out_a = a.redact(OPENAI_KEY);
        let out_b = b.redact(OPENAI_KEY);
        assert_ne!(
            out_a, out_b,
            "distinct SessionRedactor instances must not agree on a fingerprint \
             (that would make the placeholder a stable cross-session identifier for the secret)"
        );
    }

    #[test]
    fn session_redactor_debug_never_prints_a_held_secret() {
        let redactor = SessionRedactor::new();
        redactor.redact(OPENAI_KEY);
        let debug = format!("{redactor:?}");
        assert!(!debug.contains(OPENAI_KEY), "{debug}");
    }

    #[test]
    fn session_redactor_mapping_growth_is_capped() {
        let redactor = SessionRedactor::new();
        // One past the cap's worth of distinct secrets -- each one is a distinct AWS-key-shaped
        // string so every call is a genuinely new mapping entry, not a repeat.
        for i in 0..(MAX_MAPPING_ENTRIES + 1) {
            let secret = format!("AKIA{i:016}");
            let out = redactor.redact(&secret);
            // Redaction itself still happens correctly regardless of the cap.
            assert!(!out.contains(&secret), "{out}");
        }
        assert!(
            redactor.mapping_len() <= MAX_MAPPING_ENTRIES,
            "mapping grew past its cap: {}",
            redactor.mapping_len()
        );
    }

    /// A local mirror of `tm_provider::decide::{DecideRequest, Question}`'s serde shape (tag
    /// `"type"`, `rename_all = "snake_case"`), used only so this test can exercise
    /// `redact_decide_request` without `tm-auth` depending on `tm-provider` (which would close a
    /// dependency cycle — see that method's own doc comment). The real `DecideRequest`'s outbound
    /// body never containing a raw secret is proven where the HTTP `systemone` provider actually
    /// calls this method, in `tm-provider`'s own tests.
    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    struct MockDecideRequest {
        model: String,
        state: String,
        questions: std::collections::BTreeMap<String, MockQuestion>,
    }

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum MockQuestion {
        Choice { instructions: String },
        Noul { statement: String },
    }

    /// D-020 decision 7: a `DecideRequest`'s `state` and each `Question`'s own text must come
    /// out redacted the same way [`SessionRedactor::redact`] would scrub the identical string in
    /// a normal turn, before the request reaches a remote `DecisionProvider`.
    #[test]
    fn redact_decide_request_scrubs_state_and_question_text() {
        use std::collections::BTreeMap;

        let mut questions = BTreeMap::new();
        questions.insert(
            "q1".to_string(),
            MockQuestion::Choice {
                instructions: format!("Does this look risky? {OPENAI_KEY}"),
            },
        );
        questions.insert(
            "q2".to_string(),
            MockQuestion::Noul {
                statement: format!("The state contains {AWS_KEY}"),
            },
        );
        let request = MockDecideRequest {
            model: "systemone/jev-latest".to_string(),
            state: format!("ticket objective: rotate {GITHUB_TOKEN} now"),
            questions,
        };

        let redactor = SessionRedactor::new();
        let redacted = redactor
            .redact_decide_request(&request)
            .expect("MockDecideRequest round-trips through serde_json::Value");

        assert_eq!(
            redacted.state,
            redactor.redact(&request.state),
            "state must be redacted identically to a normal SessionRedactor::redact call"
        );
        assert!(!redacted.state.contains(GITHUB_TOKEN), "{}", redacted.state);

        let MockQuestion::Choice { instructions } = &redacted.questions["q1"] else {
            panic!("expected MockQuestion::Choice to survive the round-trip");
        };
        assert!(!instructions.contains(OPENAI_KEY), "{instructions}");
        assert!(
            instructions.contains("<redacted:api_key:"),
            "{instructions}"
        );

        let MockQuestion::Noul { statement } = &redacted.questions["q2"] else {
            panic!("expected MockQuestion::Noul to survive the round-trip");
        };
        assert!(!statement.contains(AWS_KEY), "{statement}");
        assert!(statement.contains("<redacted:aws_key:"), "{statement}");

        // Unrelated fields (model, with no secret) pass through unchanged.
        assert_eq!(redacted.model, request.model);
    }
}
