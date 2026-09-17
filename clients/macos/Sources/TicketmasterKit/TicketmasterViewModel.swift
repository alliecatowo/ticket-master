import Foundation
import Observation

/// Connection lifecycle for [`TicketmasterViewModel`], surfaced in the UI as a status pill.
public enum ConnectionState: Equatable, Sendable {
    case disconnected
    case connecting
    case connected
    case failed(String)
}

/// The app's one view model: connects to a `tm-server` instance, loads the initial ticket/
/// milestone/participant lists over REST, then tails `GET /events` and refreshes on every event.
///
/// This is the vertical slice's core: "connect, stream events, list live tickets, one mutation."
/// Reconnection is intentionally simple (no backoff/retry loop) — see the client README for what
/// is and isn't built here.
@MainActor
@Observable
public final class TicketmasterViewModel {
    public private(set) var tickets: [Ticket] = []
    public private(set) var milestones: [Milestone] = []
    public private(set) var participants: [PresenceEntry] = []
    public private(set) var connection: ConnectionState = .disconnected
    public private(set) var lastEvent: TMEvent?
    public var selectedTicketID: String?

    private let api: TicketmasterAPI
    private let stream: TicketmasterEventStream?
    private var streamTask: Task<Void, Never>?

    public init(api: TicketmasterAPI, stream: TicketmasterEventStream? = nil) {
        self.api = api
        self.stream = stream
    }

    public var selectedTicket: Ticket? {
        tickets.first { $0.id == selectedTicketID }
    }

    /// Load current state over REST, then (if a stream client was supplied) start tailing
    /// `/events`, refreshing lists on every event this session receives.
    public func connect() async {
        connection = .connecting
        do {
            try await refreshAll()
            connection = .connected
        } catch {
            connection = .failed(String(describing: error))
            return
        }
        guard let stream else { return }
        streamTask?.cancel()
        streamTask = Task { [weak self] in
            guard let self else { return }
            do {
                for try await event in stream.events(from: 0) {
                    await self.handle(event)
                }
            } catch {
                self.markFailed(error)
            }
        }
    }

    public func disconnect() {
        streamTask?.cancel()
        streamTask = nil
        connection = .disconnected
    }

    /// The slice's one mutation: activate a `Draft` ticket. Refreshes the ticket list afterward
    /// so the UI reflects the server's authoritative state rather than an optimistic guess.
    public func activate(ticketID: String, actor: String) async {
        do {
            try await api.activateTicket(id: ticketID, actor: actor)
            try await refreshAll()
        } catch {
            connection = .failed(String(describing: error))
        }
    }

    private func handle(_ event: TMEvent) async {
        lastEvent = event
        try? await refreshAll()
    }

    private func markFailed(_ error: Error) {
        connection = .failed(String(describing: error))
    }

    private func refreshAll() async throws {
        async let ticketsResult = api.listTickets()
        async let milestonesResult = api.listMilestones()
        async let presenceResult = api.listPresence()
        tickets = try await ticketsResult
        milestones = try await milestonesResult
        participants = try await presenceResult
    }
}
