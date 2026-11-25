import Combine
import SwiftUI
import UIKit

@MainActor final class CalendarModel: ObservableObject {
    @Published var state: GPState?
    @Published var year = GPDate.year
    @Published var busy = false
    @Published var failure: String?
    private var refreshVersion: UInt = 0
    let core = CalendarClient.shared

    private enum Update { case full, stamp, unchanged }

    func snapshot(year: Int) async throws -> GPState {
        do { return try await read(year: year).0 }
        catch { await refresh(silent: true); throw error }
    }

    private func read(year: Int) async throws -> (GPState, Update) {
        let full: GPCommand = ["op": .string("state"), "year": .integer(year)]
        if let prior = state, prior.year == year, let token = prior.stateToken {
            var request = full
            request["ifToken"] = .string(token)
            if prior.supportsSyncStamp == true { request["acceptSyncStamp"] = .flag(true) }
            do {
                let reply = try await core.value(request, as: GPSnapshot.self)
                switch reply {
                case .state(let current) where current.year == year: return (current, .full)
                case .unchanged where state?.year == year && state?.stateToken == token:
                    return (prior, .unchanged)
                case .stamp(let stamp) where prior.supportsSyncStamp == true && state?.year == year && state?.stateToken == token:
                    if let next = stamp.applying(to: prior, year: year, token: token) { return (next, .stamp) }
                default: break
                }
            } catch is DecodingError {
                // Recover from an incompatible conditional response with one full read.
            }
        }
        guard case .state(let current) = try await core.value(full, as: GPSnapshot.self),
              current.year == year else { throw GPError("Не удалось обновить календарь.") }
        return (current, .full)
    }

    func refresh(silent: Bool = false) async {
        refreshVersion &+= 1
        let version = refreshVersion
        let requestedYear = year
        do {
            let (current, update) = try await read(year: requestedYear)
            guard !Task.isCancelled && requestedYear == year && version == refreshVersion else { return }
            switch update {
            case .full:
                state = current
                GPReminderSchedule.refresh(dates: current.notificationDates)
            case .stamp:
                state = current
                if !silent { GPReminderSchedule.refresh(dates: current.notificationDates) }
            case .unchanged:
                if !silent { GPReminderSchedule.refresh(dates: current.notificationDates) }
            }
        } catch {
            guard !Task.isCancelled && version == refreshVersion && requestedYear == year else { return }
            let wasShowingCalendar = state != nil
            state = nil
            GPReminderSchedule.refresh(dates: [])
            if !silent || wasShowingCalendar { failure = error.localizedDescription }
        }
    }
    var needsPeerPolling: Bool { state?.lanEnabled != false }

    func pollWhileNeeded() async {
        while needsPeerPolling && !Task.isCancelled {
            do { try await Task.sleep(nanoseconds: 4_000_000_000) } catch { return }
            if needsPeerPolling && !busy { await refresh(silent: true) }
        }
    }
    func check(event: GPEvent, task: GPTask) async -> Bool {
        guard !busy else { return false }
        guard let prior = state else {
            await refresh(silent: true)
            return false
        }
        guard year == event.year, prior.year == year,
              prior.canUseCheckPatch else {
            return await run("check", ["eventId": .string(event.id), "year": .integer(event.year),
                                       "taskId": .string(task.id), "done": .flag(!task.done)])
        }
        guard let context = GPCheckContext(prior: prior, displayed: event, task: task) else {
            failure = "Карточка изменилась. Проверьте актуальный чек-лист."
            await refresh(silent: true)
            return false
        }
        busy = true
        defer { busy = false }
        refreshVersion &+= 1
        let version = refreshVersion
        do {
            let next = try await core.check(prior, context: context)
            guard !Task.isCancelled else { return false }
            guard refreshVersion == version, year == context.year, state?.stateToken == context.token else {
                await refresh(silent: true)
                return true
            }
            state = next
            GPReminderSchedule.refresh(dates: next.notificationDates)
            return true
        } catch {
            failure = error.localizedDescription
            await refresh(silent: true)
            return false
        }
    }

    func run(_ op: String, _ fields: GPCommand = [:]) async -> Bool {
        guard !busy else { return false }
        busy = true
        defer { busy = false }
        var command = fields; command["op"] = .string(op)
        do { try await core.run(command); await refresh(); return true }
        catch { failure = error.localizedDescription; await refresh(silent: true); return false }
    }
    func copy(_ op: String, fields: GPCommand = [:]) async -> Bool {
        guard !busy else { return false }
        busy = true; defer { busy = false }
        var command = fields; command["op"] = .string(op)
        do {
            let text = try await core.value(command, as: String.self)
            UIPasteboard.general.setItems([["public.utf8-plain-text": text]], options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(15 * 60)])
            await refresh(silent: true)
            return true
        } catch { failure = error.localizedDescription; await refresh(silent: true); return false }
    }
}

@main struct GodovoyPlanApp: App {
    @StateObject private var model = CalendarModel()
    @Environment(\.scenePhase) private var phase
    @State private var observedZone: String?
    @State private var zoneCheck: UUID?
    private var shouldPoll: Bool { phase == .active && model.needsPeerPolling }
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
                let zone = TimeZone.autoupdatingCurrent.identifier
                if observedZone != zone {
                    observedZone = zone
                    zoneCheck = UUID()
                }
                do { try await model.core.lifecycle(foreground: true) } catch { model.failure = error.localizedDescription }
                await model.refresh()
            }
            .onReceive(NotificationCenter.default.publisher(for: UIApplication.significantTimeChangeNotification)
                .merge(with: NotificationCenter.default.publisher(for: .NSSystemTimeZoneDidChange))) { notification in
                let zone = TimeZone.autoupdatingCurrent.identifier
                if notification.name == .NSSystemTimeZoneDidChange || zone != observedZone {
                    observedZone = zone
                    zoneCheck = UUID()
                }
                if phase == .active { Task { await model.refresh() } }
            }
            .task(id: phase == .active ? zoneCheck : nil) {
                guard phase == .active, let expected = zoneCheck else { return }
                // Chrono may reuse the old zone for one second after a rapid change.
                do { try await Task.sleep(nanoseconds: 1_200_000_000) } catch { return }
                guard !Task.isCancelled && zoneCheck == expected else { return }
                await model.refresh(silent: true)
                if !Task.isCancelled && zoneCheck == expected { zoneCheck = nil }
            }
            .task(id: shouldPoll) {
                if shouldPoll { await model.pollWhileNeeded() }
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
