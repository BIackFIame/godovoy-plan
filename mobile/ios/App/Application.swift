import SwiftUI
import UIKit

@MainActor final class CalendarModel: ObservableObject {
    @Published var state: GPState?
    @Published var year = GPDate.year
    @Published var busy = false
    @Published var failure: String?
    private var refreshVersion: UInt = 0
    let core = CalendarClient.shared

    func snapshot(year: Int) async throws -> GPState {
        try await core.value(["op": .string("state"), "year": .integer(year)], as: GPState.self)
    }
    func refresh(silent: Bool = false) async {
        refreshVersion &+= 1
        let version = refreshVersion
        let requestedYear = year
        do {
            var fields: GPCommand = ["op": .string("state"), "year": .integer(requestedYear)]
            if silent, let prior = state, prior.year == year, let token = prior.stateToken {
                fields["ifToken"] = .string(token)
            }
            let result = try await core.value(fields, as: GPSnapshot.self)
            guard requestedYear == year && version == refreshVersion else { return }
            switch result {
            case .state(let current):
                state = current
                GPReminderSchedule.refresh(dates: current.notificationDates)
            case .unchanged:
                break
            }
        } catch {
            guard version == refreshVersion && requestedYear == year else { return }
            let wasShowingCalendar = state != nil
            state = nil
            GPReminderSchedule.refresh(dates: [])
            if !silent || wasShowingCalendar { failure = error.localizedDescription }
        }
    }
    func run(_ op: String, _ fields: GPCommand = [:]) async -> Bool {
        guard !busy else { return false }
        busy = true
        defer { busy = false }
        var command = fields; command["op"] = .string(op)
        do { try await core.run(command); await refresh(); return true }
        catch { failure = error.localizedDescription; return false }
    }
    func copy(_ op: String, fields: GPCommand = [:]) async -> Bool {
        guard !busy else { return false }
        busy = true; defer { busy = false }
        var command = fields; command["op"] = .string(op)
        do {
            let text = try await core.value(command, as: String.self)
            UIPasteboard.general.setItems([["public.utf8-plain-text": text]], options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(15 * 60)])
            return true
        } catch { failure = error.localizedDescription; return false }
    }
}

@main struct GodovoyPlanApp: App {
    @StateObject private var model = CalendarModel()
    @Environment(\.scenePhase) private var phase
    var body: some Scene {
        WindowGroup {
            ZStack {
                Group {
                    if let state = model.state {
                        if state.workspaceName == nil { NavigationStack { WelcomeView() } }
                        else {
                            TabView {
                                NavigationStack { CalendarView() }.tabItem { Label("Календарь", systemImage: "calendar") }
                                NavigationStack { TasksView() }.tabItem { Label("Мои задачи", systemImage: "checklist") }
                                NavigationStack { SyncView() }.tabItem { Label("Обмен", systemImage: "arrow.triangle.2.circlepath") }
                            }
                        }
                    } else {
                        VStack(spacing: 16) {
                            Text("Годовой план").font(.largeTitle.bold())
                            if model.failure == nil { ProgressView() }
                            Button("Открыть календарь") { Task { await model.refresh() } }
                        }
                    }
                }.environmentObject(model)
                if phase != .active { Color(uiColor: .systemBackground).ignoresSafeArea(); Image(systemName: "calendar").font(.largeTitle).foregroundStyle(.secondary).accessibilityHidden(true) }
            }
            .environment(\.locale, Locale(identifier: "ru_RU"))
            .alert("Годовой план", isPresented: Binding(get: { model.failure != nil }, set: { if !$0 { model.failure = nil } })) { Button("Закрыть", role: .cancel) {} } message: { Text(model.failure ?? "") }
            .task(id: phase) {
                if phase == .background { try? await model.core.lifecycle(foreground: false); return }
                guard phase == .active else { return }
                do { try await model.core.lifecycle(foreground: true) } catch { model.failure = error.localizedDescription }
                await model.refresh()
                while !Task.isCancelled {
                    do { try await Task.sleep(nanoseconds: 4_000_000_000) } catch { return }
                    if !model.busy { await model.refresh(silent: true) }
                }
            }
        }
    }
}

