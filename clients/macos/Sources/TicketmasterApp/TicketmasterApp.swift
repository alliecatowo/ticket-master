import SwiftUI

@main
struct TicketmasterApp: App {
    var body: some Scene {
        WindowGroup {
            RootView()
                .frame(minWidth: 900, minHeight: 560)
        }
        .windowStyle(.automatic)
        .windowToolbarStyle(.unifiedCompact)
    }
}
