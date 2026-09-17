import Foundation
import Observation
import TicketmasterKit

/// Owns the connection form (server URL, token, actor identity) and, once "Connect" is pressed,
/// the live [`TicketmasterViewModel`]. Kept separate from `RootView` so the view stays a thin
/// rendering layer.
@MainActor
@Observable
final class ConnectionController {
    var serverURLText: String = "http://127.0.0.1:4477"
    var tokenText: String = ""
    var actorText: String = "human:\(NSUserName())"
    private(set) var model: TicketmasterViewModel?
    private(set) var connectError: String?

    func connect() async {
        connectError = nil
        guard let url = URL(string: serverURLText), url.scheme != nil else {
            connectError = "Enter a valid server URL, e.g. http://127.0.0.1:4477"
            return
        }
        let token = tokenText.isEmpty ? nil : tokenText
        let api = APIClient(baseURL: url, token: token)
        let stream = EventStreamClient(baseURL: url, token: token)
        let newModel = TicketmasterViewModel(api: api, stream: stream)
        model = newModel
        await newModel.connect()
    }

    func disconnect() {
        model?.disconnect()
        model = nil
    }
}
