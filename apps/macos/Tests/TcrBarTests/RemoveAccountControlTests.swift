import Foundation
import XCTest

@testable import TcrBarCore

/// Pure argument-building and classification for `tcr remove`. Same posture as
/// `GroupCommandTests`/`ControlAccountCommandTests`: nothing here spawns a
/// process — `ImageRenderer` cannot draw `Menu` contents at all, so the gear's
/// "Delete Account…" item is never covered by a snapshot either.
///
/// Account names are obviously fake — this repository is public.
final class RemoveAccountCommandTests: XCTestCase {

    private let alice = "alice@example.com"

    // MARK: - argument shape (a normal name, a name needing shell quoting, and
    // an org-qualified name)

    /// A normal query, passed positionally and verbatim.
    func testArgumentsWithAnOrdinaryName() {
        XCTAssertEqual(RemoveAccountCommand.arguments(query: alice), ["remove", alice])
    }

    /// `Process` takes an argument vector, not a shell command line, so a
    /// name containing characters a shell would need quoted (a space, a
    /// single quote) is still passed through completely unmodified — this
    /// is the one thing that must NOT happen here, unlike
    /// `GroupCommand.shellQuote`'s copy-to-clipboard text.
    func testArgumentsPassANameNeedingShellQuotingUnmodified() {
        XCTAssertEqual(
            RemoveAccountCommand.arguments(query: "o'brien's mac"),
            ["remove", "o'brien's mac"]
        )
    }

    /// A qualified name — the shape `tcr` gives the second of one person's two
    /// orgs — is one argument, passed through whole.
    func testArgumentsPassAQualifiedNameWhole() {
        XCTAssertEqual(
            RemoveAccountCommand.arguments(query: "\(alice)/acme"),
            ["remove", "\(alice)/acme"]
        )
    }

    // MARK: - classification

    func testCleanExitZeroNoStderr() {
        XCTAssertEqual(RemoveAccountCommand.classify(exitCode: 0, stderr: ""), .clean)
    }

    func testExitZeroWithStderrIsSpokeNotClean() {
        XCTAssertEqual(
            RemoveAccountCommand.classify(exitCode: 0, stderr: "warning: proxy too old for this route\n"),
            .spoke(notice: "warning: proxy too old for this route")
        )
    }

    func testNonZeroExitIsFailed() {
        let outcome = RemoveAccountCommand.classify(exitCode: 1, stderr: "error: no match for query\n")
        guard case .failed(let failure) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertEqual(failure.exitCode, 1)
        XCTAssertEqual(failure.message, "error: no match for query")
    }

    func testFailureSummaryIsNeverEmpty() {
        let failure = RemoveAccountCommand.Failure(exitCode: 1, message: "")
        XCTAssertEqual(failure.summary, "delete failed (exit 1): no output")
    }

    // MARK: - the confirmation's words
    //
    // Over the canonical fleet (apps/macos/Tests/Fixtures/codex-status.json): alice
    // is the one Claude row, and bob and carol are two of its nine Codex rows.

    /// The second paragraph every delete alert ends with: a delete that lands takes
    /// the row off the list on the next poll.
    private let stopsAtOnce =
        "The account stops serving at once and leaves this list. No restart is needed."

    /// What a Codex row's alert says first: it comes back through the Codex sign-in,
    /// not `tcr login`, and the codex CLI's own login stays where it is.
    private let codexFirst =
        "This removes “bob@example.com/codex” and its saved login from the config. It is "
        + "not reversible from here: getting it back means signing it in again "
        + "(Add account… › Codex account…). The codex CLI's own login is not touched."

    /// A Claude row's alert is today's, word for word.
    func testTheClaudeDeleteAlertIsTodays() throws {
        let fleet = try canonicalFleet()
        let alice = try XCTUnwrap(fleet.accounts.first { $0.name == "alice@example.com" })
        let copy = RemoveAccountCommand.confirmation(for: alice, in: fleet.accounts, deleted: [])
        XCTAssertEqual(copy.message, "Delete account “alice@example.com”?")
        XCTAssertEqual(
            copy.informative,
            "This removes “alice@example.com” and its saved credentials from the config. It "
                + "is not reversible from here: getting it back means running `tcr login` "
                + "again from scratch.\n\n" + stopsAtOnce)
    }

    /// A Codex row's alert names its own sign-in. bob is one of nine Codex rows
    /// here, none of them deleted, so this is not the last one.
    func testTheCodexDeleteAlertNamesItsLogin() throws {
        let fleet = try canonicalFleet()
        let bob = try XCTUnwrap(fleet.accounts.first { $0.name == "bob@example.com/codex" })
        let copy = RemoveAccountCommand.confirmation(for: bob, in: fleet.accounts, deleted: [])
        XCTAssertEqual(copy.message, "Delete account “bob@example.com/codex”?")
        XCTAssertEqual(copy.informative, codexFirst + "\n\n" + stopsAtOnce)
    }

