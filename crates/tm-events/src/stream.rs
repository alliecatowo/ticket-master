//! In-process fan-out of appended events to live subscribers.
//!
//! [`EventHub`] is the publisher-side broadcaster [`crate::log::EventLog`] owns internally;
//! [`EventStream`] is the per-subscriber handle returned by [`crate::log::EventLog::subscribe`].
//! Built on `std::sync::mpsc` (unbounded, per subscriber) rather than a broadcast crate: sends
//! never block, and a subscriber that stops draining its channel only grows its own queue — it
//! never slows down the writer or other subscribers. A subscriber whose [`EventStream`] has been
//! dropped simply fails its next send with a disconnect error, which [`EventHub::publish`] uses
//! to prune it from the subscriber list on the next publish.
//!
//! # Resume-from-seq semantics
//!
//! [`EventStream`] carries **no history** — it only yields events appended after
//! [`EventHub::subscribe`] was called. There is deliberately no "subscribe from seq N" API,
//! because serving backlog through the same live channel would require buffering unbounded
//! history in memory or blocking the writer while a slow subscriber catches up, exactly what
//! this module exists to avoid. Callers that need to resume from a known position (e.g. a
//! materializer that persisted `last_seen_seq`) must instead:
//!
//! 1. Call `subscribe()` *first*, so no event appended from this point on is missed.
//! 2. Call `EventLog::read_from(last_seen_seq + 1, ..)` to backfill anything appended between
//!    the last read and step 1.
//! 3. Drain the backfill, then drain the live [`EventStream`], discarding any live event whose
//!    `seq` is `<= ` the last backfilled `seq` (the two windows can overlap by a few events,
//!    never gap).

use std::sync::mpsc;

use parking_lot::Mutex;

use crate::event::Event;

/// The publisher side: holds one sender per live subscriber and fans an appended event out to
/// all of them. Owned by [`crate::log::EventLog`]; not constructed by callers outside this
/// crate's `log.rs`.
pub struct EventHub {
    senders: Mutex<Vec<mpsc::Sender<Event>>>,
}

impl EventHub {
    /// A hub with no subscribers yet.
    pub fn new() -> Self {
        EventHub {
            senders: Mutex::new(Vec::new()),
        }
    }

    /// Register a new subscriber, returning the [`EventStream`] it will receive events on.
    pub fn subscribe(&self) -> EventStream {
        let (tx, rx) = mpsc::channel();
        self.senders.lock().push(tx);
        EventStream { rx }
    }

    /// Fan `event` out to every live subscriber, dropping any whose receiver has disconnected.
    pub fn publish(&self, event: &Event) {
        let mut senders = self.senders.lock();
        senders.retain(|sender| sender.send(event.clone()).is_ok());
    }

    /// How many subscribers are currently live. Exposed for diagnostics/tests, not load-bearing
    /// for correctness (a subscriber can disconnect between this call returning and the next
    /// `publish`).
    pub fn subscriber_count(&self) -> usize {
        self.senders.lock().len()
    }
}

impl Default for EventHub {
    fn default() -> Self {
        EventHub::new()
    }
}

/// A live view onto events appended to the log from the moment [`EventHub::subscribe`] was
/// called. See the module docs for resume-from-seq semantics.
pub struct EventStream {
    rx: mpsc::Receiver<Event>,
}

impl EventStream {
    /// Return the next event if one is already queued, without blocking.
    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// Block the current thread until the next event is published, or the hub is dropped.
    /// Returns `None` only once the hub (and thus every sender) is gone.
    pub fn recv(&self) -> Option<Event> {
        self.rx.recv().ok()
    }

    /// A blocking iterator over every subsequent event, ending when the hub is dropped.
    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        self.rx.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    fn make_test_event(seq: u64) -> Event {
        Event {
            seq,
            ts: tm_types::Timestamp::EPOCH,
            kind: crate::kind::EventKind::SessionStarted,
            subject: tm_types::Id::none(),
            actor: tm_types::ParticipantId::system(),
            session: None,
            causation: None,
            correlation: None,
            payload: crate::payload::Payload::SessionStarted(
                crate::payload::SessionStartedPayload {
                    // Test invariant: "S-1" is a fixed literal, always a valid SessionId.
                    session: tm_types::SessionId::new("S-1").unwrap(),
                    participant: tm_types::ParticipantId::system(),
                },
            ),
            hash: "hash".to_string(),
        }
    }

    #[test]
    fn publish_and_receive_happy_path() {
        let hub = EventHub::new();
        let stream = hub.subscribe();

        let event = make_test_event(1);
        hub.publish(&event);

        let received = stream.try_recv().expect("event should be queued");
        assert_eq!(received.seq, 1);
    }

    #[test]
    fn multiple_subscribers_all_receive() {
        let hub = EventHub::new();
        let stream1 = hub.subscribe();
        let stream2 = hub.subscribe();
        let stream3 = hub.subscribe();

        let event = make_test_event(1);
        hub.publish(&event);

        assert_eq!(stream1.try_recv().expect("stream1 should receive").seq, 1);
        assert_eq!(stream2.try_recv().expect("stream2 should receive").seq, 1);
        assert_eq!(stream3.try_recv().expect("stream3 should receive").seq, 1);
    }

