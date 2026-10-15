import Foundation
import XCTest

@testable import TcrBarCore

final class PanelSessionFilesTests: XCTestCase {
    @MainActor
    func testSessionFilesAreReadOffTheMainThread() async {
        let reading = await PanelSessionFiles.read(
            codexHome: "/Users/example/.codex", sessionIds: ["example-session"],
            join: CodexRolloutJoin(), now: Date(timeIntervalSince1970: 100),
            readClaude: {
                XCTAssertFalse(Thread.isMainThread)
                return ["claude-session": SessionFile(sessionId: "claude-session")]
            },
            walk: { home, ids in
                XCTAssertFalse(Thread.isMainThread)
                XCTAssertEqual(home, "/Users/example/.codex")
                XCTAssertEqual(ids, ["example-session"])
                return ["example-session": SessionFile(sessionId: "example-session")]
            })
        XCTAssertEqual(Set(reading.files.keys), ["claude-session", "example-session"])
    }
}
