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
