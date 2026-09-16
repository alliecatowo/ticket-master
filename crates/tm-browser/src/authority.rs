//! Authority gating for navigation and downloads, plus the session trace that makes a browser
//! session's actions usable as ticket evidence.
//!
//! Every navigation and download a [`crate::session::BrowserSession`] performs is checked
//! against `tm_types::Authority.network` before it happens: `arbitrary: false` with an
//! allowlist lets an agent drive `localhost:7777` for a smoke test but never wander onto the
//! open internet (`SPEC.md` §19.4). Every check, success or denial, is appended to a
//! [`SessionTrace`] alongside the actions the session performs, so "I verified the page works"
//! is a claim backed by a replayable log rather than an unverifiable assertion.

use serde::{Deserialize, Serialize};
use tm_types::{Authority, Result, SessionId, Timestamp};

/// Checks navigation and download URLs against an [`Authority`]'s network grant.
///
/// Holds a borrow rather than an owned `Authority` because it is expected to be constructed
/// fresh (cheaply) from whatever authority a [`crate::session::BrowserSession`] was leased,
/// each time a check is needed — it carries no state of its own.
#[derive(Debug, Clone, Copy)]
pub struct NavigationGuard<'a> {
    authority: &'a Authority,
}

impl<'a> NavigationGuard<'a> {
    /// Gate checks against `authority.network`.
    pub fn new(authority: &'a Authority) -> Self {
        NavigationGuard { authority }
    }

    /// Check whether navigating to `url` is permitted.
    ///
    /// # Errors
    /// [`tm_types::TmError::AuthorityDenied`] when `url`'s host is not covered by
    /// `docs`, `arbitrary` or the `allowlist`.
    pub fn check_navigate(&self, url: &str) -> Result<()> {
        if self.authority.network.permits_url(url) {
            Ok(())
        } else {
            let host = extract_host(url).unwrap_or("<unknown host>");
            Err(tm_types::TmError::AuthorityDenied(format!(
                "navigation to {} denied",
                host
            )))
        }
    }

    /// Check whether downloading from `url` is permitted.
    ///
    /// # Errors
    /// Same as [`NavigationGuard::check_navigate`].
    pub fn check_download(&self, url: &str) -> Result<()> {
        if self.authority.network.permits_url(url) {
            Ok(())
        } else {
            let host = extract_host(url).unwrap_or("<unknown host>");
            Err(tm_types::TmError::AuthorityDenied(format!(
                "download from {} denied",
                host
            )))
        }
    }
}

