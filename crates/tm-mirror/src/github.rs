//! [`GitHubTracker`]: GitHub Issues over REST v3, via `reqwest`.
//!
//! Owns everything specific to talking to GitHub: issue create/update, comments, labels,
//! assignees, milestones, the `GITHUB_TOKEN` credential, and GitHub's rate-limit headers.
//! [`crate::tracker::Tracker`] and [`crate::projection`] own everything adapter-agnostic; this
//! module only shapes JSON at the HTTP boundary and maps it onto those contracts.
//!
//! GitHub Issues has no native parent/child relationship and only two workflow states
//! (`open`/`closed`), so [`GitHubTracker::capabilities`] declares `parent_child: false` and
//! `arbitrary_states: false` — [`crate::projection::ProjectionPolicy`] is responsible for
//! folding descendants into a checklist and coarsening internal states before a [`Projection`]
//! ever reaches this module.
//!
//! Every ticket this adapter pushes is tagged with a `tm-id:<ticket>` label, which is how
//! [`GitHubTracker::push`] finds an already-mirrored issue to update instead of creating a
//! duplicate, and how [`GitHubTracker::pull`] tells a ticket-linked issue apart from one a human
//! opened directly (which becomes [`crate::tracker::ExternalChangeKind::IssueCreated`]).
//!
//! Wire shaping and response parsing are pure functions, unit-tested here against recorded JSON.
//! The network calls that use them (`push`/`pull` and their private helpers) are exercised by
//! `sync`'s round-trip tests against [`crate::tracker::RecordingTracker`], never against the
//! network, per this crate's no-network testing rule.

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};

use tm_types::{Result, TicketId, Timestamp, TmError};

use crate::projection::Projection;
use crate::tracker::{
    ExternalChange, ExternalChangeKind, ExternalRef, Tracker, TrackerCapabilities,
};

/// Name of the environment variable [`GitHubTracker::from_env`] reads the token from.
pub const GITHUB_TOKEN_ENV_VAR: &str = "GITHUB_TOKEN";

/// The GitHub REST v3 endpoint this adapter targets by default.
pub const DEFAULT_BASE_URL: &str = "https://api.github.com";

/// The REST media type this adapter requests.
const ACCEPT_HEADER: &str = "application/vnd.github+json";

/// GitHub Issues has exactly two workflow states; every internal state not `Closed`/`Cancelled`
/// maps to `open`.
const GITHUB_CLOSED_STATES: [&str; 2] = ["Closed", "Cancelled"];

/// A live GitHub Issues adapter for one `owner/repo`.
pub struct GitHubTracker {
    name: String,
    owner: String,
    repo: String,
    token: String,
    base_url: String,
    http: reqwest::Client,
}

impl GitHubTracker {
    /// Build a tracker for `owner/repo`, reading the token from [`GITHUB_TOKEN_ENV_VAR`].
    pub fn from_env(
        name: impl Into<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
    ) -> Result<Self> {
        let token = std::env::var(GITHUB_TOKEN_ENV_VAR).map_err(|_| {
            TmError::invariant(format!("github: {GITHUB_TOKEN_ENV_VAR} is not set"))
        })?;
        Self::with_config(name, owner, repo, token, DEFAULT_BASE_URL.to_string())
    }

