import Foundation

enum GPMobileLimits {
    static let maxTitleChars = 160
    static let maxDescriptionChars = 4000
    static let maxNoteChars = 4000
    static let maxPreparationDays = 366
    static let maxTasks = 100
    static let maxExchangeBytes = 9 * 1024 * 1024

    static func characterCount(_ value: String) -> Int { value.unicodeScalars.count }
    static func withinTitleLimit(_ value: String) -> Bool { characterCount(value) <= maxTitleChars }
    static func withinDescriptionLimit(_ value: String) -> Bool { characterCount(value) <= maxDescriptionChars }
    static func withinNoteLimit(_ value: String) -> Bool { characterCount(value) <= maxNoteChars }
    static func withinExchangeLimit(_ size: Int) -> Bool { size >= 0 && size <= maxExchangeBytes }
}

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

struct GPEnvelope<T: Decodable>: Decodable {
    let ok: Bool
    let data: T?
    let error: String?

    func requireSuccess(fallback: String) throws {
        guard ok else { throw GPError(error ?? fallback) }
    }
}

struct GPMember: Equatable, Decodable, Identifiable { let id, name, employeeId: String; let revoked: Bool }
struct GPEmployee: Equatable, Decodable, Identifiable { let id, name: String }
struct GPTask: Equatable, Decodable, Identifiable {
    let id, title, dueDate: String
    let done: Bool
    let daysBefore: Int

    private enum CodingKeys: String, CodingKey { case id, title, dueDate, daysBefore, done }
    init(from decoder: Decoder) throws {
        if var tuple = try? decoder.unkeyedContainer() {
            guard tuple.count == 5 else {
                throw DecodingError.dataCorruptedError(in: tuple, debugDescription: "Expected tuple-v1 checklist row")
            }
            id = try tuple.decode(String.self)
            title = try tuple.decode(String.self)
            dueDate = try tuple.decode(String.self)
            daysBefore = try tuple.decode(Int.self)
            done = try tuple.decode(Bool.self)
            guard tuple.isAtEnd else {
                throw DecodingError.dataCorruptedError(in: tuple, debugDescription: "Extra tuple-v1 checklist values")
            }
        } else {
            let fields = try decoder.container(keyedBy: CodingKeys.self)
            id = try fields.decode(String.self, forKey: .id)
            title = try fields.decode(String.self, forKey: .title)
            dueDate = try fields.decode(String.self, forKey: .dueDate)
            daysBefore = try fields.decode(Int.self, forKey: .daysBefore)
            done = try fields.decode(Bool.self, forKey: .done)
        }
    }
}
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

struct GPNextOpenTask: Equatable, Decodable, Identifiable {
    let id, title, dueDate: String
}

struct GPAgendaSummary: Equatable, Decodable, Identifiable {
    let id, title, date: String
    let year: Int
    let ownerId, employeeId: String?
    let nextOpenTask: GPNextOpenTask?

    private enum CodingKeys: String, CodingKey { case id, title, date, year, ownerId, employeeId, nextOpenTask, checklist }
    init(from decoder: Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        guard !fields.contains(.checklist) else {
            throw DecodingError.dataCorruptedError(forKey: .checklist, in: fields, debugDescription: "An agenda summary must not contain a checklist")
        }
        id = try fields.decode(String.self, forKey: .id)
        title = try fields.decode(String.self, forKey: .title)
        date = try fields.decode(String.self, forKey: .date)
        year = try fields.decode(Int.self, forKey: .year)
        ownerId = try fields.decodeIfPresent(String.self, forKey: .ownerId)
        employeeId = try fields.decodeIfPresent(String.self, forKey: .employeeId)
        nextOpenTask = try fields.decodeIfPresent(GPNextOpenTask.self, forKey: .nextOpenTask)
    }

    var month: Int { Int(date.dropFirst(5).prefix(2)) ?? 0 }
    var key: String { "\(id):\(year)" }
}

struct GPAgendaItem: Identifiable {
    let id, title, date: String
    let year: Int
    let ownerId, employeeId: String?
    let nextOpenTask: GPNextOpenTask?

    init(event: GPEvent, includeNextOpenTask: Bool = false) {
        id = event.id
        title = event.title
        date = event.date
        year = event.year
        ownerId = event.ownerId
        employeeId = event.employeeId
        if includeNextOpenTask, let task = event.nextOpenTask {
            nextOpenTask = GPNextOpenTask(id: task.id, title: task.title, dueDate: task.dueDate)
        } else { nextOpenTask = nil }
    }

