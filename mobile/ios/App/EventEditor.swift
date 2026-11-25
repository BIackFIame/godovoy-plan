import SwiftUI

private struct DraftTask: Identifiable {
    let id = UUID()
    var savedID: String?
    var title: String
    var days: Int
}
struct EventEditor: View {
    @EnvironmentObject var model: CalendarModel
    @Environment(\.dismiss) var dismiss
    let event: GPEvent?
    @State private var title: String
    @State private var day: Date
    @State private var yearly: Bool
    @State private var prep: Int
    @State private var category: String
    @State private var owner: String
    @State private var details: String
    @State private var tasks: [DraftTask]
    @State private var scope = "year"
    @State private var discard = false

    init(event: GPEvent?, year: Int) {
        self.event = event
        let initial = GPDate.gregorian.date(from: DateComponents(year: year, month: 1, day: 1, hour: 12)) ?? Date()
        _title = State(initialValue: event?.title ?? "")
        _day = State(initialValue: event.flatMap { GPDate.parse($0.date) } ?? (year == GPDate.year ? Date() : initial))
        _yearly = State(initialValue: event?.yearly ?? false)
        _prep = State(initialValue: event?.preparationDays ?? 7)
        _category = State(initialValue: event?.category ?? "Team")
        _owner = State(initialValue: event?.employeeId ?? "")
        _details = State(initialValue: event?.description ?? "")
        _tasks = State(initialValue: event?.checklist.map { DraftTask(savedID: $0.id, title: $0.title, days: $0.daysBefore) } ?? [])
    }
    var body: some View {
        Form {
            if event?.yearly == true { Section("Применить изменения") { Picker("Период", selection: $scope) { Text("Только этот год").tag("year"); Text("С этого года").tag("future") }.pickerStyle(.segmented) } }
            Section("Событие") {
                TextField("Название события", text: $title).accessibilityIdentifier("eventTitle")
                DatePicker("Дата", selection: $day, displayedComponents: .date)
                if event?.yearly != true || scope == "future" { Toggle("Повторять каждый год", isOn: $yearly) }
                Picker("Категория", selection: $category) { ForEach(GPCategory.allCases, id: \.rawValue) { Text($0.label).tag($0.rawValue) } }
                Picker("Ответственный", selection: $owner) {
                    Text("Без ответственного").tag("")
                    if let state = model.state {
                        ForEach(state.employees) { person in Text(state.employeeLabel(person)).tag(person.id) }
                        if let original = event?.employeeId, !state.employees.contains(where: { $0.id == original }) {
                            Text((state.ownerName(original) ?? original) + " · нет активных устройств").tag(original)
                        }
                    }
                }
                TextField("Описание", text: $details, axis: .vertical).lineLimit(2...8)
            }
            Section { Stepper("За \(prep) дн. до события", value: $prep, in: 0...366) } header: { Text("Начало подготовки") }
            Section("Чек-лист") {
                ForEach($tasks) { $task in
                    VStack(alignment: .leading) {
                        TextField("Название пункта", text: $task.title)
                        Stepper("Срок за \(task.days) дн.", value: $task.days, in: 0...366).font(.subheadline)
                        if task.days > prep { Text("Срок раньше начала подготовки").font(.caption).foregroundStyle(.red) }
                    }
                }.onDelete { tasks.remove(atOffsets: $0) }
                Button { tasks.append(DraftTask(title: "", days: 0)) } label: { Label("Добавить пункт", systemImage: "plus") }.disabled(tasks.count >= 100)
            }
            if let issue = model.failure { Text(issue).foregroundStyle(.red) }
        }
        .navigationTitle(event == nil ? "Новое событие" : "Изменить событие").navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Отмена") { if modified { discard = true } else { dismiss() } }.disabled(model.busy) }
            ToolbarItem(placement: .confirmationAction) { Button("Сохранить") { Task { await save() } }.disabled(model.busy || title.isBlank || title.count > 160 || details.count > 4000 || tasks.contains { $0.title.isBlank || $0.days > prep || $0.title.count > 160 }).accessibilityIdentifier("eventSave") }
        }
        .interactiveDismissDisabled(modified)
        .confirmationDialog("Закрыть без сохранения?", isPresented: $discard, titleVisibility: .visible) { Button("Закрыть без сохранения", role: .destructive) { dismiss() }; Button("Продолжить редактирование", role: .cancel) {} }
    }
    private var modified: Bool {
        guard let event else { return !title.isEmpty || !tasks.isEmpty || !details.isEmpty || !owner.isEmpty || yearly || prep != 7 || category != "Team" }
        if title != event.title || details != event.description || GPDate.iso(day) != event.date || owner != (event.employeeId ?? "") || yearly != event.yearly || prep != event.preparationDays || category != event.category || tasks.count != event.checklist.count { return true }
        return zip(tasks, event.checklist).contains { draft, saved in draft.savedID != saved.id || draft.title != saved.title || draft.days != saved.daysBefore }
    }
    private func save() async {
        let checklist: [JSONValue] = tasks.map { task in
            var row: GPCommand = ["title": .string(task.title), "daysBefore": .integer(task.days)]
            if let id = task.savedID { row["id"] = .string(id) }
            return .object(row)
        }
        let savedOwner = event?.employeeId == owner ? (event?.ownerId ?? owner) : owner
        var fields: GPCommand = ["title": .string(title), "date": .string(GPDate.iso(day)), "yearly": .flag(yearly), "preparationDays": .integer(prep), "ownerId": savedOwner.isEmpty ? .null : .string(savedOwner), "category": .string(category), "checklist": .list(checklist), "description": .string(details)]
        if let event { fields["eventId"] = .string(event.id); fields["year"] = .integer(event.year); fields["revision"] = .string(event.revision); fields["scope"] = .string(event.yearly ? scope : "future") }
        if await model.run(event == nil ? "createEvent" : "editEvent", fields) { model.year = GPDate.gregorian.component(.year, from: day); dismiss() }
    }
}
