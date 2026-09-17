import Testing
@testable import TicketmasterKit

@Suite("SSE line parsing")
struct SSEParsingTests {
    @Test("parses a single complete frame in one feed")
    func parsesSingleFrame() {
        var parser = SSELineParser()
        let frames = parser.feed("event: ticket.created\nid: 1\ndata: {\"seq\":1}\n\n")
        #expect(frames.count == 1)
        #expect(frames[0].event == "ticket.created")
        #expect(frames[0].id == "1")
        #expect(frames[0].data == "{\"seq\":1}")
    }

    @Test("reassembles a frame split across multiple feeds, mid-line")
    func reassemblesSplitFrame() {
        var parser = SSELineParser()
        #expect(parser.feed("event: tick").isEmpty)
        #expect(parser.feed("et.created\nid: 1\nda").isEmpty)
        let frames = parser.feed("ta: {\"seq\":1}\n\n")
        #expect(frames.count == 1)
        #expect(frames[0].event == "ticket.created")
        #expect(frames[0].data == "{\"seq\":1}")
    }

    @Test("joins multi-line data fields with a newline, per the SSE spec")
    func joinsMultilineData() {
        var parser = SSELineParser()
        let frames = parser.feed("data: line one\ndata: line two\n\n")
        #expect(frames.count == 1)
        #expect(frames[0].data == "line one\nline two")
    }

    @Test("ignores comment lines")
    func ignoresComments() {
        var parser = SSELineParser()
        let frames = parser.feed(": keep-alive\nevent: ping\ndata: {}\n\n")
        #expect(frames.count == 1)
        #expect(frames[0].event == "ping")
    }

    @Test("parses two consecutive frames delivered in one chunk")
    func parsesConsecutiveFrames() {
        var parser = SSELineParser()
        let frames = parser.feed(
            "event: a\nid: 1\ndata: {\"seq\":1}\n\nevent: b\nid: 2\ndata: {\"seq\":2}\n\n"
        )
        #expect(frames.count == 2)
        #expect(frames[0].id == "1")
        #expect(frames[1].id == "2")
    }

    @Test("decodes a frame's data payload into a TMEvent matching sse.rs's WireEvent shape")
    func decodesEventPayload() throws {
        let frame = SSEFrame(
            event: "ticket.created",
            id: "42",
            data: """
            {"seq":42,"ts":"2026-01-01T00:00:00Z","kind":"ticket.created","subject":"T-1","actor":"agent:local/abc","session":null,"causation":null,"correlation":null,"payload":{"objective":"do it"}}
            """
        )
        let event = try decodeEvent(frame)
        #expect(event.seq == 42)
        #expect(event.kind == "ticket.created")
        #expect(event.subject == "T-1")
        if case .object(let payload) = event.payload {
            #expect(payload["objective"] == .string("do it"))
        } else {
            Issue.record("expected object payload")
        }
    }

    @Test("throws on malformed frame data")
    func throwsOnMalformedPayload() {
        let frame = SSEFrame(event: "ticket.created", id: "1", data: "not json")
        #expect(throws: EventStreamError.self) {
            try decodeEvent(frame)
        }
    }
}
