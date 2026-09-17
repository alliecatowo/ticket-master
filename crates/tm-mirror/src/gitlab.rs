//! GitLab Issues REST v4 adapter: issues, notes, labels, and milestones.
//!
//! Owns: GitLab issue lifecycle (create, update state via open/close), note threading,
//! label management, and milestone tracking. Credentials via `GITLAB_TOKEN` environment
//! variable. Recorded JSON only—no network calls; all I/O is test-doubled.
//!
//! State resolution: GitLab issues have only `open` and `closed` states. State hints
//! are mapped to these two states; any other state results in an error.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_types::{Result, Timestamp};

use crate::projection::Projection;
use crate::tracker::{ExternalChange, ExternalRef, Tracker, TrackerCapabilities};

/// A GitLab REST v4 adapter instance.
///
/// Handles issues, notes (comments), labels, and milestones. GitLab issues are constrained
/// to `open` and `closed` states; this adapter maps state hints onto these two states or
/// reports that the state cannot be represented.
#[derive(Debug)]
pub struct GitLabTracker {
    name: String,
    api_token: String,
}

impl GitLabTracker {
    /// Build a GitLab adapter from environment variable: `GITLAB_TOKEN`.
    ///
    /// # Errors
    /// Returns an error if the environment variable is missing or empty.
    pub fn from_env(name: impl Into<String>) -> Result<Self> {
        let api_token = std::env::var("GITLAB_TOKEN")
            .map_err(|_| tm_types::TmError::invariant("mirror: env var GITLAB_TOKEN is not set"))?;

        if api_token.is_empty() {
            return Err(tm_types::TmError::invariant(
                "mirror: GITLAB_TOKEN is empty",
            ));
        }

        Ok(GitLabTracker {
            name: name.into(),
            api_token,
        })
    }

    /// Build a GitLab adapter with explicit credentials for testing.
    #[cfg(test)]
    fn new_test(name: impl Into<String>, api_token: impl Into<String>) -> Self {
        GitLabTracker {
            name: name.into(),
            api_token: api_token.into(),
        }
    }

    /// Resolve a target state to GitLab's allowed states: `open` or `closed`.
    ///
    /// GitLab issues support only two workflow states. This method maps a state hint
    /// to one of these or returns an error if the state cannot be represented.
    fn resolve_state(&self, target_state: &str) -> Result<String> {
        let normalized = target_state.to_lowercase();
        match normalized.as_str() {
            "open" | "todo" | "in progress" | "ready" => Ok("open".to_string()),
            "closed" | "done" | "resolved" | "wontfix" => Ok("closed".to_string()),
            _ => Err(tm_types::TmError::invariant(format!(
                "gitlab: cannot map state {target_state:?} to open/closed"
            ))),
        }
    }
}

#[async_trait]
impl Tracker for GitLabTracker {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> TrackerCapabilities {
        TrackerCapabilities {
            // GitLab REST v4 does not have native parent/child issues in this adapter's scope.
            parent_child: false,
            // GitLab issues are constrained to open and closed states.
            arbitrary_states: false,
            // GitLab has milestones (project-level).
            milestones: true,
            // GitLab has labels.
            labels: true,
            // GitLab has notes (comments) on issues.
            comments: true,
            // GitLab REST v4 issue description field has a practical limit of 64 KiB.
            max_body_bytes: 65536,
        }
    }

    async fn push(&self, projection: &Projection) -> Result<ExternalRef> {
        // In recorded JSON mode, we record the push (for test doubles) but don't
        // make a network call. The result is a synthetic external reference.
        // Production implementations would POST to /projects/{id}/issues or PUT to
        // /projects/{id}/issues/{issue_iid}, then return the created/updated issue IID and URL.
        if self.api_token.is_empty() {
            return Err(tm_types::TmError::invariant("gitlab: missing API token"));
        }

        // GitLab issues only accept `opened`/`closed`, not the coarse hint verbatim, so every
        // push resolves it through the adapter-specific mapping first and fails fast if the
        // hint can't be represented at all.
        let gitlab_state = self.resolve_state(&projection.state_hint)?;

        let issue_iid = format!("{}_{}", self.name, projection.ticket);
        let url = Some(format!(
            "https://gitlab.com/project/-/issues/{issue_iid}?state={gitlab_state}"
        ));

        Ok(ExternalRef {
            adapter: self.name.clone(),
            external_id: issue_iid,
            url,
        })
    }

    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>> {
        // In recorded JSON mode, pull returns an empty set; production would query
        // /projects/{id}/issues with an updated_after filter.
        // The recorded mode is sufficient for push/pull idempotence tests via
        // SyncEngine, which couples with RecordingTracker for scripted changes.
        let _ = since;
        Ok(Vec::new())
    }
}

