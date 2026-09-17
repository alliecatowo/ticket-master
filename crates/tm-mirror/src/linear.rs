//! [`LinearTracker`]: Linear over GraphQL (a single `POST /graphql` endpoint, not REST).
//!
//! Owns everything specific to talking to Linear: issue create/update, workflow states, native
//! parent/child sub-issues, projects (Linear's milestone-shaped concept), labels, comments, the
//! `LINEAR_API_KEY` credential, and Linear's cursor-based `pageInfo`/`endCursor` pagination.
//! [`crate::tracker::Tracker`] and [`crate::projection`] own everything adapter-agnostic; this
//! module only shapes GraphQL queries/responses and maps them onto those contracts.
//!
//! Linear has both a native parent/child relationship (`Issue.parent`/`Issue.children`) and an
//! arbitrary, per-team set of workflow states, so [`LinearTracker::capabilities`] declares both
//! `parent_child: true` and `arbitrary_states: true` — unlike GitHub, [`Projection::checklist`]
//! is always empty here and state names pass through unmapped.
//!
//! Every ticket this adapter pushes is tagged with a `tm-id:<ticket>` label (mirroring the
//! `github` module's convention), which is how [`LinearTracker::push`] finds an already-mirrored
//! issue to update instead of creating a duplicate, and how [`LinearTracker::pull`] tells a
//! ticket-linked issue apart from one a human opened directly (which becomes
//! [`crate::tracker::ExternalChangeKind::IssueCreated`]).
//!
//! Wire shaping and response parsing are pure functions, unit-tested here against recorded JSON.
//! The network calls that use them (`push`/`pull` and their private helpers) are exercised by
//! `sync`'s round-trip tests against [`crate::tracker::RecordingTracker`], never against the
//! network, per this crate's no-network testing rule.

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

use tm_types::{Result, TicketId, Timestamp, TmError};

use crate::projection::Projection;
use crate::tracker::{
    ExternalChange, ExternalChangeKind, ExternalRef, Tracker, TrackerCapabilities,
};

/// Name of the environment variable [`LinearTracker::from_env`] reads the API key from.
pub const LINEAR_API_KEY_ENV_VAR: &str = "LINEAR_API_KEY";

/// The Linear GraphQL endpoint this adapter targets by default.
pub const DEFAULT_BASE_URL: &str = "https://api.linear.app/graphql";

/// Linear's own conservative issue description ceiling.
const MAX_BODY_BYTES: usize = 255_000;

/// Page size used for every cursor-paginated list query.
const PAGE_SIZE: u32 = 50;

/// A live Linear adapter for one team.
pub struct LinearTracker {
    name: String,
    team_id: String,
    api_key: String,
    base_url: String,
    http: reqwest::Client,
}

impl LinearTracker {
    /// Build a tracker for `team_id`, reading the API key from [`LINEAR_API_KEY_ENV_VAR`].
    pub fn from_env(name: impl Into<String>, team_id: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var(LINEAR_API_KEY_ENV_VAR).map_err(|_| {
            TmError::invariant(format!("linear: {LINEAR_API_KEY_ENV_VAR} is not set"))
        })?;
        Self::with_config(name, team_id, api_key, DEFAULT_BASE_URL.to_string())
    }

    /// Build a tracker with an explicit API key and base URL, for tests that stand up a local
    /// mock HTTP server (shaping tests use recorded JSON directly and never need this).
    pub fn with_config(
        name: impl Into<String>,
        team_id: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| TmError::Provider(format!("linear: failed to build HTTP client: {e}")))?;
        Ok(LinearTracker {
            name: name.into(),
            team_id: team_id.into(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            http,
        })
    }