    /// Build a tracker with an explicit token and base URL, for tests that stand up a local mock
    /// HTTP server (shaping tests use recorded JSON directly and never need this).
    pub fn with_config(
        name: impl Into<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
        token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| TmError::Provider(format!("github: failed to build HTTP client: {e}")))?;
        Ok(GitHubTracker {
            name: name.into(),
            owner: owner.into(),
            repo: repo.into(),
            token: token.into(),
            base_url: base_url.into(),
            http,
        })
    }

    fn issues_url(&self) -> String {
        format!(
            "{}/repos/{}/{}/issues",
            self.base_url, self.owner, self.repo
        )
    }

    fn issue_url(&self, number: u64) -> String {
        format!("{}/{}", self.issues_url(), number)
    }

    fn milestones_url(&self) -> String {
        format!(
            "{}/repos/{}/{}/milestones",
            self.base_url, self.owner, self.repo
        )
    }

    fn comments_url(&self) -> String {
        format!(
            "{}/repos/{}/{}/issues/comments",
            self.base_url, self.owner, self.repo
        )
    }

    async fn send(
        &self,
        method: Method,
        url: String,
        body: Option<&serde_json::Value>,
    ) -> Result<Vec<u8>> {
        let mut req = self
            .http
            .request(method, url)
            .headers(build_headers(&self.token));
        if let Some(b) = body {
            req = req.json(b);
        }
        let response = req
            .send()
            .await
            .map_err(|e| TmError::Provider(format!("github: request failed: {e}")))?;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| TmError::Provider(format!("github: failed to read response body: {e}")))?;
        classify_status(status, &headers, &bytes)?;
        Ok(bytes.to_vec())
    }

    async fn find_issue_by_label(&self, label: &str) -> Result<Option<GitHubIssue>> {
        let url = format!(
            "{}?labels={}&state=all&per_page=1",
            self.issues_url(),
            label
        );
        let body = self.send(Method::GET, url, None).await?;
        let mut issues = parse_issue_list(&body)?;
        Ok(if issues.is_empty() {
            None
        } else {
            Some(issues.remove(0))
        })
    }

    async fn resolve_milestone(&self, title: &str) -> Result<u64> {
        let body = self
            .send(
                Method::GET,
                format!("{}?state=all", self.milestones_url()),
                None,
            )
            .await?;
        let milestones = parse_milestone_list(&body)?;
        if let Some(number) = find_milestone_number(&milestones, title) {
            return Ok(number);
        }
        let created = self
            .send(
                Method::POST,
                self.milestones_url(),
                Some(&serde_json::json!({ "title": title })),
            )
            .await?;
        let milestone: GitHubMilestone = serde_json::from_slice(&created)
            .map_err(|e| TmError::parse(format!("github: malformed milestone response: {e}")))?;
        Ok(milestone.number)
    }

    async fn list_issues_since(&self, since: Timestamp) -> Result<Vec<GitHubIssue>> {
        let url = format!(
            "{}?since={}&state=all&sort=updated&direction=asc",
            self.issues_url(),
            since.to_rfc3339()
        );
        let body = self.send(Method::GET, url, None).await?;
        parse_issue_list(&body)
    }

    async fn list_comments_since(&self, since: Timestamp) -> Result<Vec<GitHubComment>> {
        let url = format!(
            "{}?since={}&sort=created&direction=asc",
            self.comments_url(),
            since.to_rfc3339()
        );
        let body = self.send(Method::GET, url, None).await?;
        parse_comment_list(&body)
    }
}

#[async_trait]
impl Tracker for GitHubTracker {
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
        let label = ticket_label(&projection.ticket);
        let existing = self.find_issue_by_label(&label).await?;

        let mut labels = projection.labels.clone();
        if !labels.iter().any(|l| l == &label) {
            labels.push(label);
        }

        let milestone = match &projection.milestone {
            Some(title) => Some(self.resolve_milestone(title).await?),
            None => None,
        };

        let request = issue_request_body(projection, &labels, milestone);
        let body = match &existing {
            Some(issue) => {
                self.send(Method::PATCH, self.issue_url(issue.number), Some(&request))
                    .await?
            }
            None => {
                self.send(Method::POST, self.issues_url(), Some(&request))
                    .await?
            }
        };
        let issue = parse_issue(&body)?;
        Ok(issue_to_external_ref(&self.name, &issue))
    }

    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>> {
        let issues = self.list_issues_since(since).await?;
        let mut changes: Vec<ExternalChange> = issues
            .iter()
            .flat_map(|issue| issue_changes(issue, &self.name))
            .collect();

        let comments = self.list_comments_since(since).await?;
        for comment in &comments {
            if let Some(change) = comment_to_external_change(comment, &self.name) {
                changes.push(change);
            }
        }
        Ok(changes)
    }
}

// ---- wire shapes -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct GitHubLabel {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct GitHubUser {
    login: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct GitHubIssue {
    number: u64,
    html_url: String,
    title: String,
    #[serde(default)]
    body: Option<String>,
    state: String,
    #[serde(default)]
    labels: Vec<GitHubLabel>,
    #[serde(default)]
    assignee: Option<GitHubUser>,
    user: GitHubUser,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct GitHubComment {
    body: String,
    user: GitHubUser,
    issue_url: String,
    html_url: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct GitHubMilestone {
    number: u64,
    title: String,
}

#[derive(Debug, Deserialize)]
struct GitHubErrorEnvelope {
    message: String,
}

/// Every non-error header this adapter cares about, plus a bearer token, `Accept`, and a
/// `User-Agent` GitHub requires of every client.
fn build_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let auth = format!("Bearer {token}");
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&auth).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(ACCEPT, HeaderValue::from_static(ACCEPT_HEADER));
    headers.insert(USER_AGENT, HeaderValue::from_static("ticketmaster-mirror"));
    headers
}

