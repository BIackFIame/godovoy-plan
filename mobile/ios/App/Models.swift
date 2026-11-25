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
struct GPReminder: Decodable { let eventId, title, date: String; let year: Int }
struct GPState: Decodable {
    let deviceId: String
    let workspaceName, address, networkError, lastSync, accessManagerId, stateToken: String?
    let year: Int
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

enum GPSnapshot: Decodable {
    case unchanged
    case state(GPState)

    private enum CodingKeys: String, CodingKey { case unchanged }

    init(from decoder: Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        if try fields.decodeIfPresent(Bool.self, forKey: .unchanged) == true {
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
