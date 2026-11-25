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

struct GPMember: Decodable, Identifiable { let id, name, employeeId: String; let revoked: Bool }
struct GPEmployee: Decodable, Identifiable { let id, name: String }
struct GPTask: Decodable, Identifiable { let id, title, dueDate: String; let done: Bool; let daysBefore: Int }
struct GPEvent: Decodable, Identifiable {
    let id, title, date, category, preparationDate, note, description: String
    let year, preparationDays: Int
    let yearly: Bool
    let ownerId, employeeId: String?
    let checklist: [GPTask]
    let revision: String
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

struct GPReminder: Decodable { let eventId, title, date: String; let year: Int }
struct GPState: Decodable {
    let deviceId: String
    let workspaceName, address, networkError, accessManagerId: String?
    var lastSync, stateToken: String?
    let supportsSyncStamp: Bool?
    let year: Int
    let lanEnabled: Bool?
    let canManage, paused: Bool
    let employees: [GPEmployee]
    let members: [GPMember]
    let events, tasks: [GPEvent]
    let reminders: [GPReminder]
    let notificationDates: [String]
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

enum GPSnapshot: Decodable {
    case unchanged
    case stamp(GPSyncStamp)
    case state(GPState)

    private enum CodingKeys: String, CodingKey { case unchanged, syncStamp }

    init(from decoder: Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        if fields.contains(.syncStamp) {
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
