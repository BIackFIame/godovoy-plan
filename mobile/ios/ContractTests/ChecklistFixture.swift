import Foundation

private struct Fixture: Decodable {
    struct Command: Decodable {
        let eventId, taskId: String
        let done: Bool
    }
    struct InvalidPatch: Decodable {
        let name: String
        let patch: GPSnapshot
    }
    let before, expected: GPState
    let command: Command
    let patch: GPSnapshot
    let invalidPatches: [InvalidPatch]
}

private struct Limits: Decodable {
    let maxTasks, maxTitleChars, maxDescriptionChars, maxNoteChars, maxPreparationDays: Int
    let maxExchangeBytes: Int
}

private struct StampFixture: Decodable {
    let prior: GPState
    let update: GPSyncStamp
}

private struct IndexedFixture: Decodable {
    struct Command: Decodable {
        let eventId, taskId: String
        let done: Bool
    }
    let before, expected: GPState
    let command: Command
    let card: GPAgendaCard
    let fullCard: GPEvent
    let page: GPReminderPage
    let fullPage: [GPReminder]
    let patch: GPSnapshot
}

private struct ContractBundle: Decodable {
    let limits: Limits
    let cases: [Fixture]
    let syncStamp: StampFixture
    let indexed: IndexedFixture
}

@main
enum ChecklistFixture {
    static func main() throws {
        let data = try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1]))
        let bundle = try JSONDecoder().decode(ContractBundle.self, from: data)
        try checkLimits(bundle.limits)
        try checkDates()
        try checkSyncStamp(bundle.syncStamp)
        try checkIndexed(bundle.indexed)
        try checkEnvelope()
        try checkSections()
        try checkTaskDecoder()

        guard bundle.cases.count == 2 else { throw GPError("Expected one object check and one tuple-v1 uncheck fixture") }
        for test in bundle.cases {
            guard case let .check(patch) = test.patch,
                  let event = test.before.events.first(where: { $0.id == test.command.eventId }),
                  let task = event.checklist.first(where: { $0.id == test.command.taskId }),
                  let check = GPCheckContext(prior: test.before, displayed: event, task: task),
                  check.done == test.command.done,
                  patch.applying(to: test.before, for: check) == test.expected,
                  patch.applying(to: test.expected, for: check) == nil
            else { throw GPError("The iOS checklist merge diverged from the Rust snapshot") }
            for invalid in test.invalidPatches {
                guard case let .check(bad) = invalid.patch,
                      bad.applying(to: test.before, for: check) == nil else {
                    throw GPError("The iOS checklist merge accepted \(invalid.name)")
                }
            }
        }
        print("Swift mobile contracts match Rust fixtures.")
    }

    private static func checkLimits(_ limits: Limits) throws {
        guard limits.maxTasks == GPMobileLimits.maxTasks,
              limits.maxTitleChars == GPMobileLimits.maxTitleChars,
              limits.maxDescriptionChars == GPMobileLimits.maxDescriptionChars,
              limits.maxNoteChars == GPMobileLimits.maxNoteChars,
              limits.maxPreparationDays == GPMobileLimits.maxPreparationDays,
              limits.maxExchangeBytes == GPMobileLimits.maxExchangeBytes,
              GPMobileLimits.withinTitleLimit(String(repeating: "🙂", count: 160)),
              !GPMobileLimits.withinTitleLimit(String(repeating: "🙂", count: 161)),
              GPMobileLimits.withinDescriptionLimit(String(repeating: "e\u{301}", count: 2000)),
              !GPMobileLimits.withinDescriptionLimit(String(repeating: "e\u{301}", count: 2001)),
              GPMobileLimits.withinNoteLimit(String(repeating: "🙂", count: 4000)),
              !GPMobileLimits.withinNoteLimit(String(repeating: "🙂", count: 4001)),
              GPMobileLimits.withinExchangeLimit(9 * 1024 * 1024),
              !GPMobileLimits.withinExchangeLimit(9 * 1024 * 1024 + 1),
              !GPMobileLimits.withinExchangeLimit(-1) else {
            throw GPError("The iOS mobile limits diverged from the Rust model")
        }
    }

    private static func checkDates() throws {
        guard GPDate.parse("2024-02-29") != nil,
              GPDate.components("2024-02-29")?.day == 29,
              GPDate.parse("2000-02-29") != nil,
              GPDate.parse("1900-02-29") == nil,
              GPDate.parse("2023-02-29") == nil,
              GPDate.parse("2024-13-01") == nil,
              GPDate.parse("2024-2-09") == nil,
              GPDate.parse("2024/02/09") == nil,
              GPDate.parse("2024-02-09x") == nil else {
            throw GPError("The shared ISO date parser accepted an invalid calendar date")
        }
    }

    private static func checkSyncStamp(_ fixture: StampFixture) throws {
        let token = fixture.prior.stateToken ?? ""
        guard let updated = fixture.update.applying(to: fixture.prior, year: fixture.prior.year, token: token) else {
            throw GPError("The valid syncStamp was rejected")
        }
        var expected = fixture.prior
        expected.stateToken = fixture.update.stateToken
        expected.lastSync = fixture.update.lastSync
        guard updated == expected else { throw GPError("syncStamp changed fields outside lastSync and stateToken") }

        try rejectStamp(fixture) { $0["year"] = fixture.prior.year + 1 }
        try rejectStamp(fixture) { $0["baseToken"] = "00000000-0000-4000-8000-000000000004" }
        try rejectStamp(fixture) { $0["stateToken"] = token }
        try rejectStamp(fixture) { $0["stateToken"] = "not-a-uuid" }
        try rejectStamp(fixture) { $0["lastSync"] = "not-a-date" }
        try rejectStamp(fixture) { $0["lastSync"] = 1 }

        var cleared = object(fixture.update)
        cleared["lastSync"] = NSNull()
        let nullUpdate = try decode(GPSyncStamp.self, cleared)
        guard let nullResult = nullUpdate.applying(to: fixture.prior, year: fixture.prior.year, token: token),
              nullResult.lastSync == nil else { throw GPError("syncStamp did not accept explicit null") }

        var missing = object(fixture.update)
        missing.removeValue(forKey: "lastSync")
        guard (try? decode(GPSyncStamp.self, missing)) == nil else { throw GPError("syncStamp accepted a missing lastSync field") }

        var conflict = object(fixture.update)
        conflict["unchanged"] = true
        guard (try? decode(GPSnapshot.self, conflict)) == nil else { throw GPError("Conflicting conditional markers were accepted") }
    }

    private static func rejectStamp(_ fixture: StampFixture, changed: (inout [String: Any]) -> Void) throws {
        var update = object(fixture.update)
        changed(&update)
        guard let candidate = try? decode(GPSyncStamp.self, update) else { return }
        guard candidate.applying(to: fixture.prior, year: fixture.prior.year, token: fixture.prior.stateToken ?? "") == nil else {
            throw GPError("syncStamp accepted an invalid field")
        }
    }

    private static func checkEnvelope() throws {
        let object = try decode(GPEnvelope<JSONValue>.self, ["ok": true, "data": ["value": 1]])
        try object.requireSuccess(fallback: "read fallback")
        guard case .object = object.data else { throw GPError("The envelope object payload was lost") }

        let string = try decode(GPEnvelope<String>.self, ["ok": true, "data": "copied"])
        try string.requireSuccess(fallback: "read fallback")
        guard string.data == "copied" else { throw GPError("The envelope string payload was lost") }

        let null = try decode(GPEnvelope<JSONValue>.self, ["ok": true, "data": NSNull()])
        try null.requireSuccess(fallback: "write fallback")
        guard null.data == nil else { throw GPError("Successful null mutation data was not treated as empty") }

        let supplied = try decode(GPEnvelope<String>.self, ["ok": false, "error": "Сохранение отклонено"])
        do { try supplied.requireSuccess(fallback: "write fallback"); throw GPError("A failed envelope was accepted") }
        catch let issue as GPError { guard issue.message == "Сохранение отклонено" else { throw issue } }

        let absent = try decode(GPEnvelope<String>.self, ["ok": false])
        do { try absent.requireSuccess(fallback: "write fallback"); throw GPError("A failed envelope without text was accepted") }
        catch let issue as GPError { guard issue.message == "write fallback" else { throw issue } }

        guard (try? decode(GPEnvelope<String>.self, ["data": "missing marker"])) == nil else {
            throw GPError("An incomplete envelope was accepted")
        }
    }

    private static func checkSections() throws {
        let lowerID = try event(id: "00000000-0000-4000-8000-000000000001", title: "Яблоко")
        let higherID = try event(id: "00000000-0000-4000-8000-000000000002", title: "Альфа")
        let sections = GPYearSections(events: [lowerID, higherID], selectedMonth: nil, search: "")
        guard sections.rows[1].map(\.id) == [higherID.id, lowerID.id] else {
            throw GPError("The iOS month sections changed their existing same-day title order")
        }
        let filtered = GPYearSections(events: [lowerID, higherID], selectedMonth: 2, search: "яб")
        guard filtered.rows[1].map(\.id) == [lowerID.id] else { throw GPError("Month/search filtering changed") }
    }

    private static func event(id: String, title: String, tasks: [[String: Any]] = []) throws -> GPEvent {
        try decode(GPEvent.self, [
            "id": id, "year": 2027, "title": title, "date": "2027-02-01", "category": "Team",
            "preparationDate": "2027-01-01", "note": "", "description": "", "preparationDays": 31,
            "yearly": false, "ownerId": NSNull(), "employeeId": NSNull(), "checklist": tasks, "revision": "1:1"
        ])
    }

    private static func checkTaskDecoder() throws {
        let id = "00000000-0000-4000-8000-000000000005"
        let object: [String: Any] = ["id": id, "title": "Пункт", "dueDate": "2027-01-01", "daysBefore": 7, "done": false]
        let task = try decode(GPTask.self, object)
        let tuple = try decode(GPTask.self, [id, "Пункт", "2027-01-01", 7, false])
        guard task == tuple else { throw GPError("Object and tuple-v1 checklist rows differ") }
        let invalid: [Any] = [[id, "Пункт", "2027-01-01", 7], [id, "Пункт", "2027-01-01", 7, false, "extra"],
                              [id, "Пункт", "2027-01-01", "7", false], [id, "Пункт", "2027-01-01", 7, "false"]]
        for row in invalid {
            guard (try? decode(GPTask.self, row)) == nil else { throw GPError("The tuple-v1 decoder accepted a malformed row") }
        }
        let closed = try event(id: "00000000-0000-4000-8000-000000000006", title: "Closed", tasks: [
            ["id": id, "title": "Done", "dueDate": "2027-01-01", "daysBefore": 7, "done": true]
        ])
        let open = try event(id: "00000000-0000-4000-8000-000000000007", title: "Open", tasks: [
            ["id": id, "title": "Open", "dueDate": "2027-01-01", "daysBefore": 7, "done": false]
        ])
        let empty = try event(id: "00000000-0000-4000-8000-000000000008", title: "Empty")
        guard closed.nextOpenTask == nil, open.nextOpenTask?.id == id, empty.nextOpenTask == nil else {
            throw GPError("nextOpenTask selection changed")
        }
    }

    private static func checkIndexed(_ fixture: IndexedFixture) throws {
        let before = fixture.before
        guard before.agendaIndex, fixture.expected.agendaIndex,
              before.events.isEmpty, before.tasks.isEmpty, before.reminders.isEmpty,
              let token = before.stateToken,
              let card = fixture.card.event(id: fixture.command.eventId, eventYear: fixture.fullCard.year, in: before),
              card == fixture.fullCard,
              fixture.card.events == [fixture.fullCard],
              let page = fixture.page.rows(in: before, expectedOffset: 0), page == fixture.fullPage,
              page.count == min(64, before.reminderCount),
              let task = card.checklist.first(where: { $0.id == fixture.command.taskId }),
              let context = GPIndexedCheckContext(prior: before, card: card, cardToken: token, viewYear: before.year, task: task),
              context.done == fixture.command.done,
              case let .check(patch) = fixture.patch,
              patch.applying(to: before, for: context) == fixture.expected,
              patch.applying(to: fixture.expected, for: context) == nil else {
            throw GPError("The indexed iOS card, page, or checklist merge diverged from Rust")
        }

        var staleToken = before
        staleToken.stateToken = "00000000-0000-4000-8000-000000000099"
        guard fixture.card.event(id: fixture.command.eventId, eventYear: fixture.fullCard.year, in: staleToken) == nil,
              fixture.page.rows(in: staleToken, expectedOffset: 0) == nil,
              fixture.card.event(id: fixture.command.eventId, eventYear: fixture.fullCard.year + 1, in: before) == nil,
              fixture.page.rows(in: before, expectedOffset: 1) == nil,
              fixture.page.rows(in: before, expectedOffset: 0, limit: 65) == nil else {
            throw GPError("A stale or out-of-range indexed card/page was accepted")
        }
        let wrongYearCard = GPAgendaCard(baseToken: fixture.card.baseToken, year: before.year + 1,
                                        events: fixture.card.events)
        let wrongYearPage = GPReminderPage(baseToken: fixture.page.baseToken, year: before.year + 1,
                                           offset: fixture.page.offset, reminders: fixture.page.reminders)
        guard wrongYearCard.event(id: fixture.command.eventId, eventYear: fixture.fullCard.year, in: before) == nil,
              wrongYearPage.rows(in: before, expectedOffset: 0) == nil,
              GPIndexedCheckContext(prior: before, card: card, cardToken: token, viewYear: before.year + 1, task: task) == nil,
              GPIndexedCheckContext(prior: before, card: card, cardToken: "00000000-0000-4000-8000-000000000099", viewYear: before.year, task: task) == nil else {
            throw GPError("An indexed response crossed its year or card token")
        }
    }

    private static func decode<T: Decodable>(_ type: T.Type, _ object: Any) throws -> T {
        let bytes = try JSONSerialization.data(withJSONObject: object, options: [.fragmentsAllowed])
        return try JSONDecoder().decode(type, from: bytes)
    }

    private static func object(_ value: GPSyncStamp) -> [String: Any] {
        ["syncStamp": true, "year": value.year, "baseToken": value.baseToken,
         "stateToken": value.stateToken, "lastSync": value.lastSync ?? NSNull()]
    }
}
