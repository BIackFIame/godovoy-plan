import SwiftUI

struct CalendarView: View {
    @EnvironmentObject var model: CalendarModel
    @State private var month: Int?
    @State private var creating = false
    @State private var search = ""
    var body: some View {
        let sections = model.state.map { state in
            state.agendaIndex
                ? GPYearSections(summaries: state.agendaEvents, selectedMonth: month, search: search)
                : GPYearSections(events: state.events, selectedMonth: month, search: search)
        }
        return List {
            Section {
                HStack { yearButton(-1); Spacer(); Text(String(model.year)).font(.title2.bold()).monospacedDigit(); Spacer(); yearButton(1) }
                if let sections, search.isEmpty, sections.counts.contains(where: { $0 > 0 }) {
                    GPYearWheel(counts: sections.counts, selected: $month)
                }
                if let month {
                    if !search.isEmpty { Text("Поиск за месяц · \(GPDate.months[month - 1])").foregroundStyle(.secondary) }
                    Button(search.isEmpty ? "Показать весь год" : "Искать во всём году") { self.month = nil }
                        .accessibilityHint("Снять фильтр по месяцу")
                }
            }
            if let sections {
                if sections.rows.allSatisfy(\.isEmpty) {
                    Text(!search.isEmpty ? "Событий по этому запросу нет" : month == nil ? "В этом году пока нет событий" : "В выбранном месяце пока нет событий")
                        .foregroundStyle(.secondary)
                }
                ForEach(sections.rows.indices.filter { !sections.rows[$0].isEmpty }, id: \.self) { index in
                    Section(GPDate.months[index]) {
                        ForEach(sections.rows[index]) { event in EventLink(item: event, viewYear: model.state?.year ?? model.year) }
                    }
                }
            }
        }
        .navigationTitle("Календарь")
        .navigationDestination(for: EventRoute.self) { route in
            DetailView(id: route.id, year: route.year, viewYear: route.viewYear, targetTask: route.targetTask)
        }
        .searchable(text: $search, prompt: "Найти событие")
        .refreshable { await model.refresh() }
        .toolbar { Button { creating = true } label: { Image(systemName: "plus") }.accessibilityLabel("Новое событие").accessibilityIdentifier("newEvent") }
        .sheet(isPresented: $creating) { NavigationStack { EventEditor(event: nil, year: model.year) } }
        .task(id: model.year) { await model.refresh() }
    }
    private func yearButton(_ delta: Int) -> some View {
        Button { month = nil; model.year += delta } label: { Image(systemName: delta < 0 ? "chevron.left" : "chevron.right").frame(minWidth: 44, minHeight: 44) }.buttonStyle(.borderless).disabled(!(1900...9999).contains(model.year + delta)).accessibilityLabel(delta < 0 ? "Предыдущий год" : "Следующий год")
    }
}
private struct EventRoute: Hashable {
    let id: String
    let year: Int
    let viewYear: Int
    let targetTask: String?
}

struct EventLink: View {
    let item: GPAgendaItem
    let viewYear: Int
    var showsPreparation = false

    var body: some View {
        let nextOpenTask = showsPreparation ? item.nextOpenTask : nil
        NavigationLink(value: EventRoute(id: item.id, year: item.year, viewYear: viewYear, targetTask: nextOpenTask?.id)) {
            VStack(alignment: .leading, spacing: 4) {
                if showsPreparation, let task = nextOpenTask {
                    Text(task.title).font(.headline)
                    Text("Срок · \(GPDate.label(task.dueDate))").font(.subheadline)
                    Text(item.title).font(.subheadline).foregroundStyle(.secondary)
                } else {
                    Text(item.title).font(.headline)
                    Text(showsPreparation ? "Дата события · \(GPDate.label(item.date))" : GPDate.label(item.date))
                        .font(.subheadline).foregroundStyle(.secondary)
                }
            }
        }
    }
}
struct TasksView: View {
    @EnvironmentObject var model: CalendarModel
    let isActive: Bool
    @State private var loadedReminders: [GPReminder] = []
    @State private var loadingPage = false
    @State private var pageIssue: String?