    async fn graphql(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let request_body = serde_json::json!({ "query": query, "variables": variables });
        let response = self
            .http
            .post(&self.base_url)
            .headers(build_headers(&self.api_key))
            .json(&request_body)
            .send()
            .await
            .map_err(|e| TmError::Provider(format!("linear: request failed: {e}")))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| TmError::Provider(format!("linear: failed to read response body: {e}")))?;
        parse_graphql_response(status.as_u16(), &bytes)
    }

    async fn find_issue_by_label(&self, label: &str) -> Result<Option<LinearIssue>> {
        let data = self
            .graphql(
                FIND_ISSUE_BY_LABEL_QUERY,
                serde_json::json!({ "teamId": self.team_id, "label": label }),
            )
            .await?;
        let mut issues = parse_issue_list(&data, "issues")?;
        Ok(if issues.is_empty() {
            None
        } else {
            Some(issues.remove(0))
        })
    }

    async fn resolve_state_id(&self, state_name: &str) -> Result<String> {
        let data = self
            .graphql(
                WORKFLOW_STATES_QUERY,
                serde_json::json!({ "teamId": self.team_id }),
            )
            .await?;
        let states = parse_workflow_states(&data)?;
        states
            .iter()
            .find(|s| s.name == state_name)
            .map(|s| s.id.clone())
            .ok_or_else(|| TmError::not_found("linear workflow state", state_name.to_string()))
    }

    async fn resolve_project_id(&self, title: &str) -> Result<String> {
        let data = self
            .graphql(
                FIND_PROJECT_QUERY,
                serde_json::json!({ "teamId": self.team_id }),
            )
            .await?;
        let projects = parse_project_list(&data)?;
        if let Some(project) = projects.iter().find(|p| p.name == title) {
            return Ok(project.id.clone());
        }
        let created = self
            .graphql(
                CREATE_PROJECT_MUTATION,
                serde_json::json!({ "teamId": self.team_id, "name": title }),
            )
            .await?;
        parse_created_project(&created)
    }

    async fn resolve_label_id(&self, label: &str) -> Result<String> {
        let data = self
            .graphql(
                FIND_LABEL_QUERY,
                serde_json::json!({ "teamId": self.team_id, "name": label }),
            )
            .await?;
        let labels = parse_label_list(&data)?;
        if let Some(existing) = labels.iter().find(|l| l.name == label) {
            return Ok(existing.id.clone());
        }
        let created = self
            .graphql(
                CREATE_LABEL_MUTATION,
                serde_json::json!({ "teamId": self.team_id, "name": label }),
            )
            .await?;
        parse_created_label(&created)
    }

    async fn resolve_label_ids(&self, labels: &[String]) -> Result<Vec<String>> {
        let mut ids = Vec::with_capacity(labels.len());
        for label in labels {
            ids.push(self.resolve_label_id(label).await?);
        }
        Ok(ids)
    }

    async fn list_issues_since(&self, since: Timestamp) -> Result<Vec<LinearIssue>> {
        let mut issues = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let data = self
                .graphql(
                    ISSUES_SINCE_QUERY,
                    serde_json::json!({
                        "teamId": self.team_id,
                        "since": since.to_rfc3339(),
                        "first": PAGE_SIZE,
                        "after": cursor,
                    }),
                )
                .await?;
            let page = parse_issue_page(&data)?;
            issues.extend(page.nodes);
            if !page.has_next_page {
                break;
            }
            cursor = page.end_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(issues)
    }

    async fn list_comments_since(&self, since: Timestamp) -> Result<Vec<LinearComment>> {
        let mut comments = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let data = self
                .graphql(
                    COMMENTS_SINCE_QUERY,
                    serde_json::json!({
                        "teamId": self.team_id,
                        "since": since.to_rfc3339(),
                        "first": PAGE_SIZE,
                        "after": cursor,
                    }),
                )
                .await?;
            let page = parse_comment_page(&data)?;
            comments.extend(page.nodes);
            if !page.has_next_page {
                break;
            }
            cursor = page.end_cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(comments)
    }
}

#[async_trait]
impl Tracker for LinearTracker {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> TrackerCapabilities {
        TrackerCapabilities {
            parent_child: true,
            arbitrary_states: true,
            milestones: true,
            labels: true,
            comments: true,
            max_body_bytes: MAX_BODY_BYTES,
        }
    }