/// Extract the host from a URL using the same logic as [`tm_types::authority::NetworkAuthority`].
fn extract_host(url: &str) -> Option<&str> {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split('/').next()?;
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_types::Authority;

    #[test]
    fn navigation_guard_permits_allowed_navigation() {
        let mut auth = Authority::default();
        auth.network.arbitrary = true;
        let guard = NavigationGuard::new(&auth);
        assert!(guard.check_navigate("https://example.com").is_ok());
        assert!(guard.check_navigate("http://localhost:7777").is_ok());
    }

    #[test]
    fn navigation_guard_denies_disallowed_navigation() {
        let auth = Authority::default();
        let guard = NavigationGuard::new(&auth);
        let result = guard.check_navigate("https://example.com");
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(
                e.to_string(),
                "authority denied: navigation to example.com denied"
            );
        }
    }

    #[test]
    fn navigation_guard_permits_docs_hosts() {
        let mut auth = Authority::default();
        auth.network.docs = true;
        let guard = NavigationGuard::new(&auth);
        assert!(guard.check_navigate("https://docs.rs/serde").is_ok());
        assert!(guard
            .check_navigate("https://doc.rust-lang.org/std")
            .is_ok());
        assert!(guard
            .check_navigate("https://developer.mozilla.org/en-US")
            .is_ok());
    }

    #[test]
    fn navigation_guard_permits_allowlisted_hosts() {
        let mut auth = Authority::default();
        auth.network.allowlist.insert("localhost".into());
        let guard = NavigationGuard::new(&auth);
        assert!(guard.check_navigate("http://localhost:7777").is_ok());
        assert!(guard.check_navigate("http://localhost:8080/path").is_ok());
    }

    #[test]
    fn navigation_guard_permits_subdomains_of_allowlisted_host() {
        let mut auth = Authority::default();
        auth.network.allowlist.insert("example.com".into());
        let guard = NavigationGuard::new(&auth);
        assert!(guard.check_navigate("https://api.example.com/v1").is_ok());
        assert!(guard.check_navigate("https://sub.api.example.com").is_ok());
    }

    #[test]
    fn navigation_guard_extracts_host_from_url_with_port() {
        let mut auth = Authority::default();
        auth.network.allowlist.insert("example.com".into());
        let guard = NavigationGuard::new(&auth);
        assert!(guard.check_navigate("https://example.com:443/path").is_ok());
    }

    #[test]
    fn navigation_guard_extracts_host_from_url_with_auth() {
        let mut auth = Authority::default();
        auth.network.allowlist.insert("example.com".into());
        let guard = NavigationGuard::new(&auth);
        assert!(guard
            .check_navigate("https://user:pass@example.com/path")
            .is_ok());
    }

    #[test]
    fn download_guard_uses_same_authority_as_navigation() {
        let mut auth = Authority::default();
        auth.network.allowlist.insert("example.com".into());
        let guard = NavigationGuard::new(&auth);

        assert!(guard.check_download("https://example.com/file.bin").is_ok());
        assert!(guard.check_download("https://other.com/file.bin").is_err());
    }

    #[test]
    fn download_guard_denies_disallowed_download() {
        let auth = Authority::default();
        let guard = NavigationGuard::new(&auth);
        let result = guard.check_download("https://evil.com/payload");
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(
                e.to_string(),
                "authority denied: download from evil.com denied"
            );
        }
    }

    #[test]
    fn session_trace_new_has_no_events() {
        let session_id = tm_types::SessionId::new("S-1").unwrap();
        let trace = SessionTrace::new(session_id.clone());
        assert_eq!(trace.session, session_id);
        assert!(trace.events.is_empty());
    }

    #[test]
    fn session_trace_records_events_in_order() {
        let session_id = tm_types::SessionId::new("S-1").unwrap();
        let mut trace = SessionTrace::new(session_id);
        let ts = tm_types::Timestamp::from_unix_seconds(1000);

        trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Click,
            reference: Some("button".into()),
            at: ts,
        });
        trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Type,
            reference: None,
            at: ts,
        });

        assert_eq!(trace.events.len(), 2);
    }

    #[test]
    fn has_denials_returns_false_when_no_denials() {
        let session_id = tm_types::SessionId::new("S-1").unwrap();
        let mut trace = SessionTrace::new(session_id);
        let ts = tm_types::Timestamp::from_unix_seconds(1000);

        trace.record(TraceEvent::Navigated {
            url: "https://example.com".into(),
            at: ts,
        });
        trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Click,
            reference: None,
            at: ts,
        });

        assert!(!trace.has_denials());
    }

    #[test]
    fn has_denials_returns_true_when_navigation_blocked() {
        let session_id = tm_types::SessionId::new("S-1").unwrap();
        let mut trace = SessionTrace::new(session_id);
        let ts = tm_types::Timestamp::from_unix_seconds(1000);

        trace.record(TraceEvent::Navigated {
            url: "https://allowed.com".into(),
            at: ts,
        });
        trace.record(TraceEvent::NavigationBlocked {
            url: "https://denied.com".into(),
            reason: "host not in allowlist".into(),
            at: ts,
        });

        assert!(trace.has_denials());
    }

    #[test]
    fn has_denials_returns_true_when_download_blocked() {
        let session_id = tm_types::SessionId::new("S-1").unwrap();
        let mut trace = SessionTrace::new(session_id);
        let ts = tm_types::Timestamp::from_unix_seconds(1000);

        trace.record(TraceEvent::DownloadBlocked {
            url: "https://denied.com/file.bin".into(),
            reason: "host not in allowlist".into(),
            at: ts,
        });

        assert!(trace.has_denials());
    }

    #[test]
    fn to_evidence_json_serializes_empty_trace() {
        let session_id = tm_types::SessionId::new("S-1").unwrap();
        let trace = SessionTrace::new(session_id);

        let json = trace.to_evidence_json().unwrap();
        assert!(json.is_object());
        assert!(json.get("session").is_some());
        assert!(json.get("events").is_some());

        let events = json.get("events").unwrap();
        assert!(events.is_array());
        assert_eq!(events.as_array().unwrap().len(), 0);
    }

    #[test]
    fn to_evidence_json_serializes_trace_with_events() {
        let session_id = tm_types::SessionId::new("S-1").unwrap();
        let mut trace = SessionTrace::new(session_id);
        let ts = tm_types::Timestamp::from_unix_seconds(1000);

        trace.record(TraceEvent::Navigated {
            url: "https://example.com".into(),
            at: ts,
        });
        trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Click,
            reference: Some("button-1".into()),
            at: ts,
        });

        let json = trace.to_evidence_json().unwrap();
        let events = json.get("events").unwrap().as_array().unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn extract_host_handles_simple_url() {
        assert_eq!(
            extract_host("https://example.com/path"),
            Some("example.com")
        );
    }

    #[test]
    fn extract_host_handles_url_with_port() {
        assert_eq!(
            extract_host("https://example.com:443/path"),
            Some("example.com")
        );
    }

    #[test]
    fn extract_host_handles_url_with_auth() {
        assert_eq!(
            extract_host("https://user:pass@example.com/path"),
            Some("example.com")
        );
    }

    #[test]
    fn extract_host_handles_url_without_protocol() {
        assert_eq!(extract_host("example.com/path"), Some("example.com"));
    }

    #[test]
    fn extract_host_handles_localhost_with_port() {
        assert_eq!(extract_host("http://localhost:7777/"), Some("localhost"));
    }

    #[test]
    fn extract_host_handles_empty_url() {
        assert_eq!(extract_host(""), None);
    }

    #[test]
    fn navigation_guard_creates_error_with_proper_host() {
        let auth = Authority::default();
        let guard = NavigationGuard::new(&auth);

        let result = guard.check_navigate("https://evil.example.com/attack");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("evil.example.com"));
    }

    #[test]
    fn trace_events_serialize_all_variants() {
        let session_id = tm_types::SessionId::new("S-1").unwrap();
        let mut trace = SessionTrace::new(session_id);
        let ts = tm_types::Timestamp::from_unix_seconds(1000);

        trace.record(TraceEvent::Navigated {
            url: "https://example.com".into(),
            at: ts,
        });
        trace.record(TraceEvent::NavigationBlocked {
            url: "https://denied.com".into(),
            reason: "denied".into(),
            at: ts,
        });
        trace.record(TraceEvent::DownloadBlocked {
            url: "https://denied.com/file".into(),
            reason: "denied".into(),
            at: ts,
        });
        trace.record(TraceEvent::ActionPerformed {
            action: ActionKind::Click,
            reference: Some("ref".into()),
            at: ts,
        });
        trace.record(TraceEvent::ConsoleError {
            text: "error text".into(),
            at: ts,
        });

        let json = trace.to_evidence_json().unwrap();
        let events = json.get("events").unwrap().as_array().unwrap();
        assert_eq!(events.len(), 5);
    }
}

