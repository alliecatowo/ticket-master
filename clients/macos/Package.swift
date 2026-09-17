// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "Ticketmaster",
    platforms: [.macOS(.v26)],
    products: [
        .library(name: "TicketmasterKit", targets: ["TicketmasterKit"]),
        .executable(name: "TicketmasterApp", targets: ["TicketmasterApp"]),
    ],
    targets: [
        .target(
            name: "TicketmasterKit",
            swiftSettings: [.swiftLanguageMode(.v6)]
        ),
        .executableTarget(
            name: "TicketmasterApp",
            dependencies: ["TicketmasterKit"],
            swiftSettings: [.swiftLanguageMode(.v6)]
        ),
        .testTarget(
            name: "TicketmasterKitTests",
            dependencies: ["TicketmasterKit"],
            swiftSettings: [.swiftLanguageMode(.v6)]
        ),
    ]
)