    /// Deleting the last Codex account changes what codex does next, and the alert
    /// says so. A row whose delete already landed (it carries a delete notice) does
    /// not count; any other Codex row does, a disabled one included, because it is
    /// still in the config.
    func testTheLastCodexAccountSaysTcrStopsIntercepting() throws {
        let lastCodex =
            codexFirst + "\n\n" + stopsAtOnce + "\n\nThis is your last Codex account. tcr stops "
            + "intercepting chatgpt.com, and codex runs on its own login."
        let notLast = codexFirst + "\n\n" + stopsAtOnce
        func informative(_ rows: [[String: Any]], deleted: Set<String>) throws -> String {
            let fleet = try decodeFleet(rows)
            let bob = try XCTUnwrap(fleet.accounts.first { $0.name == "bob@example.com/codex" })
            return RemoveAccountCommand.confirmation(
                for: bob, in: fleet.accounts, deleted: deleted
            ).informative
        }
        let alice = try canonicalRow("alice@example.com")
        let bob = try canonicalRow("bob@example.com/codex")
        let carol = try canonicalRow("carol@example.com")
        let disabledCarol = try canonicalRow("carol@example.com", ["disabled": true])
        XCTAssertEqual(try informative([alice, bob], deleted: []), lastCodex)
        XCTAssertEqual(
            try informative([alice, bob, carol], deleted: ["carol@example.com"]), lastCodex,
            "a Codex row whose delete already landed still counted")
        XCTAssertEqual(try informative([alice, bob, carol], deleted: []), notLast)
        XCTAssertEqual(
            try informative([alice, bob, disabledCarol], deleted: []), notLast,
            "a disabled Codex row is still in the config, so bob is not the last one")
    }

    /// The panel asks the core for both lines of the alert, passes the names whose
    /// delete already landed, and still polls once a delete is accepted. Read from
    /// the source, because the alert is an `NSAlert` in the app target.
    func testThePanelAsksTheCoreForTheDeleteAlert() throws {
        let fleetView = try String(
            contentsOf: URL(fileURLWithPath: #filePath)
                .deletingLastPathComponent()  // TcrBarTests
                .deletingLastPathComponent()  // Tests
                .deletingLastPathComponent()  // apps/macos
                .deletingLastPathComponent()  // apps
                .deletingLastPathComponent()  // repo root
                .appendingPathComponent("apps/macos/Sources/TcrBar/FleetView.swift"),
            encoding: .utf8)
        let start = try XCTUnwrap(
            fleetView.range(of: "private func confirmDeleteAccount() {"),
            "confirmDeleteAccount() has moved or been renamed in FleetView.swift")
        let end = try XCTUnwrap(
            fleetView.range(
                of: "private func performCopyToken() async {",
                range: start.upperBound..<fleetView.endIndex),
            "performCopyToken() no longer follows confirmDeleteAccount() in FleetView.swift")
        let body = fleetView[start.upperBound..<end.lowerBound]
        for line in [
            "RemoveAccountCommand.confirmation(",
            "deleted: Set(removeController.notices.keys)",
            "if case .accepted = await removeController.remove(account: account.ref) {",
            "_ = await onChanged()",
        ] {
            XCTAssertTrue(body.contains(line), "confirmDeleteAccount() lost \(line)")
        }
        XCTAssertFalse(
            fleetView.contains("and its saved credentials from the config"),
            "the panel words the delete alert itself again, so a Codex row reads tcr login")
        XCTAssertFalse(fleetView.contains("removeController.removed"))
    }
}

/// The mutation controller's defaults. What it records after a call is in
/// `DeleteAccountPanelTests.swift`, which scripts the subprocess.
@MainActor
final class RemoveAccountControllerTests: XCTestCase {

    private let alice = AccountRef(name: "alice@example.com")

    func testNoNoticeByDefault() {
        let controller = RemoveAccountController()
        XCTAssertNil(controller.notice(for: alice))
    }

    func testNoFailureByDefault() {
        let controller = RemoveAccountController()
        XCTAssertNil(controller.failure(for: alice))
        XCTAssertFalse(controller.isPending(alice))
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

/// All ten canonical rows: alice (Claude, in orchard) and nine Codex rows.
private func canonicalFleet() throws -> Fleet {
    try decodeFleet(
        [
            "alice@example.com", "bob@example.com/codex", "carol@example.com", "dana@example.com",
            "erin@example.com/codex", "gil@example.com", "hal@example.com", "ivy@example.com",
            "jo@example.com", "kim@example.com",
        ].map { try canonicalRow($0) })
}