/// One entry in a [`SessionTrace`]: everything worth replaying to judge whether a browser
/// session's claims about a page are true.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TraceEvent {
    /// A navigation completed.
    Navigated {
        /// The URL navigated to.
        url: String,
        /// When the navigation completed.
        at: Timestamp,
    },
    /// A navigation was refused by [`NavigationGuard::check_navigate`].
    NavigationBlocked {
        /// The URL that was refused.
        url: String,
        /// The denial reason.
        reason: String,
        /// When the refusal happened.
        at: Timestamp,
    },
    /// A download was refused by [`NavigationGuard::check_download`].
    DownloadBlocked {
        /// The URL that was refused.
        url: String,
        /// The denial reason.
        reason: String,
        /// When the refusal happened.
        at: Timestamp,
    },
    /// An agent action (click/hover/type/select/press/eval) was performed.
    ActionPerformed {
        /// Which kind of action.
        action: ActionKind,
        /// The ref it targeted, for ref-addressed actions.
        reference: Option<String>,
        /// When the action was performed.
        at: Timestamp,
    },
    /// The page emitted a console error while the session was open.
    ConsoleError {
        /// The console message text.
        text: String,
        /// When it was observed.
        at: Timestamp,
    },
}

/// The kind of action an [`TraceEvent::ActionPerformed`] entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// `BrowserSession::click`.
    Click,
    /// `BrowserSession::hover`.
    Hover,
    /// `BrowserSession::type_text`.
    Type,
    /// `BrowserSession::select`.
    Select,
    /// `BrowserSession::press`.
    Press,
    /// `BrowserSession::eval`.
    Eval,
}

/// A replayable, append-only record of one [`crate::session::BrowserSession`]'s navigations,
/// actions and authority decisions — attachable to a ticket as verification evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTrace {
    /// The session this trace belongs to.
    pub session: SessionId,
    /// Every recorded event, in the order it happened.
    pub events: Vec<TraceEvent>,
}

impl SessionTrace {
    /// An empty trace for a freshly opened session.
    pub fn new(session: SessionId) -> Self {
        SessionTrace {
            session,
            events: Vec::new(),
        }
    }

    /// Append an event. Traces are append-only: there is no removal API, so a trace attached
    /// as evidence cannot be quietly edited down after the fact.
    pub fn record(&mut self, event: TraceEvent) {
        self.events.push(event);
    }

    /// `true` when any recorded event is a denial ([`TraceEvent::NavigationBlocked`] or
    /// [`TraceEvent::DownloadBlocked`]).
    pub fn has_denials(&self) -> bool {
        self.events.iter().any(|event| {
            matches!(
                event,
                TraceEvent::NavigationBlocked { .. } | TraceEvent::DownloadBlocked { .. }
            )
        })
    }

    /// Render this trace as a JSON value suitable for storage as ticket evidence (via an
    /// artifact, per `SPEC.md` §8.2, for a trace long enough to warrant one).
    pub fn to_evidence_json(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self)
            .map_err(|e| tm_types::TmError::invariant(format!("failed to serialize trace: {}", e)))
    }
}
