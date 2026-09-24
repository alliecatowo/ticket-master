import Foundation
import Testing
@testable import TicketmasterKit

@Suite("Model decoding against real tm-server wire shapes")
struct ModelDecodingTests {
    @Test("decodes a ticket as rendered by tm-core's Ticket Serialize impl")
    func decodesTicket() throws {
        let json = """
        {
            "id": "T-1",
            "kind": "work",
            "objective": "Ship the thing",
            "state": "ready",
            "parent": null,
            "children": ["T-2"],
            "dependencies": [],
            "milestone": "M-1",
            "authority": {"none": null},
            "resources": [],
            "executor": {"kind": "any"},
            "context_refs": [],
            "success": [],
            "verification": "none",
            "budget": {"none": null},
            "retry": {"max_attempts": 3, "base_delay_seconds": 10, "backoff_multiplier": 2.0, "max_delay_seconds": 120},
            "cycle": null,
            "attempts": 0,
            "failures": [],
            "priority": 5,
            "created": "2026-01-01T00:00:00Z",
            "updated": "2026-01-01T00:00:00Z"
        }
        """
        let ticket = try JSONDecoder().decode(Ticket.self, from: Data(json.utf8))
        #expect(ticket.id == "T-1")
        #expect(ticket.kind == .work)
        #expect(ticket.state == .ready)
        #expect(ticket.milestone == "M-1")
        #expect(ticket.children == ["T-2"])
        #expect(ticket.priority == 5)
        // The rich substructures round-trip opaquely rather than being fully modeled.
        if case .object(let obj) = ticket.executor {
            #expect(obj["kind"] == .string("any"))
        } else {
            Issue.record("expected executor to decode as a JSON object")
        }
    }

    @Test("decodes a milestone as rendered by routes.rs's milestone_json")
    func decodesMilestone() throws {
        let json = """
        {"id": "M-1", "title": "Beta", "tickets": ["T-1"], "state": "open", "closed_by": null, "assumptions": ["D-001"]}
        """
        let milestone = try JSONDecoder().decode(Milestone.self, from: Data(json.utf8))
        #expect(milestone.id == "M-1")
        #expect(milestone.state == .open)
        #expect(milestone.tickets == ["T-1"])
        #expect(milestone.assumptions == ["D-001"])
    }

    @Test("decodes a presence entry as rendered by routes.rs's presence_entry_json")
    func decodesPresenceEntry() throws {
        let json = """
        {"participant": "agent:local/abc123", "ticket": "T-1", "file": "src/main.rs", "action": "editing", "last_seen": "2026-01-01T00:00:00Z", "ttl_seconds": 30}
        """
        let entry = try JSONDecoder().decode(PresenceEntry.self, from: Data(json.utf8))
        #expect(entry.participant == "agent:local/abc123")
        #expect(entry.action == "editing")
        #expect(entry.id == entry.participant)
    }

    @Test("decodes a closed milestone with no assumptions")
    func decodesClosedMilestone() throws {
        let json = """
        {"id": "M-2", "title": "GA", "tickets": [], "state": "closed", "closed_by": "human:allie", "assumptions": []}
        """
        let milestone = try JSONDecoder().decode(Milestone.self, from: Data(json.utf8))
        #expect(milestone.state == .closed)
        #expect(milestone.closedBy == "human:allie")
    }
}

/// Captures the last request body `APIClient` sent, instead of hitting a real server.
///
/// Registered as a `URLProtocol` on an ephemeral `URLSessionConfiguration`, so `APIClient`'s
/// `URLSession.data(for:)` call is intercepted before it ever reaches the network; it always
/// answers with an empty `200 {}` response, which is all `submitTicket`/`acceptTicket`/
/// `rejectTicket` need since they discard the response body.
final class CapturingURLProtocol: URLProtocol, @unchecked Sendable {
    static var capturedBody: Data?

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        if let stream = request.httpBodyStream {
            stream.open()
            defer { stream.close() }
            var data = Data()
            let bufferSize = 4096
            var buffer = [UInt8](repeating: 0, count: bufferSize)
            while stream.hasBytesAvailable {
                let read = stream.read(&buffer, maxLength: bufferSize)
                guard read > 0 else { break }
                data.append(buffer, count: read)
            }
            Self.capturedBody = data
        } else {
            Self.capturedBody = request.httpBody
        }
        let response = HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: nil, headerFields: nil)!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        client?.urlProtocol(self, didLoad: Data("{}".utf8))
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}

/// `.serialized` because these tests share `CapturingURLProtocol.capturedBody` as their only way
/// to observe what `APIClient` sent; running them concurrently would let one test's request
/// clobber another's before it gets read.
@Suite("APIClient transition request bodies", .serialized)
struct TransitionRequestBodyTests {
    private func makeClient() -> APIClient {
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [CapturingURLProtocol.self]
        CapturingURLProtocol.capturedBody = nil
        return APIClient(baseURL: URL(string: "https://tm.test")!, session: URLSession(configuration: config))
    }

    @Test("submitTicket POSTs a submit-tagged body matching TransitionRequest::Submit")
    func submitBodyShape() async throws {
        let client = makeClient()
        try await client.submitTicket(id: "T-1", summary: "Shipped it", evidence: ["A-1", "A-2"], actor: "human:allie")
        let body = try #require(CapturingURLProtocol.capturedBody)
        let json = try #require(JSONSerialization.jsonObject(with: body) as? [String: Any])
        let submit = try #require(json["submit"] as? [String: Any])
        #expect(submit["summary"] as? String == "Shipped it")
        #expect(submit["evidence"] as? [String] == ["A-1", "A-2"])
        #expect(submit["actor"] as? String == "human:allie")
    }

    @Test("acceptTicket POSTs an accept-tagged body matching TransitionRequest::Accept")
    func acceptBodyShape() async throws {
        let client = makeClient()
        try await client.acceptTicket(id: "T-1", note: "Looks good", actor: "human:allie")
        let body = try #require(CapturingURLProtocol.capturedBody)
        let json = try #require(JSONSerialization.jsonObject(with: body) as? [String: Any])
        let accept = try #require(json["accept"] as? [String: Any])
        #expect(accept["note"] as? String == "Looks good")
        #expect(accept["actor"] as? String == "human:allie")
    }

    @Test("acceptTicket with no note encodes a null note, not a missing key")
    func acceptBodyShapeNoNote() async throws {
        let client = makeClient()
        try await client.acceptTicket(id: "T-1", note: nil, actor: "human:allie")
        let body = try #require(CapturingURLProtocol.capturedBody)
        let json = try #require(JSONSerialization.jsonObject(with: body) as? [String: Any])
        let accept = try #require(json["accept"] as? [String: Any])
        #expect(accept.keys.contains("note"))
        #expect(accept["note"] as? String == nil)
        #expect(accept["actor"] as? String == "human:allie")
    }

    @Test("rejectTicket POSTs a reject-tagged body matching TransitionRequest::Reject")
    func rejectBodyShape() async throws {
        let client = makeClient()
        try await client.rejectTicket(id: "T-1", reason: "no tests", actor: "human:allie")
        let body = try #require(CapturingURLProtocol.capturedBody)
        let json = try #require(JSONSerialization.jsonObject(with: body) as? [String: Any])
        let reject = try #require(json["reject"] as? [String: Any])
        #expect(reject["reason"] as? String == "no tests")
        #expect(reject["actor"] as? String == "human:allie")
    }
}
