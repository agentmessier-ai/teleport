// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "TeleportPanel",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "TeleportPanel",
            linkerSettings: [.linkedLibrary("sqlite3")]
        )
    ]
)
