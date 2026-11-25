import Foundation

enum JSONValue: Codable, Equatable {
    case string(String), integer(Int), flag(Bool), list([JSONValue]), object([String: JSONValue]), null

    init(from decoder: Decoder) throws {
        let value = try decoder.singleValueContainer()
        if value.decodeNil() { self = .null }
        else if let flag = try? value.decode(Bool.self) { self = .flag(flag) }
        else if let number = try? value.decode(Int.self) { self = .integer(number) }
        else if let text = try? value.decode(String.self) { self = .string(text) }
        else if let list = try? value.decode([Self].self) { self = .list(list) }
        else { self = .object(try value.decode([String: Self].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var out = encoder.singleValueContainer()
        switch self {
        case .null: try out.encodeNil()
        case .string(let value): try out.encode(value)
        case .integer(let value): try out.encode(value)
        case .flag(let value): try out.encode(value)
        case .list(let value): try out.encode(value)
        case .object(let value): try out.encode(value)
        }
    }
}

typealias GPCommand = [String: JSONValue]
struct GPError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
    init(_ message: String) { self.message = message }
}

struct GPMember: Equatable, Decodable, Identifiable { let id, name, employeeId: String; let revoked: Bool }
struct GPEmployee: Equatable, Decodable, Identifiable { let id, name: String }
struct GPTask: Equatable, Decodable, Identifiable { let id, title, dueDate: String; let done: Bool; let daysBefore: Int }
struct GPEvent: Equatable, Decodable, Identifiable {
    let id, title, date, category, preparationDate, note, description: String
    let year, preparationDays: Int
    let yearly: Bool
    let ownerId, employeeId: String?
    let checklist: [GPTask]
    let revision: String
    var nextOpenTask: GPTask? {
        checklist.lazy.filter { !$0.done }.min { $0.dueDate < $1.dueDate }
    }
    var month: Int { Int(date.dropFirst(5).prefix(2)) ?? 0 }
}
/// The wheel counts the whole year while the list follows the month and search filters.
struct GPYearSections {
    private(set) var counts = Array(repeating: 0, count: 12)
    private(set) var rows = Array(repeating: [GPEvent](), count: 12)

    init(events: [GPEvent], selectedMonth: Int?, search: String) {
        for event in events {
            // The wheel is hidden during search. Discard misses before grouping them.
            if !search.isEmpty {
                if let selectedMonth, event.month != selectedMonth { continue }
                guard event.title.localizedCaseInsensitiveContains(search) else { continue }
            }
            let month = event.month
            guard (1...12).contains(month) else { continue }
            if search.isEmpty { counts[month - 1] += 1 }
            if selectedMonth == nil || selectedMonth == month {
                rows[month - 1].append(event)
            }
        }
        for index in rows.indices where rows[index].count > 1 {
            rows[index].sort { ($0.date, $0.title) < ($1.date, $1.title) }
        }
    }
}

struct GPReminder: Equatable, Decodable { let eventId, title, date: String; let year: Int }
struct GPState: Equatable, Decodable {
    let deviceId: String
    let workspaceName, address, networkError, accessManagerId: String?
    var lastSync, stateToken: String?
    let supportsSyncStamp, supportsCheckPatch: Bool?
    let employeeId: String?
    let year: Int
    let lanEnabled: Bool?
    let canManage, paused: Bool
    let employees: [GPEmployee]
    let members: [GPMember]
    var events, tasks: [GPEvent]
    var reminders: [GPReminder]
    let notificationDates: [String]
    var canUseCheckPatch: Bool { supportsCheckPatch == true && events.count >= 600 }
    func employeeLabel(_ person: GPEmployee) -> String {
        if employees.contains(where: { $0.name == person.name && $0.id != person.id }) {
            return "\(person.name) · \(person.id.prefix(8))"
        }
        return person.name
    }
    func ownerName(_ id: String?) -> String? { members.first { $0.id == id }?.name }
}

struct GPSyncStamp: Decodable {
    let year: Int
    let baseToken, stateToken: String
    let lastSync: String?

    private enum CodingKeys: String, CodingKey { case year, baseToken, stateToken, lastSync }
    init(from decoder: Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        year = try fields.decode(Int.self, forKey: .year)
        baseToken = try fields.decode(String.self, forKey: .baseToken)
        stateToken = try fields.decode(String.self, forKey: .stateToken)
        // An explicit null clears the displayed timestamp. A missing field is invalid.
        lastSync = try fields.decode(String?.self, forKey: .lastSync)
    }

    func applying(to prior: GPState, year requestedYear: Int, token: String) -> GPState? {
        guard prior.year == requestedYear, year == requestedYear, prior.stateToken == token,
              baseToken == token, stateToken != token,
              UUID(uuidString: stateToken)?.uuidString.lowercased() == stateToken else { return nil }
        if let lastSync, ISO8601DateFormatter().date(from: lastSync) == nil { return nil }
        var updated = prior
        updated.stateToken = stateToken
        updated.lastSync = lastSync
        return updated
    }
}

struct GPCheckContext {
    let token, eventID, taskID: String
    let year: Int
    let done: Bool

    init?(prior: GPState, displayed: GPEvent, task: GPTask) {
        guard prior.canUseCheckPatch,
              prior.year == displayed.year, prior.workspaceName != nil, prior.employeeId != nil,
              let token = prior.stateToken, UUID(uuidString: token)?.uuidString.lowercased() == token,
              let row = prior.events.first(where: { $0.id == displayed.id }),
              row == displayed, row.checklist.contains(task) else { return nil }
        self.token = token
        self.eventID = displayed.id
        self.taskID = task.id
        self.year = displayed.year
        self.done = !task.done
    }
}

struct GPCheckPatch: Decodable {
    let updated: GPState
    let baseToken: String
    let taskPositions, reminderPositions: [Int]
    let taskCount, reminderCount: Int
    private enum CodingKeys: String, CodingKey { case baseToken, taskPositions, reminderPositions, taskCount, reminderCount }

    init(from decoder: Decoder) throws {
        updated = try GPState(from: decoder)
        let c = try decoder.container(keyedBy: CodingKeys.self)
        baseToken = try c.decode(String.self, forKey: .baseToken)
        taskPositions = try c.decode([Int].self, forKey: .taskPositions)
        reminderPositions = try c.decode([Int].self, forKey: .reminderPositions)
        taskCount = try c.decode(Int.self, forKey: .taskCount)
        reminderCount = try c.decode(Int.self, forKey: .reminderCount)
    }

    func applying(to prior: GPState, for check: GPCheckContext) -> GPState? {
        guard check.token == prior.stateToken, baseToken == check.token,
              updated.year == check.year, prior.year == check.year,
              let token = updated.stateToken, token != baseToken,
              UUID(uuidString: token)?.uuidString.lowercased() == token,
              updated.supportsCheckPatch == true, prior.supportsCheckPatch == true,
              prior.deviceId == updated.deviceId, prior.employeeId == updated.employeeId,
              prior.workspaceName == updated.workspaceName, prior.members == updated.members,
              prior.employees == updated.employees, prior.accessManagerId == updated.accessManagerId,
              prior.canManage == updated.canManage, prior.lanEnabled == updated.lanEnabled,
              prior.paused == updated.paused, prior.address == updated.address,
              prior.networkError == updated.networkError, prior.lastSync == updated.lastSync,
              updated.events.count == 1, let row = updated.events.first,
              row.id == check.eventID, row.year == check.year,
              let taskIndex = row.checklist.firstIndex(where: { $0.id == check.taskID }),
              row.checklist[taskIndex].done == check.done,
              !row.checklist.dropFirst(taskIndex + 1).contains(where: { $0.id == check.taskID }),
              let index = prior.events.firstIndex(where: { $0.id == row.id }),
              prior.events[index].date == row.date, prior.events[index].year == row.year,
              !prior.events.dropFirst(index + 1).contains(where: { $0.id == row.id }),
              let tasks = merge(prior.tasks, updated.tasks, at: taskPositions, count: taskCount, matching: { $0.id == row.id }),
              let reminders = merge(prior.reminders, updated.reminders, at: reminderPositions, count: reminderCount, matching: { $0.eventId == row.id })
        else { return nil }
        var result = updated
        result.events = prior.events
        result.events[index] = row
        result.tasks = tasks
        result.reminders = reminders
        return result
    }

    private func merge<T>(_ original: [T], _ changes: [T], at positions: [Int], count: Int, matching: (T) -> Bool) -> [T]? {
        guard positions.count == changes.count, count >= 0, count - changes.count <= original.count,
              changes.allSatisfy(matching) else { return nil }
        var result: [T] = []
        result.reserveCapacity(count)
        var old = 0
        var change = 0
        for position in 0..<count {
            if change < positions.count, position == positions[change] {
                result.append(changes[change])
                change += 1
            } else {
                while old < original.count && matching(original[old]) { old += 1 }
                guard old < original.count else { return nil }
                result.append(original[old])
                old += 1
            }
        }
        while old < original.count && matching(original[old]) { old += 1 }
        return old == original.count && change == changes.count ? result : nil
    }
}

enum GPSnapshot: Decodable {
    case check(GPCheckPatch)
    case unchanged
    case stamp(GPSyncStamp)
    case state(GPState)

    private enum CodingKeys: String, CodingKey { case unchanged, syncStamp, checkPatch }

    init(from decoder: Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        if fields.contains(.checkPatch) {
            guard try fields.decode(Bool.self, forKey: .checkPatch), !fields.contains(.syncStamp), !fields.contains(.unchanged) else {
                throw DecodingError.dataCorruptedError(forKey: .checkPatch, in: fields, debugDescription: "Conflicting checklist markers")
            }
            self = .check(try GPCheckPatch(from: decoder))
        } else if fields.contains(.syncStamp) {
            guard try fields.decode(Bool.self, forKey: .syncStamp), !fields.contains(.unchanged) else {
                throw DecodingError.dataCorruptedError(forKey: .syncStamp, in: fields, debugDescription: "Conflicting snapshot markers")
            }
            self = .stamp(try GPSyncStamp(from: decoder))
        } else if fields.contains(.unchanged) {
            guard try fields.decode(Bool.self, forKey: .unchanged) else {
                throw DecodingError.dataCorruptedError(forKey: .unchanged, in: fields, debugDescription: "Invalid conditional marker")
            }
            self = .unchanged
        } else {
            self = .state(try GPState(from: decoder))
        }
    }
}

enum GPDate {
    static let gregorian = Calendar(identifier: .gregorian)
    static var year: Int { gregorian.component(.year, from: Date()) }
    static func parse(_ text: String) -> Date? {
        let parts = text.split(separator: "-").compactMap { Int($0) }
        guard parts.count == 3 else { return nil }
        return gregorian.date(from: DateComponents(year: parts[0], month: parts[1], day: parts[2], hour: 12))
    }
    static func iso(_ date: Date) -> String {
        let p = gregorian.dateComponents([.year, .month, .day], from: date)
        return String(format: "%04d-%02d-%02d", p.year ?? year, p.month ?? 1, p.day ?? 1)
    }
    static func label(_ text: String) -> String {
        guard let date = parse(text) else { return text }
        return date.formatted(Date.FormatStyle().day().month(.wide).year().locale(Locale(identifier: "ru_RU")))
    }
    static let months = ["Январь", "Февраль", "Март", "Апрель", "Май", "Июнь", "Июль", "Август", "Сентябрь", "Октябрь", "Ноябрь", "Декабрь"]
}

enum GPCategory: String, CaseIterable {
    case campaign = "Campaign", partners = "Partners", team = "Team", operations = "Operations"
    var label: String {
        switch self { case .campaign: return "Акция"; case .partners: return "Партнёры"; case .team: return "Команда"; case .operations: return "Рабочие процессы" }
    }
}