    init(summary: GPAgendaSummary) {
        id = summary.id
        title = summary.title
        date = summary.date
        year = summary.year
        ownerId = summary.ownerId
        employeeId = summary.employeeId
        nextOpenTask = summary.nextOpenTask
    }

    var month: Int { Int(date.dropFirst(5).prefix(2)) ?? 0 }
    var key: String { "\(id):\(year)" }
}

/// The wheel counts the whole year while the list follows the month and search filters.
struct GPYearSections {
    private(set) var counts = Array(repeating: 0, count: 12)
    private(set) var rows = Array(repeating: [GPAgendaItem](), count: 12)

    init(events: [GPEvent], selectedMonth: Int?, search: String, includeNextOpenTask: Bool = false) {
        self.init(items: events.map { GPAgendaItem(event: $0, includeNextOpenTask: includeNextOpenTask) }, selectedMonth: selectedMonth, search: search)
    }

    init(summaries: [GPAgendaSummary], selectedMonth: Int?, search: String) {
        self.init(items: summaries.map(GPAgendaItem.init(summary:)), selectedMonth: selectedMonth, search: search)
    }

    private init(items: [GPAgendaItem], selectedMonth: Int?, search: String) {
        for event in items {
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

struct GPReminder: Equatable, Decodable, Identifiable {
    let eventId, title, date: String
    let year: Int
    let taskId: String?
    var id: String { "\(eventId):\(year):\(taskId ?? "event"):\(date)" }
}
struct GPState: Equatable, Decodable {
    let deviceId: String
    let workspaceName, address, networkError, accessManagerId: String?
    var lastSync, stateToken: String?
    let supportsSyncStamp, supportsCheckPatch, supportsIndexCheckPatch: Bool?
    let checklistFormat: String?
    let employeeId: String?
    let year: Int
    let agendaIndex: Bool
    var reminderCount: Int
    let lanEnabled: Bool?
    let canManage, paused: Bool
    let employees: [GPEmployee]
    let members: [GPMember]
    var events, tasks: [GPEvent]
    var reminders: [GPReminder]
    let notificationDates: [String]
    var agendaEvents: [GPAgendaSummary]
    var agendaTasks: [GPAgendaSummary]
    var checkPatchCard: GPEvent?
    var canUseCheckPatch: Bool { supportsCheckPatch == true && (agendaIndex ? supportsIndexCheckPatch == true : events.count >= 600) }

    private enum CodingKeys: String, CodingKey {
        case deviceId, workspaceName, address, networkError, accessManagerId, lastSync, stateToken
        case supportsSyncStamp, supportsCheckPatch, supportsIndexCheckPatch, checklistFormat, employeeId, year
        case agendaIndex, reminderCount, lanEnabled, canManage, paused, employees, members, events, tasks, reminders, notificationDates, checkPatch
    }

    init(from decoder: Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        deviceId = try fields.decode(String.self, forKey: .deviceId)
        workspaceName = try fields.decodeIfPresent(String.self, forKey: .workspaceName)
        address = try fields.decodeIfPresent(String.self, forKey: .address)
        networkError = try fields.decodeIfPresent(String.self, forKey: .networkError)
        accessManagerId = try fields.decodeIfPresent(String.self, forKey: .accessManagerId)
        lastSync = try fields.decodeIfPresent(String.self, forKey: .lastSync)
        stateToken = try fields.decodeIfPresent(String.self, forKey: .stateToken)
        supportsSyncStamp = try fields.decodeIfPresent(Bool.self, forKey: .supportsSyncStamp)
        supportsCheckPatch = try fields.decodeIfPresent(Bool.self, forKey: .supportsCheckPatch)
        supportsIndexCheckPatch = try fields.decodeIfPresent(Bool.self, forKey: .supportsIndexCheckPatch)
        checklistFormat = try fields.decodeIfPresent(String.self, forKey: .checklistFormat)
        employeeId = try fields.decodeIfPresent(String.self, forKey: .employeeId)
        year = try fields.decode(Int.self, forKey: .year)
        agendaIndex = try fields.decodeIfPresent(Bool.self, forKey: .agendaIndex) ?? false
        reminderCount = agendaIndex ? try fields.decode(Int.self, forKey: .reminderCount) : 0
        lanEnabled = try fields.decodeIfPresent(Bool.self, forKey: .lanEnabled)
        canManage = try fields.decode(Bool.self, forKey: .canManage)
        paused = try fields.decode(Bool.self, forKey: .paused)
        employees = try fields.decode([GPEmployee].self, forKey: .employees)
        members = try fields.decode([GPMember].self, forKey: .members)
        reminders = try fields.decode([GPReminder].self, forKey: .reminders)
        notificationDates = try fields.decode([String].self, forKey: .notificationDates)

        if agendaIndex {
            do {
                let summaries = try fields.decode([GPAgendaSummary].self, forKey: .events)
                agendaEvents = summaries
                events = []
                checkPatchCard = nil
            } catch {
                guard try fields.decodeIfPresent(Bool.self, forKey: .checkPatch) == true else { throw error }
                let card = try fields.decode([GPEvent].self, forKey: .events)
                guard card.count == 1 else {
                    throw DecodingError.dataCorruptedError(forKey: .events, in: fields, debugDescription: "An indexed check patch must contain one full card")
                }
                agendaEvents = []
                events = []
                checkPatchCard = card.first
            }
            agendaTasks = try fields.decode([GPAgendaSummary].self, forKey: .tasks)
            tasks = []
        } else {
            events = try fields.decode([GPEvent].self, forKey: .events)
            tasks = try fields.decode([GPEvent].self, forKey: .tasks)
            agendaEvents = []
            agendaTasks = []
            checkPatchCard = nil
        }
    }

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

    init?(prior: GPState, displayed: GPEvent, task: GPTask, cardToken: String? = nil) {
        guard prior.canUseCheckPatch,
              prior.year == displayed.year, prior.workspaceName != nil, prior.employeeId != nil,
              let token = prior.stateToken, UUID(uuidString: token)?.uuidString.lowercased() == token,
              cardToken == nil || cardToken == token,
              let row = prior.events.first(where: { $0.id == displayed.id }),
              row == displayed, row.checklist.contains(task) else { return nil }
        self.token = token
        self.eventID = displayed.id
        self.taskID = task.id
        self.year = displayed.year
        self.done = !task.done
    }
}

struct GPIndexedCheckContext {
    let token, eventID, taskID: String
    let viewYear, eventYear: Int
    let done: Bool

    init?(prior: GPState, card: GPEvent, cardToken: String, viewYear: Int, task: GPTask) {
        let personal = prior.agendaTasks.first(where: { $0.id == card.id && $0.year == card.year })
        guard prior.agendaIndex, prior.supportsIndexCheckPatch == true,
              prior.year == viewYear, cardToken == prior.stateToken,
              let token = prior.stateToken, UUID(uuidString: token)?.uuidString.lowercased() == token,
              card.checklist.filter({ $0.id == task.id }).count == 1,
              let row = personal ?? prior.agendaEvents.first(where: { $0.id == card.id && $0.year == card.year }),
              row.title == card.title, row.date == card.date, row.ownerId == card.ownerId, row.employeeId == card.employeeId,
              (personal == nil || Self.summaryTask(row.nextOpenTask, matches: card.nextOpenTask)) else { return nil }
        self.token = token
        self.eventID = card.id
        self.taskID = task.id
        self.viewYear = viewYear
        self.eventYear = card.year
        self.done = !task.done
    }

    static func matchesSummaryTask(_ summary: GPNextOpenTask?, event: GPEvent) -> Bool {
        summaryTask(summary, matches: event.nextOpenTask)
    }

    private static func summaryTask(_ summary: GPNextOpenTask?, matches full: GPTask?) -> Bool {
        switch (summary, full) {
        case (nil, nil): return true
        case (let summary?, let full?): return summary.id == full.id && summary.title == full.title && summary.dueDate == full.dueDate
        default: return false
        }
    }
}

struct GPAgendaCard: Decodable {
    let baseToken: String
    let year: Int
    let events: [GPEvent]

    func event(id: String, eventYear: Int, in prior: GPState) -> GPEvent? {
        let personal = prior.agendaTasks.first(where: { $0.id == id && $0.year == eventYear })
        guard prior.agendaIndex, prior.year == year, baseToken == prior.stateToken, events.count == 1,
              let event = events.first, event.id == id, event.year == eventYear,
              let row = personal ?? prior.agendaEvents.first(where: { $0.id == id && $0.year == eventYear }),
              row.title == event.title, row.date == event.date, row.ownerId == event.ownerId, row.employeeId == event.employeeId,
              (personal == nil || GPIndexedCheckContext.matchesSummaryTask(row.nextOpenTask, event: event)) else { return nil }
        return event
    }
}

struct GPReminderPage: Decodable {
    let baseToken: String
    let year, offset: Int
    let reminders: [GPReminder]

    func rows(in prior: GPState, expectedOffset: Int, limit: Int = 64) -> [GPReminder]? {
        guard prior.agendaIndex, year == prior.year, baseToken == prior.stateToken,
              limit > 0, limit <= 64, offset == expectedOffset,
              offset >= 0, offset <= prior.reminderCount,
              reminders.count == min(limit, prior.reminderCount - offset),
              reminders.allSatisfy(valid) else { return nil }
        return reminders
    }

    private func valid(_ reminder: GPReminder) -> Bool {
        guard UUID(uuidString: reminder.eventId)?.uuidString.lowercased() == reminder.eventId,
              GPDate.parse(reminder.date) != nil else { return false }
        guard let taskId = reminder.taskId else { return true }
        return UUID(uuidString: taskId)?.uuidString.lowercased() == taskId
    }
}

struct GPCheckPatch: Decodable {
    let updated: GPState
    let baseToken: String
    let taskPositions: [Int]
    let reminderPositions: [Int]?
    let taskCount, reminderCount: Int
    private enum CodingKeys: String, CodingKey { case baseToken, taskPositions, reminderPositions, taskCount, reminderCount }

    init(from decoder: Decoder) throws {
        updated = try GPState(from: decoder)
        let c = try decoder.container(keyedBy: CodingKeys.self)
        baseToken = try c.decode(String.self, forKey: .baseToken)
        taskPositions = try c.decode([Int].self, forKey: .taskPositions)
        reminderPositions = try c.decodeIfPresent([Int].self, forKey: .reminderPositions)
        taskCount = try c.decode(Int.self, forKey: .taskCount)
        reminderCount = try c.decode(Int.self, forKey: .reminderCount)
    }

    func applying(to prior: GPState, for check: GPCheckContext) -> GPState? {
        guard !prior.agendaIndex && !updated.agendaIndex else { return nil }
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
              let reminders = merge(prior.reminders, updated.reminders, at: reminderPositions ?? [], count: reminderCount, matching: { $0.eventId == row.id })
        else { return nil }
        var result = updated
        result.events = prior.events
        result.events[index] = row
        result.tasks = tasks
        result.reminders = reminders
        return result
    }

    func applying(to prior: GPState, for check: GPIndexedCheckContext) -> GPState? {
        guard prior.agendaIndex, updated.agendaIndex,
              check.viewYear == prior.year, check.viewYear == updated.year,
              check.eventYear == check.viewYear,
              check.eventYear == updated.checkPatchCard?.year,
              check.token == prior.stateToken, baseToken == check.token,
              let token = updated.stateToken, token != baseToken,
              UUID(uuidString: token)?.uuidString.lowercased() == token,
              updated.supportsCheckPatch == true, updated.supportsIndexCheckPatch == true,
              prior.supportsIndexCheckPatch == true,
              prior.deviceId == updated.deviceId, prior.employeeId == updated.employeeId,
              prior.workspaceName == updated.workspaceName, prior.members == updated.members,
              prior.employees == updated.employees, prior.accessManagerId == updated.accessManagerId,
              prior.canManage == updated.canManage, prior.lanEnabled == updated.lanEnabled,
              prior.paused == updated.paused, prior.address == updated.address,
              prior.networkError == updated.networkError, prior.lastSync == updated.lastSync,
              updated.events.isEmpty, updated.tasks.isEmpty, updated.agendaEvents.isEmpty,
              updated.reminders.isEmpty, reminderPositions == nil,
              let row = updated.checkPatchCard,
              row.id == check.eventID, row.year == check.eventYear,
              let task = row.checklist.first(where: { $0.id == check.taskID }), task.done == check.done,
              row.checklist.filter({ $0.id == check.taskID }).count == 1,
              let priorSummary = prior.agendaTasks.first(where: { $0.id == row.id && $0.year == row.year }) ??
                                 prior.agendaEvents.first(where: { $0.id == row.id && $0.year == row.year }),
              summaryMetadata(priorSummary, matches: row),
              validIndexedTaskRows(prior: prior.agendaTasks, replacements: updated.agendaTasks, card: row, eventID: row.id),
              updated.agendaTasks.filter({ $0.id == row.id }).count == taskPositions.count,
              updated.agendaTasks.allSatisfy(validIndexedSummary),
              Set(updated.agendaTasks.filter({ $0.id == row.id }).map(\.year)).count == taskPositions.count,
              let tasks = merge(prior.agendaTasks, updated.agendaTasks, at: taskPositions, count: taskCount, matching: { $0.id == row.id })
        else { return nil }

        // The annual list and pages remain lightweight; only the current detail screen owns the full card.
        var result = updated
        result.events = []
        result.tasks = []
        result.agendaEvents = prior.agendaEvents
        result.agendaTasks = tasks
        result.reminders = []
        result.reminderCount = reminderCount
        result.checkPatchCard = nil
        return result
    }

    private func summaryMetadata(_ summary: GPAgendaSummary, matches row: GPEvent) -> Bool {
        summary.id == row.id && summary.year == row.year && summary.title == row.title &&
        summary.date == row.date && summary.ownerId == row.ownerId && summary.employeeId == row.employeeId
    }

    private func validIndexedTaskRows(prior: [GPAgendaSummary], replacements: [GPAgendaSummary], card: GPEvent, eventID: String) -> Bool {
        let oldRows = prior.filter { $0.id == eventID }
        let newRows = replacements.filter { $0.id == eventID }
        let oldYears = Set(oldRows.map(\.year))
        let newYears = Set(newRows.map(\.year))
        guard oldYears.count == oldRows.count, newYears.count == newRows.count,
              oldYears.subtracting([card.year]) == newYears.subtracting([card.year]) else { return false }
        let oldCurrent = oldRows.first(where: { $0.year == card.year })
        let newCurrent = newRows.first(where: { $0.year == card.year })
        if oldCurrent != nil && newCurrent == nil && card.nextOpenTask != nil { return false }
        if oldCurrent == nil && newCurrent != nil && card.nextOpenTask == nil { return false }
        if let current = newCurrent {
            guard current.title == card.title, current.date == card.date,
                  current.ownerId == card.ownerId, current.employeeId == card.employeeId,
                  GPIndexedCheckContext.matchesSummaryTask(current.nextOpenTask, event: card) else { return false }
        }
        for row in newRows where row.year != card.year {
            guard let old = oldRows.first(where: { $0.year == row.year }), row == old else { return false }
        }
        return true
    }

    private func validIndexedSummary(_ row: GPAgendaSummary) -> Bool {
        guard UUID(uuidString: row.id)?.uuidString.lowercased() == row.id,
              GPDate.parse(row.date) != nil else { return false }
        if let ownerId = row.ownerId, UUID(uuidString: ownerId)?.uuidString.lowercased() != ownerId { return false }
        if let employeeId = row.employeeId, UUID(uuidString: employeeId)?.uuidString.lowercased() != employeeId { return false }
        if let task = row.nextOpenTask {
            return UUID(uuidString: task.id)?.uuidString.lowercased() == task.id && GPDate.parse(task.dueDate) != nil
        }
        return true
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
    static func components(_ text: String) -> DateComponents? {
        let bytes = text.utf8
        guard bytes.count == 10,
              bytes[bytes.index(bytes.startIndex, offsetBy: 4)] == 45,
              bytes[bytes.index(bytes.startIndex, offsetBy: 7)] == 45,
              let year = digits(bytes, offset: 0, count: 4),
              let month = digits(bytes, offset: 5, count: 2),
              let day = digits(bytes, offset: 8, count: 2),
              (1...9999).contains(year), (1...12).contains(month) else { return nil }
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
        let monthLengths = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        guard (1...monthLengths[month - 1]).contains(day) else { return nil }
        var parts = DateComponents()
        parts.year = year
        parts.month = month
        parts.day = day
        parts.calendar = gregorian
        return parts
    }
    private static func digits(_ bytes: String.UTF8View, offset: Int, count: Int) -> Int? {
        var value = 0
        var index = bytes.index(bytes.startIndex, offsetBy: offset)
        for _ in 0..<count {
            let byte = bytes[index]
            guard (48...57).contains(byte) else { return nil }
            value = value * 10 + Int(byte - 48)
            index = bytes.index(after: index)
        }
        return value
    }
    static func parse(_ text: String) -> Date? {
        guard var parts = components(text) else { return nil }
        parts.hour = 12
        return gregorian.date(from: parts)
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