struct WelcomeView: View {
    @EnvironmentObject var model: CalendarModel
    @State private var joining = false
    @State private var name = ""
    @State private var team = ""
    @State private var code = ""
    @State private var address = ""
    @State private var copied = false
    var body: some View {
        Form {
            Section { Picker("Действие", selection: $joining) { Text("Создать").tag(false); Text("Присоединиться").tag(true) }.pickerStyle(.segmented) }
            Section {
                TextField("Ваше имя", text: $name).textContentType(.name).accessibilityIdentifier("employee")
                if !joining { TextField("Название команды", text: $team).accessibilityIdentifier("workspace") }
            }
            if joining {
                Section {
                    Button(copied ? "Запрос скопирован" : "Скопировать запрос устройства") { Task { copied = await model.copy("pairingRequest") } }
                    SecureField("Код от коллеги", text: $code).textInputAutocapitalization(.never).autocorrectionDisabled()
                    AddressField(address: $address)
                } header: { Text("Приглашение") } footer: { Text("Отправьте запрос коллеге и оставьте приложение открытым. Затем вставьте код, выданный для этого устройства.") }
            }
            Section {
                Button(joining ? "Подключиться" : "Создать календарь") {
                    Task { if joining { _ = await model.run("join", ["code": .string(code), "employee": .string(name), "address": address.jsonAddress]) }
                        else { _ = await model.run("createWorkspace", ["name": .string(team), "employee": .string(name)]) } }
                }.disabled(model.busy || name.isBlank || (joining ? code.isBlank : team.isBlank)).accessibilityIdentifier("welcomeSubmit")
            }
            Section { NavigationLink("Серверный календарь") { HostedCalendarEntry() } }
        }.navigationTitle("Годовой план")
    }
}

extension String {
    var isBlank: Bool { trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
    var jsonAddress: JSONValue { isBlank ? .null : .string(trimmingCharacters(in: .whitespacesAndNewlines)) }
}
struct AddressField: View {
    @Binding var address: String
    var body: some View { DisclosureGroup("Указать адрес вручную") { TextField("IPv4 и порт", text: $address).textInputAutocapitalization(.never).keyboardType(.numbersAndPunctuation).autocorrectionDisabled() } }
}

struct HostedCalendarEntry: View {
    @Environment(\.openURL) private var openURL
    @State private var address = ""
    @State private var error = ""
    var body: some View {
        Form {
            Section {
                TextField("https://calendar.example.org", text: $address)
                    .keyboardType(.URL).textInputAutocapitalization(.never).autocorrectionDisabled()
                    .accessibilityLabel("Адрес от администратора")
                Button("Открыть в браузере") {
                    let text = address.trimmingCharacters(in: .whitespacesAndNewlines)
                    guard text.utf8.count <= 300,
                          Array(text.utf8).withUnsafeBufferPointer({ gp_valid_hosted_address($0.baseAddress, $0.count) != 0 }),
                          let url = URL(string: text) else {
                        error = "Нужен HTTPS-адрес без пути, кода и параметров. Для туннеля используйте http://127.0.0.1:45838."
                        return
                    }
                    openURL(url) { accepted in
                        if accepted { address = ""; error = "" }
                        else { error = "Не удалось открыть браузер." }
                    }
                }.disabled(address.isBlank)
                if !error.isEmpty { Text(error).foregroundStyle(.red) }
            } header: { Text("Адрес от администратора") } footer: {
                Text("Отдельный календарь компании работает в браузере, пока доступен сервер. События с этого устройства автоматически не переносятся.")
            }
        }.navigationTitle("Серверный календарь").navigationBarTitleDisplayMode(.inline)
    }
}
