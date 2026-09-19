//! GitHub Issues, REST v3, over `reqwest`.
//!
//! Owns issue create/update, comments, labels, milestones and inbound state/assignee/label
//! reads. Credentials are read once, by name, from [`GITHUB_TOKEN_ENV_VAR`]; nothing in this
//! module reads `GITHUB_TOKEN` anywhere except [`GitHubTracker::from_env`], and shaping is
//! unit-testable against recorded JSON with no network call.
//!
//! GitHub's issue model has no native parent/child relationship and only two states
//! (`open`/`closed`), so [`GitHubTracker::capabilities`] declares `parent_child: false` and
//! `arbitrary_states: false`: [`crate::projection::ProjectionPolicy`] rolls descendants into a
//! checklist and coarsens state before this adapter ever sees a [`Projection`].
//!
//! `push` is idempotent across repeated calls for the same ticket, and durably so: every issue
//! this adapter pushes is tagged with a `tm-id:<ticket>` label (mirroring `linear`/`gitlab`'s own
//! convention), and `push` searches GitHub for that label before deciding whether to `POST` a new
//! issue or `PATCH` an existing one — never a local, process-lifetime cache, since a cache that
//! forgets on restart used to mean every fresh `tm mirror push` invocation created a duplicate
//! issue rather than updating the one already there (`docs/audit-2026-09-18-fable.md` A-05/B-11).
//! [`GitHubTracker::confirm`] reuses the same search as the `SPEC.md` §21.5 recovery probe for
//! "the push may have already landed, the local idempotency receipt was lost".

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tm_types::{Clock, Result, TicketId, Timestamp, TmError};

use crate::projection::Projection;
use crate::tracker::{
    ExternalChange, ExternalChangeKind, ExternalRef, Tracker, TrackerCapabilities,
};

/// Name of the environment variable this adapter reads its personal-access/App token from.
pub const GITHUB_TOKEN_ENV_VAR: &str = "GITHUB_TOKEN";

/// Default GitHub REST v3 base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.github.com";

/// GitHub's own ceiling on issue body length, in bytes.
const MAX_BODY_BYTES: usize = 65536;

/// Maximum attempts on a rate-limited or 5xx response before giving up.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Floor for exponential backoff when GitHub gives no usable rate-limit or `Retry-After` header.
const BACKOFF_FLOOR: Duration = Duration::from_millis(500);

/// A prefix on a label name treated as carrying priority (e.g. `priority:high`), the only signal
/// this adapter has for [`ExternalChangeKind::PriorityChanged`] since GitHub Issues has no native
/// priority field.
const PRIORITY_LABEL_PREFIX: &str = "priority:";

/// A live GitHub Issues adapter for one `owner/repo`.
pub struct GitHubTracker {
    name: String,
    owner: String,
    repo: String,
    token: String,
    base_url: String,
    http: reqwest::Client,
    clock: std::sync::Arc<dyn Clock>,
    max_retries: u32,
    /// Milestone title -> milestone number, resolved lazily and cached for the process lifetime.
    /// Purely a performance cache, not an idempotency mechanism: [`GitHubTracker::resolve_milestone`]
    /// always lists GitHub's own milestones and matches by title before ever creating one, so a
    /// cold cache after a restart just costs one extra `GET`, not a duplicate milestone (unlike
    /// the removed `issue_numbers` cache this module used to carry, which decided create-vs-update
    /// from local memory alone with no check against GitHub's own state — see this module's doc
    /// comment).
    milestone_numbers: Mutex<BTreeMap<String, i64>>,
}

