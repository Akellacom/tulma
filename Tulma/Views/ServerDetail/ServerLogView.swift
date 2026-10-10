import SwiftUI

struct ServerLogView: View {
    @Environment(AppStore.self) private var store
    let server: ServerView
    @State private var follow = true

    private struct Line: Identifiable {
        let id: String
        let ts: Int64
        let text: String
        let isEvent: Bool
        let level: String
    }

    private var lines: [Line] {
        let stderr = (store.stderr[server.id] ?? []).map {
            Line(id: "s\($0.id)", ts: $0.ts, text: $0.line, isEvent: false, level: "info")
        }
        let events = store.liveEvents.filter { $0.serverId == server.id }.map {
            Line(id: "e\($0.id)", ts: $0.ts, text: "[\($0.kind)] \($0.message)", isEvent: true, level: $0.level)
        }
        return (stderr + events).sorted { $0.ts < $1.ts }
    }

    var body: some View {
        let lines = self.lines
        VStack(spacing: 0) {
            HStack {
                Toggle("Follow", isOn: $follow)
                    .toggleStyle(.checkbox)
                Spacer()
                CopyButton(text: lines.map { "\(Self.time($0.ts)) \($0.text)" }.joined(separator: "\n"), title: "Copy All")
                Button("Clear") { store.clearStderr(server.id) }
            }
            .controlSize(.small)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            Divider()
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 1) {
                        ForEach(lines) { line in
                            HStack(alignment: .firstTextBaseline, spacing: 8) {
                                Text(Self.time(line.ts))
                                    .foregroundStyle(.tertiary)
                                Text(line.text)
                                    .foregroundStyle(color(line))
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .font(.caption.monospaced())
                            .id(line.id)
                        }
                    }
                    .textSelection(.enabled)
                    .padding(10)
                }
                .onChange(of: lines.last?.id) { _, last in
                    if follow, let last { proxy.scrollTo(last, anchor: .bottom) }
                }
                .onAppear {
                    if let last = lines.last?.id { proxy.scrollTo(last, anchor: .bottom) }
                }
            }
            .overlay {
                if lines.isEmpty {
                    ContentUnavailableView("No output yet", systemImage: "text.alignleft", description: Text("stderr of the server and lifecycle events appear here."))
                }
            }
        }
        .task(id: server.id) { await store.loadStderr(server.id) }
    }

    private func color(_ line: Line) -> Color {
        guard line.isEvent else { return .primary }
        switch line.level {
        case "error": return .red
        case "warn": return .orange
        default: return .accentColor
        }
    }

    static func time(_ ts: Int64) -> String {
        ts.date.formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute(.twoDigits).second(.twoDigits))
    }
}
