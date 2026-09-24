import Foundation

/// Everything that can go wrong talking to a `tm-server` instance.
public enum APIError: Error, Equatable, Sendable {
    case invalidURL
    case transport(String)
    case http(status: Int, body: String)
    case decoding(String)
}

/// The subset of `tm-server`'s HTTP API (`SPEC.md` §14, `crates/tm-server/src/routes.rs`) this
/// client's vertical slice needs. Kept as a protocol so `TicketmasterViewModel` is testable
/// against an in-memory fake instead of a real server.
public protocol TicketmasterAPI: Sendable {
    func health() async throws -> Bool
    func listTickets() async throws -> [Ticket]
    func listMilestones() async throws -> [Milestone]
    func listPresence() async throws -> [PresenceEntry]
    /// The one wired mutation for this slice: `POST /tickets/:id/transition` with an
    /// `{"activate": {"actor": ...}}` body, moving a `Draft` ticket to `Blocked`/`Ready`.
    func activateTicket(id: String, actor: String) async throws
    /// `POST /tickets/:id/transition` with `{"submit": {"summary": ..., "evidence": [...],
    /// "actor": ...}}`, matching `tm-server`'s `TransitionRequest::Submit`
    /// (`crates/tm-server/src/routes.rs:353-358`).
    func submitTicket(id: String, summary: String, evidence: [String], actor: String) async throws
    /// `POST /tickets/:id/transition` with `{"accept": {"note": ..., "actor": ...}}`, matching
    /// `tm-server`'s `TransitionRequest::Accept` (`crates/tm-server/src/routes.rs`).
    func acceptTicket(id: String, note: String?, actor: String) async throws
    /// `POST /tickets/:id/transition` with `{"reject": {"reason": ..., "actor": ...}}`, matching
    /// `tm-server`'s `TransitionRequest::Reject` (`crates/tm-server/src/routes.rs`).
    func rejectTicket(id: String, reason: String, actor: String) async throws
}

/// Default implementations for the transition methods added alongside `activateTicket`, so an
/// existing `TicketmasterAPI` conformer (an in-memory test fake, say) that predates them keeps
/// compiling without having to grow three new stub methods of its own. `APIClient` below
/// overrides all three with the real `tm-server` calls.
extension TicketmasterAPI {
    public func submitTicket(id: String, summary: String, evidence: [String], actor: String) async throws {
        throw APIError.transport("submitTicket not implemented")
    }

    public func acceptTicket(id: String, note: String?, actor: String) async throws {
        throw APIError.transport("acceptTicket not implemented")
    }

    public func rejectTicket(id: String, reason: String, actor: String) async throws {
        throw APIError.transport("rejectTicket not implemented")
    }
}

/// A real `tm-server` client over `URLSession`.
///
/// Auth follows `crates/tm-server/src/auth.rs`: a bearer token is only required (and only sent)
/// when the server is bound off loopback, so `token` is optional and, when set, is attached as
/// `Authorization: Bearer <token>` on every request.
public struct APIClient: TicketmasterAPI {
    public let baseURL: URL
    public let token: String?
    private let session: URLSession

    public init(baseURL: URL, token: String? = nil, session: URLSession = .shared) {
        self.baseURL = baseURL
        self.token = token
        self.session = session
    }

    private func request(_ path: String, method: String = "GET", body: Data? = nil) -> URLRequest {
        var req = URLRequest(url: baseURL.appendingPathComponent(path))
        req.httpMethod = method
        if let body {
            req.httpBody = body
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }
        if let token {
            req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }
        return req
    }

    private func send(_ req: URLRequest) async throws -> Data {
        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.data(for: req)
        } catch {
            throw APIError.transport(error.localizedDescription)
        }
        guard let http = response as? HTTPURLResponse else {
            throw APIError.transport("non-HTTP response")
        }
        guard (200..<300).contains(http.statusCode) else {
            throw APIError.http(status: http.statusCode, body: String(data: data, encoding: .utf8) ?? "")
        }
        return data
    }

    private func decode<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        do {
            return try JSONDecoder().decode(type, from: data)
        } catch {
            throw APIError.decoding(String(describing: error))
        }
    }

    public func health() async throws -> Bool {
        let data = try await send(request("health"))
        struct Health: Decodable { let status: String }
        return try decode(Health.self, from: data).status == "ok"
    }

    public func listTickets() async throws -> [Ticket] {
        try await decode([Ticket].self, from: send(request("tickets")))
    }

    public func listMilestones() async throws -> [Milestone] {
        try await decode([Milestone].self, from: send(request("milestones")))
    }

    public func listPresence() async throws -> [PresenceEntry] {
        try await decode([PresenceEntry].self, from: send(request("presence")))
    }

    public func activateTicket(id: String, actor: String) async throws {
        let body = try JSONEncoder().encode(["activate": ["actor": actor]])
        _ = try await send(request("tickets/\(id)/transition", method: "POST", body: body))
    }

    public func submitTicket(id: String, summary: String, evidence: [String], actor: String) async throws {
        struct SubmitBody: Encodable {
            let summary: String
            let evidence: [String]
            let actor: String
        }
        let body = try JSONEncoder().encode(["submit": SubmitBody(summary: summary, evidence: evidence, actor: actor)])
        _ = try await send(request("tickets/\(id)/transition", method: "POST", body: body))
    }

    public func acceptTicket(id: String, note: String?, actor: String) async throws {
        struct AcceptBody: Encodable {
            let note: String?
            let actor: String
        }
        let body = try JSONEncoder().encode(["accept": AcceptBody(note: note, actor: actor)])
        _ = try await send(request("tickets/\(id)/transition", method: "POST", body: body))
    }

    public func rejectTicket(id: String, reason: String, actor: String) async throws {
        struct RejectBody: Encodable {
            let reason: String
            let actor: String
        }
        let body = try JSONEncoder().encode(["reject": RejectBody(reason: reason, actor: actor)])
        _ = try await send(request("tickets/\(id)/transition", method: "POST", body: body))
    }
}
