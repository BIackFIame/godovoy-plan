import SwiftUI
import UniformTypeIdentifiers
import UIKit

private extension UTType { static let planExchange = UTType(exportedAs: "local.godovoyplan.exchange", conformingTo: .data) }
private struct ExchangeDocument: FileDocument {
    static var readableContentTypes: [UTType] { [.planExchange] }
    let data: Data
    init(data: Data) { self.data = data }
    init(configuration: ReadConfiguration) throws { guard let data = configuration.file.regularFileContents else { throw GPError("Файл обмена недоступен.") }; self.data = data }
    func fileWrapper(configuration: WriteConfiguration) throws -> FileWrapper { FileWrapper(regularFileWithContents: data) }
}
private struct SharedFile: Identifiable {
    let url: URL
    var id: URL { url }
}
private struct FileSharing: UIViewControllerRepresentable {
    let url: URL
    let finish: () -> Void
    func makeUIViewController(context: Context) -> UIActivityViewController {
        let controller = UIActivityViewController(activityItems: [url], applicationActivities: nil)
        controller.completionWithItemsHandler = { _, _, _, _ in DispatchQueue.main.async { finish() } }
        return controller
    }
    func updateUIViewController(_ controller: UIActivityViewController, context: Context) {}
}
struct SyncView: View {
    @EnvironmentObject var model: CalendarModel
    @State private var peer = ""
    @State private var request = ""
    @State private var newCode = ""
    @State private var direct = ""
    @State private var selected: Set<String> = []
    @State private var confirm = false
    @State private var exported: ExchangeDocument?
    @State private var sharing: SharedFile?
    @State private var sharedURL: URL?
    @State private var showingExport = false
    @State private var showingImport = false
    @State private var workingFile = false
    @State private var message = ""
    var body: some View {
        Form {
            if let state = model.state {
                Section { NavigationLink("Серверный календарь") { HostedCalendarEntry() } }
                Section("Локальная сеть") {
                    LabeledContent("Команда", value: state.workspaceName ?? "")
                    if let address = state.address { LabeledContent("Этот адрес", value: address).textSelection(.enabled) }
                    if let stamp = state.lastSync, let date = ISO8601DateFormatter().date(from: stamp) {
                        LabeledContent("Последний обмен") { Text(date, format: .dateTime.day().month().hour().minute()) }
                    }
                    if let error = state.networkError { Text(error).foregroundStyle(.red) }
                    if state.lanEnabled == false {
                        Button("Включить обмен по локальной сети") { Task { _ = await model.run("pause", ["paused": .flag(false)]) } }.disabled(model.busy)
                    } else {
                        Toggle("Приостановить обмен", isOn: Binding(get: { state.paused }, set: { paused in Task { _ = await model.run("pause", ["paused": .flag(paused)]) } })).disabled(model.busy)
                    }
                    DisclosureGroup("Подключить по адресу") {
                        TextField("IPv4 и порт коллеги", text: $peer).textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.numbersAndPunctuation)
                        Button("Сохранить адрес") { Task { if await model.run("peer", ["address": .string(peer)]) { peer = ""; message = "Адрес сохранён" } } }.disabled(peer.isBlank || model.busy || state.paused)
                    }
                }
                if state.canManage {
                    Section("Пригласить коллегу") {
                        TextField("Запрос устройства коллеги", text: $request).textInputAutocapitalization(.never).autocorrectionDisabled()
                        Button("Скопировать приглашение") { Task { if await model.copy("invite", fields: ["request": .string(request)]) { request = ""; message = "Код скопирован. Передайте его коллеге в течение 15 минут." } } }.disabled(model.busy || request.isBlank || state.paused)
                    }
                }
                Section("Обмен через файл") {
                    Button("Импортировать .yrplan") { showingImport = true }.disabled(workingFile || model.busy)
                    Button("Поделиться файлом…") { Task { await sharePlan() } }.disabled(workingFile || model.busy)
                    Button("Сохранить .yrplan в Файлы") { Task { await exportPlan() } }.disabled(workingFile || model.busy)
                }
                Section("Уведомления") { Button("Разрешить напоминания на устройстве") { GPReminderSchedule.requestPermission { allowed in if allowed { GPReminderSchedule.refresh(dates: state.notificationDates) }; message = allowed ? "Уведомления разрешены" : "Проверьте уведомления «Годового плана» в настройках устройства." } } }
                Section("Устройства команды") {
                    ForEach(state.members) { m in
                        HStack {
                            VStack(alignment: .leading) {
                                Text(m.name)
                                Text(m.id).font(.caption2.monospaced()).foregroundStyle(.secondary).textSelection(.enabled)
                                if m.id != m.employeeId, let person = state.ownerName(m.employeeId) {
                                    Text("Назначения: \(person)").font(.caption).foregroundStyle(.secondary)
                                }
                            }
                            Spacer()
                            if m.id == state.deviceId { Text("Вы").foregroundStyle(.secondary) }
                            else if m.revoked { Text("Отозвано").foregroundStyle(.secondary) }
                        }
                    }
                    if state.canManage {
                        NavigationLink("Связать устройства сотрудника") { deviceLinks }
                        NavigationLink("Управлять доступом") { access(state) }
                    }
                    else if let manager = state.ownerName(state.accessManagerId) { LabeledContent("Управляющее устройство", value: manager) }
                }
                Section {
                    DisclosureGroup("Получить новый ключ команды") {
                        Button("Скопировать запрос устройства") { Task { if await model.copy("pairingRequest") { message = "Передайте запрос управляющему устройству" } } }
                        SecureField("Новое приглашение", text: $newCode).textInputAutocapitalization(.never).autocorrectionDisabled()
                        AddressField(address: $direct)
                        Button("Подключиться по новому коду") { Task { if await model.run("rejoin", ["code": .string(newCode), "address": direct.jsonAddress]) { newCode = ""; message = "Новый ключ принят" } } }.disabled(newCode.isBlank || model.busy)
                    }
                }
                if !message.isEmpty { Text(message).foregroundStyle(.secondary) }
            }
        }
        .navigationTitle("Обмен")
        .sheet(item: $sharing, onDismiss: { if let sharedURL { try? FileManager.default.removeItem(at: sharedURL); self.sharedURL = nil } }) { item in
            FileSharing(url: item.url, finish: { sharing = nil })
        }
        .fileImporter(isPresented: $showingImport, allowedContentTypes: [.planExchange]) { result in
            switch result { case .success(let url): Task { await importPlan(url) }; case .failure(let error): if (error as NSError).code != NSUserCancelledError { model.failure = error.localizedDescription } }
        }
        .fileExporter(isPresented: $showingExport, document: exported, contentType: .planExchange, defaultFilename: "Годовой план.yrplan") { result in
            exported = nil
            switch result { case .success: message = "Файл обмена сохранён"; case .failure(let error): if (error as NSError).code != NSUserCancelledError { model.failure = error.localizedDescription } }
        }
    }
    private var deviceLinks: some View {
        List {
            if let state = model.state, state.canManage {
                Section {
                    ForEach(state.members.filter { !$0.revoked }) { device in
                        NavigationLink {
                            GPDeviceAssignments(deviceID: device.id)
                        } label: {
                            VStack(alignment: .leading) {
                                Text(device.name)
                                Text(device.id).font(.caption2.monospaced()).foregroundStyle(.secondary)
                            }
                        }
                    }
                } header: { Text("Выберите устройство") }
                  footer: { Text("Подключите оба устройства к календарю, затем выберите, чьи задачи и напоминания будут на этом устройстве.") }
            }
        }.navigationTitle("Устройства сотрудника")
    }
    private func access(_ state: GPState) -> some View {
        Form {
            Section { Text("После смены ключа передайте новые приглашения оставшимся коллегам. Старые копии сохранят полученные ранее данные.") }
            Section("Отозвать устройства") { ForEach(state.members.filter { $0.id != state.deviceId && !$0.revoked }) { m in
                Toggle(isOn: Binding(get: { selected.contains(m.id) }, set: { on in if on { selected.insert(m.id) } else { selected.remove(m.id) } })) { VStack(alignment: .leading) { Text(m.name); Text(m.id).font(.caption2.monospaced()) } }
            } }
            Button(selected.isEmpty ? "Перевыпустить ключ…" : "Отозвать выбранные устройства…", role: .destructive) { confirm = true }.disabled(model.busy)
        }
        .navigationTitle("Доступ к календарю").navigationBarTitleDisplayMode(.inline)
        .alert(selected.isEmpty ? "Перевыпустить ключ?" : "Отозвать доступ?", isPresented: $confirm) {
            Button("Отмена", role: .cancel) {}
            Button("Подтвердить", role: .destructive) { Task { let ids = selected.sorted().map(JSONValue.string); if await model.run(ids.isEmpty ? "renew" : "revoke", ids.isEmpty ? [:] : ["deviceIds": .list(ids)]) { selected.removeAll(); message = "Ключ изменён" } } }
        } message: { Text(selected.isEmpty ? "Остальным устройствам потребуются новые приглашения." : state.members.filter { selected.contains($0.id) }.map { "\($0.name) · \($0.id)" }.joined(separator: "\n")) }
    }
    private func sharePlan() async {
        workingFile = true
        defer { workingFile = false }
        do {
            let url = try await model.core.exportURL()
            sharedURL = url
            sharing = SharedFile(url: url)
        } catch { model.failure = error.localizedDescription }
    }
    private func exportPlan() async {
        workingFile = true; defer { workingFile = false }
        do { exported = ExchangeDocument(data: try await model.core.exportFile()); showingExport = true } catch { model.failure = error.localizedDescription }
    }
    private func importPlan(_ url: URL) async {
        workingFile = true; defer { workingFile = false }
        do { try await model.core.importFile(url); await model.refresh(); message = "Обмен завершён" } catch { model.failure = error.localizedDescription }
    }
}