    async fn push(&self, projection: &Projection) -> Result<ExternalRef> {
        let label = ticket_label(&projection.ticket);
        let existing = self.find_issue_by_label(&label).await?;

        let mut labels = projection.labels.clone();
        if !labels.iter().any(|l| l == &label) {
            labels.push(label);
        }
        let label_ids = self.resolve_label_ids(&labels).await?;
        let state_id = self.resolve_state_id(&projection.state_hint).await?;
        let project_id = match &projection.milestone {
            Some(title) => Some(self.resolve_project_id(title).await?),
            None => None,
        };

        let data = match &existing {
            Some(issue) => {
                self.graphql(
                    UPDATE_ISSUE_MUTATION,
                    issue_update_variables(
                        &issue.id,
                        projection,
                        &label_ids,
                        &state_id,
                        &project_id,
                    ),
                )
                .await?
            }
            None => {
                self.graphql(
                    CREATE_ISSUE_MUTATION,
                    issue_create_variables(
                        &self.team_id,
                        projection,
                        &label_ids,
                        &state_id,
                        &project_id,
                    ),
                )
                .await?
            }
        };
        let issue = parse_mutated_issue(
            &data,
            if existing.is_some() {
                "issueUpdate"
            } else {
                "issueCreate"
            },
        )?;
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
            changes.push(comment_to_external_change(comment, &self.name));
        }
        Ok(changes)
    }
}

// ---- GraphQL documents -------------------------------------------------------------------------

const FIND_ISSUE_BY_LABEL_QUERY: &str = r#"query($teamId: String!, $label: String!) {
  issues(filter: { team: { id: { eq: $teamId } }, labels: { name: { eq: $label } } }, first: 1) {
    nodes { id identifier title description url state { name } assignee { name } creator { name } labels { nodes { name } } }
  }
}"#;

const WORKFLOW_STATES_QUERY: &str = r#"query($teamId: String!) {
  workflowStates(filter: { team: { id: { eq: $teamId } } }) {
    nodes { id name }
  }
}"#;

const FIND_PROJECT_QUERY: &str = r#"query($teamId: String!) {
  projects(filter: { accessibleTeams: { id: { eq: $teamId } } }) {
    nodes { id name }
  }
}"#;

const CREATE_PROJECT_MUTATION: &str = r#"mutation($teamId: String!, $name: String!) {
  projectCreate(input: { teamIds: [$teamId], name: $name }) {
    project { id name }
  }
}"#;

const FIND_LABEL_QUERY: &str = r#"query($teamId: String!, $name: String!) {
  issueLabels(filter: { team: { id: { eq: $teamId } }, name: { eq: $name } }) {
    nodes { id name }
  }
}"#;

const CREATE_LABEL_MUTATION: &str = r#"mutation($teamId: String!, $name: String!) {
  issueLabelCreate(input: { teamId: $teamId, name: $name }) {
    issueLabel { id name }
  }
}"#;

const CREATE_ISSUE_MUTATION: &str = r#"mutation($teamId: String!, $title: String!, $description: String!, $stateId: String!, $labelIds: [String!]!, $projectId: String) {
  issueCreate(input: { teamId: $teamId, title: $title, description: $description, stateId: $stateId, labelIds: $labelIds, projectId: $projectId }) {
    issue { id identifier title description url state { name } assignee { name } creator { name } labels { nodes { name } } }
  }
}"#;

const UPDATE_ISSUE_MUTATION: &str = r#"mutation($issueId: String!, $title: String!, $description: String!, $stateId: String!, $labelIds: [String!]!, $projectId: String) {
  issueUpdate(id: $issueId, input: { title: $title, description: $description, stateId: $stateId, labelIds: $labelIds, projectId: $projectId }) {
    issue { id identifier title description url state { name } assignee { name } creator { name } labels { nodes { name } } }
  }
}"#;

const ISSUES_SINCE_QUERY: &str = r#"query($teamId: String!, $since: DateTimeOrDuration!, $first: Int!, $after: String) {
  issues(filter: { team: { id: { eq: $teamId } }, updatedAt: { gt: $since } }, first: $first, after: $after) {
    nodes { id identifier title description url state { name } assignee { name } creator { name } labels { nodes { name } } }
    pageInfo { hasNextPage endCursor }
  }
}"#;

