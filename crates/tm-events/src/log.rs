//! [`EventLog`]: the durable, hash-chained event log itself.
//!
//! Owns the one serialized write connection (guarded by a mutex so concurrent callers queue
//! rather than race) plus on-demand read connections (WAL mode lets those run concurrently with
//! the writer and with each other). This is also where the critical cross-crate contract lives:
//! [`Tx`] lets `tm-core` write its materialized state tables and append this operation's events
//! in one SQLite transaction, so a crash can never leave state and log disagreeing.
//!
//! # Why `Tx` doesn't hold a `rusqlite::Transaction`
//!
//! A handle that both holds the write mutex's guard *and* a `rusqlite::Transaction` borrowed
//! from that guard would be self-referential, which isn't expressible without `unsafe` (and
//! this crate forbids it). Instead `Tx` holds the [`parking_lot::MutexGuard`] for the duration
//! of the transaction, issues `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` itself as plain statements,
//! and exposes the guarded [`rusqlite::Connection`] directly via [`Tx::raw`]/[`Tx::raw_mut`] —
//! which, for the duration of the `Tx`'s life, *is* the raw connection mid-transaction. Callers
//! use it exactly as they would a `rusqlite::Transaction`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};
use rusqlite::{params, Connection, OptionalExtension, Row};
use tm_types::{Clock, Id, ParticipantId, SessionId, SystemClock, Timestamp, TmError};

use crate::event::{canonical_body, chain_hash, Event, EventDraft, GENESIS_PREV_HASH};
use crate::kind::EventKind;
use crate::payload::Payload;
use crate::schema;
use crate::stream::{EventHub, EventStream};

/// The outcome of [`EventLog::verify_chain`]: whether every event's `hash` correctly chains to
/// the one before it, and if not, where the chain first breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainReport {
    /// How many events were checked (the full log, from `seq = 1`).
    pub events_checked: u64,
    /// True when every checked event's hash matched its recomputed value.
    pub valid: bool,
    /// The `seq` of the first event whose hash did not match, if `valid` is false.
    pub first_broken_seq: Option<u64>,
    /// A human-readable description of the break, if `valid` is false.
    pub detail: Option<String>,
}

impl ChainReport {
    /// True when the chain is intact end to end. Equivalent to `self.valid`.
    pub fn is_valid(&self) -> bool {
        self.valid
    }
}

/// A handle onto the log's single write connection, held open across a caller-defined sequence
/// of writes to materialized state tables and event appends, all committing together.
///
/// Obtained from [`EventLog::begin`]; must end in exactly one of [`Tx::commit`] or
/// [`Tx::rollback`]. Dropping without either rolls back (best-effort; see the [`Drop`] impl).
pub struct Tx<'a> {
    guard: MutexGuard<'a, Connection>,
    done: bool,
}

impl<'a> Tx<'a> {
    /// Read-only access to the connection while this transaction is open.
    pub fn raw(&self) -> &Connection {
        &self.guard
    }

    /// Mutable access to the connection while this transaction is open, for statements that
    /// need it (e.g. `rusqlite`'s savepoint helpers).
    pub fn raw_mut(&mut self) -> &mut Connection {
        &mut self.guard
    }

    /// Commit every write made through this handle, including any events appended via
    /// [`EventLog::append_in`].
    pub fn commit(mut self) -> tm_types::Result<()> {
        self.raw()
            .execute_batch("COMMIT")
            .map_err(|e| TmError::storage(e.to_string()))?;
        self.done = true;
        Ok(())
    }

    /// Discard every write made through this handle.
    pub fn rollback(mut self) -> tm_types::Result<()> {
        self.raw()
            .execute_batch("ROLLBACK")
            .map_err(|e| TmError::storage(e.to_string()))?;
        self.done = true;
        Ok(())
    }
}

impl<'a> Drop for Tx<'a> {
    fn drop(&mut self) {
        if !self.done {
            // Best-effort safety net for a `Tx` dropped without an explicit commit/rollback
            // (e.g. an early `?` return past this point). Errors here are unrecoverable inside
            // `drop` and are deliberately swallowed rather than panicking.
            let _ = self.guard.execute_batch("ROLLBACK");
        }
    }
}

/// The append-only, hash-chained event log for one project's `project.db`, wherever its state
/// directory lives (`<root>/.tm` in repo scope, `$TM_HOME/projects/<key>/` in global scope --
/// see D-003).
///
/// Cheap to clone-by-reference (wrap in `Arc` at the call site); internally, writers serialize
/// through `write`'s mutex while reads open independent connections that run concurrently under
/// WAL.
pub struct EventLog {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    write: Mutex<Connection>,
    hub: EventHub,
}