private struct GPDeviceAssignments: View {
    @EnvironmentObject private var model: CalendarModel
    @Environment(\.dismiss) private var dismiss
    let deviceID: String
    @State private var selection: String?

    var body: some View {
        Form {
            if let state = model.state, state.canManage,
               let device = state.members.first(where: { $0.id == deviceID && !$0.revoked }) {
                let chosen = selection ?? device.employeeId
                Section {
                    Text(device.name)
                    Text(device.id).font(.caption.monospaced()).textSelection(.enabled)
                } header: { Text("Устройство") }
                Picker("Чьи задачи показывать?", selection: Binding(get: { chosen }, set: { selection = $0 })) {
                    Text("Устройство отдельно").tag(deviceID)
                    ForEach(state.employees.filter { $0.id != deviceID }) { person in
                        Text(state.employeeLabel(person)).tag(person.id)
                    }
                }
                Button("Применить") {
                    Task {
                        if await model.run("linkEmployeeDevice", ["deviceId": .string(deviceID), "employeeId": .string(chosen)]) { dismiss() }
                    }
                }.disabled(model.busy || chosen == device.employeeId)
            } else { Text("Устройство больше недоступно.") }
        }.navigationTitle("Назначения").navigationBarTitleDisplayMode(.inline)
    }
}
