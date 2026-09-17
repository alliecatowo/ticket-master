//! Jira Cloud REST v3 adapter: issues, epics, comments, and transitions.
//!
//! Owns: Jira issue lifecycle (create, update state via transitions), epic hierarchy,
//! comment threading, and state resolution. Credentials via `JIRA_EMAIL` and `JIRA_API_TOKEN`
//! environment variables. Recorded JSON only—no network calls; all I/O is test-doubled.
//!
//! State resolution: Jira constrains reachable states by workflow; this adapter resolves a
//! target state hint to an allowed transition or reports that it cannot be reached.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_types::{Result, Timestamp};

use crate::projection::Projection;
use crate::tracker::{ExternalChange, ExternalRef, Tracker, TrackerCapabilities};

/// A Jira Cloud adapter instance, backed by REST v3 APIs.
///
/// Handles issues, epics, comments, and state transitions. Workflow states are constrained
/// by Jira's transition model; this adapter resolves hints onto reachable states or reports
/// that a transition is unavailable.
#[derive(Debug)]
pub struct JiraTracker {
    name: String,
    email: String,
    api_token: String,
}

impl JiraTracker {
    /// Build a Jira adapter from environment variables: `JIRA_EMAIL` and `JIRA_API_TOKEN`.
    ///
    /// # Errors
    /// Returns an error if either environment variable is missing or empty.
    pub fn from_env(name: impl Into<String>) -> Result<Self> {
        let email = std::env::var("JIRA_EMAIL")
            .map_err(|_| tm_types::TmError::invariant("mirror: env var JIRA_EMAIL is not set"))?;
        let api_token = std::env::var("JIRA_API_TOKEN").map_err(|_| {
            tm_types::TmError::invariant("mirror: env var JIRA_API_TOKEN is not set")
        })?;

        if email.is_empty() {
            return Err(tm_types::TmError::invariant("mirror: JIRA_EMAIL is empty"));
        }
        if api_token.is_empty() {
            return Err(tm_types::TmError::invariant(
                "mirror: JIRA_API_TOKEN is empty",
            ));
        }

        Ok(JiraTracker {
            name: name.into(),
            email,
            api_token,
        })
    }

    /// Build a Jira adapter with explicit credentials for testing.
    #[cfg(test)]
    fn new_test(
        name: impl Into<String>,
        email: impl Into<String>,
        api_token: impl Into<String>,
    ) -> Self {
        JiraTracker {
            name: name.into(),
            email: email.into(),
            api_token: api_token.into(),
        }
    }

    /// Resolve a target state to an allowed transition, or report that it cannot be reached.
    ///
    /// Jira workflow transitions are restricted by the current state and transition rules.
    /// This method consults the workflow (mocked in test mode) to determine if the target
    /// state is reachable via a valid transition.
    fn resolve_transition(&self, current_state: &str, target_state: &str) -> Result<String> {
        // In recorded JSON mode, we check against a hard-coded set of valid transitions.
        // In production, this would query the Jira workflow schema.
        let valid_transitions = match current_state.to_lowercase().as_str() {
            "open" | "to do" => vec!["in progress", "done", "closed"],
            "in progress" | "in review" => vec!["open", "done", "closed"],
            "done" | "closed" => vec!["open", "in progress"],
            _ => vec![],
        };

        if valid_transitions
            .iter()
            .any(|t| t.eq_ignore_ascii_case(target_state))
        {
            Ok(target_state.to_string())
        } else {
            Err(tm_types::TmError::invariant(format!(
                "jira: cannot transition from {current_state:?} to {target_state:?}"
            )))
        }
    }
}

#[async_trait]
impl Tracker for JiraTracker {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> TrackerCapabilities {
        TrackerCapabilities {
            // Jira Cloud has sub-tasks, which map to parent/child.
            parent_child: true,
            // Jira workflows are limited; states must be mapped via the transition model.
            arbitrary_states: false,
            // Jira has sprint/iteration concepts that map to milestones.
            milestones: true,
            // Jira has labels (called "labels" field on issues).
            labels: true,
            // Jira has threaded comments on issues.
            comments: true,
            // Jira Cloud REST v3 issue body field has a practical limit; we use 64 KiB.
            max_body_bytes: 65536,
        }
    }

    async fn push(&self, projection: &Projection) -> Result<ExternalRef> {
        // In recorded JSON mode, we record the push (for test doubles) but don't
        // make a network call. The result is a synthetic external reference.
        // Production implementations would POST to /rest/api/3/issues or PUT to
        // /rest/api/3/issues/{key}, then return the created/updated issue key and URL.
        if self.email.is_empty() || self.api_token.is_empty() {
            return Err(tm_types::TmError::invariant("jira: missing credentials"));
        }

        // Every ticket lands in Jira's default workflow entry state on creation. Only a hint
        // that asks for something else needs an actual transition, resolved through the
        // workflow's reachability table so an unreachable target fails before we push at all.
        const INITIAL_STATE: &str = "open";
        if !projection.state_hint.eq_ignore_ascii_case(INITIAL_STATE) {
            self.resolve_transition(INITIAL_STATE, &projection.state_hint)?;
        }

        let issue_key = format!("{}_{}", self.name, projection.ticket);
        let url = Some(format!("https://jira.atlassian.net/browse/{}", issue_key));

        Ok(ExternalRef {
            adapter: self.name.clone(),
            external_id: issue_key,
            url,
        })
    }

    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>> {
        // In recorded JSON mode, pull returns an empty set; production would query
        // /rest/api/3/issues with a JQL filter like `updated >= :since`.
        // The recorded mode is sufficient for push/pull idempotence tests via
        // SyncEngine, which couples with RecordingTracker for scripted changes.
        let _ = since;
        Ok(Vec::new())
    }
}