/// The raw column values of one `events` row, before decoding `kind`/`subject`/`actor`/
/// `session`/`payload` into their typed forms. Kept separate from that decoding so a
/// `rusqlite::Row` (borrowed, `Send`-unfriendly) never needs to escape the `query_map` closure.
struct RawEventRow {
    seq: i64,
    ts: String,
    kind: String,
    subject: String,
    actor: String,
    session: Option<String>,
    causation: Option<i64>,
    correlation: Option<String>,
    payload: String,
    hash: String,
}

fn row_to_raw(row: &Row<'_>) -> rusqlite::Result<RawEventRow> {
    Ok(RawEventRow {
        seq: row.get(0)?,
        ts: row.get(1)?,
        kind: row.get(2)?,
        subject: row.get(3)?,
        actor: row.get(4)?,
        session: row.get(5)?,
        causation: row.get(6)?,
        correlation: row.get(7)?,
        payload: row.get(8)?,
        hash: row.get(9)?,
    })
}

/// Decode one raw row into a typed [`Event`], failing loudly (`TmError::storage`) on anything
/// that does not round-trip cleanly — a row that made it into `events` but cannot be decoded is
/// data corruption, not a normal not-found case.
fn decode_event(raw: RawEventRow) -> tm_types::Result<Event> {
    let kind: EventKind = raw
        .kind
        .parse()
        .map_err(|e: TmError| TmError::storage(format!("corrupt kind at seq {}: {e}", raw.seq)))?;
    let ts = Timestamp::parse_rfc3339(&raw.ts)
        .map_err(|e| TmError::storage(format!("corrupt ts at seq {}: {e}", raw.seq)))?;
    let actor: ParticipantId = raw
        .actor
        .parse()
        .map_err(|e: TmError| TmError::storage(format!("corrupt actor at seq {}: {e}", raw.seq)))?;
    let session = raw
        .session
        .map(|s| s.parse::<SessionId>())
        .transpose()
        .map_err(|e: TmError| {
            TmError::storage(format!("corrupt session at seq {}: {e}", raw.seq))
        })?;
    let payload_json: serde_json::Value = serde_json::from_str(&raw.payload)
        .map_err(|e| TmError::storage(format!("corrupt payload json at seq {}: {e}", raw.seq)))?;
    let payload = Payload::from_json(kind, payload_json)
        .map_err(|e| TmError::storage(format!("corrupt payload at seq {}: {e}", raw.seq)))?;

    Ok(Event {
        seq: raw.seq as u64,
        ts,
        kind,
        subject: Id::new(raw.subject),
        actor,
        session,
        causation: raw.causation.map(|c| c as u64),
        correlation: raw.correlation,
        payload,
        hash: raw.hash,
    })
}

impl EventLog {
    /// Open (creating if absent) the event log at `path`, migrating its schema forward if
    /// needed, using the real wall clock. Prefer [`EventLog::open_with_clock`] in tests.
    pub fn open(path: &Path) -> tm_types::Result<Self> {
        EventLog::open_with_clock(path, Arc::new(SystemClock))
    }

    /// Open the event log at `path` using an injected clock, so `ts` stamping is deterministic
    /// and replayable in tests.
    pub fn open_with_clock(path: &Path, clock: Arc<dyn Clock>) -> tm_types::Result<Self> {
        let conn = schema::open_write_connection(path, clock.as_ref())?;
        Ok(EventLog {
            path: path.to_path_buf(),
            clock,
            write: Mutex::new(conn),
            hub: EventHub::new(),
        })
    }

