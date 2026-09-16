//! `GET /events?from=<seq>`: the resumable, gap-free, dupe-free event stream (`SPEC.md` §14).
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
use axum::response::sse::{Event as SseEvent, Sse};
use futures::Stream;
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
        // IMPL: if event.seq < self.next_seq { return false }; self.next_seq = event.seq + 1;
        // true. The strict `<` (not `<=`) is what makes replaying `seq == next_seq` exactly once
        // work when backlog and live both deliver it.
        todo!("admit event.seq only if it's still >= next_seq, advancing next_seq past it")
    }
}

/// Render one [`Event`] as an SSE wire event: `event: <kind>`, `id: <seq>`, `data: <json>`.
///
/// # Errors
/// Whatever [`tm_events::payload::Payload::to_json`] returns on a payload that somehow fails to
/// serialize (should not happen for any payload actually produced by `tm-core`/`tm-events`).
pub fn to_sse_event(event: &Event) -> tm_types::Result<SseEvent> {
    // IMPL: build a serde_json object { "seq": event.seq, "ts": event.ts, "kind":
    // event.kind.as_str()-equivalent, "subject": event.subject, "actor": event.actor, "session":
    // event.session, "causation": event.causation, "correlation": event.correlation, "payload":
    // event.payload.to_json()? }, serialize to a string, and return
    // SseEvent::default().id(event.seq.to_string()).event(<kind string>).data(<json string>).
    todo!("serialize this Event to an SSE wire event with id=seq and the JSON body as data")
}

type BoxEventStream = Pin<Box<dyn Stream<Item = Result<SseEvent, Infallible>> + Send>>;

/// `GET /events?from=<seq>` handler: replay the backlog from `from` (or the beginning) up to the
/// log's head at subscribe time, then continue with live events, admitting each through a single
/// [`ResumableStream`] so the handover neither drops nor duplicates.
///
/// # Errors
/// [`ServerError::Domain`] if the initial backlog read fails (e.g. storage I/O error).
pub async fn sse_handler(
    State(state): State<AppState>,
    Query(params): Query<EventStreamParams>,
) -> Result<Sse<BoxEventStream>, ServerError> {
    // IMPL: let from = params.from.unwrap_or(0); let mut resumer = ResumableStream::new(from);
    // 1) live_rx = state.events.subscribe() (subscribe before reading backlog, per
    //    tm_events::stream's documented protocol, so nothing appended after this point is
    //    missed).
    // 2) backlog: loop state.events.read_from(resumer.next_seq(), state.config.
    //    sse_replay_page_size)? in pages until a page returns fewer than the page size (caught
    //    up to head), yielding each event that resumer.admit(&event) accepts via to_sse_event.
    // 3) live: convert live_rx (an mpsc-backed tm_events::EventStream, blocking `recv`/`iter`)
    //    into an async stream (e.g. via tokio::task::spawn_blocking driving `.iter()` into an
    //    mpsc channel bridged to a stream, or a small polling adapter using `try_recv` inside a
    //    tokio interval), still filtering every event through the same `resumer.admit`.
    // Chain backlog-then-live into one boxed Stream (futures::stream::iter for the backlog page
    // loop, chained with the live adapter), wrap in Sse::new(...).keep_alive(..) and Box::pin.
    todo!("replay backlog then switch to live broadcast through one ResumableStream, gap/dupe free")
}
