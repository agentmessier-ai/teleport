import SwiftUI

@main
struct TeleportPanelApp: App {
    var body: some Scene {
        MenuBarExtra("Teleport", systemImage: "arrow.left.arrow.right") {
            ContentView()
        }
        .menuBarExtraStyle(.window)
    }
}