impl GitHubTracker {
    /// Build a tracker for `owner/repo`, reading the token from [`GITHUB_TOKEN_ENV_VAR`].
    pub fn from_env(
        name: impl Into<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self> {
        let token = std::env::var(GITHUB_TOKEN_ENV_VAR).map_err(|_| {
            TmError::invariant(format!("mirror: {GITHUB_TOKEN_ENV_VAR} is not set"))
        })?;
        Self::with_config(
            name,
            owner,
            repo,
            token,
            DEFAULT_BASE_URL.to_string(),
            clock,
        )
    }

    /// Build a tracker with an explicit token and base URL (tests that stand up a local mock
    /// server, or a GitHub Enterprise Server host).
    pub fn with_config(
        name: impl Into<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
        token: String,
        base_url: String,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("ticketmaster-mirror")
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| TmError::Provider(format!("failed to build HTTP client: {e}")))?;
        Ok(GitHubTracker {
            name: name.into(),
            owner: owner.into(),
            repo: repo.into(),
            token,
            base_url,
            http,
            clock,
            max_retries: DEFAULT_MAX_RETRIES,
            milestone_numbers: Mutex::new(BTreeMap::new()),
        })
    }

    /// Maximum retry attempts on rate limit/5xx before giving up. Default is set in the
    /// constructors; exposed for tests that want to force exhaustion quickly.
    pub fn set_max_retries(&mut self, max_retries: u32) {
        self.max_retries = max_retries;
    }

    fn issues_url(&self) -> String {
        format!(
            "{}/repos/{}/{}/issues",
            self.base_url, self.owner, self.repo
        )
    }

    fn issue_url(&self, number: u64) -> String {
        format!("{}/{number}", self.issues_url())
    }

    fn comments_url(&self, number: u64) -> String {
        format!("{}/comments", self.issue_url(number))
    }

    fn milestones_url(&self) -> String {
        format!(
            "{}/repos/{}/{}/milestones",
            self.base_url, self.owner, self.repo
        )
    }

    fn issues_by_label_url(&self, label: &str) -> String {
        format!("{}?labels={label}&state=all", self.issues_url())
    }