/// Jira issue JSON shape (partial; production would be more complete).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JiraIssue {
    /// Issue key (e.g. "PROJ-123").
    pub key: String,
    /// Issue summary (title).
    pub fields: JiraIssueFields,
}

/// Jira issue fields (partial; for testing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JiraIssueFields {
    /// Issue summary (title).
    pub summary: String,
    /// Issue description (body).
    pub description: Option<String>,
    /// Current workflow state.
    pub status: Option<JiraStatus>,
    /// Issue labels.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Assignee, if any.
    pub assignee: Option<JiraUser>,
}

/// Jira workflow status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JiraStatus {
    /// Status name (e.g. "Open", "In Progress").
    pub name: String,
}

/// Jira user reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JiraUser {
    /// User display name.
    #[serde(rename = "displayName")]
    pub display_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `from_env` reads process-wide env vars, so tests that set/clear them must not
    // interleave with each other (the test harness runs tests on multiple threads).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn jira_tracker_from_env_valid() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("JIRA_EMAIL", "user@example.com");
        std::env::set_var("JIRA_API_TOKEN", "token123");

        let tracker = JiraTracker::from_env("jira_prod").expect("should create tracker");
        assert_eq!(tracker.name(), "jira_prod");
        assert_eq!(tracker.email, "user@example.com");
        assert_eq!(tracker.api_token, "token123");

        std::env::remove_var("JIRA_EMAIL");
        std::env::remove_var("JIRA_API_TOKEN");
    }

    #[test]
    fn jira_tracker_from_env_missing_email() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("JIRA_EMAIL");
        std::env::set_var("JIRA_API_TOKEN", "token123");

        let result = JiraTracker::from_env("jira_prod");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("JIRA_EMAIL"));

        std::env::remove_var("JIRA_API_TOKEN");
    }

    #[test]
    fn jira_tracker_from_env_missing_token() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("JIRA_EMAIL", "user@example.com");
        std::env::remove_var("JIRA_API_TOKEN");

        let result = JiraTracker::from_env("jira_prod");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("JIRA_API_TOKEN"));

        std::env::remove_var("JIRA_EMAIL");
    }

    #[test]
    fn jira_tracker_from_env_empty_email() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("JIRA_EMAIL", "");
        std::env::set_var("JIRA_API_TOKEN", "token123");

        let result = JiraTracker::from_env("jira_prod");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));

        std::env::remove_var("JIRA_EMAIL");
        std::env::remove_var("JIRA_API_TOKEN");
    }

    #[test]
    fn jira_tracker_from_env_empty_token() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("JIRA_EMAIL", "user@example.com");
        std::env::set_var("JIRA_API_TOKEN", "");

        let result = JiraTracker::from_env("jira_prod");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));

        std::env::remove_var("JIRA_EMAIL");
        std::env::remove_var("JIRA_API_TOKEN");
    }

    #[test]
    fn jira_tracker_capabilities() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");
        let caps = tracker.capabilities();

        assert!(caps.parent_child);
        assert!(!caps.arbitrary_states);
        assert!(caps.milestones);
        assert!(caps.labels);
        assert!(caps.comments);
        assert_eq!(caps.max_body_bytes, 65536);
    }

    #[tokio::test]
    async fn jira_tracker_push_happy_path() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");
        let projection = Projection {
            ticket: "T-123".parse().expect("valid ticket id"),
            title: "Test Issue".to_string(),
            body: "Issue description".to_string(),
            state_hint: "open".to_string(),
            labels: vec!["bug".to_string(), "urgent".to_string()],
            milestone: Some("v1.0".to_string()),
            checklist: vec![],
            degradations: vec![],
        };

        let result = tracker.push(&projection).await;
        assert!(result.is_ok());

        let external_ref = result.unwrap();
        assert_eq!(external_ref.adapter, "jira");
        assert_eq!(external_ref.external_id, "jira_T-123");
        assert!(external_ref.url.is_some());
        assert!(external_ref.url.unwrap().contains("jira.atlassian.net"));
    }

    #[tokio::test]
    async fn jira_tracker_pull_empty() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");
        let since = Timestamp::EPOCH.plus_millis(1000);

        let result = tracker.pull(since).await;
        assert!(result.is_ok());

        let changes = result.unwrap();
        assert_eq!(changes.len(), 0);
    }

    #[test]
    fn jira_tracker_resolve_transition_from_open_to_in_progress() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");

        let result = tracker.resolve_transition("open", "in progress");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "in progress");
    }

    #[test]
    fn jira_tracker_resolve_transition_from_open_to_done() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");

        let result = tracker.resolve_transition("open", "done");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "done");
    }

    #[test]
    fn jira_tracker_resolve_transition_from_in_progress_to_done() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");

        let result = tracker.resolve_transition("in progress", "done");
        assert!(result.is_ok());
    }

    #[test]
    fn jira_tracker_resolve_transition_invalid() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");

        let result = tracker.resolve_transition("open", "deleted");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot transition"));
    }

    #[test]
    fn jira_tracker_resolve_transition_case_insensitive() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");

        let result = tracker.resolve_transition("Open", "IN PROGRESS");
        assert!(result.is_ok());
    }

    #[test]
    fn jira_tracker_resolve_transition_from_in_review_to_closed() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");

        let result = tracker.resolve_transition("in review", "closed");
        assert!(result.is_ok());
    }

    #[test]
    fn jira_tracker_resolve_transition_from_done_back_to_open() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");

        let result = tracker.resolve_transition("done", "open");
        assert!(result.is_ok());
    }

    #[test]
    fn jira_tracker_resolve_transition_unknown_current_state() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");

        let result = tracker.resolve_transition("unknown_state", "done");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("cannot transition"));
    }

    #[tokio::test]
    async fn jira_tracker_push_empty_projection() {
        let tracker = JiraTracker::new_test("jira", "user@example.com", "token123");
        let projection = Projection {
            ticket: "T-001".parse().expect("valid ticket id"),
            title: "".to_string(),
            body: "".to_string(),
            state_hint: "open".to_string(),
            labels: vec![],
            milestone: None,
            checklist: vec![],
            degradations: vec![],
        };

        let result = tracker.push(&projection).await;
        assert!(result.is_ok());
        let external_ref = result.unwrap();
        assert_eq!(external_ref.external_id, "jira_T-001");
    }

    #[test]
    fn jira_issue_fields_serialization() {
        let fields = JiraIssueFields {
            summary: "Test Issue".to_string(),
            description: Some("Description text".to_string()),
            status: Some(JiraStatus {
                name: "Open".to_string(),
            }),
            labels: vec!["label1".to_string(), "label2".to_string()],
            assignee: Some(JiraUser {
                display_name: "Alice".to_string(),
            }),
        };

        let json = serde_json::to_string(&fields).expect("should serialize");
        assert!(json.contains("Test Issue"));
        assert!(json.contains("Description text"));
        assert!(json.contains("Open"));
        assert!(json.contains("label1"));
        assert!(json.contains("Alice"));
    }

    #[test]
    fn jira_issue_fields_deserialization() {
        let json = r#"{
            "summary": "Test Issue",
            "description": "Description",
            "status": { "name": "In Progress" },
            "labels": ["bug", "urgent"],
            "assignee": { "displayName": "Bob" }
        }"#;

        let fields: JiraIssueFields = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(fields.summary, "Test Issue");
        assert_eq!(fields.description, Some("Description".to_string()));
        assert_eq!(fields.status.unwrap().name, "In Progress");
        assert_eq!(fields.labels, vec!["bug", "urgent"]);
        assert_eq!(fields.assignee.unwrap().display_name, "Bob");
    }

    #[test]
    fn jira_issue_fields_no_optional_fields() {
        let json = r#"{
            "summary": "Minimal Issue",
            "description": null,
            "status": null,
            "labels": [],
            "assignee": null
        }"#;

        let fields: JiraIssueFields = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(fields.summary, "Minimal Issue");
        assert_eq!(fields.description, None);
        assert_eq!(fields.status, None);
        assert!(fields.labels.is_empty());
        assert_eq!(fields.assignee, None);
    }

    #[test]
    fn jira_issue_serialization() {
        let issue = JiraIssue {
            key: "PROJ-456".to_string(),
            fields: JiraIssueFields {
                summary: "Issue Title".to_string(),
                description: Some("Issue description".to_string()),
                status: Some(JiraStatus {
                    name: "Done".to_string(),
                }),
                labels: vec!["resolved".to_string()],
                assignee: Some(JiraUser {
                    display_name: "Charlie".to_string(),
                }),
            },
        };

        let json = serde_json::to_string(&issue).expect("should serialize");
        assert!(json.contains("PROJ-456"));
        assert!(json.contains("Issue Title"));
    }

    #[test]
    fn jira_issue_deserialization() {
        let json = r#"{
            "key": "TEST-789",
            "fields": {
                "summary": "Test Issue",
                "description": "Test Description",
                "status": { "name": "Closed" },
                "labels": ["test"],
                "assignee": null
            }
        }"#;

        let issue: JiraIssue = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(issue.key, "TEST-789");
        assert_eq!(issue.fields.summary, "Test Issue");
        assert_eq!(issue.fields.status.unwrap().name, "Closed");
    }
}
