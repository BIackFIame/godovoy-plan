import Foundation

// All FFI calls, including closing the handle, execute in this actor.
actor CalendarClient {
    static let shared = CalendarClient()
    private var handle: UnsafeMutableRawPointer?
    private var root: URL?
    private let documentIO = DispatchQueue(label: "local.godovoyplan.documents", qos: .utility)
    private var transferring = false
    private func profile() throws -> URL {
        if let root { return root }
        var dir = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true).appendingPathComponent("GodovoyPlan", isDirectory: true)
        #if DEBUG
        // UI tests exercise a separate container subdirectory and never open the user's calendar.
        if let testID = ProcessInfo.processInfo.environment["GP_UI_TEST_PROFILE"], UUID(uuidString: testID) != nil { dir.appendPathComponent(testID, isDirectory: true) }
        #endif
        let attributes: [FileAttributeKey: Any] = [.posixPermissions: 0o700, .protectionKey: FileProtectionType.complete]
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true, attributes: attributes)
        try FileManager.default.setAttributes(attributes, ofItemAtPath: dir.path)
        var values = URLResourceValues(); values.isExcludedFromBackup = true
        try dir.setResourceValues(values)
        root = dir
        return dir
    }

    private func open() throws -> UnsafeMutableRawPointer {
        if handle == nil {
            let url = try profile()
            handle = url.path.withCString { gp_open($0) }
        }
        guard let handle else {
            if let diagnostic = gp_open_error() {
                defer { gp_string_free(diagnostic) }
                throw GPError(String(cString: diagnostic))
            }
            throw GPError("Не удалось открыть календарь. Разблокируйте устройство и повторите попытку.")
        }
        return handle
    }

    private func call(_ input: GPCommand) throws -> Data {
        let handle = try open()
        let json = String(decoding: try JSONEncoder().encode(input), as: UTF8.self)
        guard let out = json.withCString({ gp_request(handle, $0) }) else { throw GPError("Календарь не ответил.") }
        defer { gp_string_free(out) }
        return Data(bytes: out, count: strlen(out))
    }

    func value<T: Decodable>(_ input: GPCommand, as: T.Type) throws -> T {
        let result = try JSONDecoder().decode(GPEnvelope<T>.self, from: call(input))
        try result.requireSuccess(fallback: "Календарь вернул неполные данные.")
        guard let data = result.data else { throw GPError("Календарь вернул неполные данные.") }
        return data
    }

    func agendaCard(id: String, viewYear: Int, eventYear: Int, token: String) throws -> GPAgendaCard {
        try value(["op": .string("agendaCard"), "year": .integer(viewYear), "eventYear": .integer(eventYear),
                   "eventId": .string(id), "baseToken": .string(token)], as: GPAgendaCard.self)
    }

    func reminderPage(year: Int, token: String, offset: Int, limit: Int = 64) throws -> GPReminderPage {
        try value(["op": .string("reminderPage"), "year": .integer(year), "baseToken": .string(token),
                   "offset": .integer(offset), "limit": .integer(limit)], as: GPReminderPage.self)
    }

    // The actor serializes write, read, decoding and merge. No suspension between the two ABI calls.
    func check(_ prior: GPState, context: GPCheckContext) throws -> GPState {
        try run(["op": .string("check"), "year": .integer(context.year), "eventId": .string(context.eventID),
                 "taskId": .string(context.taskID), "done": .flag(context.done), "baseToken": .string(context.token)])
        let full: GPCommand = ["op": .string("state"), "year": .integer(context.year), "acceptChecklistTuples": .flag(true)]
        var request = full
        request["ifToken"] = .string(context.token)
        request["acceptCheckPatch"] = .flag(true)
        do {
            switch try value(request, as: GPSnapshot.self) {
            case .check(let patch):
                if let next = patch.applying(to: prior, for: context) { return next }
            case .state(let next) where next.year == context.year: return next
            default: break
            }
        } catch is DecodingError {
            // Only retry the read. Mutations have already reached durable storage.
        }
        guard case .state(let next) = try value(full, as: GPSnapshot.self), next.year == context.year
        else { throw GPError("Не удалось обновить календарь.") }
        return next
    }

    func check(_ prior: GPState, context: GPIndexedCheckContext) throws -> GPState {
        try run(["op": .string("check"), "year": .integer(context.eventYear),
                 "viewYear": .integer(context.viewYear), "eventId": .string(context.eventID),
                 "taskId": .string(context.taskID), "done": .flag(context.done),
                 "baseToken": .string(context.token)])
        let full: GPCommand = ["op": .string("state"), "year": .integer(context.viewYear),
                               "acceptChecklistTuples": .flag(true), "acceptAgendaIndex": .flag(true)]
        var request = full
        request["ifToken"] = .string(context.token)
        request["acceptIndexCheckPatch"] = .flag(true)
        do {
            switch try value(request, as: GPSnapshot.self) {
            case .check(let patch):
                if let next = patch.applying(to: prior, for: context) { return next }
            case .state(let next) where next.year == context.viewYear: return next
            default: break
            }
        } catch is DecodingError {
            // The mutation has already committed; recover with a fresh list read only.
        }
        guard case .state(let next) = try value(full, as: GPSnapshot.self), next.year == context.viewYear
        else { throw GPError("Не удалось обновить календарь.") }
        return next
    }

    func run(_ command: GPCommand) throws {
        let result = try JSONDecoder().decode(GPEnvelope<JSONValue>.self, from: call(command))
        try result.requireSuccess(fallback: "Не удалось сохранить изменения.")
    }

    func lifecycle(foreground: Bool) throws {
        guard handle != nil else { return }
        try run(["op": .string(foreground ? "foreground" : "background")])
    }

    deinit { if let handle { gp_close(handle) } }

    private func exchangeDirectory() throws -> URL {
        let dir = try profile().appendingPathComponent("exchange", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700, .protectionKey: FileProtectionType.complete])
        return dir
    }

    private func beginTransfer() throws {
        guard !transferring else { throw GPError("Дождитесь завершения обмена файлом.") }
        transferring = true
    }

    private func documentWork<T: Sendable>(_ operation: @escaping @Sendable () throws -> T) async throws -> T {
        try await withCheckedThrowingContinuation { continuation in
            documentIO.async { continuation.resume(with: Result { try operation() }) }
        }
    }

    func importFile(_ external: URL) async throws {
        try beginTransfer()
        defer { transferring = false }
        _ = try open()
        let temporary = try exchangeDirectory().appendingPathComponent(UUID().uuidString + ".yrplan")
        defer { try? FileManager.default.removeItem(at: temporary) }
        try await documentWork { try Self.copyDocument(external, to: temporary) }
        try Task.checkCancellation()
        try run(["op": .string("import"), "path": .string(temporary.path)])
    }

    private static func copyDocument(_ external: URL, to temporary: URL) throws {
        let scoped = external.startAccessingSecurityScopedResource()
        defer { if scoped { external.stopAccessingSecurityScopedResource() } }
        var coordinatorError: NSError?
        var copied: Result<Void, Error>?
        NSFileCoordinator().coordinate(readingItemAt: external, options: [], error: &coordinatorError) { readable in
            copied = Result { try Self.copyBounded(readable, to: temporary) }
        }
        if let coordinatorError { throw coordinatorError }
        guard let copied else { throw GPError("Не удалось прочитать выбранный файл.") }
        try copied.get()
    }

    func exportURL() throws -> URL {
        try beginTransfer()
        defer { transferring = false }
        return try prepareExport()
    }

    private func prepareExport() throws -> URL {
        let path = try value(["op": .string("export")], as: String.self)
        let source = URL(fileURLWithPath: path).standardizedFileURL
        let directory = try exchangeDirectory().resolvingSymlinksInPath()
        guard source.deletingLastPathComponent().resolvingSymlinksInPath().path == directory.path,
              UUID(uuidString: source.deletingPathExtension().lastPathComponent) != nil,
              source.pathExtension == "yrplan" else {
            throw GPError("Неизвестный путь файла обмена.")
        }
        do {
            let metadata = try source.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey, .isSymbolicLinkKey])
            guard metadata.isRegularFile == true, metadata.isSymbolicLink != true,
                  let size = metadata.fileSize, GPMobileLimits.withinExchangeLimit(size) else {
                throw GPError("Нужен обычный файл обмена размером до 9 МиБ.")
            }
        } catch {
            try? FileManager.default.removeItem(at: source)
            throw error
        }
        return source
    }

    func exportFile() async throws -> Data {
        try beginTransfer()
        defer { transferring = false }
        let source = try prepareExport()
        defer { try? FileManager.default.removeItem(at: source) }
        let contents = try await documentWork { try Data(contentsOf: source) }
        try Task.checkCancellation()
        return contents
    }

    private static func copyBounded(_ input: URL, to output: URL) throws {
        let source = try FileHandle(forReadingFrom: input)
        defer { try? source.close() }
        guard FileManager.default.createFile(atPath: output.path, contents: nil, attributes: [.posixPermissions: 0o600, .protectionKey: FileProtectionType.complete]) else { throw GPError("Нет доступа к папке обмена.") }
        let dest = try FileHandle(forWritingTo: output)
        defer { try? dest.close() }
        var total = 0
        while let block = try source.read(upToCount: 64 * 1024), !block.isEmpty {
            total += block.count
            guard GPMobileLimits.withinExchangeLimit(total) else { throw GPError("Файл обмена превышает 9 МиБ.") }
            try dest.write(contentsOf: block)
        }
    }
}
