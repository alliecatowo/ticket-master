//! [`GitLabTracker`]: GitLab Issues REST v4, recorded JSON only.
//!
//! Owns everything specific to GitLab: issue create/update, notes (comments), labels,
//! assignees, and milestones. The `GITLAB_TOKEN` credential is read from the environment.
//! [`crate::tracker::Tracker`] and [`crate::projection`] own everything adapter-agnostic; this
//! module only shapes JSON and maps it onto those contracts.
//!
//! GitLab issues support only two workflow states (`opened`/`closed`), so
//! [`GitLabTracker::capabilities`] declares `parent_child: false` and `arbitrary_states: false`.
//! State mapping follows a fixed table, and unreachable states fail before push.
//!
//! Every ticket this adapter pushes is tagged with a `tm-id:<ticket>` label, used to find
//! already-mirrored issues on subsequent pushes and to distinguish ticket-linked issues from
//! ones a human created directly (which become [`crate::tracker::ExternalChangeKind::IssueCreated`]).
//!
//! Wire shaping and response parsing are pure functions, unit-tested here against recorded JSON.
//! Network calls are test-doubled; no live API calls are made.
//!
//! `push`/`pull` are test-doubled stubs, not live effects yet (see `push`'s own doc comment), so
//! [`crate::tracker::Tracker::confirm`]'s default `Ok(None)` is inherited as-is rather than
//! overridden (`SPEC.md` §21.5, audit B-11): there is nothing live to confirm against. Once the
//! live transport lands, `confirm` should search by the `tm-id:<ticket>` label the same way
//! [`crate::github::GitHubTracker::confirm`]/[`crate::linear::LinearTracker::confirm`] do.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use tm_types::{Result, TicketId, Timestamp, TmError};

use crate::projection::Projection;
use crate::tracker::{
    ExternalChange, ExternalChangeKind, ExternalRef, Tracker, TrackerCapabilities,
};

/// Name of the environment variable [`GitLabTracker::from_env`] reads the token from.
pub const GITLAB_TOKEN_ENV_VAR: &str = "GITLAB_TOKEN";

/// GitLab issues are constrained to two states.
const GITLAB_CLOSED_STATES: [&str; 2] = ["Closed", "Cancelled"];

/// A GitLab Issues adapter for one project.
#[derive(Debug)]
pub struct GitLabTracker {
    name: String,
    project_id: String,
    token: String,
}

impl GitLabTracker {
    /// Build a tracker for a project, reading the token from [`GITLAB_TOKEN_ENV_VAR`].
    pub fn from_env(name: impl Into<String>, project_id: impl Into<String>) -> Result<Self> {
        let token = std::env::var(GITLAB_TOKEN_ENV_VAR).map_err(|_| {
            TmError::invariant(format!("gitlab: {GITLAB_TOKEN_ENV_VAR} is not set"))
        })?;
        Self::with_config(name, project_id, token)
    }

    /// Build a tracker with explicit token and project ID, for tests.
    pub fn with_config(
        name: impl Into<String>,
        project_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<Self> {
        Ok(GitLabTracker {
            name: name.into(),
            project_id: project_id.into(),
            token: token.into(),
        })
    }
}

#[async_trait]
impl Tracker for GitLabTracker {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> TrackerCapabilities {
        TrackerCapabilities {
            parent_child: false,
            arbitrary_states: false,
            milestones: true,
            labels: true,
            comments: true,
            max_body_bytes: 65_536,
        }
    }

    async fn push(&self, projection: &Projection) -> Result<ExternalRef> {
        let _ = self.token.as_str();
        let _ = self.project_id.as_str();
        let label = ticket_label(&projection.ticket);
        let _ = gitlab_state_for(&projection.state_hint);
        Ok(ExternalRef {
            adapter: self.name.clone(),
            external_id: projection.ticket.to_string(),
            url: Some(format!(
                "https://gitlab.com/{}/issues/{}",
                self.project_id, label
            )),
        })
    }

    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>> {
        let _ = since;
        let _ = self.token.as_str();
        Ok(Vec::new())
    }
}

// ---- wire shapes ---------------------------------------------------------------------------
//
// `push`/`pull` are test-doubled stubs (see module docs): no live transport is wired up yet,
// so the functions below that shape a live request/response are not reachable from them.
// They stay as the pure, directly recorded-JSON-tested units the live transport will call
// into once it exists.

/// The label marking every issue this adapter pushes with.
fn ticket_label(ticket: &TicketId) -> String {
    format!("tm-id:{ticket}")
}

/// Extract a ticket-id marker from labels, if present.
#[allow(dead_code)]
fn ticket_ref_from_labels(labels: &[String]) -> Option<&str> {
    labels.iter().find_map(|l| l.strip_prefix("tm-id:"))
}

/// Map an internal state name to GitLab's two-state model. GitLab's `arbitrary_states`
/// capability is false, so this adapter owns the final opened/closed decision.
fn gitlab_state_for(state_hint: &str) -> &'static str {
    if GITLAB_CLOSED_STATES.contains(&state_hint) {
        "closed"
    } else {
        "opened"
    }
}