/// GitLab issue JSON shape (partial; production would be more complete).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitLabIssue {
    /// Issue internal ID (unique within project).
    pub id: u64,
    /// Issue internal project ID.
    pub project_id: u64,
    /// Issue internal iid (sequential within project).
    pub iid: u64,
    /// Issue title.
    pub title: String,
    /// Issue description (body).
    pub description: Option<String>,
    /// Issue state: `opened` or `closed`.
    pub state: String,
    /// Issue labels.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Assignee, if any.
    pub assignee: Option<GitLabUser>,
    /// Milestone, if any.
    pub milestone: Option<GitLabMilestone>,
}

/// GitLab user reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitLabUser {
    /// User ID.
    pub id: u64,
    /// User username.
    pub username: String,
    /// User display name.
    pub name: String,
}

/// GitLab milestone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitLabMilestone {
    /// Milestone ID.
    pub id: u64,
    /// Milestone title.
    pub title: String,
    /// Milestone state: `active`, `closed`, or `opened`.
    #[serde(default)]
    pub state: String,
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
    /// When the note was created.
    pub created_at: String,
    /// When the note was last updated.
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitlab_tracker_from_env_valid() {
        std::env::set_var("GITLAB_TOKEN", "glpat-token123");

        let tracker = GitLabTracker::from_env("gitlab_prod").expect("should create tracker");
        assert_eq!(tracker.name(), "gitlab_prod");
        assert_eq!(tracker.api_token, "glpat-token123");

        std::env::remove_var("GITLAB_TOKEN");
    }

    #[test]
    fn gitlab_tracker_from_env_missing_token() {
        std::env::remove_var("GITLAB_TOKEN");

        let result = GitLabTracker::from_env("gitlab_prod");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("GITLAB_TOKEN"));
    }

    #[test]
    fn gitlab_tracker_from_env_empty_token() {
        std::env::set_var("GITLAB_TOKEN", "");

        let result = GitLabTracker::from_env("gitlab_prod");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));

        std::env::remove_var("GITLAB_TOKEN");
    }

    #[test]
    fn gitlab_tracker_capabilities() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");
        let caps = tracker.capabilities();

        assert!(!caps.parent_child);
        assert!(!caps.arbitrary_states);
        assert!(caps.milestones);
        assert!(caps.labels);
        assert!(caps.comments);
        assert_eq!(caps.max_body_bytes, 65536);
    }

    #[tokio::test]
    async fn gitlab_tracker_push_happy_path() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");
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
        assert_eq!(external_ref.adapter, "gitlab");
        assert_eq!(external_ref.external_id, "gitlab_T-123");
        assert!(external_ref.url.is_some());
        assert!(external_ref.url.unwrap().contains("gitlab.com"));
    }

    #[tokio::test]
    async fn gitlab_tracker_pull_empty() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");
        let since = Timestamp::from_unix_seconds(1);

        let result = tracker.pull(since).await;
        assert!(result.is_ok());

        let changes = result.unwrap();
        assert_eq!(changes.len(), 0);
    }

    #[test]
    fn gitlab_tracker_resolve_state_open_variants() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");

        assert_eq!(tracker.resolve_state("open").unwrap(), "open");
        assert_eq!(tracker.resolve_state("todo").unwrap(), "open");
        assert_eq!(tracker.resolve_state("in progress").unwrap(), "open");
        assert_eq!(tracker.resolve_state("ready").unwrap(), "open");
    }

    #[test]
    fn gitlab_tracker_resolve_state_closed_variants() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");

        assert_eq!(tracker.resolve_state("closed").unwrap(), "closed");
        assert_eq!(tracker.resolve_state("done").unwrap(), "closed");
        assert_eq!(tracker.resolve_state("resolved").unwrap(), "closed");
        assert_eq!(tracker.resolve_state("wontfix").unwrap(), "closed");
    }

    #[test]
    fn gitlab_tracker_resolve_state_case_insensitive() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");

        assert_eq!(tracker.resolve_state("OPEN").unwrap(), "open");
        assert_eq!(tracker.resolve_state("Closed").unwrap(), "closed");
        assert_eq!(tracker.resolve_state("IN PROGRESS").unwrap(), "open");
    }

    #[test]
    fn gitlab_tracker_resolve_state_invalid() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");

        let result = tracker.resolve_state("deleted");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot map state"));
    }

    #[test]
    fn gitlab_tracker_resolve_state_unknown() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");

        let result = tracker.resolve_state("unknown_state");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn gitlab_tracker_push_minimal_projection() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");
        let projection = Projection {
            ticket: "T-001".parse().expect("valid ticket id"),
            title: "".to_string(),
            body: "".to_string(),
            state_hint: "closed".to_string(),
            labels: vec![],
            milestone: None,
            checklist: vec![],
            degradations: vec![],
        };

        let result = tracker.push(&projection).await;
        assert!(result.is_ok());
        let external_ref = result.unwrap();
        assert_eq!(external_ref.external_id, "gitlab_T-001");
    }

    #[test]
    fn gitlab_issue_serialization() {
        let issue = GitLabIssue {
            id: 1,
            project_id: 100,
            iid: 1,
            title: "Test Issue".to_string(),
            description: Some("Description text".to_string()),
            state: "opened".to_string(),
            labels: vec!["bug".to_string(), "label2".to_string()],
            assignee: Some(GitLabUser {
                id: 42,
                username: "alice".to_string(),
                name: "Alice".to_string(),
            }),
            milestone: Some(GitLabMilestone {
                id: 5,
                title: "v1.0".to_string(),
                state: "active".to_string(),
            }),
        };

        let json = serde_json::to_string(&issue).expect("should serialize");
        assert!(json.contains("Test Issue"));
        assert!(json.contains("Description text"));
        assert!(json.contains("opened"));
        assert!(json.contains("bug"));
        assert!(json.contains("alice"));
    }

    #[test]
    fn gitlab_issue_deserialization() {
        let json = r#"{
            "id": 2,
            "project_id": 101,
            "iid": 2,
            "title": "Test Issue",
            "description": "Description",
            "state": "opened",
            "labels": ["urgent"],
            "assignee": { "id": 43, "username": "bob", "name": "Bob" },
            "milestone": { "id": 6, "title": "v2.0", "state": "active" }
        }"#;

        let issue: GitLabIssue = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(issue.iid, 2);
        assert_eq!(issue.title, "Test Issue");
        assert_eq!(issue.state, "opened");
        assert_eq!(issue.labels, vec!["urgent"]);
        assert_eq!(issue.assignee.unwrap().name, "Bob");
        assert_eq!(issue.milestone.unwrap().title, "v2.0");
    }

    #[test]
    fn gitlab_issue_minimal_fields() {
        let json = r#"{
            "id": 3,
            "project_id": 102,
            "iid": 3,
            "title": "Minimal Issue",
            "description": null,
            "state": "closed",
            "labels": [],
            "assignee": null,
            "milestone": null
        }"#;

        let issue: GitLabIssue = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(issue.iid, 3);
        assert_eq!(issue.title, "Minimal Issue");
        assert_eq!(issue.state, "closed");
        assert!(issue.description.is_none());
        assert!(issue.labels.is_empty());
        assert!(issue.assignee.is_none());
        assert!(issue.milestone.is_none());
    }

    #[test]
    fn gitlab_user_serialization() {
        let user = GitLabUser {
            id: 99,
            username: "charlie".to_string(),
            name: "Charlie".to_string(),
        };

        let json = serde_json::to_string(&user).expect("should serialize");
        assert!(json.contains("charlie"));
        assert!(json.contains("Charlie"));
    }

    #[test]
    fn gitlab_user_deserialization() {
        let json = r#"{ "id": 50, "username": "dave", "name": "Dave" }"#;

        let user: GitLabUser = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(user.id, 50);
        assert_eq!(user.username, "dave");
        assert_eq!(user.name, "Dave");
    }

    #[test]
    fn gitlab_milestone_serialization() {
        let milestone = GitLabMilestone {
            id: 10,
            title: "Release v1.5".to_string(),
            state: "active".to_string(),
        };

        let json = serde_json::to_string(&milestone).expect("should serialize");
        assert!(json.contains("Release v1.5"));
        assert!(json.contains("active"));
    }

    #[test]
    fn gitlab_milestone_deserialization() {
        let json = r#"{ "id": 11, "title": "Next Sprint", "state": "opened" }"#;

        let milestone: GitLabMilestone = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(milestone.id, 11);
        assert_eq!(milestone.title, "Next Sprint");
        assert_eq!(milestone.state, "opened");
    }

    #[test]
    fn gitlab_note_serialization() {
        let note = GitLabNote {
            id: 200,
            body: "This is a comment".to_string(),
            author: GitLabUser {
                id: 44,
                username: "eve".to_string(),
                name: "Eve".to_string(),
            },
            created_at: "2024-01-15T10:30:00Z".to_string(),
            updated_at: "2024-01-15T11:00:00Z".to_string(),
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
            "author": { "id": 45, "username": "frank", "name": "Frank" },
            "created_at": "2024-02-01T12:00:00Z",
            "updated_at": "2024-02-01T13:00:00Z"
        }"#;

        let note: GitLabNote = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(note.id, 201);
        assert_eq!(note.body, "Another comment");
        assert_eq!(note.author.name, "Frank");
        assert_eq!(note.created_at, "2024-02-01T12:00:00Z");
    }

    #[tokio::test]
    async fn gitlab_tracker_push_with_labels() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");
        let projection = Projection {
            ticket: "T-999".parse().expect("valid ticket id"),
            title: "Issue with labels".to_string(),
            body: "Multi-label test".to_string(),
            state_hint: "in progress".to_string(),
            labels: vec![
                "backend".to_string(),
                "performance".to_string(),
                "v2".to_string(),
            ],
            milestone: Some("Release Q4".to_string()),
            checklist: vec![],
            degradations: vec![],
        };

        let result = tracker.push(&projection).await;
        assert!(result.is_ok());
        let ref_obj = result.unwrap();
        assert_eq!(ref_obj.external_id, "gitlab_T-999");
    }

    #[test]
    fn gitlab_issue_with_multiple_labels() {
        let json = r#"{
            "id": 4,
            "project_id": 103,
            "iid": 4,
            "title": "Multi-label issue",
            "description": null,
            "state": "opened",
            "labels": ["alpha", "beta", "gamma"],
            "assignee": null,
            "milestone": null
        }"#;

        let issue: GitLabIssue = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(issue.labels.len(), 3);
        assert!(issue.labels.contains(&"alpha".to_string()));
        assert!(issue.labels.contains(&"beta".to_string()));
        assert!(issue.labels.contains(&"gamma".to_string()));
    }

    #[test]
    fn gitlab_tracker_resolve_state_all_open_variants() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");

        for state in &["open", "todo", "in progress", "ready"] {
            assert_eq!(
                tracker.resolve_state(state).unwrap(),
                "open",
                "state '{}' should resolve to 'open'",
                state
            );
        }
    }

    #[test]
    fn gitlab_tracker_resolve_state_all_closed_variants() {
        let tracker = GitLabTracker::new_test("gitlab", "token123");

        for state in &["closed", "done", "resolved", "wontfix"] {
            assert_eq!(
                tracker.resolve_state(state).unwrap(),
                "closed",
                "state '{}' should resolve to 'closed'",
                state
            );
        }
    }
}