    #[test]
    fn dropped_subscriber_is_pruned() {
        let hub = EventHub::new();
        let stream1 = hub.subscribe();
        let stream2 = hub.subscribe();

        assert_eq!(hub.subscriber_count(), 2);

        drop(stream1);

        let event = make_test_event(1);
        hub.publish(&event);

        assert_eq!(
            hub.subscriber_count(),
            1,
            "dropped subscriber should be pruned"
        );
        assert_eq!(stream2.try_recv().expect("stream2 should receive").seq, 1);
    }

    #[test]
    fn hub_dropped_closes_all_receivers() {
        let hub = EventHub::new();
        let stream1 = hub.subscribe();
        let stream2 = hub.subscribe();

        drop(hub);

        assert!(
            stream1.try_recv().is_none(),
            "recv should return None after hub dropped"
        );
        assert!(
            stream2.try_recv().is_none(),
            "recv should return None after hub dropped"
        );
    }

    #[test]
    fn non_blocking_publish_with_dropped_subscribers() {
        let hub = EventHub::new();
        let stream1 = hub.subscribe();
        let stream2 = hub.subscribe();
        let stream3 = hub.subscribe();

        drop(stream1);
        drop(stream2);

        let event = make_test_event(1);
        hub.publish(&event);

        assert_eq!(hub.subscriber_count(), 1);
        assert_eq!(stream3.try_recv().expect("stream3 should receive").seq, 1);
    }

    #[test]
    fn subscriber_receives_only_events_after_subscribe() {
        let hub = EventHub::new();
        let stream1 = hub.subscribe();

        let event1 = make_test_event(1);
        hub.publish(&event1);

        let stream2 = hub.subscribe();

        let event2 = make_test_event(2);
        hub.publish(&event2);

        assert_eq!(stream1.try_recv().expect("stream1 first").seq, 1);
        assert_eq!(stream1.try_recv().expect("stream1 second").seq, 2);

        assert_eq!(
            stream2
                .try_recv()
                .expect("stream2 receives only after subscribe")
                .seq,
            2,
            "stream2 should not receive event1"
        );
        assert!(stream2.try_recv().is_none());
    }

    #[test]
    fn recv_blocks_until_event_published() {
        let hub = std::sync::Arc::new(EventHub::new());
        let stream = hub.subscribe();

        let hub_clone = hub.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let event = make_test_event(1);
            hub_clone.publish(&event);
        });

        let start = std::time::Instant::now();
        let received = stream.recv().expect("event should be received");
        let elapsed = start.elapsed();

        assert_eq!(received.seq, 1);
        assert!(elapsed.as_millis() >= 40, "should have blocked for ~50ms");
        handle.join().unwrap();
    }

    #[test]
    fn recv_returns_none_when_all_senders_dropped() {
        let hub = EventHub::new();
        let stream = hub.subscribe();

        drop(hub);

        let result = stream.recv();
        assert!(
            result.is_none(),
            "recv should return None when all senders dropped"
        );
    }

    #[test]
    fn iter_yields_published_events() {
        let hub = std::sync::Arc::new(EventHub::new());
        let stream = hub.subscribe();

        let hub_clone = hub.clone();
        let handle = std::thread::spawn(move || {
            for seq in 1..=3 {
                let event = make_test_event(seq);
                hub_clone.publish(&event);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        let mut seqs = Vec::new();
        for event in stream.iter() {
            seqs.push(event.seq);
            if seqs.len() == 3 {
                break;
            }
        }

        handle.join().unwrap();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn try_recv_returns_none_when_queue_empty() {
        let hub = EventHub::new();
        let stream = hub.subscribe();

        assert!(stream.try_recv().is_none(), "queue should be empty");

        let event = make_test_event(1);
        hub.publish(&event);

        assert!(stream.try_recv().is_some(), "queue should have event");
        assert!(stream.try_recv().is_none(), "queue should be empty again");
    }

    #[test]
    fn many_subscribers_same_event() {
        let hub = EventHub::new();
        let streams: Vec<_> = (0..10).map(|_| hub.subscribe()).collect();

        let event = make_test_event(42);
        hub.publish(&event);

        for (i, stream) in streams.iter().enumerate() {
            let received = stream
                .try_recv()
                .unwrap_or_else(|| panic!("stream {} should receive", i));
            assert_eq!(received.seq, 42);
        }
    }

    #[test]
    fn subscriber_count_reflects_active_subscriptions() {
        let hub = EventHub::new();
        assert_eq!(hub.subscriber_count(), 0);

        let _stream1 = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 1);

        let _stream2 = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 2);

        drop(_stream1);
        let event = make_test_event(1);
        hub.publish(&event);

        assert_eq!(hub.subscriber_count(), 1);
    }
}