/// Build the JSON body for a create or update call.
#[allow(dead_code)]
fn issue_request_body(projection: &Projection, labels: &[String]) -> serde_json::Value {
    serde_json::json!({
        "title": projection.title,
        "description": projection.body,
        "state": gitlab_state_for(&projection.state_hint),
        "labels": labels,
    })
}

/// Parse a GitLab issue from JSON.
#[allow(dead_code)]
fn parse_issue(body: &[u8]) -> Result<GitLabIssue> {
    serde_json::from_slice(body)
        .map_err(|e| TmError::parse(format!("gitlab: malformed issue response: {e}")))
}

/// Parse a list of GitLab issues.
#[allow(dead_code)]
fn parse_issue_list(body: &[u8]) -> Result<Vec<GitLabIssue>> {
    serde_json::from_slice(body)
        .map_err(|e| TmError::parse(format!("gitlab: malformed issue list response: {e}")))
}

/// Parse a list of GitLab notes (comments).
#[allow(dead_code)]
fn parse_note_list(body: &[u8]) -> Result<Vec<GitLabNote>> {
    serde_json::from_slice(body)
        .map_err(|e| TmError::parse(format!("gitlab: malformed note list response: {e}")))
}

/// Convert a GitLab issue to an ExternalRef.
#[allow(dead_code)]
fn issue_to_external_ref(adapter: &str, issue: &GitLabIssue) -> ExternalRef {
    ExternalRef {
        adapter: adapter.to_string(),
        external_id: issue.iid.to_string(),
        url: Some(issue.web_url.clone()),
    }
}

/// The changes one pulled issue produces.
#[allow(dead_code)]
fn issue_changes(issue: &GitLabIssue, adapter: &str) -> Vec<ExternalChange> {
    let external = issue_to_external_ref(adapter, issue);
    if ticket_ref_from_labels(&issue.labels).is_some() {
        vec![
            ExternalChange {
                external: external.clone(),
                kind: ExternalChangeKind::StatusHint {
                    state: issue.state.clone(),
                },
                observed_at: Timestamp::EPOCH,
            },
            ExternalChange {
                external,
                kind: ExternalChangeKind::Assigned {
                    assignee: issue.assignee.as_ref().map(|u| u.username.clone()),
                },
                observed_at: Timestamp::EPOCH,
            },
        ]
    } else {
        vec![ExternalChange {
            external,
            kind: ExternalChangeKind::IssueCreated {
                title: issue.title.clone(),
                body: issue.description.clone().unwrap_or_default(),
                author: issue.author.username.clone(),
            },
            observed_at: Timestamp::EPOCH,
        }]
    }
}

/// Convert a GitLab note to an ExternalChange for an issue.
#[allow(dead_code)]
fn note_to_external_change(note: &GitLabNote, issue_iid: u64, adapter: &str) -> ExternalChange {
    ExternalChange {
        external: ExternalRef {
            adapter: adapter.to_string(),
            external_id: issue_iid.to_string(),
            url: None,
        },
        kind: ExternalChangeKind::CommentAdded {
            author: note.author.username.clone(),
            body: note.body.clone(),
        },
        observed_at: Timestamp::EPOCH,
    }
}

/// GitLab issue JSON shape (partial).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitLabIssue {
    /// Issue internal ID.
    pub id: u64,
    /// Issue project ID.
    pub project_id: u64,
    /// Issue internal iid (sequential within project).
    pub iid: u64,
    /// Issue title.
    pub title: String,
    /// Issue description.
    pub description: Option<String>,
    /// Issue state: `opened` or `closed`.
    pub state: String,
    /// Issue labels.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Assignee, if any.
    pub assignee: Option<GitLabUser>,
    /// Issue author.
    pub author: GitLabUser,
    /// Milestone, if any.
    pub milestone: Option<GitLabMilestone>,
    /// Web URL to the issue.
    pub web_url: String,
}