    /// Search GitHub for an issue already tagged with `ticket`'s `tm-id:<ticket>` label — the
    /// durable substitute for a local "have I created this before" cache (see this module's own
    /// doc comment). Used by both [`GitHubTracker::push`] (create-vs-update) and
    /// [`GitHubTracker::confirm`] (crash recovery).
    async fn find_issue_by_tm_id(&self, ticket: &TicketId) -> Result<Option<Issue>> {
        let label = tm_id_label(ticket);
        let body = self
            .send_with_retry(reqwest::Method::GET, self.issues_by_label_url(&label), None)
            .await?;
        let issues = parse_issue_list(&body)?;
        Ok(issues.into_iter().find(|i| !i.is_pull_request()))
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token))
                .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers
    }

    /// Resolve a milestone title to GitHub's numeric id, creating it if it doesn't exist yet.
    async fn resolve_milestone(&self, title: &str) -> Result<i64> {
        if let Some(number) = self
            .milestone_numbers
            .lock()
            .map_err(|_| TmError::invariant("github: milestone cache mutex poisoned"))?
            .get(title)
        {
            return Ok(*number);
        }

        let body = self
            .send_with_retry(reqwest::Method::GET, self.milestones_url(), None)
            .await?;
        let milestones = parse_milestone_list(&body)?;
        if let Some(number) = find_milestone_number(&milestones, title) {
            self.milestone_numbers
                .lock()
                .map_err(|_| TmError::invariant("github: milestone cache mutex poisoned"))?
                .insert(title.to_string(), number);
            return Ok(number);
        }

        let created_body = self
            .send_with_retry(
                reqwest::Method::POST,
                self.milestones_url(),
                Some(serde_json::json!({ "title": title })),
            )
            .await?;
        let created = parse_milestone(&created_body)?;
        self.milestone_numbers
            .lock()
            .map_err(|_| TmError::invariant("github: milestone cache mutex poisoned"))?
            .insert(title.to_string(), created.number);
        Ok(created.number)
    }

    /// Send one HTTP request, retrying on rate limit / 5xx up to `max_retries` times.
    async fn send_with_retry(
        &self,
        method: reqwest::Method,
        url: String,
        json_body: Option<serde_json::Value>,
    ) -> Result<Vec<u8>> {
        let mut attempt: u32 = 0;
        loop {
            let mut builder = self
                .http
                .request(method.clone(), &url)
                .headers(self.headers());
            if let Some(body) = &json_body {
                builder = builder.json(body);
            }
            let response = builder
                .send()
                .await
                .map_err(|e| TmError::Provider(format!("github: request failed: {e}")))?;

            let status = response.status();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let rate_remaining = response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u32>().ok());
            let rate_reset = response
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<i64>().ok());

            let body = response.bytes().await.map_err(|e| {
                TmError::Provider(format!("github: failed to read response body: {e}"))
            })?;

            let now = self.clock.now();
            match classify_status(status, retry_after, rate_remaining, rate_reset, now, &body) {
                StatusOutcome::Ok => return Ok(body.to_vec()),
                StatusOutcome::Retry(wait) => {
                    if attempt >= self.max_retries {
                        return Err(TmError::Provider(format!(
                            "github: exhausted {} retries against {url}",
                            self.max_retries
                        )));
                    }
                    attempt += 1;
                    tokio::time::sleep(wait.max(BACKOFF_FLOOR)).await;
                }
                StatusOutcome::Err(err) => return Err(err),
            }
        }
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
            max_body_bytes: MAX_BODY_BYTES,
        }
    }

    async fn push(&self, projection: &Projection) -> Result<ExternalRef> {
        let milestone_number = match &projection.milestone {
            Some(title) => Some(self.resolve_milestone(title).await?),
            None => None,
        };

        // Tag every pushed issue with a `tm-id:<ticket>` marker label (mirroring `linear`'s/
        // `gitlab`'s own convention), so a later `push`/`confirm` can ask GitHub itself whether
        // this ticket was already mirrored instead of trusting process-lifetime local state.
        let marker = tm_id_label(&projection.ticket);
        let mut labeled = projection.clone();
        if !labeled.labels.iter().any(|l| l == &marker) {
            labeled.labels.push(marker);
        }
        let payload = issue_payload(&labeled, milestone_number);

        let existing = self.find_issue_by_tm_id(&projection.ticket).await?;
        let body = match &existing {
            Some(issue) => {
                self.send_with_retry(
                    reqwest::Method::PATCH,
                    self.issue_url(issue.number),
                    Some(payload),
                )
                .await?
            }
            None => {
                self.send_with_retry(reqwest::Method::POST, self.issues_url(), Some(payload))
                    .await?
            }
        };
        let issue = parse_issue(&body)?;
        Ok(issue_external_ref(&self.name, &issue))
    }

    async fn pull(&self, since: Timestamp) -> Result<Vec<ExternalChange>> {
        let url = format!(
            "{}?state=all&sort=updated&direction=asc&since={}",
            self.issues_url(),
            since.to_rfc3339()
        );
        let body = self
            .send_with_retry(reqwest::Method::GET, url, None)
            .await?;
        let issues = parse_issue_list(&body)?;

        let mut changes = Vec::new();
        for issue in issues.iter().filter(|i| !i.is_pull_request()) {
            changes.extend(changes_for_issue(&self.name, issue, since));

            if issue.comments == 0 {
                continue;
            }
            let comments_body = self
                .send_with_retry(reqwest::Method::GET, self.comments_url(issue.number), None)
                .await?;
            let comments = parse_comment_list(&comments_body)?;
            let external = issue_external_ref(&self.name, issue);
            for comment in &comments {
                if let Some(change) = comment_change(external.clone(), comment, since) {
                    changes.push(change);
                }
            }
        }
        Ok(changes)
    }

    /// `SPEC.md` §21.5 recovery probe: search for an issue already tagged with `ticket`'s
    /// `tm-id:<ticket>` label. Called by the effect-guard-wrapped call site (`tm-cli`'s
    /// `mirror_push`) only when resuming a `journaled`-but-never-`completed` push, to tell "the
    /// issue was already created, the receipt was lost" apart from "never pushed" before
    /// deciding whether to call `push` again.
    async fn confirm(&self, ticket: &TicketId) -> Result<Option<ExternalRef>> {
        let issue = self.find_issue_by_tm_id(ticket).await?;
        Ok(issue.map(|i| issue_external_ref(&self.name, &i)))
    }
}

