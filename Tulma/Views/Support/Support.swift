import AppKit
import SwiftUI

func copyToPasteboard(_ text: String) {
    NSPasteboard.general.clearContents()
    NSPasteboard.general.setString(text, forType: .string)
}

/// Same rule the daemon uses to decide what goes into the Keychain.
func isSensitiveKey(_ key: String) -> Bool {
    key.range(of: "token|secret|key|password|auth", options: [.regularExpression, .caseInsensitive]) != nil
}

extension RunState {
    var color: Color {
        switch self {
        case .running: .green
        case .starting: .yellow
        case .error: .red
        case .stopped: .secondary
        }
    }

    var label: String {
        switch self {
        case .running: "Running"
        case .starting: "Starting"
        case .error: "Error"
        case .stopped: "Stopped"
        }
    }
}

struct StatusDot: View {
    let state: RunState
    var enabled = true

    var body: some View {
        Circle()
            .fill(enabled ? state.color : Color.secondary.opacity(0.4))
            .frame(width: 8, height: 8)
            .accessibilityLabel(state.label)
    }
}

struct Badge: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.caption2.weight(.medium))
            .padding(.horizontal, 5)
            .padding(.vertical, 1)
            .background(.quaternary, in: Capsule())
            .foregroundStyle(.secondary)
    }
}

struct CopyButton: View {
    let text: String
    var title = "Copy"
    @State private var copied = false

    var body: some View {
        Button {
            copyToPasteboard(text)
            copied = true
            Task {
                try? await Task.sleep(for: .seconds(1.2))
                copied = false
            }
        } label: {
            Label(copied ? "Copied" : title, systemImage: copied ? "checkmark" : "doc.on.doc")
        }
    }
}

func prettyJSON(_ text: String) -> String {
    guard let data = text.data(using: .utf8),
          let object = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed]),
          let pretty = try? JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]),
          let string = String(data: pretty, encoding: .utf8)
    else { return text }
    return string
}
