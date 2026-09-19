//! `{{param}}` template substitution for `NodeDef::objective`.
//!
//! A small, local `{{...}}` scanner rather than a templating crate or a dependency on
//! `tm-templates` (B-15's project-template substitution, a sibling audit track): the two crates
//! solve different problems (`tm-templates` substitutes into scaffolded *files*, potentially
//! landing concurrently with this one) and a real dependency edge between two same-day tracks
//! would be a build-order hazard for no shared logic -- both substitutions are "find `{{x}}`,
//! look `x` up, replace" over a flat string, small enough that duplicating the ~20 lines is
//! simpler than coordinating a shared helper crate neither track otherwise needs.
//!
//! Three token forms, resolved against `params` and the current fan-out `item` (`None` outside a
//! fanned-out node):
//!
//! - `{{param_name}}`: looked up in `params`.
//! - `{{item}}`: the current fan-out item's raw text (`item` must be `Some`).
//! - `{{item.field}}`: `item`'s text is parsed as JSON; `field` is read off it as a string (or,
//!   if not a string, rendered via its own `Display`/JSON text). `SPEC.md` §25.2's own example --
//!   `objective = "Try to refute this finding: {{item.summary}}"` -- fans out over structured
//!   findings, not bare strings, so this is required for that example to mean anything.

use std::collections::BTreeMap;

use tm_types::{Result as TmResult, TmError};

/// Render `template`, substituting every `{{...}}` token against `params` and, when fanning out,
/// `item`.
///
/// # Errors
/// `TmError::parse` naming the offending token for an unclosed `{{`, an unknown parameter, an
/// `{{item...}}` token with no `item` in scope, or an `{{item.field}}` token whose `item` text
/// does not parse as JSON or has no such field.
pub fn render(
    template: &str,
    params: &BTreeMap<String, String>,
    item: Option<&str>,
) -> TmResult<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    loop {
        let Some(start) = rest.find("{{") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            return Err(TmError::parse(format!(
                "workflow template: unclosed {{{{ in {template:?}"
            )));
        };
        let token = after_open[..end].trim();
        out.push_str(&resolve_token(token, template, params, item)?);
        rest = &after_open[end + 2..];
    }
    Ok(out)
}

fn resolve_token(
    token: &str,
    template: &str,
    params: &BTreeMap<String, String>,
    item: Option<&str>,
) -> TmResult<String> {
    if token == "item" {
        return item.map(str::to_string).ok_or_else(|| {
            TmError::parse(format!(
                "workflow template: {{{{item}}}} used outside a fanned-out node, in {template:?}"
            ))
        });
    }
    if let Some(field) = token.strip_prefix("item.") {
        let item = item.ok_or_else(|| {
            TmError::parse(format!(
                "workflow template: {{{{item.{field}}}}} used outside a fanned-out node, in {template:?}"
            ))
        })?;
        let parsed: serde_json::Value = serde_json::from_str(item).map_err(|e| {
            TmError::parse(format!(
                "workflow template: {{{{item.{field}}}}} needs a JSON fan-out item, got {item:?} ({e})"
            ))
        })?;
        let value = parsed.get(field).ok_or_else(|| {
            TmError::parse(format!(
                "workflow template: fan-out item {item:?} has no field {field:?}"
            ))
        })?;
        return Ok(match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    }
    params.get(token).cloned().ok_or_else(|| {
        TmError::parse(format!(
            "workflow template: unknown parameter {{{{{token}}}}} in {template:?}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_a_plain_param() {
        let out = render(
            "Review {{target}}.",
            &params(&[("target", "src/lib.rs")]),
            None,
        )
        .expect("substitution succeeds");
        assert_eq!(out, "Review src/lib.rs.");
    }

    #[test]
    fn substitutes_item_in_a_fanned_out_node() {
        let out = render("Check {{item}}.", &BTreeMap::new(), Some("security"))
            .expect("substitution succeeds");
        assert_eq!(out, "Check security.");
    }

    #[test]
    fn substitutes_a_structured_item_field() {
        let item = r#"{"summary": "unchecked index", "severity": "high"}"#;
        let out = render("Refute: {{item.summary}}", &BTreeMap::new(), Some(item))
            .expect("substitution succeeds");
        assert_eq!(out, "Refute: unchecked index");
    }

    #[test]
    fn errors_on_unknown_param() {
        let err = render("{{nope}}", &BTreeMap::new(), None).unwrap_err();
        assert!(err.to_string().contains("unknown parameter"));
    }

    #[test]
    fn errors_on_item_outside_fan_out() {
        let err = render("{{item}}", &BTreeMap::new(), None).unwrap_err();
        assert!(err.to_string().contains("outside a fanned-out node"));
    }

    #[test]
    fn errors_on_unclosed_token() {
        let err = render("{{oops", &BTreeMap::new(), None).unwrap_err();
        assert!(err.to_string().contains("unclosed"));
    }

    #[test]
    fn passes_through_text_with_no_tokens() {
        let out = render("plain text", &BTreeMap::new(), None).expect("no tokens");
        assert_eq!(out, "plain text");
    }
}