// ---- wire shapes -----------------------------------------------------------------------------
//
// Mirror the GitHub REST v3 JSON shape exactly and exist only to (de)serialize at the HTTP
// boundary.

#[derive(Debug, Clone, Serialize)]
struct IssuePayload {
    title: String,
    body: String,
    state: &'static str,
    labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    milestone: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct User {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Milestone {
    number: i64,
    title: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Issue {
    number: u64,
    html_url: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    labels: Vec<Label>,
    assignee: Option<User>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    comments: u64,
    /// Present (any shape) only on pull requests, since GitHub's issues list endpoint returns
    /// both issues and PRs.
    pull_request: Option<serde_json::Value>,
}

impl Issue {
    fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Comment {
    body: String,
    user: User,
    #[serde(default)]
    created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ErrorEnvelope {
    message: String,
}

fn issue_payload(projection: &Projection, milestone_number: Option<i64>) -> serde_json::Value {
    let payload = IssuePayload {
        title: projection.title.clone(),
        body: projection.body.clone(),
        state: normalize_state(&projection.state_hint),
        labels: projection.labels.clone(),
        milestone: milestone_number,
    };
    // Infallible: `IssuePayload` has no non-JSON-representable field (no maps with non-string
    // keys, no floats that could be NaN).
    serde_json::to_value(payload).unwrap_or(serde_json::Value::Null)
}

/// GitHub's issue `state` field accepts exactly `"open"` or `"closed"`; anything else the
/// projection produced (its state was already coarsened by `ProjectionPolicy` against this
/// adapter's `arbitrary_states: false`, but a caller could still hand a raw ticket state
/// straight through) falls back to `"open"` rather than sending a value GitHub would reject.
fn normalize_state(state_hint: &str) -> &'static str {
    if state_hint.eq_ignore_ascii_case("closed") {
        "closed"
    } else {
        "open"
    }
}

fn parse_issue(body: &[u8]) -> Result<Issue> {
    serde_json::from_slice(body).map_err(|e| TmError::Parse(format!("github: issue: {e}")))
}

fn parse_issue_list(body: &[u8]) -> Result<Vec<Issue>> {
    serde_json::from_slice(body).map_err(|e| TmError::Parse(format!("github: issue list: {e}")))
}

fn parse_comment_list(body: &[u8]) -> Result<Vec<Comment>> {
    serde_json::from_slice(body).map_err(|e| TmError::Parse(format!("github: comment list: {e}")))
}

fn parse_milestone(body: &[u8]) -> Result<Milestone> {
    serde_json::from_slice(body).map_err(|e| TmError::Parse(format!("github: milestone: {e}")))
}

fn parse_milestone_list(body: &[u8]) -> Result<Vec<Milestone>> {
    serde_json::from_slice(body).map_err(|e| TmError::Parse(format!("github: milestone list: {e}")))
}

fn find_milestone_number(milestones: &[Milestone], title: &str) -> Option<i64> {
    milestones
        .iter()
        .find(|m| m.title == title)
        .map(|m| m.number)
}

/// The label marking every issue this adapter pushes with, mirroring `linear`/`gitlab`'s own
/// `tm-id:<ticket>` convention.
fn tm_id_label(ticket: &TicketId) -> String {
    format!("tm-id:{ticket}")
}

fn issue_external_ref(adapter: &str, issue: &Issue) -> ExternalRef {
    ExternalRef {
        adapter: adapter.to_string(),
        external_id: issue.number.to_string(),
        url: Some(issue.html_url.clone()),
    }
}

fn priority_label(labels: &[Label]) -> Option<String> {
    labels.iter().find_map(|l| {
        l.name
            .strip_prefix(PRIORITY_LABEL_PREFIX)
            .map(|p| p.to_string())
    })
}

/// Derive the allowlisted changes an issue represents relative to `since`. An issue whose
/// `created_at` is at or after `since` is treated as newly created on GitHub (`IssueCreated`);
/// otherwise it's an update, and each of state/assignee/priority is emitted only when GitHub
/// reports it (an unassigned, unlabeled issue produces no `Assigned`/`PriorityChanged` noise).
fn changes_for_issue(adapter: &str, issue: &Issue, since: Timestamp) -> Vec<ExternalChange> {
    let external = issue_external_ref(adapter, issue);
    let observed_at = Timestamp::parse_rfc3339(&issue.updated_at).unwrap_or(since);

    if Timestamp::parse_rfc3339(&issue.created_at)
        .map(|t| t >= since)
        .unwrap_or(false)
    {
        return vec![ExternalChange {
            external,
            kind: ExternalChangeKind::IssueCreated {
                title: String::new(),
                body: String::new(),
                author: String::new(),
            },
            observed_at,
        }];
    }

    let mut changes = vec![ExternalChange {
        external: external.clone(),
        kind: ExternalChangeKind::StatusHint {
            state: issue.state.clone(),
        },
        observed_at,
    }];

    changes.push(ExternalChange {
        external: external.clone(),
        kind: ExternalChangeKind::Assigned {
            assignee: issue.assignee.as_ref().map(|u| u.login.clone()),
        },
        observed_at,
    });

    if let Some(priority) = priority_label(&issue.labels) {
        changes.push(ExternalChange {
            external,
            kind: ExternalChangeKind::PriorityChanged { priority },
            observed_at,
        });
    }

    changes
}

fn comment_change(
    external: ExternalRef,
    comment: &Comment,
    since: Timestamp,
) -> Option<ExternalChange> {
    let observed_at = Timestamp::parse_rfc3339(&comment.created_at).ok()?;
    if observed_at < since {
        return None;
    }
    Some(ExternalChange {
        external,
        kind: ExternalChangeKind::CommentAdded {
            author: comment.user.login.clone(),
            body: comment.body.clone(),
        },
        observed_at,
    })
}

enum StatusOutcome {
    Ok,
    Retry(Duration),
    Err(TmError),
}

/// Classify a GitHub REST response. 2xx -> `Ok`. 403/429 with `x-ratelimit-remaining: 0` (or a
/// bare 429) -> `Retry`, waiting until `x-ratelimit-reset` (an absolute Unix timestamp, converted
/// to a duration against `now`) or `Retry-After`, whichever is present, falling back to the
/// backoff floor. 5xx -> `Retry` with the backoff floor. 404 -> terminal not-found. Any other
/// non-2xx -> terminal, message taken from the body's `message` field when present.
fn classify_status(
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    rate_remaining: Option<u32>,
    rate_reset_unix_secs: Option<i64>,
    now: Timestamp,
    body: &[u8],
) -> StatusOutcome {
    if status.is_success() {
        return StatusOutcome::Ok;
    }

    let rate_limited =
        status.as_u16() == 429 || (status.as_u16() == 403 && rate_remaining == Some(0));
    if rate_limited {
        let wait = retry_after.or_else(|| {
            rate_reset_unix_secs.map(|reset| {
                let remaining = reset - now.unix_seconds();
                Duration::from_secs(remaining.max(0) as u64)
            })
        });
        return StatusOutcome::Retry(wait.unwrap_or(BACKOFF_FLOOR));
    }

    if status.is_server_error() {
        return StatusOutcome::Retry(BACKOFF_FLOOR);
    }

    if status.as_u16() == 404 {
        return StatusOutcome::Err(TmError::not_found("github issue", error_message(body)));
    }

    StatusOutcome::Err(TmError::Provider(format!(
        "github: request failed with status {status}: {}",
        error_message(body)
    )))
}

fn error_message(body: &[u8]) -> String {
    serde_json::from_slice::<ErrorEnvelope>(body)
        .map(|e| e.message)
        .unwrap_or_else(|_| "no error message".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{ChecklistItem, Degradation};

    fn projection() -> Projection {
        Projection {
            ticket: TicketId::new("T-1").expect("valid ticket id"),
            title: "Fix the thing".to_string(),
            body: "Do the work.".to_string(),
            state_hint: "open".to_string(),
            labels: vec!["bug".to_string()],
            milestone: None,
            checklist: Vec::<ChecklistItem>::new(),
            degradations: Vec::<Degradation>::new(),
        }
    }

    #[test]
    fn capabilities_declare_no_parent_child_and_no_arbitrary_states() {
        let caps = GitHubTracker {
            name: "gh".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            token: "t".to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            http: reqwest::Client::new(),
            clock: std::sync::Arc::new(tm_types::FixedClock::epoch()),
            max_retries: DEFAULT_MAX_RETRIES,
            milestone_numbers: Mutex::new(BTreeMap::new()),
        }
        .capabilities();
        assert!(!caps.parent_child);
        assert!(!caps.arbitrary_states);
        assert!(caps.milestones);
        assert!(caps.labels);
        assert!(caps.comments);
        assert_eq!(caps.max_body_bytes, MAX_BODY_BYTES);
    }

    #[test]
    fn normalize_state_passes_closed_through() {
        assert_eq!(normalize_state("closed"), "closed");
        assert_eq!(normalize_state("Closed"), "closed");
    }

    #[test]
    fn normalize_state_defaults_unknown_values_to_open() {
        assert_eq!(normalize_state("Draft"), "open");
        assert_eq!(normalize_state("anything else"), "open");
    }

    #[test]
    fn issue_payload_carries_title_body_state_labels() {
        let value = issue_payload(&projection(), None);
        assert_eq!(value["title"], "Fix the thing");
        assert_eq!(value["body"], "Do the work.");
        assert_eq!(value["state"], "open");
        assert_eq!(value["labels"][0], "bug");
        assert!(value.get("milestone").is_none());
    }

    #[test]
    fn issue_payload_includes_resolved_milestone_number() {
        let value = issue_payload(&projection(), Some(7));
        assert_eq!(value["milestone"], 7);
    }

    #[test]
    fn parse_issue_reads_number_and_url() {
        let json =
            br#"{"number": 42, "html_url": "https://github.com/o/r/issues/42", "state": "open"}"#;
        let issue = parse_issue(json).expect("valid issue json");
        assert_eq!(issue.number, 42);
        assert_eq!(issue.html_url, "https://github.com/o/r/issues/42");
    }

    #[test]
    fn parse_issue_rejects_malformed_json() {
        let result = parse_issue(b"not json");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse:"));
    }

    #[test]
    fn issue_external_ref_uses_stringified_issue_number() {
        let issue = Issue {
            number: 99,
            html_url: "https://github.com/o/r/issues/99".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            assignee: None,
            created_at: String::new(),
            updated_at: String::new(),
            comments: 0,
            pull_request: None,
        };
        let external = issue_external_ref("gh", &issue);
        assert_eq!(external.adapter, "gh");
        assert_eq!(external.external_id, "99");
        assert_eq!(
            external.url,
            Some("https://github.com/o/r/issues/99".to_string())
        );
    }

    #[test]
    fn parse_issue_list_reads_multiple_issues_and_skips_nothing_itself() {
        let json = br#"[
            {"number": 1, "html_url": "https://x/1", "state": "open"},
            {"number": 2, "html_url": "https://x/2", "state": "closed", "pull_request": {}}
        ]"#;
        let issues = parse_issue_list(json).expect("valid list");
        assert_eq!(issues.len(), 2);
        assert!(!issues[0].is_pull_request());
        assert!(issues[1].is_pull_request());
    }

    #[test]
    fn parse_comment_list_reads_author_and_body() {
        let json = br#"[{"body": "hi", "user": {"login": "alice"}, "created_at": "2024-01-01T00:00:00Z"}]"#;
        let comments = parse_comment_list(json).expect("valid comments");
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "hi");
        assert_eq!(comments[0].user.login, "alice");
    }

    #[test]
    fn find_milestone_number_matches_by_title() {
        let milestones = vec![
            Milestone {
                number: 1,
                title: "v1".to_string(),
            },
            Milestone {
                number: 2,
                title: "v2".to_string(),
            },
        ];
        assert_eq!(find_milestone_number(&milestones, "v2"), Some(2));
        assert_eq!(find_milestone_number(&milestones, "v3"), None);
    }

    #[test]
    fn tm_id_label_uses_stable_marker_prefix() {
        let ticket = TicketId::new("T-42").expect("valid ticket id");
        assert_eq!(tm_id_label(&ticket), "tm-id:T-42");
    }

    #[test]
    fn issues_by_label_url_includes_the_label_and_state_all() {
        let tracker = GitHubTracker::with_config(
            "gh",
            "owner",
            "repo",
            "token".to_string(),
            DEFAULT_BASE_URL.to_string(),
            std::sync::Arc::new(tm_types::FixedClock::epoch()),
        )
        .expect("build tracker");
        let url = tracker.issues_by_label_url("tm-id:T-1");
        assert_eq!(
            url,
            "https://api.github.com/repos/owner/repo/issues?labels=tm-id:T-1&state=all"
        );
    }

    #[test]
    fn priority_label_extracts_suffix_after_prefix() {
        let labels = vec![
            Label {
                name: "bug".to_string(),
            },
            Label {
                name: "priority:high".to_string(),
            },
        ];
        assert_eq!(priority_label(&labels), Some("high".to_string()));
    }

    #[test]
    fn priority_label_absent_when_no_matching_label() {
        let labels = vec![Label {
            name: "bug".to_string(),
        }];
        assert_eq!(priority_label(&labels), None);
    }

    fn issue_fixture(created_at: &str, updated_at: &str) -> Issue {
        Issue {
            number: 7,
            html_url: "https://github.com/o/r/issues/7".to_string(),
            state: "closed".to_string(),
            labels: vec![Label {
                name: "priority:urgent".to_string(),
            }],
            assignee: Some(User {
                login: "bob".to_string(),
            }),
            created_at: created_at.to_string(),
            updated_at: updated_at.to_string(),
            comments: 0,
            pull_request: None,
        }
    }

    #[test]
    fn changes_for_issue_created_after_since_is_issue_created_only() {
        let since = Timestamp::parse_rfc3339("2024-01-01T00:00:00Z").expect("valid timestamp");
        let issue = issue_fixture("2024-06-01T00:00:00Z", "2024-06-01T00:00:00Z");
        let changes = changes_for_issue("gh", &issue, since);
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            changes[0].kind,
            ExternalChangeKind::IssueCreated { .. }
        ));
    }

    #[test]
    fn changes_for_issue_updated_before_since_emits_status_assigned_and_priority() {
        let since = Timestamp::parse_rfc3339("2024-06-01T00:00:00Z").expect("valid timestamp");
        let issue = issue_fixture("2024-01-01T00:00:00Z", "2024-06-15T00:00:00Z");
        let changes = changes_for_issue("gh", &issue, since);
        assert_eq!(changes.len(), 3);
        assert!(
            matches!(&changes[0].kind, ExternalChangeKind::StatusHint { state } if state == "closed")
        );
        assert!(
            matches!(&changes[1].kind, ExternalChangeKind::Assigned { assignee } if assignee.as_deref() == Some("bob"))
        );
        assert!(
            matches!(&changes[2].kind, ExternalChangeKind::PriorityChanged { priority } if priority == "urgent")
        );
    }

    #[test]
    fn changes_for_issue_unassigned_and_unlabeled_omits_priority_change() {
        let since = Timestamp::parse_rfc3339("2024-06-01T00:00:00Z").expect("valid timestamp");
        let issue = Issue {
            number: 8,
            html_url: "https://github.com/o/r/issues/8".to_string(),
            state: "open".to_string(),
            labels: Vec::new(),
            assignee: None,
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-06-15T00:00:00Z".to_string(),
            comments: 0,
            pull_request: None,
        };
        let changes = changes_for_issue("gh", &issue, since);
        assert_eq!(changes.len(), 2);
        assert!(
            matches!(&changes[1].kind, ExternalChangeKind::Assigned { assignee } if assignee.is_none())
        );
    }

    #[test]
    fn comment_change_skips_comments_before_since() {
        let since = Timestamp::parse_rfc3339("2024-06-01T00:00:00Z").expect("valid timestamp");
        let comment = Comment {
            body: "old".to_string(),
            user: User {
                login: "carol".to_string(),
            },
            created_at: "2024-01-01T00:00:00Z".to_string(),
        };
        let external = ExternalRef {
            adapter: "gh".to_string(),
            external_id: "7".to_string(),
            url: None,
        };
        assert!(comment_change(external, &comment, since).is_none());
    }

    #[test]
    fn comment_change_includes_comments_at_or_after_since() {
        let since = Timestamp::parse_rfc3339("2024-06-01T00:00:00Z").expect("valid timestamp");
        let comment = Comment {
            body: "new".to_string(),
            user: User {
                login: "carol".to_string(),
            },
            created_at: "2024-06-01T00:00:00Z".to_string(),
        };
        let external = ExternalRef {
            adapter: "gh".to_string(),
            external_id: "7".to_string(),
            url: None,
        };
        let change = comment_change(external, &comment, since).expect("comment should be included");
        match change.kind {
            ExternalChangeKind::CommentAdded { author, body } => {
                assert_eq!(author, "carol");
                assert_eq!(body, "new");
            }
            _ => panic!("expected CommentAdded"),
        }
    }

    #[test]
    fn classify_status_success_is_ok() {
        assert!(matches!(
            classify_status(
                reqwest::StatusCode::OK,
                None,
                None,
                None,
                Timestamp::EPOCH,
                b"{}"
            ),
            StatusOutcome::Ok
        ));
    }

    #[test]
    fn classify_status_429_retries() {
        let outcome = classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            None,
            None,
            None,
            Timestamp::EPOCH,
            b"{}",
        );
        assert!(matches!(outcome, StatusOutcome::Retry(_)));
    }

