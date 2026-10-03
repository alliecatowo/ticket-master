//! `GET /events?from=<seq>` (or `Last-Event-ID: <seq>`): the resumable, gap-free, dupe-free event
//! stream (`SPEC.md` §14).
//!
//! The hard part, called out explicitly in the spec, is the handover between "replay from the
//! log" and "live broadcast": a client resuming from `from=N` must see every event with
//! `seq > N` exactly once, in order, whether it was already committed (backlog, read from
//! [`AppState::events`] via [`tm_events::EventLog::read_from`]) or arrives after the client
//! connected (live, via [`AppState::broadcaster`]). [`tm_events::stream`]'s own module docs
//! prescribe the protocol this module follows: subscribe to the live hub *first*, then read the
//! backlog, then drain live — so nothing appended between "read backlog" and "subscribe" is
//! lost, at the cost of a possible overlap window the two sources can double-report.
//!
//! [`ResumableStream`] is the pure de-duplication/ordering state machine that closes that
//! overlap: every candidate event from either source passes through
//! [`ResumableStream::admit`], which admits it only if its `seq` is still `>=` the next expected
//! one, and advances the expectation. This is the piece the module docs demand be tested, and it
//! is deliberately free of axum/tokio/SQLite so it can be tested as plain data in, bool out.

use std::convert::Infallible;
use std::pin::Pin;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, KeepAliveStream, Sse};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use tm_events::Event;

use crate::state::{AppState, ServerError};

/// Query parameters for `GET /events`.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct EventStreamParams {
    /// Resume after this `seq`; omitted or `0` means "from the beginning of the log".
    pub from: Option<u64>,
}

/// The pure handover state machine: tracks the next `seq` this stream expects, and admits a
/// candidate event only once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumableStream {
    next_seq: u64,
}

impl ResumableStream {
    /// Start expecting the first event after `from_seq` (i.e. `seq == from_seq + 1`).
    pub fn new(from_seq: u64) -> Self {
        ResumableStream {
            next_seq: from_seq + 1,
        }
    }

    /// The next `seq` this stream has not yet admitted.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Admit `event` if it hasn't already been admitted, advancing [`Self::next_seq`] past it.
    ///
    /// # Invariant
    /// Returns `true` at most once per distinct `seq`, and only for `seq >= next_seq()` at call
    /// time; a backlog/live overlap re-delivering the same `seq` (or an out-of-order arrival of
    /// an already-passed `seq`) is silently dropped (`false`), never re-emitted. A gap (an event
    /// with `seq` strictly greater than `next_seq()`) is still admitted — this type only guards
    /// against duplication, not against a caller skipping events outright; callers must drive it
    /// from a source (`read_from` + subscribe, per the module docs) that itself guarantees no
    /// gap.
    pub fn admit(&mut self, event: &Event) -> bool {
        if event.seq < self.next_seq {
            return false;
        }
        self.next_seq = event.seq + 1;
        true
    }
}

/// The exact field set serialized into an SSE event's `data:` payload. Field order mirrors
/// [`Event`] itself; `payload` is pre-rendered JSON via [`tm_events::Payload::to_json`].
#[derive(serde::Serialize)]
struct WireEvent<'a> {
    seq: u64,
    ts: tm_types::Timestamp,
    kind: &'a str,
    subject: &'a tm_types::Id,
    actor: &'a tm_types::ParticipantId,
    session: Option<&'a tm_types::SessionId>,
    causation: Option<u64>,
    correlation: Option<&'a str>,
    payload: serde_json::Value,
}

/// Render one [`Event`] as an SSE wire event: `event: <kind>`, `id: <seq>`, `data: <json>`.
///
/// # Errors
/// Whatever [`tm_events::payload::Payload::to_json`] returns on a payload that somehow fails to
/// serialize (should not happen for any payload actually produced by `tm-core`/`tm-events`).
pub fn to_sse_event(event: &Event) -> tm_types::Result<SseEvent> {
    let kind = event.kind.as_str();
    let wire = WireEvent {
        seq: event.seq,
        ts: event.ts,
        kind,
        subject: &event.subject,
        actor: &event.actor,
        session: event.session.as_ref(),
        causation: event.causation,
        correlation: event.correlation.as_deref(),
        payload: event.payload.to_json()?,
    };
    let body = serde_json::to_string(&wire).map_err(|e| tm_types::TmError::parse(e.to_string()))?;
    Ok(SseEvent::default()
        .id(event.seq.to_string())
        .event(kind)
        .data(body))
}

