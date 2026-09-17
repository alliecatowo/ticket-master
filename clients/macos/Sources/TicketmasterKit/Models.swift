import Foundation

/// `TicketKind`, mirroring `tm_core::TicketKind` (`crates/tm-core/src/ticket.rs`), which
/// serializes with `#[serde(rename_all = "snake_case")]`.
public enum TicketKind: String, Codable, Sendable, CaseIterable {
    case work
    case verification
    case audit
    case investigation
    case recovery
    case harness
}

/// `TicketState`, mirroring `tm_core::TicketState`. Order matches the Rust enum for readability;
/// it is not a stable sort key.
public enum TicketState: String, Codable, Sendable, CaseIterable {
    case draft
    case blocked
    case ready
    case leased
    case running
    case submitted
    case verifying
    case auditing
    case rework
    case replan
    case recovery
    case escalated
    case closed
    case cancelled

    /// True for terminal states, used to dim/gray a ticket row rather than treat it as live work.
    public var isTerminal: Bool {
        self == .closed || self == .cancelled
    }
}

/// A ticket, decoded from `GET /tickets`, `GET /tickets/:id`, and the `tickets` array in
/// `GET /state`.
///
/// Only the fields this client's vertical slice actually reads or displays are strongly typed.
/// The rest (`authority`, `resources`, `executor`, `context_refs`, `success`, `verification`,
/// `budget`, `retry`, `cycle`, `failures`) round-trip as [`JSONValue`] — see that type's doc
/// comment for why.
public struct Ticket: Codable, Identifiable, Equatable, Sendable {
    public let id: String
    public let kind: TicketKind
    public let objective: String
    public let state: TicketState
    public let parent: String?
    public let children: [String]
    public let dependencies: [String]
    public let milestone: String?
    public let priority: Int
    public let attempts: UInt32
    public let created: String
    public let updated: String

    public let authority: JSONValue
    public let resources: JSONValue
    public let executor: JSONValue
    public let contextRefs: JSONValue
    public let success: JSONValue
    public let verification: JSONValue
    public let budget: JSONValue
    public let retry: JSONValue
    public let cycle: JSONValue
    public let failures: JSONValue

    enum CodingKeys: String, CodingKey {
        case id, kind, objective, state, parent, children, dependencies, milestone, priority
        case attempts, created, updated
        case authority, resources, executor
        case contextRefs = "context_refs"
        case success, verification, budget, retry, cycle, failures
    }

    public init(
        id: String, kind: TicketKind, objective: String, state: TicketState, parent: String?,
        children: [String], dependencies: [String], milestone: String?, priority: Int,
        attempts: UInt32, created: String, updated: String,
        authority: JSONValue = .null, resources: JSONValue = .null, executor: JSONValue = .null,
        contextRefs: JSONValue = .null, success: JSONValue = .null, verification: JSONValue = .null,
        budget: JSONValue = .null, retry: JSONValue = .null, cycle: JSONValue = .null,
        failures: JSONValue = .null
    ) {
        self.id = id
        self.kind = kind
        self.objective = objective
        self.state = state
        self.parent = parent
        self.children = children
        self.dependencies = dependencies
        self.milestone = milestone
        self.priority = priority
        self.attempts = attempts
        self.created = created
        self.updated = updated
        self.authority = authority
        self.resources = resources
        self.executor = executor
        self.contextRefs = contextRefs
        self.success = success
        self.verification = verification
        self.budget = budget
        self.retry = retry
        self.cycle = cycle
        self.failures = failures
    }
}

/// `MilestoneState`, mirroring `tm_core::milestone::MilestoneState`.
public enum MilestoneState: String, Codable, Sendable {
    case open
    case closed
}

/// A milestone, decoded from `GET /milestones`'s hand-rendered `milestone_json`
/// (`crates/tm-server/src/routes.rs`).
public struct Milestone: Codable, Identifiable, Equatable, Sendable {
    public let id: String
    public let title: String
    public let tickets: [String]
    public let state: MilestoneState
    public let closedBy: String?
    public let assumptions: [String]

    enum CodingKeys: String, CodingKey {
        case id, title, tickets, state
        case closedBy = "closed_by"
        case assumptions
    }

    public init(
        id: String, title: String, tickets: [String], state: MilestoneState, closedBy: String?,
        assumptions: [String]
    ) {
        self.id = id
        self.title = title
        self.tickets = tickets
        self.state = state
        self.closedBy = closedBy
        self.assumptions = assumptions
    }
}

/// A presence entry, decoded from `GET /presence`'s `presence_entry_json`. Drives the
/// sidebar's participant list.
public struct PresenceEntry: Codable, Equatable, Sendable, Identifiable {
    public let participant: String
    public let ticket: String?
    public let file: String?
    public let action: String
    public let lastSeen: String
    public let ttlSeconds: UInt32?

    public var id: String { participant }

    enum CodingKeys: String, CodingKey {
        case participant, ticket, file, action
        case lastSeen = "last_seen"
        case ttlSeconds = "ttl_seconds"
    }

    public init(
        participant: String, ticket: String?, file: String?, action: String, lastSeen: String,
        ttlSeconds: UInt32?
    ) {
        self.participant = participant
        self.ticket = ticket
        self.file = file
        self.action = action
        self.lastSeen = lastSeen
        self.ttlSeconds = ttlSeconds
    }
}

/// One event off `GET /events`, mirroring `crates/tm-server/src/sse.rs`'s `WireEvent`.
///
/// `kind` and `payload` are intentionally weakly typed (`String` / [`JSONValue`]): the payload
/// shape varies per event kind (`tm_events::Payload`), and this vertical slice reacts to *that an
/// event arrived* by refreshing ticket state rather than interpreting every payload shape.
public struct TMEvent: Codable, Equatable, Sendable {
    public let seq: UInt64
    public let ts: String
    public let kind: String
    public let subject: String
    public let actor: String
    public let session: String?
    public let causation: UInt64?
    public let correlation: String?
    public let payload: JSONValue

    public init(
        seq: UInt64, ts: String, kind: String, subject: String, actor: String, session: String?,
        causation: UInt64?, correlation: String?, payload: JSONValue
    ) {
        self.seq = seq
        self.ts = ts
        self.kind = kind
        self.subject = subject
        self.actor = actor
        self.session = session
        self.causation = causation
        self.correlation = correlation
        self.payload = payload
    }
}