    var body: some View {
        let state = model.state
        let feedID = isActive ? "\(state?.year ?? -1):\(model.contentRevision)" : "inactive"
        List {
            if let state {
                Section("Подготовка") {
                    if state.agendaIndex {
                        if state.agendaTasks.isEmpty { Text("На сегодня задач нет").foregroundStyle(.secondary) }
                        ForEach(state.agendaTasks, id: \.key) { row in EventLink(item: GPAgendaItem(summary: row), viewYear: state.year, showsPreparation: true) }
                    } else {
                        if state.tasks.isEmpty { Text("На сегодня задач нет").foregroundStyle(.secondary) }
                        ForEach(state.tasks, id: \.selfKey) { row in EventLink(item: GPAgendaItem(event: row, includeNextOpenTask: true), viewYear: state.year, showsPreparation: true) }
                    }
                }
                Section("Напоминания") {
                    if state.agendaIndex {
                        if state.reminderCount == 0 { Text("Напоминаний пока нет").foregroundStyle(.secondary) }
                        ForEach(loadedReminders) { reminder in
                            NavigationLink(value: EventRoute(id: reminder.eventId, year: reminder.year, viewYear: state.year, targetTask: reminder.taskId)) {
                                VStack(alignment: .leading) {
                                    Text(reminder.title)
                                    Text(GPDate.label(reminder.date)).foregroundStyle(.secondary).font(.subheadline)
                                }
                            }
                        }
                        if loadedReminders.count < state.reminderCount {
                            if loadingPage { ProgressView("Загружаем напоминания…").frame(maxWidth: .infinity) }
                            else {
                                if let pageIssue { Text(pageIssue).font(.footnote).foregroundStyle(.secondary) }
                                Button(pageIssue == nil ? "Показать ещё" : "Повторить") { Task { await loadMore() } }
                                    .frame(maxWidth: .infinity)
                            }
                        }
                    } else {
                        if state.reminders.isEmpty { Text("Напоминаний пока нет").foregroundStyle(.secondary) }
                        ForEach(state.reminders) { reminder in
                            NavigationLink(value: EventRoute(id: reminder.eventId, year: reminder.year, viewYear: state.year, targetTask: reminder.taskId)) {
                                VStack(alignment: .leading) {
                                    Text(reminder.title)
                                    Text(GPDate.label(reminder.date)).foregroundStyle(.secondary).font(.subheadline)
                                }
                            }
                        }
                    }
                }
            }
        }.navigationTitle("Мои задачи").refreshable { await model.refresh() }
        .navigationDestination(for: EventRoute.self) { route in DetailView(id: route.id, year: route.year, viewYear: route.viewYear, targetTask: route.targetTask) }
        .task(id: feedID) {
            loadedReminders = []
            pageIssue = nil
            guard isActive, let state = model.state, state.agendaIndex, state.reminderCount > 0 else { return }
            await loadMore(expectedRevision: model.contentRevision)
        }
    }

    private func loadMore(expectedRevision: UInt? = nil) async {
        guard isActive, !loadingPage, let state = model.state, state.agendaIndex,
              loadedReminders.count < state.reminderCount else { return }
        let revision = model.contentRevision
        guard expectedRevision == nil || expectedRevision == revision else { return }
        loadingPage = true
        defer { loadingPage = false }
        pageIssue = nil
        do {
            let page = try await model.reminderPage(offset: loadedReminders.count)
            guard isActive, !Task.isCancelled, revision == model.contentRevision,
                  model.state?.year == state.year else { return }
            loadedReminders.append(contentsOf: page)
        } catch {
            guard isActive, !Task.isCancelled, revision == model.contentRevision else { return }
            pageIssue = error.localizedDescription
            await model.refresh(silent: true)
        }
    }
}
extension GPEvent { var selfKey: String { "\(id):\(year)" } }
struct DetailView: View {
    @EnvironmentObject var model: CalendarModel
    @Environment(\.dismiss) var dismiss
    let id: String
    let year: Int
    let viewYear: Int
    var targetTask: String? = nil
    @State private var revealedTask: String?
    @AccessibilityFocusState private var focusedTask: String?
    @State private var event: GPEvent?
    @State private var note = ""
    @State private var base = ""
    @State private var edit = false
    @State private var remove = false
    @State private var loaded = false
    @State private var loadingCard = false
    @State private var pendingReload = false
    @State private var cardToken: String?
    @State private var cardViewYear: Int?
    @State private var loadedRevision: UInt?
    private var availableTarget: String? {
        guard let targetTask, event?.checklist.contains(where: { $0.id == targetTask }) == true else { return nil }
        return targetTask
    }

