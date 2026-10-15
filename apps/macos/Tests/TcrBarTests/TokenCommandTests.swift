import Foundation
import XCTest

@testable import TcrBarCore

/// "Copy Access Token" is a subprocess (`tcr token <name>`) whose stdout goes
/// straight to the pasteboard. These pin the classification: the one case a
/// user cannot tell apart from success is an EMPTY copy, so exit 0 with no
/// stdout must be a failure.
///
/// Token values are obviously fake — this repository is public.
final class TokenCommandTests: XCTestCase {

    func testArgumentsPassQueryPositionally() {
        XCTAssertEqual(TokenCommand.arguments(query: "alice@example.com"), ["token", "alice@example.com"])
        XCTAssertEqual(
            TokenCommand.arguments(query: "alice@example.com/acme"),
            ["token", "alice@example.com/acme"],
            "a qualified name is one argument, passed through whole")
    }

    func testExitZeroWithTokenIsSuccessTrimmed() {
        let out = Data("at-fake-alice\n".utf8)
        XCTAssertEqual(TokenCommand.classify(exitCode: 0, stdout: out, stderr: ""), .success("at-fake-alice"))
    }

    func testExitZeroWithEmptyStdoutIsFailure() {
        let result = TokenCommand.classify(exitCode: 0, stdout: Data(), stderr: "")
        guard case .failure(let failure) = result else {
            return XCTFail("an empty token must not reach the pasteboard")
        }
        XCTAssertEqual(failure.exitCode, 0)
        XCTAssertTrue(failure.summary.contains("no token"), failure.summary)
    }

    func testNonZeroExitCarriesStderrVerbatim() {
        let result = TokenCommand.classify(
            exitCode: 1, stdout: Data(), stderr: "Error: no account matches 'nobody'\n")
        XCTAssertEqual(
            result, .failure(TokenCommand.Failure(exitCode: 1, message: "Error: no account matches 'nobody'"))
        )
    }

    /// On a Codex row the item copies a ChatGPT bearer, which works only with its
    /// account-id header, and nothing else on the item says so. A Claude row's
    /// item keeps today's title and has no help, as today.
    func testTheCopyItemNamesChatGPTOnACodexRow() throws {
        let fleet = try decodeFleet(
            [canonicalRow("alice@example.com"), canonicalRow("bob@example.com/codex")])
        let alice = try XCTUnwrap(fleet.accounts.first { $0.name == "alice@example.com" })
        let bob = try XCTUnwrap(fleet.accounts.first { $0.name == "bob@example.com/codex" })
        XCTAssertEqual(TokenCommand.menuTitle(for: alice), "Copy Access Token")
        XCTAssertNil(TokenCommand.menuHelp(for: alice))
        XCTAssertEqual(TokenCommand.menuTitle(for: bob), "Copy ChatGPT Access Token")
        XCTAssertEqual(
            TokenCommand.menuHelp(for: bob),
            "Copies this account's ChatGPT bearer. It works only with the account's "
                + "chatgpt-account-id header, which the codex CLI sends for you.")
    }
}

/// One canonical row from apps/macos/Tests/Fixtures/codex-status.json, with `overrides`
/// written over its JSON object (`NSNull()` writes a JSON null).
private func canonicalRow(_ name: String, _ overrides: [String: Any] = [:]) throws -> [String: Any] {
    let url = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // TcrBarTests
        .deletingLastPathComponent()  // Tests
        .appendingPathComponent("Fixtures/codex-status.json")
    let rows = try XCTUnwrap(
        JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [[String: Any]])
    var row = try XCTUnwrap(rows.first { $0["name"] as? String == name }, "no canonical \(name)")
    for (key, value) in overrides { row[key] = value }
    return row
}

private func decodeFleet(_ rows: [[String: Any]]) throws -> Fleet {
    try Fleet.decode(JSONSerialization.data(withJSONObject: rows))
}
