import XCTest

final class PhoneFooterUITests: XCTestCase {
    func testFooterShowsInstalledVersionAndBuild() throws {
        continueAfterFailure = false
        let environment = ProcessInfo.processInfo.environment
        let bundleID = try XCTUnwrap(environment["VISOR_APP_ID"])
        let expected = try XCTUnwrap(environment["VISOR_EXPECTED_FOOTER"])
        let app = XCUIApplication(bundleIdentifier: bundleID)
        app.launch()

        let window = app.windows.firstMatch
        let webView = app.webViews.firstMatch
        XCTAssertTrue(webView.waitForExistence(timeout: 30), "The installed app must open its WebView")
        let footer = webView.staticTexts[expected].firstMatch
        for _ in 0..<4 {
            if footer.waitForExistence(timeout: 3),
               !footer.frame.isEmpty, window.frame.contains(footer.frame) {
                XCTAssertEqual(footer.label, expected)
                return
            }
            webView.swipeUp()
        }
        XCTFail("The installed app did not show the footer \(expected)")
    }
}
