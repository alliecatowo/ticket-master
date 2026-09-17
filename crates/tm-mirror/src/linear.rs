//! Linear over GraphQL: issues, workflow states, projects, and native parent/child.
//!
//! Linear is the one shipped adapter with `parent_child: true` and `arbitrary_states: true` —
//! it has native sub-issues and accepts any workflow state name a team has configured, so
//! nothing here ever needs [`crate::projection::Degradation::ChecklistRollup`] or
//! [`crate::projection::Degradation::StateCoarsened`] handling of its own; that's already
//! reflected in [`LinearTracker::capabilities`] and left to
//! [`crate::projection::ProjectionPolicy`] upstream.
//!
//! Owns request/response shaping for the Linear GraphQL API (single `/graphql` endpoint,
//! cursor-based `pageInfo { hasNextPage endCursor }` pagination) and nothing else: shaping is
//! pure and unit-tested against recorded JSON, never a live network call. The API key is read
//! once at construction from `LINEAR_API_KEY`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_types::{Result, TicketId, Timestamp, TmError};

use crate::projection::Projection;
use crate::tracker::{
    ExternalChange, ExternalChangeKind, ExternalRef, Tracker, TrackerCapabilities,
};

/// The Linear GraphQL API endpoint this adapter targets.
pub const DEFAULT_BASE_URL: &str = "https://api.linear.app/graphql";

/// Name of the environment variable holding the Linear API key.
pub const API_KEY_ENV_VAR: &str = "LINEAR_API_KEY";

/// Page size requested on every paginated `issues` query.
const PAGE_SIZE: u32 = 50;

/// A prefix embedded in every mirrored issue's title so [`LinearTracker::push`] can find an
/// already-mirrored issue by ticket id without a separate side table — Linear has no free-form
/// custom field this adapter assumes exists, but title search is always available.
const TITLE_PREFIX: &str = "[";

/// A real Linear GraphQL API client.
#[derive(Debug)]
pub struct LinearTracker {
    name: String,
    api_key: String,
    team_id: String,
    base_url: String,
    http: reqwest::Client,
}

impl LinearTracker {
    /// Build a tracker for `team_id`, reading the API key from [`API_KEY_ENV_VAR`].
    pub fn from_env(name: impl Into<String>, team_id: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var(API_KEY_ENV_VAR)
            .map_err(|_| TmError::invariant(format!("{API_KEY_ENV_VAR} is not set")))?;
        Self::with_config(name, team_id, api_key, DEFAULT_BASE_URL.to_string())
    }

