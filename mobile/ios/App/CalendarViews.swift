import SwiftUI

struct CalendarView: View {
    @EnvironmentObject var model: CalendarModel
    @State private var month: Int?
    @State private var creating = false
    @State private var search = ""
    var body: some View {
        List {
            Section {
                HStack { yearButton(-1); Spacer(); Text(String(model.year)).font(.title2.bold()).monospacedDigit(); Spacer(); yearButton(1) }
                if let events = model.state?.events, !events.isEmpty && search.isEmpty {
                    GPYearWheel(counts: (1...12).map { m in events.filter { $0.month == m }.count }, selected: $month)
                    if month != nil { Button("Показать весь год") { month = nil } }
                }
            }
            if let state = model.state {
                let list = state.events.filter { (month == nil || $0.month == month) && (search.isEmpty || $0.title.localizedCaseInsensitiveContains(search)) }.sorted { ($0.date, $0.title) < ($1.date, $1.title) }
                if list.isEmpty { Text(search.isEmpty ? "Событий пока нет" : "Событий по этому запросу нет").foregroundStyle(.secondary) }
                ForEach((1...12).filter { m in list.contains { $0.month == m } }, id: \.self) { m in
                    Section(GPDate.months[m - 1]) { ForEach(list.filter { $0.month == m }) { event in EventLink(event: event) } }
                }
            }
        }
        .navigationTitle("Календарь")
        .searchable(text: $search, prompt: "Найти событие")
        .refreshable { await model.refresh() }
        .toolbar { Button { creating = true } label: { Image(systemName: "plus") }.accessibilityLabel("Новое событие").accessibilityIdentifier("newEvent") }
        .sheet(isPresented: $creating) { NavigationStack { EventEditor(event: nil, year: model.year) } }
        .task(id: model.year) { await model.refresh() }
    }
    private func yearButton(_ delta: Int) -> some View {
        Button { month = nil; model.year += delta } label: { Image(systemName: delta < 0 ? "chevron.left" : "chevron.right").frame(minWidth: 40, minHeight: 40) }.buttonStyle(.borderless).disabled(!(1900...9999).contains(model.year + delta)).accessibilityLabel(delta < 0 ? "Предыдущий год" : "Следующий год")
    }
}
struct EventLink: View {
    let event: GPEvent
    var body: some View { NavigationLink { DetailView(id: event.id, year: event.year) } label: { VStack(alignment: .leading, spacing: 4) { Text(event.title).font(.headline); Text(GPDate.label(event.date)).font(.subheadline).foregroundStyle(.secondary) } } }
}
struct TasksView: View {
    @EnvironmentObject var model: CalendarModel
    var body: some View {
        List {
            if let state = model.state {
                Section("Подготовка") { if state.tasks.isEmpty { Text("На сегодня задач нет").foregroundStyle(.secondary) }; ForEach(state.tasks, id: \.selfKey) { EventLink(event: $0) } }
                Section("Напоминания") {
                    if state.reminders.isEmpty { Text("Напоминаний пока нет").foregroundStyle(.secondary) }
                    ForEach(Array(state.reminders.enumerated()), id: \.offset) { _, r in NavigationLink { DetailView(id: r.eventId, year: r.year) } label: { VStack(alignment: .leading) { Text(r.title); Text(GPDate.label(r.date)).foregroundStyle(.secondary).font(.subheadline) } } }
                }
            }
        }.navigationTitle("Мои задачи").refreshable { await model.refresh() }
    }
}
extension GPEvent { var selfKey: String { "\(id):\(year)" } }
struct DetailView: View {
    @EnvironmentObject var model: CalendarModel
    @Environment(\.dismiss) var dismiss
    let id: String
    let year: Int
    @State private var event: GPEvent?
    @State private var note = ""
    @State private var base = ""
    @State private var edit = false
    @State private var remove = false
    @State private var loaded = false
    var body: some View {
        List {
            if let event {
                Section { LabeledContent("Дата", value: GPDate.label(event.date)); LabeledContent("Начало подготовки", value: GPDate.label(event.preparationDate)); if let owner = model.state?.ownerName(event.ownerId) { LabeledContent("Ответственный", value: owner) }; if event.yearly { Label("Ежегодно", systemImage: "repeat") }; if !event.description.isEmpty { Text(event.description) } }
                if !event.checklist.isEmpty {
                    Section("Чек-лист") { ForEach(event.checklist) { t in
                        Button { Task { if await model.run("check", ["eventId": .string(id), "year": .integer(year), "taskId": .string(t.id), "done": .flag(!t.done)]) { await reload() } } } label: {
                            HStack { Image(systemName: t.done ? "checkmark.circle.fill" : "circle").foregroundStyle(Color.accentColor); VStack(alignment: .leading) { Text(t.title).foregroundStyle(.primary); Text(GPDate.label(t.dueDate)).font(.caption).foregroundStyle(.secondary) } }
                        }.disabled(model.busy).accessibilityValue(t.done ? "Выполнено" : "Открыто")
                    } }
                }
                Section("Дневник за \(String(year)) год") { TextField("Заметка", text: $note, axis: .vertical).lineLimit(3...12); Button("Сохранить заметку") { Task { if await model.run("note", ["eventId": .string(id), "year": .integer(year), "text": .string(note), "base": .string(base)]) { base = note; await reload() } } }.disabled(model.busy || note == base) }
                Section { Button("Удалить событие…", role: .destructive) { remove = true } }
            } else if loaded { Text("Событие недоступно в выбранном году.") } else { ProgressView() }
        }
        .navigationTitle(event?.title ?? "Событие").navigationBarTitleDisplayMode(.inline)
        .toolbar { if event != nil { Button("Править") { edit = true } } }
        .sheet(isPresented: $edit, onDismiss: { Task { await reload() } }) { if let event { NavigationStack { EventEditor(event: event, year: year) } } }
        .confirmationDialog("Удалить событие?", isPresented: $remove, titleVisibility: .visible) {
            if let event, event.yearly { Button("Только за этот год", role: .destructive) { delete("year") }; Button("Начиная с этого года", role: .destructive) { delete("future") } }
            else { Button("Удалить", role: .destructive) { delete("future") } }
            Button("Отмена", role: .cancel) {}
        }
        .task(id: id) { await reload() }.refreshable { await reload() }
        .onReceive(model.$state) { state in
            guard let state else { return }; if let event = (state.events + state.tasks).first(where: { $0.id == id && $0.year == year }) { update(event) } else if state.year == year { event = nil }
        }
    }
    private func update(_ next: GPEvent) { if !loaded || note == base { note = next.note; base = next.note }; event = next; loaded = true }
    private func reload() async { do { let state = try await model.snapshot(year: year); if let row = state.events.first(where: { $0.id == id }) { update(row) } else { event = nil; loaded = true } } catch { model.failure = error.localizedDescription } }
    private func delete(_ scope: String) { guard let event else { return }; Task { if await model.run("deleteEvent", ["eventId": .string(id), "year": .integer(year), "revision": .string(event.revision), "scope": .string(scope)]) { dismiss() } } }
}