    var body: some View {
        ScrollViewReader { scroll in
            detailList.task(id: availableTarget) {
                guard let target = availableTarget, revealedTask != target else { return }
                await Task.yield()
                guard !Task.isCancelled else { return }
                revealedTask = target
                scroll.scrollTo(target, anchor: .center)
                focusedTask = target
            }
        }
    }

    private var detailList: some View {
        List { detailContent }
        .navigationTitle(event?.title ?? "Событие").navigationBarTitleDisplayMode(.inline)
        .toolbar { if event != nil { Button("Править") { edit = true } } }
        .sheet(isPresented: $edit, onDismiss: { Task { await reload() } }) {
            if let event { NavigationStack { EventEditor(event: event, year: year, viewYear: cardViewYear ?? viewYear, baseToken: cardToken) } }
        }
        .confirmationDialog("Удалить событие?", isPresented: $remove, titleVisibility: .visible) {
            if let event, event.yearly { Button("Только за этот год", role: .destructive) { delete("year") }; Button("Начиная с этого года", role: .destructive) { delete("future") } }
            else { Button("Удалить", role: .destructive) { delete("future") } }
            Button("Отмена", role: .cancel) {}
        }
        .task(id: "\(id):\(year):\(viewYear)") { await reload() }.refreshable { await reload() }
        .onReceive(model.$state) { state in
            guard let state else {
                event = nil
                cardToken = nil
                cardViewYear = nil
                loadedRevision = nil
                note = ""
                base = ""
                loaded = true
                return
            }
            if state.agendaIndex, state.year == viewYear {
                // A sync stamp changes the token but not the content. Keep the visible card and
                // renew it only after a content revision or the next explicit checkbox action.
                if event == nil || (loadedRevision != nil && loadedRevision != model.contentRevision) {
                    Task { await reload() }
                }
                return
            }
            let matches: (GPEvent) -> Bool = { $0.id == id && $0.year == year }
            if let current = state.events.first(where: matches) ?? state.tasks.first(where: matches) {
                update(current)
                cardToken = state.stateToken
                cardViewYear = state.year
                loadedRevision = model.contentRevision
            } else if state.year == viewYear || state.year == year {
                event = nil
            }
        }
    }
    @ViewBuilder private var detailContent: some View {
        if let event {
            eventSummary(event)
            if !event.checklist.isEmpty {
                Section("Чек-лист") {
                    ForEach(event.checklist) { task in checklistRow(task, in: event) }
                }
            }
            journalSection
            Section { Button("Удалить событие…", role: .destructive) { remove = true } }
        } else if loaded {
            Text("Событие недоступно в выбранном году.")
        } else {
            ProgressView()
        }
    }

    private func eventSummary(_ event: GPEvent) -> some View {
        Section {
            LabeledContent("Дата", value: GPDate.label(event.date))
            LabeledContent("Начало подготовки", value: GPDate.label(event.preparationDate))
            if let owner = model.state?.ownerName(event.employeeId) {
                LabeledContent("Ответственный", value: owner)
            }
            if event.yearly { Label("Ежегодно", systemImage: "repeat") }
            if !event.description.isEmpty { Text(event.description) }
        }
    }

