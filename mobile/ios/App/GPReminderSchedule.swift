import Foundation
import UserNotifications

@MainActor enum GPReminderSchedule {
    private static let prefix = "gp-date-"
    private static var dates = Set<String>()
    private static var version: UInt = 0
    private static var updating = false
    private static var lastCheck = Date.distantPast

    static func requestPermission(completion: @escaping (Bool) -> Void) {
        Task {
            let granted = (try? await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound])) ?? false
            lastCheck = .distantPast
            refresh(dates: Array(dates))
            completion(granted)
        }
    }

    static func refresh(dates newDates: [String]) {
        let next = Set(newDates.sorted().prefix(60))
        if next == dates && Date().timeIntervalSince(lastCheck) < 60 { return }
        dates = next
        lastCheck = Date()
        version &+= 1
        guard !updating else { return }
        updating = true
        Task {
            let center = UNUserNotificationCenter.current()
            while true {
                let latest = version
                let desired = dates
                let settings = await center.notificationSettings()
                let authorized = [.authorized, .provisional].contains(settings.authorizationStatus)
                var requests = [String: UNNotificationRequest]()
                if authorized {
                    for date in desired {
                        guard var parts = GPDate.components(date) else { continue }
                        parts.calendar = GPDate.gregorian
                        parts.hour = 9
                        parts.minute = 0
                        let trigger = UNCalendarNotificationTrigger(dateMatching: parts, repeats: false)
                        guard let next = trigger.nextTriggerDate(), next > Date() else { continue }
                        let content = UNMutableNotificationContent()
                        content.title = "Годовой план"
                        content.body = "Откройте календарь, чтобы проверить свои задачи."
                        content.sound = .default
                        let id = prefix + date
                        requests[id] = UNNotificationRequest(identifier: id, content: content, trigger: trigger)
                    }
                }
                let pending = Set(await center.pendingNotificationRequests().map(\.identifier).filter { $0.hasPrefix(prefix) })
                center.removePendingNotificationRequests(withIdentifiers: Array(pending.subtracting(requests.keys)))
                for (id, request) in requests where !pending.contains(id) { try? await center.add(request) }
                if latest == version { break }
            }
            updating = false
        }
    }
}
