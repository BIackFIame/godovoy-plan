import XCTest

final class CalendarUITests: XCTestCase {
    func testCreateCalendarAndPersistEvent() {
        let app = XCUIApplication()
        app.launchEnvironment["GP_UI_TEST_PROFILE"] = UUID().uuidString
        app.launchArguments = ["-AppleLanguages", "(ru)", "-AppleLocale", "ru_RU"]
        app.launch()
        addUIInterruptionMonitor(withDescription: "Local network access") { alert in
            let allow = alert.buttons.matching(NSPredicate(format: "label IN %@", ["Разрешить", "Allow", "OK"])).first
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
        let entry = app.buttons.containing(.staticText, identifier: "Team review").first
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