    /// The path this log was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Begin a transaction spanning both materialized-state writes and event appends. See the
    /// module docs for why this returns [`Tx`] rather than a `rusqlite::Transaction`.
    pub fn begin(&self) -> tm_types::Result<Tx<'_>> {
        let guard = self.write.lock();
        guard
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| TmError::storage(e.to_string()))?;
        Ok(Tx { guard, done: false })
    }

    /// Append one event within an already-open [`Tx`] (does not commit; the caller commits the
    /// whole transaction via [`Tx::commit`]). This is the primitive `EventLog::append_in` that
    /// `tm-core`'s atomic state-plus-event commits are built on. Does not publish to `self.hub`
    /// — that happens once the enclosing `Tx` commits (see `append`/`append_all`), so
    /// subscribers never see events from a transaction that later rolled back.
    pub fn append_in(&self, tx: &Tx<'_>, draft: EventDraft) -> tm_types::Result<Event> {
        let conn = tx.raw();
        let prev_hash: Option<String> = conn
            .query_row(
                "SELECT hash FROM events ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| TmError::storage(e.to_string()))?;
        let prev_hash = prev_hash.unwrap_or_else(|| GENESIS_PREV_HASH.to_string());

        let ts = self.clock.now();
        let kind = draft.kind();
        let body = canonical_body(
            ts,
            kind,
            &draft.subject,
            &draft.actor,
            draft.session.as_ref(),
            draft.causation,
            draft.correlation.as_deref(),
            &draft.payload,
        )?;
        let hash = chain_hash(&prev_hash, &body);
        let payload_json = draft.payload.to_json()?;
        let payload_text = serde_json::to_string(&payload_json)?;

        conn.execute(
            "INSERT INTO events (ts, kind, subject, actor, session, causation, correlation, payload, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                ts.to_rfc3339(),
                kind.as_str(),
                draft.subject.as_str(),
                draft.actor.as_str(),
                draft.session.as_ref().map(|s| s.as_str()),
                draft.causation.map(|c| c as i64),
                draft.correlation,
                payload_text,
                hash,
            ],
        )
        .map_err(|e| TmError::storage(e.to_string()))?;

        let seq = conn.last_insert_rowid() as u64;

        Ok(Event {
            seq,
            ts,
            kind,
            subject: draft.subject,
            actor: draft.actor,
            session: draft.session,
            causation: draft.causation,
            correlation: draft.correlation,
            payload: draft.payload,
            hash,
        })
    }

    /// Append one event in its own transaction: assigns `seq`, `ts` and `hash`, commits, then
    /// publishes to subscribers.
    pub fn append(&self, draft: EventDraft) -> tm_types::Result<Event> {
        let mut events = self.append_all(vec![draft])?;
        // Invariant: append_all returns exactly one event per input draft, in input order, so a
        // single-draft call yields exactly one event.
        Ok(events
            .pop()
            .expect("append_all returns one Event per input EventDraft"))
    }

    /// Append every draft in `drafts`, in order, within one transaction: each event chains onto
    /// the one before it (including the one before the first, from the log's existing tail).
    pub fn append_all(&self, drafts: Vec<EventDraft>) -> tm_types::Result<Vec<Event>> {
        let tx = self.begin()?;
        let mut events = Vec::with_capacity(drafts.len());
        for draft in drafts {
            events.push(self.append_in(&tx, draft)?);
        }
        tx.commit()?;
        for event in &events {
            self.hub.publish(event);
        }
        Ok(events)
    }

    /// Read up to `limit` events starting at `seq` (inclusive), in ascending `seq` order.
    pub fn read_from(&self, seq: u64, limit: usize) -> tm_types::Result<Vec<Event>> {
        let conn = schema::open_read_connection(&self.path)?;
        let mut stmt = conn
            .prepare(
                "SELECT seq, ts, kind, subject, actor, session, causation, correlation, payload, hash
                 FROM events WHERE seq >= ?1 ORDER BY seq ASC LIMIT ?2",
            )
            .map_err(|e| TmError::storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![seq as i64, limit as i64], row_to_raw)
            .map_err(|e| TmError::storage(e.to_string()))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(decode_event(
                row.map_err(|e| TmError::storage(e.to_string()))?,
            )?);
        }
        Ok(events)
    }

    /// Read every event whose `subject` equals `subject`, in ascending `seq` order.
    pub fn read_subject(&self, subject: &Id) -> tm_types::Result<Vec<Event>> {
        let conn = schema::open_read_connection(&self.path)?;
        let mut stmt = conn
            .prepare(
                "SELECT seq, ts, kind, subject, actor, session, causation, correlation, payload, hash
                 FROM events WHERE subject = ?1 ORDER BY seq ASC",
            )
            .map_err(|e| TmError::storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![subject.as_str()], row_to_raw)
            .map_err(|e| TmError::storage(e.to_string()))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(decode_event(
                row.map_err(|e| TmError::storage(e.to_string()))?,
            )?);
        }
        Ok(events)
    }

    /// Read every event with `from <= seq <= to`, in ascending `seq` order.
    pub fn read_range(&self, from: u64, to: u64) -> tm_types::Result<Vec<Event>> {
        let conn = schema::open_read_connection(&self.path)?;
        let mut stmt = conn
            .prepare(
                "SELECT seq, ts, kind, subject, actor, session, causation, correlation, payload, hash
                 FROM events WHERE seq BETWEEN ?1 AND ?2 ORDER BY seq ASC",
            )
            .map_err(|e| TmError::storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![from as i64, to as i64], row_to_raw)
            .map_err(|e| TmError::storage(e.to_string()))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(decode_event(
                row.map_err(|e| TmError::storage(e.to_string()))?,
            )?);
        }
        Ok(events)
    }

    /// The highest `seq` in the log, or `0` if it is empty.
    pub fn head(&self) -> tm_types::Result<u64> {
        let conn = schema::open_read_connection(&self.path)?;
        let head: i64 = conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
                row.get(0)
            })
            .map_err(|e| TmError::storage(e.to_string()))?;
        Ok(head as u64)
    }

    /// Walk the entire log from `seq = 1`, recomputing each event's hash from
    /// [`GENESIS_PREV_HASH`] forward and comparing it to the stored `hash`, per `SPEC.md` §3.1.
    pub fn verify_chain(&self) -> tm_types::Result<ChainReport> {
        const BATCH: usize = 1024;
        let mut prev_hash = GENESIS_PREV_HASH.to_string();
        let mut seq = 1u64;
        let mut events_checked = 0u64;

        loop {
            let batch = self.read_from(seq, BATCH)?;
            if batch.is_empty() {
                break;
            }
            for event in &batch {
                events_checked += 1;
                if !event.verifies_against(&prev_hash)? {
                    return Ok(ChainReport {
                        events_checked,
                        valid: false,
                        first_broken_seq: Some(event.seq),
                        detail: Some(format!(
                            "event at seq {} does not chain onto prev_hash {:?}",
                            event.seq, prev_hash
                        )),
                    });
                }
                prev_hash = event.hash.clone();
            }
            seq += batch.len() as u64;
        }

        Ok(ChainReport {
            events_checked,
            valid: true,
            first_broken_seq: None,
            detail: None,
        })
    }

    /// Subscribe to events appended from this point forward. See `stream.rs` for resume and
    /// backpressure semantics.
    pub fn subscribe(&self) -> EventStream {
        self.hub.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tm_types::{FixedClock, TicketId};

    use crate::payload::{ProjectCreatedPayload, TicketCreatedPayload};

    fn open_log() -> (tempfile::TempDir, EventLog) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("project.db");
        let clock = Arc::new(FixedClock::epoch());
        let log = EventLog::open_with_clock(&path, clock).expect("open log");
        (dir, log)
    }

    fn project_created_draft() -> EventDraft {
        EventDraft::new(
            ParticipantId::system(),
            Id::none(),
            Payload::from(ProjectCreatedPayload {
                name: "demo".into(),
                root: "/tmp/demo".into(),
            }),
        )
    }

    fn ticket_created_draft(ticket: &str) -> EventDraft {
        let id = TicketId::new(ticket).expect("valid ticket id");
        EventDraft::new(
            ParticipantId::system(),
            Id::from(id.clone()),
            Payload::from(TicketCreatedPayload {
                ticket: id,
                title: "do it".into(),
                parent: None,
            }),
        )
    }

    #[test]
    fn open_creates_a_usable_empty_log() {
        let (_dir, log) = open_log();
        assert_eq!(log.head().unwrap(), 0);
    }

    #[test]
    fn append_assigns_seq_starting_at_one_and_a_genesis_chained_hash() {
        let (_dir, log) = open_log();
        let event = log.append(project_created_draft()).unwrap();
        assert_eq!(event.seq, 1);
        assert!(event.verifies_against(GENESIS_PREV_HASH).unwrap());
    }

    #[test]
    fn append_publishes_to_subscribers() {
        let (_dir, log) = open_log();
        let stream = log.subscribe();
        let event = log.append(project_created_draft()).unwrap();
        let received = stream.try_recv().expect("event should be published");
        assert_eq!(received.seq, event.seq);
        assert_eq!(received.hash, event.hash);
    }

    #[test]
    fn successive_appends_chain_onto_the_previous_hash() {
        let (_dir, log) = open_log();
        let first = log.append(project_created_draft()).unwrap();
        let second = log.append(ticket_created_draft("T-1")).unwrap();
        assert_eq!(second.seq, 2);
        assert!(second.verifies_against(&first.hash).unwrap());
    }

    #[test]
    fn append_all_commits_every_draft_in_one_transaction_in_order() {
        let (_dir, log) = open_log();
        let drafts = vec![
            project_created_draft(),
            ticket_created_draft("T-1"),
            ticket_created_draft("T-2"),
        ];
        let events = log.append_all(drafts).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[1].seq, 2);
        assert_eq!(events[2].seq, 3);
        assert_eq!(log.head().unwrap(), 3);
    }

    #[test]
    fn head_reflects_the_highest_appended_seq() {
        let (_dir, log) = open_log();
        assert_eq!(log.head().unwrap(), 0);
        log.append(project_created_draft()).unwrap();
        log.append(ticket_created_draft("T-1")).unwrap();
        assert_eq!(log.head().unwrap(), 2);
    }

    #[test]
    fn read_from_returns_events_in_ascending_seq_order_within_the_limit() {
        let (_dir, log) = open_log();
        log.append(project_created_draft()).unwrap();
        log.append(ticket_created_draft("T-1")).unwrap();
        log.append(ticket_created_draft("T-2")).unwrap();

        let page = log.read_from(2, 1).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].seq, 2);
    }

    #[test]
    fn read_from_past_the_end_returns_empty() {
        let (_dir, log) = open_log();
        log.append(project_created_draft()).unwrap();
        assert!(log.read_from(100, 10).unwrap().is_empty());
    }

    #[test]
    fn read_subject_filters_to_matching_events_only() {
        let (_dir, log) = open_log();
        log.append(project_created_draft()).unwrap();
        let ticket_event = log.append(ticket_created_draft("T-1")).unwrap();
        log.append(ticket_created_draft("T-2")).unwrap();

        let subject = Id::from(TicketId::new("T-1").unwrap());
        let events = log.read_subject(&subject).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, ticket_event.seq);
    }

    #[test]
    fn read_range_is_inclusive_on_both_ends() {
        let (_dir, log) = open_log();
        log.append(project_created_draft()).unwrap();
        log.append(ticket_created_draft("T-1")).unwrap();
        log.append(ticket_created_draft("T-2")).unwrap();

        let events = log.read_range(2, 3).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 2);
        assert_eq!(events[1].seq, 3);
    }

    #[test]
    fn verify_chain_reports_valid_on_an_untampered_log() {
        let (_dir, log) = open_log();
        log.append(project_created_draft()).unwrap();
        log.append(ticket_created_draft("T-1")).unwrap();

        let report = log.verify_chain().unwrap();
        assert!(report.is_valid());
        assert_eq!(report.events_checked, 2);
        assert_eq!(report.first_broken_seq, None);
    }

    #[test]
    fn verify_chain_reports_valid_on_an_empty_log() {
        let (_dir, log) = open_log();
        let report = log.verify_chain().unwrap();
        assert!(report.is_valid());
        assert_eq!(report.events_checked, 0);
    }

    #[test]
    fn verify_chain_detects_a_tampered_hash() {
        let (_dir, log) = open_log();
        log.append(project_created_draft()).unwrap();
        log.append(ticket_created_draft("T-1")).unwrap();

        {
            // The events table forbids UPDATE via trigger, so corrupt the chain instead by
            // inserting a row with a hash that doesn't follow the real tail, bypassing
            // EventLog::append_in (and thus the log's own hash-chaining logic).
            let conn = schema::open_write_connection(log.path(), &FixedClock::epoch())
                .expect("reopen for tamper");
            conn.execute(
                "INSERT INTO events (ts, kind, subject, actor, session, causation, correlation, payload, hash)
                 VALUES ('1970-01-01T00:00:00Z', 'ticket.created', 'T-9', 'system', NULL, NULL, NULL, '{\"ticket\":\"T-9\",\"title\":\"tampered\",\"parent\":null}', 'not-a-real-hash')",
                [],
            )
            .expect("insert tampered row");
        }

        let report = log.verify_chain().unwrap();
        assert!(!report.is_valid());
        assert_eq!(report.first_broken_seq, Some(3));
    }

    #[test]
    fn tx_rollback_discards_appended_events() {
        let (_dir, log) = open_log();
        let tx = log.begin().unwrap();
        log.append_in(&tx, project_created_draft()).unwrap();
        tx.rollback().unwrap();
        assert_eq!(log.head().unwrap(), 0);
    }

    #[test]
    fn dropping_a_tx_without_commit_rolls_back() {
        let (_dir, log) = open_log();
        {
            let tx = log.begin().unwrap();
            log.append_in(&tx, project_created_draft()).unwrap();
            // tx dropped here without commit/rollback
        }
        assert_eq!(log.head().unwrap(), 0);
    }

    #[test]
    fn append_in_does_not_publish_until_the_enclosing_tx_commits() {
        let (_dir, log) = open_log();
        let stream = log.subscribe();
        let tx = log.begin().unwrap();
        log.append_in(&tx, project_created_draft()).unwrap();
        assert!(
            stream.try_recv().is_none(),
            "should not publish before commit"
        );
        tx.commit().unwrap();
    }

    #[test]
    fn path_returns_the_opened_file_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.db");
        let log = EventLog::open_with_clock(&path, Arc::new(FixedClock::epoch())).unwrap();
        assert_eq!(log.path(), path.as_path());
    }
}
