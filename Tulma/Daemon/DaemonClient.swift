import Foundation

enum DaemonError: LocalizedError, Equatable {
    case notRunning
    case terminated(Int32)
    case rpc(code: Int, message: String)
    case badResponse(String)

    var errorDescription: String? {
        switch self {
        case .notRunning: "The Tulma daemon is not running."
        case .terminated(let status): "The Tulma daemon exited (status \(status))."
        case .rpc(_, let message): message
        case .badResponse(let detail): "Unexpected daemon response: \(detail)"
        }
    }
}

/// Owns the `tulma daemon` child and speaks NDJSON JSON-RPC over its stdin/stdout.
actor DaemonClient {
    enum Event: Sendable {
        case notification(method: String, payload: Data)
        case terminated(status: Int32)
    }

    nonisolated let events: AsyncStream<Event>
    private let sink: AsyncStream<Event>.Continuation
    private var process: Process?
    private var input: FileHandle?
    private var pending: [Int: CheckedContinuation<Data, any Error>] = [:]
    private var nextID = 0

    init() {
        (events, sink) = AsyncStream.makeStream(of: Event.self, bufferingPolicy: .unbounded)
    }

    var isRunning: Bool { process?.isRunning ?? false }

    func start(executable: URL, arguments: [String] = ["daemon"], log: URL? = nil) throws {
        guard process == nil else { return }
        let proc = Process()
        proc.executableURL = executable
        proc.arguments = arguments
        let stdinPipe = Pipe()
        let stdoutPipe = Pipe()
        proc.standardInput = stdinPipe
        proc.standardOutput = stdoutPipe
        proc.standardError = log.flatMap(Self.openLog) ?? FileHandle.standardError
        let sink = self.sink
        proc.terminationHandler = { [weak self] finished in
            let status = finished.terminationStatus
            sink.yield(.terminated(status: status))
            Task { await self?.failPending(status: status) }
        }
        try proc.run()
        process = proc
        input = stdinPipe.fileHandleForWriting
        let reader = stdoutPipe.fileHandleForReading
        Task.detached { [weak self] in
            // Split on LF only: JSON strings may legally contain U+2028, which `lines` would split on.
            var line = Data()
            do {
                for try await byte in reader.bytes {
                    if byte == 0x0A {
                        if !line.isEmpty { await self?.received(line) }
                        line.removeAll(keepingCapacity: true)
                    } else {
                        line.append(byte)
                    }
                }
            } catch {}
        }
    }

    /// Closing stdin makes the daemon stop its servers and exit; force it only if that stalls.
    func stop(timeout: Duration = .seconds(6)) async {
        guard let proc = process else { return }
        try? input?.close()
        input = nil
        let deadline = ContinuousClock.now + timeout
        while proc.isRunning, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(50))
        }
        if proc.isRunning { proc.terminate() }
        process = nil
    }

    func call<R: Decodable & Sendable>(_ method: String, _ params: some Encodable & Sendable = Empty()) async throws -> R {
        let line = try await send(method, params)
        let response: Response<R>
        do {
            response = try JSONDecoder().decode(Response<R>.self, from: line)
        } catch {
            throw DaemonError.badResponse("\(method): \(error)")
        }
        if let error = response.error { throw DaemonError.rpc(code: error.code, message: error.message) }
        guard let result = response.result else { throw DaemonError.badResponse("\(method): missing result") }
        return result
    }

    func perform(_ method: String, _ params: some Encodable & Sendable = Empty()) async throws {
        let line = try await send(method, params)
        let response = try JSONDecoder().decode(Response<JSONValue>.self, from: line)
        if let error = response.error { throw DaemonError.rpc(code: error.code, message: error.message) }
    }

    private func send(_ method: String, _ params: some Encodable & Sendable) async throws -> Data {
        guard let input else { throw DaemonError.notRunning }
        nextID += 1
        let id = nextID
        var body = try JSONEncoder().encode(Request(id: id, method: method, params: params))
        body.append(0x0A)
        return try await withCheckedThrowingContinuation { continuation in
            pending[id] = continuation
            do {
                try input.write(contentsOf: body)
            } catch {
                pending.removeValue(forKey: id)?.resume(throwing: error)
            }
        }
    }

    private func received(_ line: Data) {
        guard let object = try? JSONSerialization.jsonObject(with: line) as? [String: Any] else { return }
        if let id = object["id"] as? Int {
            pending.removeValue(forKey: id)?.resume(returning: line)
        } else if let method = object["method"] as? String {
            sink.yield(.notification(method: method, payload: line))
        }
    }

    private func failPending(status: Int32) {
        let waiting = pending
        pending.removeAll()
        for continuation in waiting.values {
            continuation.resume(throwing: DaemonError.terminated(status))
        }
        process = nil
        input = nil
    }

    private static func openLog(_ url: URL) -> FileHandle? {
        let fm = FileManager.default
        try? fm.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let size = (try? fm.attributesOfItem(atPath: url.path)[.size] as? Int) ?? 0
        if size > 5_000_000 || !fm.fileExists(atPath: url.path) {
            fm.createFile(atPath: url.path, contents: nil)
        }
        guard let handle = try? FileHandle(forWritingTo: url) else { return nil }
        _ = try? handle.seekToEnd()
        return handle
    }
}

private struct Request<P: Encodable>: Encodable {
    let jsonrpc = "2.0"
    let id: Int
    let method: String
    let params: P
}

private struct RpcError: Decodable {
    let code: Int
    let message: String
}

private struct Response<R: Decodable>: Decodable {
    let result: R?
    let error: RpcError?
}

/// Payload of a daemon notification line.
struct NotificationEnvelope<P: Decodable>: Decodable {
    let params: P
}