    private var journalSection: some View {
        Section("Дневник за \(String(year)) год") {
            TextField("Заметка", text: $note, axis: .vertical).lineLimit(3...12)
            Button("Сохранить заметку") {
                Task {
                    let submitted = note
                    var fields = viewFields()
                    fields["eventId"] = .string(id)
                    fields["year"] = .integer(year)
                    fields["text"] = .string(submitted)
                    fields["base"] = .string(base)
                    if await model.run("note", fields) {
                        base = submitted
                        await reload()
                    }
                }
            }.disabled(model.busy || note == base)
        }
    }

    private func checklistRow(_ task: GPTask, in event: GPEvent) -> some View {
        Button { Task { await toggle(task, in: event) } } label: {
            HStack {
                Image(systemName: task.done ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(Color.accentColor)
                VStack(alignment: .leading) {
                    Text(task.title).foregroundStyle(.primary)
                    Text(GPDate.label(task.dueDate)).font(.caption).foregroundStyle(.secondary)
                }
            }
        }
        .disabled(model.busy)
        .accessibilityValue(task.done ? "Выполнено" : "Открыто")
        .accessibilityFocused($focusedTask, equals: task.id)
        .id(task.id)
    }

    private func toggle(_ task: GPTask, in event: GPEvent) async {
        let changed = await model.check(event: event, task: task,
                                        viewYear: cardViewYear ?? viewYear, cardToken: cardToken)
        if model.state?.agendaIndex == true,
           changed || model.state?.stateToken != cardToken || model.contentRevision != loadedRevision {
            await reload()
        } else if changed && model.state?.year != year {
            await reload()
        }
    }

    private func update(_ next: GPEvent) {
        if !loaded || note == base { note = next.note; base = next.note }
        event = next
        loaded = true
    }

    private func reload() async {
        guard !loadingCard else { pendingReload = true; return }
        loadingCard = true
        defer {
            loadingCard = false
            if pendingReload {
                pendingReload = false
                Task { await reload() }
            }
        }
        do {
            if let current = model.state, current.year == viewYear, current.agendaIndex {
                let revision = model.contentRevision
                let reply = try await model.agendaCard(id: id, viewYear: viewYear, eventYear: year)
                guard let next = reply.event(id: id, eventYear: year, in: current),
                      model.state?.stateToken == reply.baseToken, revision == model.contentRevision else { return }
                update(next)
                cardToken = reply.baseToken
                cardViewYear = viewYear
                loadedRevision = revision
                return
            }

            if let current = model.state, !current.agendaIndex, current.year == viewYear,
               let row = current.events.first(where: { $0.id == id && $0.year == year }) ?? current.tasks.first(where: { $0.id == id && $0.year == year }) {
                update(row)
                cardToken = current.stateToken
                cardViewYear = current.year
                loadedRevision = model.contentRevision
                return
            }

            let snapshot = try await model.fullSnapshot(year: year)
            guard !snapshot.agendaIndex,
                  let row = snapshot.events.first(where: { $0.id == id && $0.year == year }) ?? snapshot.tasks.first(where: { $0.id == id && $0.year == year }) else {
                event = nil
                loaded = true
                return
            }
            update(row)
            cardToken = snapshot.stateToken
            cardViewYear = snapshot.year
            loadedRevision = model.contentRevision
        } catch {
            model.failure = error.localizedDescription
            loaded = true
            if model.state?.agendaIndex == true { await model.refresh(silent: true) }
        }
    }

    private func viewFields() -> GPCommand {
        guard let cardToken else { return [:] }
        return ["viewYear": .integer(cardViewYear ?? viewYear), "baseToken": .string(cardToken)]
    }

    private func delete(_ scope: String) {
        guard let event else { return }
        var fields = viewFields()
        fields["eventId"] = .string(id); fields["year"] = .integer(year)
        fields["revision"] = .string(event.revision); fields["scope"] = .string(scope)
        Task { if await model.run("deleteEvent", fields) { dismiss() } }
    }
}