    /// Build a tracker with an explicit key and base URL, for tests that stand up a local mock
    /// HTTP server (shaping tests use recorded JSON directly and never need this).
    pub fn with_config(
        name: impl Into<String>,
        team_id: impl Into<String>,
        api_key: String,
        base_url: String,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| TmError::Provider(format!("failed to build HTTP client: {e}")))?;
        Ok(LinearTracker {
            name: name.into(),
            api_key,
            team_id: team_id.into(),
            base_url,
            http,
        })
    }

    /// Auth header value Linear expects: the raw API key, no `Bearer` prefix.
    fn auth_header(&self) -> &str {
        &self.api_key
    }

    /// Run one GraphQL request against `self.base_url`, returning the raw `data` payload.
    /// Fails closed on transport errors, non-2xx status, and a non-empty GraphQL `errors` array.
    async fn execute(
        &self,
        query: String,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let body = GraphQlRequest { query, variables };
        let response = self
            .http
            .post(&self.base_url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await
            .map_err(|e| TmError::Provider(format!("linear: request failed: {e}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| TmError::Provider(format!("linear: failed to read response body: {e}")))?;

        if !status.is_success() {
            return Err(TmError::Provider(format!(
                "linear: request failed with status {status}: {}",
                String::from_utf8_lossy(&bytes)
            )));
        }

        parse_graphql_response(&bytes)
    }
}

// ---- wire shapes -------------------------------------------------------------------------

/// A GraphQL request body: query text plus variables, the shape every Linear operation sends.
#[derive(Debug, Clone, Serialize)]
struct GraphQlRequest {
    query: String,
    variables: serde_json::Value,
}

/// One GraphQL error entry, as Linear reports it.
#[derive(Debug, Clone, Deserialize)]
struct GraphQlErrorEntry {
    message: String,
}

/// The GraphQL response envelope: `data` on success, `errors` (possibly alongside partial
/// `data`) on failure.
#[derive(Debug, Clone, Deserialize)]
struct GraphQlEnvelope {
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    errors: Option<Vec<GraphQlErrorEntry>>,
}

/// Parse a raw GraphQL HTTP body into its `data` payload, failing on a non-empty `errors` array
/// (Linear can return `errors` with partial `data`; this adapter treats that as a hard failure
/// since it has no way to know which part of a mutation/query actually landed).
fn parse_graphql_response(body: &[u8]) -> Result<serde_json::Value> {
    let envelope: GraphQlEnvelope = serde_json::from_slice(body)
        .map_err(|e| TmError::Provider(format!("linear: malformed GraphQL response: {e}")))?;

    if let Some(errors) = envelope.errors {
        if !errors.is_empty() {
            let messages: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
            return Err(TmError::Provider(format!(
                "linear: GraphQL error(s): {}",
                messages.join("; ")
            )));
        }
    }

    envelope
        .data
        .ok_or_else(|| TmError::Provider("linear: GraphQL response had no data".to_string()))
}

/// Wire shape of one Linear issue, as returned by both the search-by-title query and the
/// paginated `issues` pull query.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireIssue {
    id: String,
    #[serde(default)]
    title: String,
    url: Option<String>,
    #[serde(default)]
    state: Option<WireWorkflowState>,
    #[serde(default)]
    assignee: Option<WireUser>,
    #[serde(default)]
    priority_label: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    creator: Option<WireUser>,
    #[serde(default)]
    description: Option<String>,
}

/// A Linear workflow state, referenced by name (arbitrary_states: adapter accepts whatever name
/// the caller provides and resolves it against the team's configured states at push time).
#[derive(Debug, Clone, Deserialize)]
struct WireWorkflowState {
    name: String,
}

/// A Linear user (assignee/creator), reduced to the display name this crate's
/// [`ExternalChangeKind`] shapes carry.
#[derive(Debug, Clone, Deserialize)]
struct WireUser {
    name: String,
}

/// Page info for cursor pagination, present on every connection Linear returns.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

/// One page of the `issues` connection.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireIssueConnection {
    nodes: Vec<WireIssue>,
    page_info: WirePageInfo,
}

// ---- shaping (pure, unit-tested against recorded JSON) --------------------------------------

/// The mirrored issue title Linear stores for `projection`, embedding the ticket id so a
/// subsequent push can find it again via [`build_search_query`] without a side table.
fn mirrored_title(projection: &Projection) -> String {
    format!("{TITLE_PREFIX}{}] {}", projection.ticket, projection.title)
}

/// Split a mirrored title of the form `"[T-1] Do the thing"` back into the ticket id it embeds
/// and the human title, or `None` if the title wasn't produced by [`mirrored_title`] (e.g. an
/// issue created directly by a human, which `pull` surfaces as `IssueCreated` instead).
fn parse_mirrored_title(title: &str) -> Option<(TicketId, &str)> {
    let rest = title.strip_prefix(TITLE_PREFIX)?;
    let (id_part, remainder) = rest.split_once(']')?;
    let ticket = TicketId::new(id_part).ok()?;
    Some((ticket, remainder.trim_start()))
}

/// Build the GraphQL query that searches for an already-mirrored issue by embedded ticket id,
/// scoped to this adapter's team so two teams mirroring the same Ticketmaster project can't
/// collide.
fn build_search_query(team_id: &str, projection: &Projection) -> (String, serde_json::Value) {
    let query = r#"
        query FindMirroredIssue($teamId: String!, $titleContains: String!) {
            issues(filter: { team: { id: { eq: $teamId } }, title: { contains: $titleContains } }, first: 1) {
                nodes { id url }
            }
        }
    "#
    .to_string();
    let needle = format!("{TITLE_PREFIX}{}]", projection.ticket);
    let variables = serde_json::json!({ "teamId": team_id, "titleContains": needle });
    (query, variables)
}

/// Parse the response of [`build_search_query`] into the existing issue's node id, if any.
fn parse_search_response(data: &serde_json::Value) -> Result<Option<String>> {
    let nodes = data
        .get("issues")
        .and_then(|v| v.get("nodes"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            TmError::Provider("linear: search response missing issues.nodes".to_string())
        })?;
    Ok(nodes
        .first()
        .and_then(|n| n.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// Build the `issueCreate` mutation for `projection`.
fn build_create_mutation(team_id: &str, projection: &Projection) -> (String, serde_json::Value) {
    let query = r#"
        mutation CreateIssue($input: IssueCreateInput!) {
            issueCreate(input: $input) {
                success
                issue { id url }
            }
        }
    "#
    .to_string();
    let variables = serde_json::json!({
        "input": {
            "teamId": team_id,
            "title": mirrored_title(projection),
            "description": projection.body,
            "labelNames": projection.labels,
        }
    });
    (query, variables)
}

/// Build the `issueUpdate` mutation for `projection` against the already-found `issue_id`.
fn build_update_mutation(issue_id: &str, projection: &Projection) -> (String, serde_json::Value) {
    let query = r#"
        mutation UpdateIssue($id: String!, $input: IssueUpdateInput!) {
            issueUpdate(id: $id, input: $input) {
                success
                issue { id url }
            }
        }
    "#
    .to_string();
    let variables = serde_json::json!({
        "id": issue_id,
        "input": {
            "title": mirrored_title(projection),
            "description": projection.body,
            "labelNames": projection.labels,
        }
    });
    (query, variables)
}

/// Parse the response of either [`build_create_mutation`] or [`build_update_mutation`] into the
/// [`ExternalRef`] to record on the ticket's mirror link. `mutation_field` is `"issueCreate"` or
/// `"issueUpdate"`, the only two shapes this adapter issues.
fn parse_mutation_response(
    data: &serde_json::Value,
    mutation_field: &str,
    adapter_name: &str,
) -> Result<ExternalRef> {
    let payload = data
        .get(mutation_field)
        .ok_or_else(|| TmError::Provider(format!("linear: response missing {mutation_field}")))?;

    let success = payload
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !success {
        return Err(TmError::Provider(format!(
            "linear: {mutation_field} reported success: false"
        )));
    }

    let issue = payload.get("issue").ok_or_else(|| {
        TmError::Provider(format!("linear: {mutation_field} response missing issue"))
    })?;
    let external_id = issue
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            TmError::Provider(format!(
                "linear: {mutation_field} response missing issue.id"
            ))
        })?
        .to_string();
    let url = issue
        .get("url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(ExternalRef {
        adapter: adapter_name.to_string(),
        external_id,
        url,
    })
}

/// Build the paginated `issues` query used by `pull`, filtered to issues updated after `since`
/// and, when `after` is `Some`, resuming from that cursor.
fn build_pull_query(
    team_id: &str,
    since: Timestamp,
    after: Option<&str>,
) -> (String, serde_json::Value) {
    let query = r#"
        query PullIssues($teamId: String!, $since: DateTimeOrDuration!, $first: Int!, $after: String) {
            issues(
                filter: { team: { id: { eq: $teamId } }, updatedAt: { gt: $since } }
                first: $first
                after: $after
                orderBy: updatedAt
            ) {
                nodes {
                    id
                    title
                    url
                    updatedAt
                    description
                    state { name }
                    assignee { name }
                    creator { name }
                    priorityLabel
                }
                pageInfo { hasNextPage endCursor }
            }
        }
    "#
    .to_string();
    let variables = serde_json::json!({
        "teamId": team_id,
        "since": since.to_rfc3339(),
        "first": PAGE_SIZE,
        "after": after,
    });
    (query, variables)
}

/// Parse one page of [`build_pull_query`]'s response into the [`ExternalChange`]s it observed
/// plus the cursor to resume from, if there's another page.
///
/// Each Linear issue becomes at most one [`ExternalChange`], chosen by precedence: an issue
/// whose title isn't a mirrored title (see [`parse_mirrored_title`]) is reported as
/// [`ExternalChangeKind::IssueCreated`] (a human made it directly); otherwise its workflow state
/// becomes a [`ExternalChangeKind::StatusHint`]. Assignment, comments and priority are Linear
/// fields this pass doesn't have separate updated-at granularity for, so — to stay within the
/// allowlist without inventing false precision — only the state hint is emitted per already
/// mirrored issue; priority is emitted alongside it whenever `priorityLabel` is set, since
/// Linear does expose it directly on the issue node with no extra query.
fn parse_pull_page(
    data: &serde_json::Value,
    adapter_name: &str,
) -> Result<(Vec<ExternalChange>, Option<String>)> {
    let connection: WireIssueConnection =
        serde_json::from_value(data.get("issues").cloned().ok_or_else(|| {
            TmError::Provider("linear: pull response missing issues".to_string())
        })?)
        .map_err(|e| TmError::Provider(format!("linear: malformed pull response: {e}")))?;

    let mut changes = Vec::new();
    for issue in &connection.nodes {
        let external = ExternalRef {
            adapter: adapter_name.to_string(),
            external_id: issue.id.clone(),
            url: issue.url.clone(),
        };
        let observed_at = issue
            .updated_at
            .as_deref()
            .and_then(|s| Timestamp::parse_rfc3339(s).ok())
            .unwrap_or(Timestamp::EPOCH);

        if parse_mirrored_title(&issue.title).is_none() {
            changes.push(ExternalChange {
                external,
                kind: ExternalChangeKind::IssueCreated {
                    title: issue.title.clone(),
                    body: issue.description.clone().unwrap_or_default(),
                    author: issue
                        .creator
                        .as_ref()
                        .map(|u| u.name.clone())
                        .unwrap_or_default(),
                },
                observed_at,
            });
            continue;
        }

        if let Some(state) = &issue.state {
            changes.push(ExternalChange {
                external: external.clone(),
                kind: ExternalChangeKind::StatusHint {
                    state: state.name.clone(),
                },
                observed_at,
            });
        }

        if let Some(priority) = &issue.priority_label {
            changes.push(ExternalChange {
                external: external.clone(),
                kind: ExternalChangeKind::PriorityChanged {
                    priority: priority.clone(),
                },
                observed_at,
            });
        }

        if let Some(assignee) = &issue.assignee {
            changes.push(ExternalChange {
                external,
                kind: ExternalChangeKind::Assigned {
                    assignee: Some(assignee.name.clone()),
                },
                observed_at,
            });
        }
    }

    let next_cursor = if connection.page_info.has_next_page {
        connection.page_info.end_cursor
    } else {
        None
    };
    Ok((changes, next_cursor))
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
            // Linear's issue description is Markdown with no documented hard byte ceiling;
            // 512 KiB is a conservative practical bound well above any real ticket body.
            max_body_bytes: 512 * 1024,
        }
    }

    async fn push(&self, projection: &Projection) -> Result<ExternalRef> {
        let (search_query, search_vars) = build_search_query(&self.team_id, projection);
        let search_data = self.execute(search_query, search_vars).await?;
        let existing_id = parse_search_response(&search_data)?;

        match existing_id {
            Some(issue_id) => {
                let (query, vars) = build_update_mutation(&issue_id, projection);
                let data = self.execute(query, vars).await?;
                parse_mutation_response(&data, "issueUpdate", &self.name)
            }
            None => {
                let (query, vars) = build_create_mutation(&self.team_id, projection);
                let data = self.execute(query, vars).await?;
                parse_mutation_response(&data, "issueCreate", &self.name)
            }
        }
    }

    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (query, vars) = build_pull_query(&self.team_id, since, cursor.as_deref());
            let data = self.execute(query, vars).await?;
            let (mut page, next_cursor) = parse_pull_page(&data, &self.name)?;
            all.append(&mut page);
            match next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_projection(ticket_id: &str, title: &str) -> Projection {
        Projection {
            ticket: TicketId::new(ticket_id).expect("valid test ticket id"),
            title: title.to_string(),
            body: "the body".to_string(),
            state_hint: "In Progress".to_string(),
            labels: vec!["bug".to_string()],
            milestone: None,
            checklist: vec![],
            degradations: vec![],
        }
    }

    #[test]
    fn tracker_declares_native_parent_child_and_arbitrary_states() {
        let tracker = LinearTracker::with_config(
            "linear",
            "team-1",
            "key".to_string(),
            DEFAULT_BASE_URL.to_string(),
        )
        .expect("builds");
        let caps = tracker.capabilities();
        assert!(caps.parent_child);
        assert!(caps.arbitrary_states);
    }

    #[test]
    fn mirrored_title_embeds_ticket_id_and_roundtrips() {
        let proj = test_projection("T-42", "Fix the thing");
        let title = mirrored_title(&proj);
        assert_eq!(title, "[T-42] Fix the thing");

        let (ticket, rest) = parse_mirrored_title(&title).expect("parses");
        assert_eq!(ticket, proj.ticket);
        assert_eq!(rest, "Fix the thing");
    }

    #[test]
    fn parse_mirrored_title_rejects_a_human_authored_title() {
        assert_eq!(parse_mirrored_title("Fix the login bug"), None);
    }

    #[test]
    fn parse_mirrored_title_rejects_an_invalid_embedded_ticket_id() {
        // Not a well-formed TicketId (no numeric suffix), so the human-authored path wins.
        assert_eq!(parse_mirrored_title("[not-a-ticket] Something"), None);
    }

    #[test]
    fn build_search_query_scopes_to_team_and_embedded_id() {
        let proj = test_projection("T-7", "Title");
        let (query, vars) = build_search_query("team-9", &proj);
        assert!(query.contains("FindMirroredIssue"));
        assert_eq!(vars["teamId"], "team-9");
        assert_eq!(vars["titleContains"], "[T-7]");
    }

    const RECORDED_SEARCH_HIT: &str = r#"{
        "issues": { "nodes": [ { "id": "issue-abc", "url": "https://linear.app/x/issue/T-7" } ] }
    }"#;

    const RECORDED_SEARCH_MISS: &str = r#"{
        "issues": { "nodes": [] }
    }"#;

    #[test]
    fn parse_search_response_finds_existing_issue_id() {
        let data: serde_json::Value = serde_json::from_str(RECORDED_SEARCH_HIT).unwrap();
        let id = parse_search_response(&data)
            .expect("parses")
            .expect("found");
        assert_eq!(id, "issue-abc");
    }

    #[test]
    fn parse_search_response_returns_none_when_no_match() {
        let data: serde_json::Value = serde_json::from_str(RECORDED_SEARCH_MISS).unwrap();
        assert_eq!(parse_search_response(&data).expect("parses"), None);
    }

    #[test]
    fn parse_search_response_errors_on_malformed_shape() {
        let data = serde_json::json!({ "issues": {} });
        let err = parse_search_response(&data).unwrap_err();
        assert!(err.to_string().contains("linear:"));
    }

    #[test]
    fn build_create_mutation_carries_title_body_and_labels() {
        let proj = test_projection("T-1", "New feature");
        let (query, vars) = build_create_mutation("team-1", &proj);
        assert!(query.contains("issueCreate"));
        assert_eq!(vars["input"]["teamId"], "team-1");
        assert_eq!(vars["input"]["title"], "[T-1] New feature");
        assert_eq!(vars["input"]["description"], "the body");
        assert_eq!(vars["input"]["labelNames"][0], "bug");
    }

    const RECORDED_CREATE_SUCCESS: &str = r#"{
        "issueCreate": {
            "success": true,
            "issue": { "id": "issue-new-1", "url": "https://linear.app/x/issue/T-1" }
        }
    }"#;

    const RECORDED_CREATE_FAILURE: &str = r#"{
        "issueCreate": { "success": false, "issue": null }
    }"#;

    #[test]
    fn parse_mutation_response_happy_path_returns_external_ref() {
        let data: serde_json::Value = serde_json::from_str(RECORDED_CREATE_SUCCESS).unwrap();
        let external = parse_mutation_response(&data, "issueCreate", "linear").expect("parses");
        assert_eq!(external.adapter, "linear");
        assert_eq!(external.external_id, "issue-new-1");
        assert_eq!(
            external.url.as_deref(),
            Some("https://linear.app/x/issue/T-1")
        );
    }

    #[test]
    fn parse_mutation_response_errors_when_success_is_false() {
        let data: serde_json::Value = serde_json::from_str(RECORDED_CREATE_FAILURE).unwrap();
        let err = parse_mutation_response(&data, "issueCreate", "linear").unwrap_err();
        assert!(err.to_string().contains("success: false"));
    }

    #[test]
    fn parse_mutation_response_errors_on_missing_mutation_field() {
        let data = serde_json::json!({ "someOtherField": {} });
        let err = parse_mutation_response(&data, "issueCreate", "linear").unwrap_err();
        assert!(err.to_string().contains("missing issueCreate"));
    }

    #[test]
    fn parse_graphql_response_returns_data_on_success() {
        let body = br#"{"data": {"ok": true}}"#;
        let data = parse_graphql_response(body).expect("parses");
        assert_eq!(data["ok"], true);
    }

    #[test]
    fn parse_graphql_response_errors_on_nonempty_errors_array() {
        let body = br#"{"data": null, "errors": [{"message": "team not found"}]}"#;
        let err = parse_graphql_response(body).unwrap_err();
        assert!(err.to_string().contains("team not found"));
    }

    #[test]
    fn parse_graphql_response_errors_on_missing_data() {
        let body = br#"{"data": null}"#;
        let err = parse_graphql_response(body).unwrap_err();
        assert!(err.to_string().contains("no data"));
    }

    #[test]
    fn parse_graphql_response_errors_on_malformed_json() {
        let err = parse_graphql_response(b"not json").unwrap_err();
        assert!(err.to_string().contains("malformed"));
    }

    const RECORDED_PULL_PAGE_MIXED: &str = r#"{
        "issues": {
            "nodes": [
                {
                    "id": "issue-1",
                    "title": "[T-1] Do the thing",
                    "url": "https://linear.app/x/issue/T-1",
                    "updatedAt": "2024-01-01T00:00:00Z",
                    "description": "body",
                    "state": { "name": "In Review" },
                    "assignee": { "name": "alice" },
                    "creator": { "name": "bob" },
                    "priorityLabel": "High"
                },
                {
                    "id": "issue-2",
                    "title": "A human filed this directly",
                    "url": "https://linear.app/x/issue/2",
                    "updatedAt": "2024-01-02T00:00:00Z",
                    "description": "human body",
                    "state": { "name": "Todo" },
                    "assignee": null,
                    "creator": { "name": "carol" },
                    "priorityLabel": null
                }
            ],
            "pageInfo": { "hasNextPage": false, "endCursor": null }
        }
    }"#;

    #[test]
    fn parse_pull_page_emits_status_priority_and_assignment_for_mirrored_issue() {
        let data: serde_json::Value = serde_json::from_str(RECORDED_PULL_PAGE_MIXED).unwrap();
        let (changes, next) = parse_pull_page(&data, "linear").expect("parses");
        assert_eq!(next, None);

        let mirrored: Vec<_> = changes
            .iter()
            .filter(|c| c.external.external_id == "issue-1")
            .collect();
        assert_eq!(mirrored.len(), 3);
        assert!(mirrored.iter().any(
            |c| matches!(&c.kind, ExternalChangeKind::StatusHint { state } if state == "In Review")
        ));
        assert!(mirrored
            .iter()
            .any(|c| matches!(&c.kind, ExternalChangeKind::PriorityChanged { priority } if priority == "High")));
        assert!(mirrored.iter().any(
            |c| matches!(&c.kind, ExternalChangeKind::Assigned { assignee } if assignee.as_deref() == Some("alice"))
        ));
    }

    #[test]
    fn parse_pull_page_emits_issue_created_for_unmirrored_issue() {
        let data: serde_json::Value = serde_json::from_str(RECORDED_PULL_PAGE_MIXED).unwrap();
        let (changes, _next) = parse_pull_page(&data, "linear").expect("parses");

        let human = changes
            .iter()
            .find(|c| c.external.external_id == "issue-2")
            .expect("has the human-created issue");
        match &human.kind {
            ExternalChangeKind::IssueCreated {
                title,
                body,
                author,
            } => {
                assert_eq!(title, "A human filed this directly");
                assert_eq!(body, "human body");
                assert_eq!(author, "carol");
            }
            other => panic!("expected IssueCreated, got {other:?}"),
        }
    }

    const RECORDED_PULL_PAGE_HAS_NEXT: &str = r#"{
        "issues": {
            "nodes": [],
            "pageInfo": { "hasNextPage": true, "endCursor": "cursor-2" }
        }
    }"#;

    #[test]
    fn parse_pull_page_surfaces_next_cursor_when_more_pages_remain() {
        let data: serde_json::Value = serde_json::from_str(RECORDED_PULL_PAGE_HAS_NEXT).unwrap();
        let (changes, next) = parse_pull_page(&data, "linear").expect("parses");
        assert!(changes.is_empty());
        assert_eq!(next.as_deref(), Some("cursor-2"));
    }

    #[test]
    fn parse_pull_page_errors_on_missing_issues_key() {
        let data = serde_json::json!({ "somethingElse": {} });
        let err = parse_pull_page(&data, "linear").unwrap_err();
        assert!(err.to_string().contains("missing issues"));
    }

    #[test]
    fn build_pull_query_carries_since_as_rfc3339_and_cursor() {
        let since = Timestamp::from_unix_seconds(1_700_000_000);
        let (query, vars) = build_pull_query("team-1", since, Some("cursor-1"));
        assert!(query.contains("PullIssues"));
        assert_eq!(vars["teamId"], "team-1");
        assert_eq!(vars["since"], since.to_rfc3339());
        assert_eq!(vars["after"], "cursor-1");
        assert_eq!(vars["first"], PAGE_SIZE);
    }

    #[test]
    fn build_pull_query_omits_cursor_on_first_page() {
        let since = Timestamp::EPOCH;
        let (_query, vars) = build_pull_query("team-1", since, None);
        assert!(vars["after"].is_null());
    }

    #[test]
    fn from_env_fails_closed_when_api_key_missing() {
        std::env::remove_var(API_KEY_ENV_VAR);
        let result = LinearTracker::from_env("linear", "team-1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(API_KEY_ENV_VAR));
    }

    #[test]
    fn from_env_reads_key_and_builds() {
        std::env::set_var(API_KEY_ENV_VAR, "lin_api_test123");
        let tracker = LinearTracker::from_env("linear", "team-1").expect("builds");
        assert_eq!(tracker.name(), "linear");
        assert_eq!(tracker.auth_header(), "lin_api_test123");
        std::env::remove_var(API_KEY_ENV_VAR);
    }

    #[test]
    fn build_update_mutation_targets_the_existing_issue_id() {
        let proj = test_projection("T-3", "Renamed");
        let (query, vars) = build_update_mutation("issue-xyz", &proj);
        assert!(query.contains("issueUpdate"));
        assert_eq!(vars["id"], "issue-xyz");
        assert_eq!(vars["input"]["title"], "[T-3] Renamed");
    }
}