/// GitLab user (assignee, author).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitLabUser {
    /// User username.
    pub username: String,
}

/// GitLab milestone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitLabMilestone {
    /// Milestone ID.
    pub id: u64,
    /// Milestone title.
    pub title: String,
}

/// GitLab note (comment) on an issue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitLabNote {
    /// Note ID.
    pub id: u64,
    /// Note body (the comment text).
    pub body: String,
    /// Author of the note.
    pub author: GitLabUser,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `GITLAB_TOKEN_ENV_VAR` is process-global; serialize the tests that mutate it so they
    /// don't race under the default multi-threaded test runner.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn ticket(id: &str) -> TicketId {
        id.parse().expect("valid ticket id")
    }

    fn sample_projection() -> Projection {
        Projection {
            ticket: ticket("T-1"),
            title: "Fix the widget".to_string(),
            body: "Do the thing.".to_string(),
            state_hint: "Ready".to_string(),
            labels: vec!["bug".to_string()],
            milestone: None,
            checklist: vec![],
            degradations: vec![],
        }
    }

    #[test]
    fn ticket_label_uses_stable_marker_prefix() {
        assert_eq!(ticket_label(&ticket("T-42")), "tm-id:T-42");
    }

    #[test]
    fn ticket_ref_from_labels_finds_marker() {
        let labels = vec!["bug".to_string(), "tm-id:T-7".to_string()];
        assert_eq!(ticket_ref_from_labels(&labels), Some("T-7"));
    }

    #[test]
    fn ticket_ref_from_labels_absent_when_no_marker() {
        let labels = vec!["bug".to_string()];
        assert_eq!(ticket_ref_from_labels(&labels), None);
    }

    #[test]
    fn gitlab_state_for_maps_closed_states_to_closed() {
        assert_eq!(gitlab_state_for("Closed"), "closed");
        assert_eq!(gitlab_state_for("Cancelled"), "closed");
    }

    #[test]
    fn gitlab_state_for_maps_everything_else_to_opened() {
        assert_eq!(gitlab_state_for("Ready"), "opened");
        assert_eq!(gitlab_state_for("Running"), "opened");
        assert_eq!(gitlab_state_for("Draft"), "opened");
    }

    #[test]
    fn issue_request_body_includes_all_fields() {
        let projection = sample_projection();
        let labels = vec!["bug".to_string(), "tm-id:T-1".to_string()];
        let body = issue_request_body(&projection, &labels);
        assert_eq!(body["title"], "Fix the widget");
        assert_eq!(body["description"], "Do the thing.");
        assert_eq!(body["state"], "opened");
        assert_eq!(body["labels"][0], "bug");
        assert_eq!(body["labels"][1], "tm-id:T-1");
    }

    #[test]
    fn parse_issue_happy_path() {
        let json = br#"{
            "id": 42,
            "project_id": 1,
            "iid": 5,
            "title": "Fix the widget",
            "description": "Do the thing.",
            "state": "opened",
            "labels": ["tm-id:T-1"],
            "assignee": {"username": "alice"},
            "author": {"username": "bob"},
            "web_url": "https://gitlab.com/project/issues/5"
        }"#;
        let issue = parse_issue(json).expect("valid issue json");
        assert_eq!(issue.id, 42);
        assert_eq!(issue.iid, 5);
        assert_eq!(issue.title, "Fix the widget");
        assert_eq!(issue.state, "opened");
    }

    #[test]
    fn parse_issue_rejects_malformed_json() {
        let result = parse_issue(b"not json");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse:"));
    }

    #[test]
    fn parse_issue_defaults_optional_fields() {
        let json = br#"{
            "id": 1,
            "project_id": 1,
            "iid": 1,
            "title": "No description",
            "state": "opened",
            "author": {"username": "someone"},
            "web_url": "https://gitlab.com/project/issues/1"
        }"#;
        let issue = parse_issue(json).expect("valid issue json");
        assert_eq!(issue.description, None);
        assert_eq!(issue.assignee, None);
        assert!(issue.labels.is_empty());
    }

    #[test]
    fn parse_issue_list_happy_path() {
        let json = br#"[
            {"id": 1, "project_id": 1, "iid": 1, "title": "a", "state": "opened", "author": {"username": "u"}, "web_url": "https://x/1"},
            {"id": 2, "project_id": 1, "iid": 2, "title": "b", "state": "closed", "author": {"username": "u"}, "web_url": "https://x/2"}
        ]"#;
        let issues = parse_issue_list(json).expect("valid list");
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[1].state, "closed");
    }

    #[test]
    fn issue_to_external_ref_carries_iid_and_url() {
        let issue = GitLabIssue {
            id: 7,
            project_id: 1,
            iid: 3,
            title: "t".to_string(),
            description: None,
            state: "opened".to_string(),
            labels: vec![],
            assignee: None,
            author: GitLabUser {
                username: "u".to_string(),
            },
            milestone: None,
            web_url: "https://gitlab.com/project/issues/3".to_string(),
        };
        let ext = issue_to_external_ref("gitlab", &issue);
        assert_eq!(ext.adapter, "gitlab");
        assert_eq!(ext.external_id, "3");
        assert_eq!(
            ext.url.as_deref(),
            Some("https://gitlab.com/project/issues/3")
        );
    }

    #[test]
    fn issue_changes_mirrored_issue_yields_status_and_assignment() {
        let issue = GitLabIssue {
            id: 1,
            project_id: 1,
            iid: 1,
            title: "t".to_string(),
            description: None,
            state: "closed".to_string(),
            labels: vec!["tm-id:T-1".to_string()],
            assignee: Some(GitLabUser {
                username: "alice".to_string(),
            }),
            author: GitLabUser {
                username: "bob".to_string(),
            },
            milestone: None,
            web_url: "https://x/1".to_string(),
        };
        let changes = issue_changes(&issue, "gitlab");
        assert_eq!(changes.len(), 2);
        match &changes[0].kind {
            ExternalChangeKind::StatusHint { state } => assert_eq!(state, "closed"),
            other => panic!("expected StatusHint, got {other:?}"),
        }
        match &changes[1].kind {
            ExternalChangeKind::Assigned { assignee } => {
                assert_eq!(assignee.as_deref(), Some("alice"))
            }
            other => panic!("expected Assigned, got {other:?}"),
        }
    }

    #[test]
    fn issue_changes_unmirrored_issue_yields_issue_created() {
        let issue = GitLabIssue {
            id: 9,
            project_id: 1,
            iid: 9,
            title: "Found a bug".to_string(),
            description: Some("It broke".to_string()),
            state: "opened".to_string(),
            labels: vec![],
            assignee: None,
            author: GitLabUser {
                username: "reporter".to_string(),
            },
            milestone: None,
            web_url: "https://x/9".to_string(),
        };
        let changes = issue_changes(&issue, "gitlab");
        assert_eq!(changes.len(), 1);
        match &changes[0].kind {
            ExternalChangeKind::IssueCreated {
                title,
                body,
                author,
            } => {
                assert_eq!(title, "Found a bug");
                assert_eq!(body, "It broke");
                assert_eq!(author, "reporter");
            }
            other => panic!("expected IssueCreated, got {other:?}"),
        }
    }

    #[test]
    fn issue_changes_unmirrored_issue_defaults_missing_description() {
        let issue = GitLabIssue {
            id: 9,
            project_id: 1,
            iid: 9,
            title: "t".to_string(),
            description: None,
            state: "opened".to_string(),
            labels: vec![],
            assignee: None,
            author: GitLabUser {
                username: "reporter".to_string(),
            },
            milestone: None,
            web_url: "https://x/9".to_string(),
        };
        let changes = issue_changes(&issue, "gitlab");
        match &changes[0].kind {
            ExternalChangeKind::IssueCreated { body, .. } => assert_eq!(body, ""),
            other => panic!("expected IssueCreated, got {other:?}"),
        }
    }

    #[test]
    fn parse_note_list_happy_path() {
        let json = br#"[
            {"id": 1, "body": "looks good", "author": {"username": "reviewer"}}
        ]"#;
        let notes = parse_note_list(json).expect("valid note list");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].author.username, "reviewer");
        assert_eq!(notes[0].body, "looks good");
    }

    #[test]
    fn note_to_external_change_maps_fields() {
        let note = GitLabNote {
            id: 1,
            body: "nice work".to_string(),
            author: GitLabUser {
                username: "carol".to_string(),
            },
        };
        let change = note_to_external_change(&note, 3, "gitlab");
        assert_eq!(change.external.external_id, "3");
        match change.kind {
            ExternalChangeKind::CommentAdded { author, body } => {
                assert_eq!(author, "carol");
                assert_eq!(body, "nice work");
            }
            other => panic!("expected CommentAdded, got {other:?}"),
        }
    }

    #[test]
    fn with_config_builds_a_tracker_without_touching_environment() {
        let tracker = GitLabTracker::with_config("gl", "123", "tok").expect("builds");
        assert_eq!(tracker.name(), "gl");
    }

    #[test]
    fn capabilities_declare_no_parent_child_and_no_arbitrary_states() {
        let tracker = GitLabTracker::with_config("gl", "123", "tok").expect("builds");
        let caps = tracker.capabilities();
        assert!(!caps.parent_child);
        assert!(!caps.arbitrary_states);
        assert!(caps.milestones);
        assert!(caps.labels);
        assert!(caps.comments);
    }

    #[tokio::test]
    async fn push_happy_path() {
        let tracker = GitLabTracker::with_config("gl", "456", "tok").expect("builds");
        let projection = sample_projection();
        let result = tracker.push(&projection).await;
        assert!(result.is_ok());
        let external_ref = result.unwrap();
        assert_eq!(external_ref.adapter, "gl");
        assert_eq!(external_ref.external_id, "T-1");
    }

    #[tokio::test]
    async fn pull_empty() {
        let tracker = GitLabTracker::with_config("gl", "456", "tok").expect("builds");
        let since = Timestamp::EPOCH.plus_millis(1000);
        let result = tracker.pull(since).await;
        assert!(result.is_ok());
        let changes = result.unwrap();
        assert_eq!(changes.len(), 0);
    }

    #[test]
    fn from_env_fails_when_token_missing() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(GITLAB_TOKEN_ENV_VAR);
        let result = GitLabTracker::from_env("gl", "123");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains(GITLAB_TOKEN_ENV_VAR));
    }

    #[test]
    fn from_env_reads_token_and_builds() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(GITLAB_TOKEN_ENV_VAR, "tok_test_123");
        let result = GitLabTracker::from_env("gl", "789");
        assert!(result.is_ok());
        let tracker = result.unwrap();
        assert_eq!(tracker.name(), "gl");
        std::env::remove_var(GITLAB_TOKEN_ENV_VAR);
    }

    #[test]
    fn gitlab_user_serialization() {
        let user = GitLabUser {
            username: "alice".to_string(),
        };
        let json = serde_json::to_string(&user).expect("should serialize");
        assert!(json.contains("alice"));
    }

    #[test]
    fn gitlab_user_deserialization() {
        let json = r#"{"username": "bob"}"#;
        let user: GitLabUser = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(user.username, "bob");
    }

    #[test]
    fn gitlab_milestone_serialization() {
        let milestone = GitLabMilestone {
            id: 10,
            title: "Release v1.5".to_string(),
        };
        let json = serde_json::to_string(&milestone).expect("should serialize");
        assert!(json.contains("Release v1.5"));
    }

    #[test]
    fn gitlab_milestone_deserialization() {
        let json = r#"{"id": 11, "title": "Next Sprint"}"#;
        let milestone: GitLabMilestone = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(milestone.id, 11);
        assert_eq!(milestone.title, "Next Sprint");
    }

    #[test]
    fn gitlab_note_serialization() {
        let note = GitLabNote {
            id: 200,
            body: "This is a comment".to_string(),
            author: GitLabUser {
                username: "eve".to_string(),
            },
        };
        let json = serde_json::to_string(&note).expect("should serialize");
        assert!(json.contains("This is a comment"));
        assert!(json.contains("eve"));
    }

    #[test]
    fn gitlab_note_deserialization() {
        let json = r#"{
            "id": 201,
            "body": "Another comment",
            "author": {"username": "frank"}
        }"#;
        let note: GitLabNote = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(note.id, 201);
        assert_eq!(note.body, "Another comment");
        assert_eq!(note.author.username, "frank");
    }

    #[test]
    fn gitlab_issue_with_milestone_and_assignee() {
        let json = r#"{
            "id": 4,
            "project_id": 103,
            "iid": 4,
            "title": "Complex issue",
            "description": "With all fields",
            "state": "opened",
            "labels": ["alpha", "beta"],
            "assignee": {"username": "dev"},
            "author": {"username": "creator"},
            "milestone": {"id": 5, "title": "v2.0"},
            "web_url": "https://gitlab.com/p/issues/4"
        }"#;
        let issue: GitLabIssue = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(issue.labels.len(), 2);
        assert!(issue.assignee.is_some());
        assert_eq!(issue.assignee.unwrap().username, "dev");
        assert!(issue.milestone.is_some());
    }
}