/// The label this adapter tags every issue it pushes with, and later searches by to find the
/// issue again without keeping any state of its own between calls.
fn ticket_label(ticket: &TicketId) -> String {
    format!("tm-id:{ticket}")
}

/// A ticket-id marker previously applied by [`ticket_label`], if `labels` carries one.
fn ticket_ref_from_labels(labels: &[GitHubLabel]) -> Option<&str> {
    labels.iter().find_map(|l| l.name.strip_prefix("tm-id:"))
}

/// Coarsen an internal state name onto GitHub's two-state model. GitHub's `arbitrary_states`
/// capability is false, so this adapter (not the caller) owns the final open/closed decision.
fn github_state_for(state_hint: &str) -> &'static str {
    if GITHUB_CLOSED_STATES.contains(&state_hint) {
        "closed"
    } else {
        "open"
    }
}

/// The JSON body for a create (`POST .../issues`) or update (`PATCH .../issues/{n}`) call.
fn issue_request_body(
    projection: &Projection,
    labels: &[String],
    milestone: Option<u64>,
) -> serde_json::Value {
    serde_json::json!({
        "title": projection.title,
        "body": projection.body,
        "state": github_state_for(&projection.state_hint),
        "labels": labels,
        "milestone": milestone,
    })
}

fn parse_issue(body: &[u8]) -> Result<GitHubIssue> {
    serde_json::from_slice(body)
        .map_err(|e| TmError::parse(format!("github: malformed issue response: {e}")))
}

fn parse_issue_list(body: &[u8]) -> Result<Vec<GitHubIssue>> {
    serde_json::from_slice(body)
        .map_err(|e| TmError::parse(format!("github: malformed issue list response: {e}")))
}

fn parse_comment_list(body: &[u8]) -> Result<Vec<GitHubComment>> {
    serde_json::from_slice(body)
        .map_err(|e| TmError::parse(format!("github: malformed comment list response: {e}")))
}

fn parse_milestone_list(body: &[u8]) -> Result<Vec<GitHubMilestone>> {
    serde_json::from_slice(body)
        .map_err(|e| TmError::parse(format!("github: malformed milestone list response: {e}")))
}

fn find_milestone_number(milestones: &[GitHubMilestone], title: &str) -> Option<u64> {
    milestones
        .iter()
        .find(|m| m.title == title)
        .map(|m| m.number)
}

fn issue_to_external_ref(adapter: &str, issue: &GitHubIssue) -> ExternalRef {
    ExternalRef {
        adapter: adapter.to_string(),
        external_id: issue.number.to_string(),
        url: Some(issue.html_url.clone()),
    }
}

/// The inbound changes one pulled issue produces: a `StatusHint`/`Assigned` pair for an issue
/// this adapter already mirrors (found via its `tm-id:` label), or a single `IssueCreated` for
/// one a human opened directly that has no such label.
fn issue_changes(issue: &GitHubIssue, adapter: &str) -> Vec<ExternalChange> {
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
                    assignee: issue.assignee.as_ref().map(|u| u.login.clone()),
                },
                observed_at: Timestamp::EPOCH,
            },
        ]
    } else {
        vec![ExternalChange {
            external,
            kind: ExternalChangeKind::IssueCreated {
                title: issue.title.clone(),
                body: issue.body.clone().unwrap_or_default(),
                author: issue.user.login.clone(),
            },
            observed_at: Timestamp::EPOCH,
        }]
    }
}

/// The issue number embedded in a comment's `issue_url`
/// (`https://api.github.com/repos/{owner}/{repo}/issues/{number}`), or `None` if the URL doesn't
/// end in one, so a malformed comment is skipped by [`comment_to_external_change`] rather than
/// failing the whole pull.
fn issue_number_from_issue_url(issue_url: &str) -> Option<u64> {
    issue_url.rsplit('/').next()?.parse().ok()
}