type BoxEventStream = Pin<Box<dyn Stream<Item = Result<SseEvent, Infallible>> + Send>>;

/// Where a `GET /events` stream resumes: the `Last-Event-ID` header when it carries a `seq`, else
/// `?from=`, else the beginning. The header wins because a browser's `EventSource` sends it on
/// every automatic reconnect while still requesting the original URL, whose `from` is stale by
/// then. A header that isn't a number is ignored rather than refused.
pub fn resume_point(headers: &HeaderMap, params: EventStreamParams) -> u64 {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(params.from)
        .unwrap_or(0)
}

/// `GET /events?from=<seq>` handler: replay the backlog from the [`resume_point`] up to the
/// log's head at subscribe time, then continue with live events, admitting each through a single
/// [`ResumableStream`] so the handover neither drops nor duplicates. An idle stream sends a
/// keep-alive comment every [`crate::state::ServerConfig::sse_keep_alive`].
///
/// # Errors
/// [`ServerError::Domain`] if the initial backlog read fails (e.g. storage I/O error).
pub async fn sse_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<EventStreamParams>,
) -> Result<Sse<KeepAliveStream<BoxEventStream>>, ServerError> {
    let from = resume_point(&headers, params);
    let mut resumer = ResumableStream::new(from);

    // Subscribe before reading the backlog: whatever lands between "subscribe" and "read
    // backlog to head" is caught by both sources, and `resumer` is what collapses that overlap.
    let live_rx = state.broadcaster.subscribe();

    let page_size = state.config.sse_replay_page_size.max(1);
    let mut backlog = Vec::new();
    loop {
        let page = state.events.read_from(resumer.next_seq(), page_size)?;
        let page_len = page.len();
        for event in page {
            if resumer.admit(&event) {
                backlog.push(event);
            }
        }
        if page_len < page_size {
            break;
        }
    }
    let backlog_stream = futures::stream::iter(backlog.into_iter().filter_map(|event| {
        match to_sse_event(&event) {
            Ok(sse) => Some(Ok(sse)),
            Err(err) => {
                tracing::warn!(seq = event.seq, %err, "dropping backlog event that failed to render");
                None
            }
        }
    }));

    // `EventStream` only exposes blocking `recv`; bridge it onto a tokio channel from a
    // blocking task so the async side can await it like any other stream.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    tokio::task::spawn_blocking(move || {
        while let Some(event) = live_rx.recv() {
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    let live_stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    })
    .filter_map(move |event| {
        let admitted = resumer.admit(&event);
        async move {
            if !admitted {
                return None;
            }
            match to_sse_event(&event) {
                Ok(sse) => Some(Ok(sse)),
                Err(err) => {
                    tracing::warn!(seq = event.seq, %err, "dropping live event that failed to render");
                    None
                }
            }
        }
    });

    let combined: BoxEventStream = Box::pin(backlog_stream.chain(live_stream));
    Ok(Sse::new(combined).keep_alive(KeepAlive::new().interval(state.config.sse_keep_alive)))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use tm_core::Store;
    use tm_events::payload::SessionStartedPayload;
    use tm_events::{Event, EventDraft, EventHub, EventLog, Payload};
    use tm_types::{Clock, CounterIds, FixedClock, Id, IdSource, ParticipantId, SessionId};

    use super::{sse_handler, ResumableStream};
    use crate::state::{AppState, ServerConfig};

    fn test_event(seq: u64) -> Event {
        Event {
            seq,
            ts: tm_types::Timestamp::EPOCH,
            kind: tm_events::EventKind::SessionStarted,
            subject: Id::none(),
            actor: ParticipantId::system(),
            session: None,
            causation: None,
            correlation: None,
            payload: Payload::SessionStarted(SessionStartedPayload {
                session: SessionId::new("S-1").unwrap(),
                participant: ParticipantId::system(),
            }),
            hash: "hash".to_string(),
        }
    }

    fn make_draft() -> EventDraft {
        EventDraft::new(
            ParticipantId::system(),
            Id::none(),
            Payload::SessionStarted(SessionStartedPayload {
                session: SessionId::new("S-1").unwrap(),
                participant: ParticipantId::system(),
            }),
        )
    }

    fn build_state(root: &std::path::Path, page_size: usize) -> AppState {
        let clock: Arc<dyn Clock> = Arc::new(FixedClock::epoch());
        let ids: Arc<dyn IdSource> = Arc::new(CounterIds::new());
        let store = Arc::new(Store::open_with(root, clock.clone(), ids.clone()).unwrap());
        let db_path = root.join(".tm").join("project.db");
        let events = Arc::new(EventLog::open_with_clock(&db_path, clock.clone()).unwrap());
        AppState {
            store,
            events,
            broadcaster: Arc::new(EventHub::new()),
            presence: Arc::new(crate::presence::PresenceTable::new()),
            approvals: Arc::new(crate::approvals::ApprovalRegistry::new()),
            credentials: Arc::new(crate::auth::Credentials::new(None)),
            config: ServerConfig {
                project_root: root.to_path_buf(),
                state_dir: root.join(".tm"),
                bind_addr: "127.0.0.1:0".parse().unwrap(),
                token: None,
                presence_ttl_seconds: 60,
                broadcast_poll_interval: Duration::from_millis(20),
                sse_replay_page_size: page_size,
                sse_keep_alive: Duration::from_secs(15),
                workers: false,
            },
            clock,
            ids,
            providers: None,
            harness: None,
        }
    }

    async fn spawn_server(state: AppState) -> SocketAddr {
        let app = axum::Router::new()
            .route("/events", axum::routing::get(sse_handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    /// Issues the `/events` request against the in-process loopback server spawned by
    /// `spawn_server` above; this never leaves the machine, so it carries no real network
    /// dependency.
    async fn get_events(addr: SocketAddr, from: u64) -> reqwest::Response {
        let url = format!("http://{addr}/events?from={from}");
        let client = reqwest::Client::new();
        client.get(url).send().await.unwrap()
    }

    /// Reads SSE frames off `resp` until `want` `id:` fields have been seen or `timeout` elapses,
    /// returning the admitted seqs in arrival order plus the raw text for wire-format assertions.
    async fn collect_ids(
        resp: reqwest::Response,
        want: usize,
        timeout: Duration,
    ) -> (Vec<u64>, String) {
        use futures::StreamExt;

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut raw = String::new();
        let mut ids = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        while ids.len() < want {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    let text = String::from_utf8_lossy(&chunk);
                    raw.push_str(&text);
                    buf.push_str(&text);
                    while let Some(pos) = buf.find("\n\n") {
                        let block = buf[..pos].to_string();
                        buf.drain(..=pos + 1);
                        for line in block.lines() {
                            if let Some(rest) = line.strip_prefix("id: ") {
                                if let Ok(n) = rest.trim().parse::<u64>() {
                                    ids.push(n);
                                }
                            }
                        }
                    }
                }
                _ => break,
            }
        }
        (ids, raw)
    }

    #[test]
    fn resumable_stream_starts_expecting_seq_after_from() {
        assert_eq!(ResumableStream::new(0).next_seq(), 1);
        assert_eq!(ResumableStream::new(5).next_seq(), 6);
    }

    #[test]
    fn admit_accepts_expected_seq_and_advances() {
        let mut resumer = ResumableStream::new(0);
        assert!(resumer.admit(&test_event(1)));
        assert_eq!(resumer.next_seq(), 2);
    }

    #[test]
    fn admit_rejects_a_seq_already_admitted() {
        let mut resumer = ResumableStream::new(0);
        assert!(resumer.admit(&test_event(1)));
        assert!(
            !resumer.admit(&test_event(1)),
            "the backlog/live overlap must not re-deliver the same seq"
        );
        assert_eq!(
            resumer.next_seq(),
            2,
            "a rejected duplicate must not advance next_seq"
        );
    }

    #[test]
    fn admit_rejects_a_stale_seq_below_next() {
        let mut resumer = ResumableStream::new(5);
        assert!(!resumer.admit(&test_event(3)));
        assert_eq!(
            resumer.next_seq(),
            6,
            "a stale event must not move next_seq backwards"
        );
    }

    #[test]
    fn admit_accepts_a_gap_since_it_only_guards_duplication() {
        let mut resumer = ResumableStream::new(0);
        assert!(resumer.admit(&test_event(5)));
        assert_eq!(resumer.next_seq(), 6);
    }

    #[test]
    fn to_sse_event_succeeds_for_a_real_payload() {
        assert!(to_sse_event_ok(&test_event(1)));

        fn to_sse_event_ok(event: &Event) -> bool {
            super::to_sse_event(event).is_ok()
        }
    }

    #[tokio::test]
    async fn replay_from_zero_returns_full_backlog_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state(dir.path(), 2);
        for _ in 0..3 {
            state.events.append(make_draft()).unwrap();
        }
        let addr = spawn_server(state).await;

        let resp = get_events(addr, 0).await;
        let (ids, raw) = collect_ids(resp, 3, Duration::from_secs(5)).await;

        assert_eq!(ids, vec![1, 2, 3]);
        assert!(raw.contains("event: session.started"));
        assert!(raw.contains("\"kind\":\"session.started\""));
    }

    #[tokio::test]
    async fn resuming_from_a_seq_skips_already_seen_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state(dir.path(), 2);
        for _ in 0..5 {
            state.events.append(make_draft()).unwrap();
        }
        let addr = spawn_server(state).await;

        let resp = get_events(addr, 3).await;
        let (ids, _raw) = collect_ids(resp, 2, Duration::from_secs(5)).await;

        assert_eq!(ids, vec![4, 5]);
    }

    #[tokio::test]
    async fn live_events_after_backlog_are_delivered_with_no_gap() {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state(dir.path(), 10);
        for _ in 0..2 {
            state.events.append(make_draft()).unwrap();
        }
        let addr = spawn_server(state.clone()).await;

        let resp = get_events(addr, 0).await;
        let handle = tokio::spawn(collect_ids(resp, 3, Duration::from_secs(5)));

        // Give the handler time to subscribe and drain the backlog before publishing live.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let live_event = state.events.append(make_draft()).unwrap();
        state.broadcaster.publish(&live_event);

        let (ids, _raw) = handle.await.unwrap();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn last_event_id_wins_over_from_and_a_garbled_one_is_ignored() {
        use super::{resume_point, EventStreamParams};
        use axum::http::{HeaderMap, HeaderValue};
        let from = |n| EventStreamParams { from: Some(n) };
        let with = |v: &'static str| {
            let mut h = HeaderMap::new();
            h.insert("last-event-id", HeaderValue::from_static(v));
            h
        };
        assert_eq!(
            resume_point(&HeaderMap::new(), EventStreamParams::default()),
            0
        );
        assert_eq!(resume_point(&HeaderMap::new(), from(3)), 3);
        assert_eq!(resume_point(&with("7"), from(3)), 7);
        assert_eq!(resume_point(&with(" 7 "), EventStreamParams::default()), 7);
        assert_eq!(resume_point(&with("not-a-seq"), from(3)), 3);
    }

    /// An `EventSource` reconnect: the URL still says `from=0`, the header says where it was.
    #[tokio::test]
    async fn a_reconnect_resumes_after_its_last_event_id() {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state(dir.path(), 2);
        for _ in 0..5 {
            state.events.append(make_draft()).unwrap();
        }
        let addr = spawn_server(state).await;

        let resp = reqwest::Client::new()
            .get(format!("http://{addr}/events?from=0"))
            .header("Last-Event-ID", "3")
            .send()
            .await
            .unwrap();
        let (ids, _raw) = collect_ids(resp, 2, Duration::from_secs(5)).await;
        assert_eq!(ids, vec![4, 5]);
    }

    #[tokio::test]
    async fn an_idle_stream_sends_keep_alive_comments() {
        use futures::StreamExt;

        let dir = tempfile::tempdir().unwrap();
        let mut state = build_state(dir.path(), 10);
        state.config.sse_keep_alive = Duration::from_millis(50);
        let addr = spawn_server(state).await;

        let mut stream = get_events(addr, 0).await.bytes_stream();
        let mut raw = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !raw.lines().any(|l| l.starts_with(':')) {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(chunk))) => raw.push_str(&String::from_utf8_lossy(&chunk)),
                other => panic!("no keep-alive comment before the stream ended: {other:?} {raw:?}"),
            }
        }
    }

    #[tokio::test]
    async fn backlog_and_live_overlap_is_deduplicated_across_the_handover() {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state(dir.path(), 10);
        for _ in 0..3 {
            state.events.append(make_draft()).unwrap();
        }
        let addr = spawn_server(state.clone()).await;

        let resp = get_events(addr, 0).await;
        let handle = tokio::spawn(collect_ids(resp, 4, Duration::from_secs(5)));

        // Let the backlog drain, then simulate the documented overlap window: the live channel
        // re-delivering an event the backlog already served, followed by a genuinely new one.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let already_seen = state.events.read_from(3, 1).unwrap().remove(0);
        state.broadcaster.publish(&already_seen);
        let new_event = state.events.append(make_draft()).unwrap();
        state.broadcaster.publish(&new_event);

        let (ids, _raw) = handle.await.unwrap();
        assert_eq!(
            ids,
            vec![1, 2, 3, 4],
            "the redelivered seq 3 must not appear twice"
        );
    }
}