const COMMENTS_SINCE_QUERY: &str = r#"query($teamId: String!, $since: DateTimeOrDuration!, $first: Int!, $after: String) {
  comments(filter: { issue: { team: { id: { eq: $teamId } } }, updatedAt: { gt: $since } }, first: $first, after: $after) {
    nodes { id body url user { name } issue { id } }
    pageInfo { hasNextPage endCursor }
  }
}"#;

// ---- wire shapes -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct LinearUser {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct LinearWorkflowState {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct LinearLabel {
    name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct LinearLabelConnection {
    nodes: Vec<LinearLabel>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct LinearIssue {
    id: String,
    identifier: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: Option<String>,
    url: String,
    state: LinearWorkflowState,
    #[serde(default)]
    assignee: Option<LinearUser>,
    creator: LinearUser,
    #[serde(default)]
    labels: LinearLabelConnection,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct LinearComment {
    body: String,
    url: String,
    user: LinearUser,
    issue: LinearCommentIssueRef,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct LinearCommentIssueRef {
    id: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct LinearWorkflowStateEntry {
    id: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct LinearProject {
    id: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct LinearLabelEntry {
    id: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

struct IssuePage {
    nodes: Vec<LinearIssue>,
    has_next_page: bool,
    end_cursor: Option<String>,
}

struct CommentPage {
    nodes: Vec<LinearComment>,
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphQlErrorEntry {
    message: String,
}

/// The `Authorization`/`Content-Type` headers every GraphQL call needs. Linear's API key goes in
/// `Authorization` bare (no `Bearer` prefix), unlike GitHub's OAuth-shaped token.
fn build_headers(api_key: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(api_key).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers
}

/// The label this adapter tags every issue it pushes with, and later searches by to find the
/// issue again without keeping any state of its own between calls.
fn ticket_label(ticket: &TicketId) -> String {
    format!("tm-id:{ticket}")
}

/// A ticket-id marker previously applied by [`ticket_label`], if `labels` carries one.
fn ticket_ref_from_labels(labels: &[LinearLabel]) -> Option<&str> {
    labels.iter().find_map(|l| l.name.strip_prefix("tm-id:"))
}

fn issue_create_variables(
    team_id: &str,
    projection: &Projection,
    label_ids: &[String],
    state_id: &str,
    project_id: &Option<String>,
) -> serde_json::Value {
    serde_json::json!({
        "teamId": team_id,
        "title": projection.title,
        "description": projection.body,
        "stateId": state_id,
        "labelIds": label_ids,
        "projectId": project_id,
    })
}

fn issue_update_variables(
    issue_id: &str,
    projection: &Projection,
    label_ids: &[String],
    state_id: &str,
    project_id: &Option<String>,
) -> serde_json::Value {
    serde_json::json!({
        "issueId": issue_id,
        "title": projection.title,
        "description": projection.body,
        "stateId": state_id,
        "labelIds": label_ids,
        "projectId": project_id,
    })
}

/// Every GraphQL response, success or failure, is HTTP 200 with a JSON body; errors surface in
/// a top-level `errors` array rather than the HTTP status. A non-2xx status (auth failure,
/// gateway error) has no such body shape and is classified from the status code alone.
fn parse_graphql_response(status: u16, body: &[u8]) -> Result<serde_json::Value> {
    if !(200..300).contains(&status) {
        let message = String::from_utf8_lossy(body);
        return Err(match status {
            401 | 403 => TmError::Provider(format!("linear: authentication failed: {message}")),
            429 => TmError::Provider(format!("linear: rate limited: {message}")),
            _ => TmError::Provider(format!("linear: http {status}: {message}")),
        });
    }
    let envelope: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| TmError::parse(format!("linear: malformed response: {e}")))?;
    if let Some(errors) = envelope.get("errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            let messages: Vec<String> = errors
                .iter()
                .filter_map(|e| serde_json::from_value::<GraphQlErrorEntry>(e.clone()).ok())
                .map(|e| e.message)
                .collect();
            return Err(TmError::Provider(format!(
                "linear: graphql error: {}",
                messages.join("; ")
            )));
        }
    }
    envelope
        .get("data")
        .cloned()
        .ok_or_else(|| TmError::parse("linear: response has no data field".to_string()))
}

fn parse_issue_list(data: &serde_json::Value, field: &str) -> Result<Vec<LinearIssue>> {
    let nodes = data
        .get(field)
        .and_then(|v| v.get("nodes"))
        .cloned()
        .unwrap_or(serde_json::Value::Array(Vec::new()));
    serde_json::from_value(nodes)
        .map_err(|e| TmError::parse(format!("linear: malformed issue list: {e}")))
}

fn parse_issue_page(data: &serde_json::Value) -> Result<IssuePage> {
    let issues = data
        .get("issues")
        .ok_or_else(|| TmError::parse("linear: response missing issues field".to_string()))?;
    let nodes: Vec<LinearIssue> = serde_json::from_value(
        issues
            .get("nodes")
            .cloned()
            .unwrap_or(serde_json::Value::Array(Vec::new())),
    )
    .map_err(|e| TmError::parse(format!("linear: malformed issue page: {e}")))?;
    let page_info: PageInfo = serde_json::from_value(
        issues
            .get("pageInfo")
            .cloned()
            .ok_or_else(|| TmError::parse("linear: issue page missing pageInfo".to_string()))?,
    )
    .map_err(|e| TmError::parse(format!("linear: malformed pageInfo: {e}")))?;
    Ok(IssuePage {
        nodes,
        has_next_page: page_info.has_next_page,
        end_cursor: page_info.end_cursor,
    })
}

fn parse_comment_page(data: &serde_json::Value) -> Result<CommentPage> {
    let comments = data
        .get("comments")
        .ok_or_else(|| TmError::parse("linear: response missing comments field".to_string()))?;
    let nodes: Vec<LinearComment> = serde_json::from_value(
        comments
            .get("nodes")
            .cloned()
            .unwrap_or(serde_json::Value::Array(Vec::new())),
    )
    .map_err(|e| TmError::parse(format!("linear: malformed comment page: {e}")))?;
    let page_info: PageInfo = serde_json::from_value(
        comments
            .get("pageInfo")
            .cloned()
            .ok_or_else(|| TmError::parse("linear: comment page missing pageInfo".to_string()))?,
    )
    .map_err(|e| TmError::parse(format!("linear: malformed pageInfo: {e}")))?;
    Ok(CommentPage {
        nodes,
        has_next_page: page_info.has_next_page,
        end_cursor: page_info.end_cursor,
    })
}

fn parse_workflow_states(data: &serde_json::Value) -> Result<Vec<LinearWorkflowStateEntry>> {
    let nodes = data
        .get("workflowStates")
        .and_then(|v| v.get("nodes"))
        .cloned()
        .unwrap_or(serde_json::Value::Array(Vec::new()));
    serde_json::from_value(nodes)
        .map_err(|e| TmError::parse(format!("linear: malformed workflow state list: {e}")))
}

fn parse_project_list(data: &serde_json::Value) -> Result<Vec<LinearProject>> {
    let nodes = data
        .get("projects")
        .and_then(|v| v.get("nodes"))
        .cloned()
        .unwrap_or(serde_json::Value::Array(Vec::new()));
    serde_json::from_value(nodes)
        .map_err(|e| TmError::parse(format!("linear: malformed project list: {e}")))
}

fn parse_created_project(data: &serde_json::Value) -> Result<String> {
    let project: LinearProject = data
        .get("projectCreate")
        .and_then(|v| v.get("project"))
        .cloned()
        .ok_or_else(|| TmError::parse("linear: malformed projectCreate response".to_string()))
        .and_then(|v| {
            serde_json::from_value(v)
                .map_err(|e| TmError::parse(format!("linear: malformed project: {e}")))
        })?;
    Ok(project.id)
}

fn parse_label_list(data: &serde_json::Value) -> Result<Vec<LinearLabelEntry>> {
    let nodes = data
        .get("issueLabels")
        .and_then(|v| v.get("nodes"))
        .cloned()
        .unwrap_or(serde_json::Value::Array(Vec::new()));
    serde_json::from_value(nodes)
        .map_err(|e| TmError::parse(format!("linear: malformed label list: {e}")))
}

fn parse_created_label(data: &serde_json::Value) -> Result<String> {
    let label: LinearLabelEntry = data
        .get("issueLabelCreate")
        .and_then(|v| v.get("issueLabel"))
        .cloned()
        .ok_or_else(|| TmError::parse("linear: malformed issueLabelCreate response".to_string()))
        .and_then(|v| {
            serde_json::from_value(v)
                .map_err(|e| TmError::parse(format!("linear: malformed label: {e}")))
        })?;
    Ok(label.id)
}

fn parse_mutated_issue(data: &serde_json::Value, mutation_field: &str) -> Result<LinearIssue> {
    let issue = data
        .get(mutation_field)
        .and_then(|v| v.get("issue"))
        .cloned()
        .ok_or_else(|| TmError::parse(format!("linear: malformed {mutation_field} response")))?;
    serde_json::from_value(issue)
        .map_err(|e| TmError::parse(format!("linear: malformed issue: {e}")))
}

fn issue_to_external_ref(adapter: &str, issue: &LinearIssue) -> ExternalRef {
    ExternalRef {
        adapter: adapter.to_string(),
        external_id: issue.id.clone(),
        url: Some(issue.url.clone()),
    }
}

/// The inbound changes one pulled issue produces: a `StatusHint`/`Assigned` pair for an issue
/// this adapter already mirrors (found via its `tm-id:` label), or a single `IssueCreated` for
/// one a human opened directly that has no such label.
fn issue_changes(issue: &LinearIssue, adapter: &str) -> Vec<ExternalChange> {
    let external = issue_to_external_ref(adapter, issue);
    if ticket_ref_from_labels(&issue.labels.nodes).is_some() {
        vec![
            ExternalChange {
                external: external.clone(),
                kind: ExternalChangeKind::StatusHint {
                    state: issue.state.name.clone(),
                },
                observed_at: Timestamp::EPOCH,
            },
            ExternalChange {
                external,
                kind: ExternalChangeKind::Assigned {
                    assignee: issue.assignee.as_ref().map(|u| u.name.clone()),
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
                author: issue.creator.name.clone(),
            },
            observed_at: Timestamp::EPOCH,
        }]
    }
}

fn comment_to_external_change(comment: &LinearComment, adapter: &str) -> ExternalChange {
    ExternalChange {
        external: ExternalRef {
            adapter: adapter.to_string(),
            external_id: comment.issue.id.clone(),
            url: Some(comment.url.clone()),
        },
        kind: ExternalChangeKind::CommentAdded {
            author: comment.user.name.clone(),
            body: comment.body.clone(),
        },
        observed_at: Timestamp::EPOCH,
    }
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
            state_hint: "In Progress".to_string(),
            labels: vec!["bug".to_string()],
            milestone: None,
            checklist: vec![],
            degradations: vec![],
        }
    }

    fn sample_issue_json() -> &'static str {
        r#"{
            "id": "issue-uuid-1",
            "identifier": "ENG-42",
            "title": "Fix the widget",
            "description": "Do the thing.",
            "url": "https://linear.app/acme/issue/ENG-42",
            "state": { "name": "In Progress" },
            "assignee": { "name": "octocat" },
            "creator": { "name": "octocat" },
            "labels": { "nodes": [{ "name": "tm-id:T-1" }] }
        }"#
    }

    #[test]
    fn ticket_label_uses_stable_marker_prefix() {
        assert_eq!(ticket_label(&ticket("T-42")), "tm-id:T-42");
    }

    #[test]
    fn ticket_ref_from_labels_finds_marker() {
        let labels = vec![
            LinearLabel {
                name: "bug".to_string(),
            },
            LinearLabel {
                name: "tm-id:T-7".to_string(),
            },
        ];
        assert_eq!(ticket_ref_from_labels(&labels), Some("T-7"));
    }

    #[test]
    fn ticket_ref_from_labels_absent_when_no_marker() {
        let labels = vec![LinearLabel {
            name: "bug".to_string(),
        }];
        assert_eq!(ticket_ref_from_labels(&labels), None);
    }

    #[test]
    fn capabilities_declare_native_parent_child_and_arbitrary_states() {
        let tracker = LinearTracker::with_config("linear", "team-1", "key", "http://x").unwrap();
        let caps = tracker.capabilities();
        assert!(caps.parent_child);
        assert!(caps.arbitrary_states);
        assert_eq!(caps.max_body_bytes, MAX_BODY_BYTES);
    }

    #[test]
    fn issue_create_variables_carries_state_and_labels() {
        let projection = sample_projection();
        let vars = issue_create_variables(
            "team-1",
            &projection,
            &["label-1".to_string()],
            "state-1",
            &Some("project-1".to_string()),
        );
        assert_eq!(vars["title"], "Fix the widget");
        assert_eq!(vars["stateId"], "state-1");
        assert_eq!(vars["labelIds"][0], "label-1");
        assert_eq!(vars["projectId"], "project-1");
    }

    #[test]
    fn issue_update_variables_targets_issue_id() {
        let projection = sample_projection();
        let vars = issue_update_variables("issue-1", &projection, &[], "state-2", &None);
        assert_eq!(vars["issueId"], "issue-1");
        assert!(vars["projectId"].is_null());
    }

    #[test]
    fn parse_graphql_response_returns_data_on_success() {
        let body = br#"{"data": {"issues": {"nodes": []}}}"#;
        let data = parse_graphql_response(200, body).expect("valid graphql envelope");
        assert!(data.get("issues").is_some());
    }

    #[test]
    fn parse_graphql_response_surfaces_graphql_errors_even_on_http_200() {
        let body = br#"{"errors": [{"message": "team not found"}], "data": null}"#;
        let result = parse_graphql_response(200, body);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("team not found"));
    }

    #[test]
    fn parse_graphql_response_classifies_auth_failure() {
        let result = parse_graphql_response(401, b"unauthorized");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("authentication failed"));
    }

    #[test]
    fn parse_graphql_response_classifies_rate_limit() {
        let result = parse_graphql_response(429, b"slow down");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rate limited"));
    }

    #[test]
    fn parse_graphql_response_rejects_malformed_json() {
        let result = parse_graphql_response(200, b"not json");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse:"));
    }

    #[test]
    fn parse_graphql_response_rejects_missing_data_field() {
        let result = parse_graphql_response(200, br#"{"foo": 1}"#);
        assert!(result.is_err());
    }

    #[test]
    fn parse_mutated_issue_happy_path() {
        let json = format!(r#"{{"issueCreate": {{"issue": {}}}}}"#, sample_issue_json());
        let data: serde_json::Value = serde_json::from_str(&json).unwrap();
        let issue = parse_mutated_issue(&data, "issueCreate").expect("valid issue");
        assert_eq!(issue.identifier, "ENG-42");
        assert_eq!(issue.state.name, "In Progress");
    }

    #[test]
    fn parse_mutated_issue_rejects_missing_field() {
        let data: serde_json::Value = serde_json::from_str(r#"{"issueCreate": {}}"#).unwrap();
        let result = parse_mutated_issue(&data, "issueCreate");
        assert!(result.is_err());
    }

    #[test]
    fn parse_issue_page_reads_nodes_and_page_info() {
        let json = format!(
            r#"{{"issues": {{"nodes": [{}], "pageInfo": {{"hasNextPage": true, "endCursor": "cursor-1"}}}}}}"#,
            sample_issue_json()
        );
        let data: serde_json::Value = serde_json::from_str(&json).unwrap();
        let page = parse_issue_page(&data).expect("valid page");
        assert_eq!(page.nodes.len(), 1);
        assert!(page.has_next_page);
        assert_eq!(page.end_cursor, Some("cursor-1".to_string()));
    }

    #[test]
    fn parse_issue_page_handles_final_page_without_cursor() {
        let json =
            r#"{"issues": {"nodes": [], "pageInfo": {"hasNextPage": false, "endCursor": null}}}"#;
        let data: serde_json::Value = serde_json::from_str(json).unwrap();
        let page = parse_issue_page(&data).expect("valid page");
        assert!(!page.has_next_page);
        assert_eq!(page.end_cursor, None);
    }

    #[test]
    fn parse_issue_page_rejects_missing_issues_field() {
        let data: serde_json::Value = serde_json::from_str(r#"{"other": 1}"#).unwrap();
        assert!(parse_issue_page(&data).is_err());
    }

    #[test]
    fn parse_created_project_extracts_id() {
        let data: serde_json::Value = serde_json::from_str(
            r#"{"projectCreate": {"project": {"id": "proj-1", "name": "M1"}}}"#,
        )
        .unwrap();
        assert_eq!(parse_created_project(&data).unwrap(), "proj-1");
    }

    #[test]
    fn parse_created_label_extracts_id() {
        let data: serde_json::Value = serde_json::from_str(
            r#"{"issueLabelCreate": {"issueLabel": {"id": "label-1", "name": "tm-id:T-1"}}}"#,
        )
        .unwrap();
        assert_eq!(parse_created_label(&data).unwrap(), "label-1");
    }

    #[test]
    fn issue_changes_mirrored_issue_produces_status_and_assignment() {
        let json: serde_json::Value = serde_json::from_str(sample_issue_json()).unwrap();
        let issue: LinearIssue = serde_json::from_value(json).unwrap();
        let changes = issue_changes(&issue, "linear");
        assert_eq!(changes.len(), 2);
        match &changes[0].kind {
            ExternalChangeKind::StatusHint { state } => assert_eq!(state, "In Progress"),
            _ => panic!("expected StatusHint"),
        }
        match &changes[1].kind {
            ExternalChangeKind::Assigned { assignee } => {
                assert_eq!(assignee.as_deref(), Some("octocat"))
            }
            _ => panic!("expected Assigned"),
        }
    }

    #[test]
    fn issue_changes_human_created_issue_produces_issue_created() {
        let mut json: serde_json::Value = serde_json::from_str(sample_issue_json()).unwrap();
        json["labels"]["nodes"] = serde_json::json!([]);
        let issue: LinearIssue = serde_json::from_value(json).unwrap();
        let changes = issue_changes(&issue, "linear");
        assert_eq!(changes.len(), 1);
        match &changes[0].kind {
            ExternalChangeKind::IssueCreated { title, author, .. } => {
                assert_eq!(title, "Fix the widget");
                assert_eq!(author, "octocat");
            }
            _ => panic!("expected IssueCreated"),
        }
    }

    #[test]
    fn comment_to_external_change_maps_fields() {
        let comment = LinearComment {
            body: "looks good".to_string(),
            url: "https://linear.app/acme/issue/ENG-42#comment-1".to_string(),
            user: LinearUser {
                name: "alice".to_string(),
            },
            issue: LinearCommentIssueRef {
                id: "issue-uuid-1".to_string(),
            },
        };
        let change = comment_to_external_change(&comment, "linear");
        assert_eq!(change.external.external_id, "issue-uuid-1");
        match change.kind {
            ExternalChangeKind::CommentAdded { author, body } => {
                assert_eq!(author, "alice");
                assert_eq!(body, "looks good");
            }
            _ => panic!("expected CommentAdded"),
        }
    }

    #[test]
    fn issue_to_external_ref_uses_graphql_node_id_and_url() {
        let json: serde_json::Value = serde_json::from_str(sample_issue_json()).unwrap();
        let issue: LinearIssue = serde_json::from_value(json).unwrap();
        let external = issue_to_external_ref("linear", &issue);
        assert_eq!(external.adapter, "linear");
        assert_eq!(external.external_id, "issue-uuid-1");
        assert_eq!(
            external.url,
            Some("https://linear.app/acme/issue/ENG-42".to_string())
        );
    }
}
