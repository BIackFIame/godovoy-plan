import XCTest

final class CalendarUITests: XCTestCase {
    func testCreateCalendarAndPersistEvent() {
        let app = XCUIApplication()
        app.launchEnvironment["GP_UI_TEST_PROFILE"] = UUID().uuidString
        app.launchArguments = ["-AppleLanguages", "(ru)", "-AppleLocale", "ru_RU"]
        app.launch()
        addUIInterruptionMonitor(withDescription: "Local network access") { alert in
            let allow = alert.buttons.matching(NSPredicate(format: "label IN %@", ["Разрешить", "Allow", "OK"])).firstMatch
            if allow.exists { allow.tap(); return true }
            return false
        }
        let employee = app.textFields["employee"]
        XCTAssertTrue(employee.waitForExistence(timeout: 12))
        employee.tap()
        employee.typeText("Maria")
        app.textFields["workspace"].tap()
        app.textFields["workspace"].typeText("Team")
        app.buttons["welcomeSubmit"].tap()
        let add = app.buttons["newEvent"]
        XCTAssertTrue(add.waitForExistence(timeout: 15))
        add.tap()
        let title = app.textFields["eventTitle"]
        XCTAssertTrue(title.waitForExistence(timeout: 5))
        title.tap()
        title.typeText("Team review")
        app.buttons["eventSave"].tap()
        let entry = app.buttons.containing(.staticText, identifier: "Team review").firstMatch
        if !entry.isHittable { app.swipeUp() }
        XCTAssertTrue(entry.waitForExistence(timeout: 5))
        app.terminate()
        app.launch()
        XCTAssertTrue(app.buttons["newEvent"].waitForExistence(timeout: 15))
        if !entry.isHittable { app.swipeUp() }
        entry.tap()
        XCTAssertTrue(app.navigationBars["Team review"].waitForExistence(timeout: 5))
    }
}

extension CalendarUITests {
    @objc func testFocusDeepChecklistAndPersist() throws {
        guard let id = ProcessInfo.processInfo.environment["GP_FOCUS_TEST_PROFILE"] else {
            throw XCTSkip("Для этого сценария нужен отдельный профиль с длинным чек-листом.")
        }
        XCTAssertNotNil(UUID(uuidString: id))
        let app = XCUIApplication()
        app.launchEnvironment["GP_UI_TEST_PROFILE"] = id
        app.launchArguments = ["-AppleLanguages", "(ru)", "-AppleLocale", "ru_RU"]
        let event = "Partner briefing"
        let target = "Partner packet 35"
        let other = "Partner packet 14"

        func waitFor(_ message: String, _ predicate: @escaping () -> Bool) throws {
            let expectation = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in predicate() }, object: nil)
            if XCTWaiter.wait(for: [expectation], timeout: 12) != .completed {
                print(app.debugDescription)
                let screenshot = XCTAttachment(screenshot: app.screenshot())
                screenshot.name = message
                screenshot.lifetime = .keepAlways
                add(screenshot)
                XCTFail(message)
                throw NSError(domain: "FocusUITest", code: 1, userInfo: [NSLocalizedDescriptionKey: message])
            }
        }
        func openMyTasks() throws {
            let tab = app.tabBars.buttons["Мои задачи"]
            try waitFor("Доступна вкладка Мои задачи") { tab.exists && tab.isHittable }
            tab.tap()
        }
        func preparation(_ name: String) -> XCUIElement {
            app.buttons.matching(NSPredicate(format: "label CONTAINS %@ AND label CONTAINS %@ AND label CONTAINS %@", name, event, "Срок")).firstMatch
        }
        func reminder(_ name: String) -> XCUIElement {
            app.buttons.matching(NSPredicate(format: "label CONTAINS %@", "\(event) · \(name)")).firstMatch
        }
        func detailRow(_ name: String) -> XCUIElement {
            app.buttons.matching(NSPredicate(format: "label CONTAINS %@ AND value IN %@", name, ["Открыто", "Выполнено"])).firstMatch
        }
        func visible(_ row: XCUIElement) -> Bool {
            guard row.exists, row.isHittable else { return false }
            let bar = app.navigationBars[event]
            let tabbar = app.tabBars.firstMatch
            let top = bar.exists ? bar.frame.maxY : app.frame.minY
            let bottom = tabbar.exists ? tabbar.frame.minY : app.frame.maxY
            return row.frame.midY > top && row.frame.midY < bottom && app.frame.contains(CGPoint(x: row.frame.midX, y: row.frame.midY))
        }
        func openAndExpect(_ link: XCUIElement, _ name: String) throws {
            try waitFor("Виден вход к \(name)") { link.exists && link.isHittable }
            link.tap()
            let bar = app.navigationBars[event]
            let task = detailRow(name)
            try waitFor("Прямой переход показывает \(name) без свайпа") {
                bar.exists && visible(task) && task.value as? String == "Открыто"
            }
        }
        func goBack() throws {
            let back = app.navigationBars[event].buttons.element(boundBy: 0)
            try waitFor("Доступен возврат из события") { back.exists && back.isHittable }
            back.tap()
        }
        app.launch()
        try openMyTasks()
        try openAndExpect(preparation(target), target)
        try goBack()
        // The reminder points to a different row from the next open task.
        try openAndExpect(reminder(other), other)
        try goBack()
        try openAndExpect(reminder(target), target)
        let row35 = detailRow(target)
        row35.tap()
        try waitFor("Пункт 35 отмечен") { row35.exists && row35.value as? String == "Выполнено" }

        app.terminate()
        app.launch()
        try openMyTasks()
        // After the relaunch item 14 has become the next open task.
        try openAndExpect(preparation(other), other)
        // Reading the saved item is the only step that deliberately scrolls.
        for _ in 0..<12 {
            if visible(row35) { break }
            let bottom = app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.72))
            let middle = app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.40))
            bottom.press(forDuration: 0.1, thenDragTo: middle, withVelocity: .slow, thenHoldForDuration: 0.15)
        }
        try waitFor("После перезапуска у пункта 35 сохранена отметка") {
            visible(row35) && row35.value as? String == "Выполнено"
        }
    }
}
