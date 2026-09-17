import Testing
@testable import TicketmasterKit

/// An in-memory `TicketmasterAPI`, so [`TicketmasterViewModel`] can be tested without a real
/// `tm-server`. An actor because the view model calls it from `Task`s that aren't necessarily
/// `@MainActor`.
actor MockAPI: TicketmasterAPI {
    var tickets: [Ticket]
    var milestones: [Milestone]
    var presence: [PresenceEntry]
    private(set) var activateCalls: [(id: String, actor: String)] = []
    var failNextActivate = false

    init(tickets: [Ticket] = [], milestones: [Milestone] = [], presence: [PresenceEntry] = []) {
        self.tickets = tickets
        self.milestones = milestones
        self.presence = presence
    }

    func health() async throws -> Bool { true }
    func listTickets() async throws -> [Ticket] { tickets }
    func listMilestones() async throws -> [Milestone] { milestones }
    func listPresence() async throws -> [PresenceEntry] { presence }

    func activateTicket(id: String, actor: String) async throws {
        activateCalls.append((id, actor))
        if failNextActivate {
            throw APIError.http(status: 409, body: "not draft")
        }
        if let index = tickets.firstIndex(where: { $0.id == id }) {
            tickets[index] = withState(tickets[index], .ready)
        }
    }

    func setTickets(_ newTickets: [Ticket]) { tickets = newTickets }
}

/// A scripted `TicketmasterEventStream`: yields a fixed sequence of events, then finishes.
struct MockEventStream: TicketmasterEventStream {
    let events: [TMEvent]

    func events(from: UInt64) -> AsyncThrowingStream<TMEvent, Error> {
        AsyncThrowingStream { continuation in
            for event in events {
                continuation.yield(event)
            }
            continuation.finish()
        }
    }
}

private func makeTicket(id: String, state: TicketState = .draft) -> Ticket {
    Ticket(
        id: id, kind: .work, objective: "objective", state: state, parent: nil, children: [],
        dependencies: [], milestone: nil, priority: 0, attempts: 0, created: "2026-01-01T00:00:00Z",
        updated: "2026-01-01T00:00:00Z"
    )
}

private func withState(_ ticket: Ticket, _ state: TicketState) -> Ticket {
    Ticket(
        id: ticket.id, kind: ticket.kind, objective: ticket.objective, state: state,
        parent: ticket.parent, children: ticket.children, dependencies: ticket.dependencies,
        milestone: ticket.milestone, priority: ticket.priority, attempts: ticket.attempts,
        created: ticket.created, updated: ticket.updated
    )
}

private func makeEvent(seq: UInt64) -> TMEvent {
    TMEvent(
        seq: seq, ts: "2026-01-01T00:00:00Z", kind: "ticket.created", subject: "T-1",
        actor: "agent:local/abc", session: nil, causation: nil, correlation: nil, payload: .null
    )
}

@Suite("TicketmasterViewModel")
@MainActor
struct TicketmasterViewModelTests {
    @Test("connect loads tickets, milestones, and participants, and reports connected")
    func connectLoadsState() async {
        let api = MockAPI(
            tickets: [makeTicket(id: "T-1")],
            milestones: [Milestone(id: "M-1", title: "Beta", tickets: ["T-1"], state: .open, closedBy: nil, assumptions: [])],
            presence: [PresenceEntry(participant: "agent:local/abc", ticket: "T-1", file: nil, action: "active", lastSeen: "2026-01-01T00:00:00Z", ttlSeconds: 30)]
        )
        let model = TicketmasterViewModel(api: api)
        await model.connect()

        #expect(model.connection == .connected)
        #expect(model.tickets.map(\.id) == ["T-1"])
        #expect(model.milestones.map(\.id) == ["M-1"])
        #expect(model.participants.map(\.participant) == ["agent:local/abc"])
    }

    @Test("connect surfaces a transport failure as .failed")
    func connectSurfacesFailure() async {
        struct AlwaysFailsAPI: TicketmasterAPI {
            func health() async throws -> Bool { throw APIError.transport("down") }
            func listTickets() async throws -> [Ticket] { throw APIError.transport("down") }
            func listMilestones() async throws -> [Milestone] { throw APIError.transport("down") }
            func listPresence() async throws -> [PresenceEntry] { throw APIError.transport("down") }
            func activateTicket(id: String, actor: String) async throws {}
        }
        let model = TicketmasterViewModel(api: AlwaysFailsAPI())
        await model.connect()

        guard case .failed = model.connection else {
            Issue.record("expected .failed, got \(model.connection)")
            return
        }
    }

    @Test("an incoming event triggers a ticket list refresh")
    func eventTriggersRefresh() async throws {
        let api = MockAPI(tickets: [makeTicket(id: "T-1", state: .draft)])
        let stream = MockEventStream(events: [makeEvent(seq: 1)])
        let model = TicketmasterViewModel(api: api, stream: stream)

        await model.connect()
        await api.setTickets([makeTicket(id: "T-1", state: .ready)])
        // Give the detached stream-consuming task a turn to observe the scripted event and
        // trigger its own refresh.
        try await Task.sleep(nanoseconds: 200_000_000)

        #expect(model.tickets.first?.state == .ready)
        #expect(model.lastEvent?.seq == 1)
    }

    @Test("activate calls the API with the ticket id and actor, then refreshes")
    func activateCallsAPIAndRefreshes() async {
        let api = MockAPI(tickets: [makeTicket(id: "T-1", state: .draft)])
        let model = TicketmasterViewModel(api: api)
        await model.connect()

        await model.activate(ticketID: "T-1", actor: "human:allie")

        let calls = await api.activateCalls
        #expect(calls.count == 1)
        #expect(calls[0].id == "T-1")
        #expect(calls[0].actor == "human:allie")
        #expect(model.tickets.first?.state == .ready)
    }

    @Test("a failed activation surfaces .failed without crashing")
    func activateFailureSurfaces() async {
        let api = MockAPI(tickets: [makeTicket(id: "T-1", state: .draft)])
        await api.setTickets([makeTicket(id: "T-1", state: .draft)])
        await MainActor.run {}
        await api.failNextActivateNow()
        let model = TicketmasterViewModel(api: api)
        await model.connect()

        await model.activate(ticketID: "T-1", actor: "human:allie")

        guard case .failed = model.connection else {
            Issue.record("expected .failed, got \(model.connection)")
            return
        }
    }

    @Test("selectedTicket resolves the currently selected id against the loaded list")
    func selectedTicketResolves() async {
        let api = MockAPI(tickets: [makeTicket(id: "T-1"), makeTicket(id: "T-2")])
        let model = TicketmasterViewModel(api: api)
        await model.connect()

        model.selectedTicketID = "T-2"
        #expect(model.selectedTicket?.id == "T-2")
    }
}

private extension MockAPI {
    func failNextActivateNow() async {
        failNextActivate = true
    }
}
