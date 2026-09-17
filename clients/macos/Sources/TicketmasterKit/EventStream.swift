import Foundation

/// One raw Server-Sent Events frame: the `event:`/`id:`/`data:` fields the wire format defines,
/// accumulated up to the blank line that terminates it. Multi-line `data:` fields are joined with
/// `\n`, per the SSE spec.
public struct SSEFrame: Equatable, Sendable {
    public var event: String?
    public var id: String?
    public var data: String = ""

    public init(event: String? = nil, id: String? = nil, data: String = "") {
        self.event = event
        self.id = id
        self.data = data
    }
}

/// A pure, incremental parser turning raw SSE text (delivered in arbitrary chunks — a line may
/// arrive split across two chunks) into complete [`SSEFrame`]s.
///
/// Deliberately free of `URLSession`/networking so the parsing logic — the part `tm-server`'s
/// module docs for `crates/tm-server/src/sse.rs` call out as the part worth testing — can be fed
/// plain strings in tests.
public struct SSELineParser: Sendable {
    private var buffer: String = ""
    private var current = SSEFrame()
    private var sawAnyField = false

    public init() {}

    /// Feed a chunk of raw bytes (decoded as UTF-8) and return every [`SSEFrame`] completed by
    /// it. Bytes for a not-yet-terminated line are retained across calls.
    public mutating func feed(_ chunk: String) -> [SSEFrame] {
        buffer += chunk
        var frames: [SSEFrame] = []
        while let newlineIndex = buffer.firstIndex(where: { $0 == "\n" }) {
            var line = String(buffer[buffer.startIndex..<newlineIndex])
            buffer.removeSubrange(buffer.startIndex...newlineIndex)
            if line.hasSuffix("\r") { line.removeLast() }

            if line.isEmpty {
                if sawAnyField {
                    frames.append(current)
                }
                current = SSEFrame()
                sawAnyField = false
                continue
            }
            if line.hasPrefix(":") { continue } // comment/heartbeat line

            let (field, value) = splitField(line)
            sawAnyField = true
            switch field {
            case "event": current.event = value
            case "id": current.id = value
            case "data":
                current.data = current.data.isEmpty ? value : current.data + "\n" + value
            default: break // "retry" and unknown fields are ignored by this client
            }
        }
        return frames
    }

    private func splitField(_ line: String) -> (String, String) {
        guard let colonIndex = line.firstIndex(of: ":") else {
            return (line, "")
        }
        let field = String(line[line.startIndex..<colonIndex])
        var valueStart = line.index(after: colonIndex)
        if valueStart < line.endIndex, line[valueStart] == " " {
            valueStart = line.index(after: valueStart)
        }
        return (field, String(line[valueStart..<line.endIndex]))
    }
}

/// Errors surfaced while decoding a live event stream frame.
public enum EventStreamError: Error, Equatable, Sendable {
    case decoding(String)
}

/// Decode an [`SSEFrame`]'s `data:` payload as a [`TMEvent`]. `tm-server` always sends one
/// complete JSON object per frame (`crates/tm-server/src/sse.rs::to_sse_event`).
public func decodeEvent(_ frame: SSEFrame) throws -> TMEvent {
    guard let data = frame.data.data(using: .utf8) else {
        throw EventStreamError.decoding("frame data was not valid UTF-8")
    }
    do {
        return try JSONDecoder().decode(TMEvent.self, from: data)
    } catch {
        throw EventStreamError.decoding(String(describing: error))
    }
}

/// The live half of event streaming, factored out as a protocol so
/// [`TicketmasterViewModel`](x-source-tag://TicketmasterViewModel) can be tested against a fake
/// stream instead of a real network connection.
public protocol TicketmasterEventStream: Sendable {
    func events(from: UInt64) -> AsyncThrowingStream<TMEvent, Error>
}

/// Opens `GET /events?from=<seq>` and yields decoded [`TMEvent`]s as they arrive, resuming from
/// `from` per `crates/tm-server/src/sse.rs`'s documented protocol.
///
/// This is the thin, live half of event streaming; the parsing logic it depends on
/// ([`SSELineParser`], [`decodeEvent`]) is unit tested independently of any real connection.
public struct EventStreamClient: TicketmasterEventStream {
    public let baseURL: URL
    public let token: String?
    private let session: URLSession

    public init(baseURL: URL, token: String? = nil, session: URLSession = .shared) {
        self.baseURL = baseURL
        self.token = token
        self.session = session
    }

    public func events(from: UInt64 = 0) -> AsyncThrowingStream<TMEvent, Error> {
        AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    var components = URLComponents(
                        url: baseURL.appendingPathComponent("events"), resolvingAgainstBaseURL: false)!
                    components.queryItems = [URLQueryItem(name: "from", value: String(from))]
                    var request = URLRequest(url: components.url!)
                    if let token {
                        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
                    }
                    let (bytes, response) = try await session.bytes(for: request)
                    guard let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode) else {
                        continuation.finish(throwing: APIError.transport("event stream did not open"))
                        return
                    }
                    var parser = SSELineParser()
                    for try await line in bytes.lines {
                        for frame in parser.feed(line + "\n") {
                            guard frame.event != nil || !frame.data.isEmpty else { continue }
                            continuation.yield(try decodeEvent(frame))
                        }
                        try Task.checkCancellation()
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}