fn comment_to_external_change(comment: &GitHubComment, adapter: &str) -> Option<ExternalChange> {
    let number = issue_number_from_issue_url(&comment.issue_url)?;
    Some(ExternalChange {
        external: ExternalRef {
            adapter: adapter.to_string(),
            external_id: number.to_string(),
            url: Some(comment.html_url.clone()),
        },
        kind: ExternalChangeKind::CommentAdded {
            author: comment.user.login.clone(),
            body: comment.body.clone(),
        },
        observed_at: Timestamp::EPOCH,
    })
}

/// 2xx -> `Ok(())`. 404 -> [`TmError::NotFound`]. 409 -> [`TmError::Conflict`]. 403 with
/// `X-RateLimit-Remaining: 0` -> [`TmError::Provider`] naming the reset time from
/// `X-RateLimit-Reset` (a Unix-seconds epoch, GitHub's documented format). Any other non-2xx ->
/// [`TmError::Provider`] with the response's `message` field, falling back to the status text.
fn classify_status(status: StatusCode, headers: &HeaderMap, body: &[u8]) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    if status == StatusCode::FORBIDDEN && header_str(headers, "x-ratelimit-remaining") == Some("0")
    {
        let reset = header_str(headers, "x-ratelimit-reset").unwrap_or("unknown");
        return Err(TmError::Provider(format!(
            "github: rate limited, resets at unix time {reset}"
        )));
    }
    let message = error_message(body, status.as_str());
    match status {
        StatusCode::NOT_FOUND => Err(TmError::not_found("github issue", message)),
        StatusCode::CONFLICT => Err(TmError::conflict(format!("github: {message}"))),
        _ => Err(TmError::Provider(format!("github: {status} {message}"))),
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn error_message(body: &[u8], fallback: &str) -> String {
    serde_json::from_slice::<GitHubErrorEnvelope>(body)
        .map(|e| e.message)
        .unwrap_or_else(|_| fallback.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let labels = vec![
            GitHubLabel {
                name: "bug".to_string(),
            },
            GitHubLabel {
                name: "tm-id:T-7".to_string(),
            },
        ];
        assert_eq!(ticket_ref_from_labels(&labels), Some("T-7"));
    }

    #[test]
    fn ticket_ref_from_labels_absent_when_no_marker() {
        let labels = vec![GitHubLabel {
            name: "bug".to_string(),
        }];
        assert_eq!(ticket_ref_from_labels(&labels), None);
    }

    #[test]
    fn github_state_for_maps_closed_states_to_closed() {
        assert_eq!(github_state_for("Closed"), "closed");
        assert_eq!(github_state_for("Cancelled"), "closed");
    }

    #[test]
    fn github_state_for_maps_everything_else_to_open() {
        assert_eq!(github_state_for("Ready"), "open");
        assert_eq!(github_state_for("Blocked"), "open");
        assert_eq!(github_state_for("Draft"), "open");
    }

    #[test]
    fn issue_request_body_includes_ticket_label_and_mapped_state() {
        let projection = sample_projection();
        let labels = vec!["bug".to_string(), "tm-id:T-1".to_string()];
        let body = issue_request_body(&projection, &labels, Some(3));
        assert_eq!(body["title"], "Fix the widget");
        assert_eq!(body["state"], "open");
        assert_eq!(body["labels"][1], "tm-id:T-1");
        assert_eq!(body["milestone"], 3);
    }

    #[test]
    fn issue_request_body_milestone_absent_is_null() {
        let projection = sample_projection();
        let body = issue_request_body(&projection, &[], None);
        assert!(body["milestone"].is_null());
    }

    #[test]
    fn parse_issue_happy_path() {
        let json = br#"{
            "number": 42,
            "html_url": "https://github.com/acme/widgets/issues/42",
            "title": "Fix the widget",
            "body": "Do the thing.",
            "state": "open",
            "labels": [{"name": "tm-id:T-1"}],
            "assignee": {"login": "octocat"},
            "user": {"login": "octocat"}
        }"#;
        let issue = parse_issue(json).expect("valid issue json");
        assert_eq!(issue.number, 42);
        assert_eq!(issue.state, "open");
        assert_eq!(issue.assignee.unwrap().login, "octocat");
    }

    #[test]
    fn parse_issue_rejects_malformed_json() {
        let result = parse_issue(b"not json");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse:"));
    }

    #[test]
    fn parse_issue_defaults_missing_body_and_assignee() {
        let json = br#"{
            "number": 1,
            "html_url": "https://github.com/acme/widgets/issues/1",
            "title": "No body",
            "state": "open",
            "user": {"login": "someone"}
        }"#;
        let issue = parse_issue(json).expect("valid issue json");
        assert_eq!(issue.body, None);
        assert_eq!(issue.assignee, None);
        assert!(issue.labels.is_empty());
    }

    #[test]
    fn parse_issue_list_happy_path() {
        let json = br#"[
            {"number": 1, "html_url": "https://x/1", "title": "a", "state": "open", "user": {"login": "u"}},
            {"number": 2, "html_url": "https://x/2", "title": "b", "state": "closed", "user": {"login": "u"}}
        ]"#;
        let issues = parse_issue_list(json).expect("valid list");
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[1].state, "closed");
    }

    #[test]
    fn issue_to_external_ref_carries_number_and_url() {
        let issue = GitHubIssue {
            number: 7,
            html_url: "https://github.com/acme/widgets/issues/7".to_string(),
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            assignee: None,
            user: GitHubUser {
                login: "u".to_string(),
            },
        };
        let ext = issue_to_external_ref("github", &issue);
        assert_eq!(ext.adapter, "github");
        assert_eq!(ext.external_id, "7");
        assert_eq!(
            ext.url.as_deref(),
            Some("https://github.com/acme/widgets/issues/7")
        );
    }

    #[test]
    fn issue_changes_mirrored_issue_yields_status_and_assignment() {
        let issue = GitHubIssue {
            number: 1,
            html_url: "https://x/1".to_string(),
            title: "t".to_string(),
            body: None,
            state: "closed".to_string(),
            labels: vec![GitHubLabel {
                name: "tm-id:T-1".to_string(),
            }],
            assignee: Some(GitHubUser {
                login: "alice".to_string(),
            }),
            user: GitHubUser {
                login: "bob".to_string(),
            },
        };
        let changes = issue_changes(&issue, "github");
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
        let issue = GitHubIssue {
            number: 9,
            html_url: "https://x/9".to_string(),
            title: "Found a bug".to_string(),
            body: Some("It broke".to_string()),
            state: "open".to_string(),
            labels: vec![],
            assignee: None,
            user: GitHubUser {
                login: "reporter".to_string(),
            },
        };
        let changes = issue_changes(&issue, "github");
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
    fn issue_changes_unmirrored_issue_defaults_missing_body() {
        let issue = GitHubIssue {
            number: 9,
            html_url: "https://x/9".to_string(),
            title: "t".to_string(),
            body: None,
            state: "open".to_string(),
            labels: vec![],
            assignee: None,
            user: GitHubUser {
                login: "reporter".to_string(),
            },
        };
        let changes = issue_changes(&issue, "github");
        match &changes[0].kind {
            ExternalChangeKind::IssueCreated { body, .. } => assert_eq!(body, ""),
            other => panic!("expected IssueCreated, got {other:?}"),
        }
    }

    #[test]
    fn parse_comment_list_happy_path() {
        let json = br#"[
            {"body": "looks good", "user": {"login": "reviewer"},
             "issue_url": "https://api.github.com/repos/acme/widgets/issues/5",
             "html_url": "https://github.com/acme/widgets/issues/5#comment-1"}
        ]"#;
        let comments = parse_comment_list(json).expect("valid comment list");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].user.login, "reviewer");
    }

    #[test]
    fn issue_number_from_issue_url_parses_trailing_number() {
        assert_eq!(
            issue_number_from_issue_url("https://api.github.com/repos/acme/widgets/issues/5"),
            Some(5)
        );
    }

    #[test]
    fn issue_number_from_issue_url_none_when_not_numeric() {
        assert_eq!(
            issue_number_from_issue_url("https://api.github.com/repos/acme/widgets"),
            None
        );
    }

    #[test]
    fn comment_to_external_change_maps_fields() {
        let comment = GitHubComment {
            body: "nice work".to_string(),
            user: GitHubUser {
                login: "carol".to_string(),
            },
            issue_url: "https://api.github.com/repos/acme/widgets/issues/3".to_string(),
            html_url: "https://github.com/acme/widgets/issues/3#comment-9".to_string(),
        };
        let change = comment_to_external_change(&comment, "github").expect("mapped change");
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
    fn comment_to_external_change_none_when_issue_url_unparseable() {
        let comment = GitHubComment {
            body: "x".to_string(),
            user: GitHubUser {
                login: "carol".to_string(),
            },
            issue_url: "not-a-url".to_string(),
            html_url: "https://x".to_string(),
        };
        assert!(comment_to_external_change(&comment, "github").is_none());
    }

    #[test]
    fn find_milestone_number_matches_by_title() {
        let milestones = vec![
            GitHubMilestone {
                number: 1,
                title: "v1.0".to_string(),
            },
            GitHubMilestone {
                number: 2,
                title: "v2.0".to_string(),
            },
        ];
        assert_eq!(find_milestone_number(&milestones, "v2.0"), Some(2));
    }

    #[test]
    fn find_milestone_number_none_when_absent() {
        let milestones = vec![GitHubMilestone {
            number: 1,
            title: "v1.0".to_string(),
        }];
        assert_eq!(find_milestone_number(&milestones, "v9.0"), None);
    }

    #[test]
    fn parse_milestone_list_happy_path() {
        let json = br#"[{"number": 4, "title": "v4.0"}]"#;
        let milestones = parse_milestone_list(json).expect("valid milestone list");
        assert_eq!(milestones[0].number, 4);
    }

    #[test]
    fn build_headers_sets_bearer_auth_accept_and_user_agent() {
        let headers = build_headers("secret-token");
        assert_eq!(headers.get(AUTHORIZATION).unwrap(), "Bearer secret-token");
        assert_eq!(headers.get(ACCEPT).unwrap(), ACCEPT_HEADER);
        assert_eq!(headers.get(USER_AGENT).unwrap(), "ticketmaster-mirror");
    }

    #[test]
    fn classify_status_ok_on_2xx() {
        let headers = HeaderMap::new();
        assert!(classify_status(StatusCode::OK, &headers, b"{}").is_ok());
        assert!(classify_status(StatusCode::CREATED, &headers, b"{}").is_ok());
    }

    #[test]
    fn classify_status_not_found_maps_to_not_found_error() {
        let headers = HeaderMap::new();
        let body = br#"{"message": "Not Found"}"#;
        let err = classify_status(StatusCode::NOT_FOUND, &headers, body).unwrap_err();
        assert!(err.to_string().contains("not found"));
        assert!(err.to_string().contains("Not Found"));
    }

    #[test]
    fn classify_status_conflict_maps_to_conflict_error() {
        let headers = HeaderMap::new();
        let body = br#"{"message": "already exists"}"#;
        let err = classify_status(StatusCode::CONFLICT, &headers, body).unwrap_err();
        assert!(err.to_string().contains("conflict:"));
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn classify_status_rate_limited_names_reset_time() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("1700000000"));
        let err = classify_status(StatusCode::FORBIDDEN, &headers, b"{}").unwrap_err();
        assert!(err.to_string().contains("rate limited"));
        assert!(err.to_string().contains("1700000000"));
    }

    #[test]
    fn classify_status_forbidden_without_rate_limit_headers_is_generic_provider_error() {
        let headers = HeaderMap::new();
        let body = br#"{"message": "Bad credentials"}"#;
        let err = classify_status(StatusCode::FORBIDDEN, &headers, body).unwrap_err();
        assert!(err.to_string().contains("Bad credentials"));
    }

    #[test]
    fn classify_status_falls_back_to_status_text_on_unparseable_error_body() {
        let headers = HeaderMap::new();
        let err =
            classify_status(StatusCode::INTERNAL_SERVER_ERROR, &headers, b"not json").unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[test]
    fn with_config_builds_a_tracker_without_touching_the_environment() {
        let tracker = GitHubTracker::with_config("gh", "acme", "widgets", "tok", DEFAULT_BASE_URL)
            .expect("builds without a real HTTP call");
        assert_eq!(tracker.name(), "gh");
        assert_eq!(
            tracker.issues_url(),
            "https://api.github.com/repos/acme/widgets/issues"
        );
    }

    #[test]
    fn capabilities_declare_no_parent_child_and_no_arbitrary_states() {
        let tracker = GitHubTracker::with_config("gh", "acme", "widgets", "tok", DEFAULT_BASE_URL)
            .expect("builds without a real HTTP call");
        let caps = tracker.capabilities();
        assert!(!caps.parent_child);
        assert!(!caps.arbitrary_states);
        assert!(caps.milestones);
        assert!(caps.labels);
        assert!(caps.comments);
    }
}