    #[test]
    fn classify_status_403_with_zero_remaining_retries_using_reset_header() {
        let now = Timestamp::EPOCH;
        let outcome = classify_status(
            reqwest::StatusCode::FORBIDDEN,
            None,
            Some(0),
            Some(now.unix_seconds() + 60),
            now,
            b"{}",
        );
        match outcome {
            StatusOutcome::Retry(wait) => assert_eq!(wait, Duration::from_secs(60)),
            _ => panic!("expected retry"),
        }
    }

    #[test]
    fn classify_status_403_without_rate_limit_signal_is_terminal() {
        let body = br#"{"message": "not authorized"}"#;
        let outcome = classify_status(
            reqwest::StatusCode::FORBIDDEN,
            None,
            None,
            None,
            Timestamp::EPOCH,
            body,
        );
        match outcome {
            StatusOutcome::Err(err) => assert!(err.to_string().contains("not authorized")),
            _ => panic!("expected terminal error"),
        }
    }

    #[test]
    fn classify_status_5xx_retries() {
        let outcome = classify_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            None,
            None,
            None,
            Timestamp::EPOCH,
            b"{}",
        );
        assert!(matches!(outcome, StatusOutcome::Retry(_)));
    }

    #[test]
    fn classify_status_404_is_not_found() {
        let body = br#"{"message": "Not Found"}"#;
        let outcome = classify_status(
            reqwest::StatusCode::NOT_FOUND,
            None,
            None,
            None,
            Timestamp::EPOCH,
            body,
        );
        match outcome {
            StatusOutcome::Err(err) => assert!(err.to_string().contains("not found")),
            _ => panic!("expected not found"),
        }
    }

    #[test]
    fn error_message_falls_back_when_body_has_no_message_field() {
        assert_eq!(error_message(b"not json"), "no error message");
    }

    #[test]
    fn error_message_reads_message_field() {
        assert_eq!(
            error_message(br#"{"message": "bad request"}"#),
            "bad request"
        );
    }
}
